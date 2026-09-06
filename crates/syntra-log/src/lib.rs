//! Runtime-reconfigurable logging shared by every Syntra process.
//!
//! The daemon runs unattended for days, so raising verbosity must never
//! require a restart: a restart discards exactly the state that reproduces the
//! bug. This crate therefore keeps the level filter in atomics that a running
//! process can rewrite at any moment, from any thread.
//!
//! Three things distinguish it from a plain `env_logger` setup:
//!
//! * **Per-subsystem levels.** [`Subsystem`] enumerates the parts of Syntra
//!   worth tuning separately. Turning `clipboard` up to `trace` leaves the
//!   input hot path at `warn`, so the interesting records are not buried.
//! * **Runtime control.** [`LogConfig::set_level`] and
//!   [`LogConfig::set_subsystem_level`] take effect immediately and are what
//!   the API exposes to a dashboard.
//! * **Non-blocking output.** Records are handed to a writer thread. A
//!   launcher or SSH session that stops draining stderr must never stall the
//!   service event loop.
//!
//! # Example
//!
//! ```
//! use syntra_log::{Subsystem, LogConfig};
//!
//! let config = LogConfig::default();
//! config.set_subsystem_level(Subsystem::Clipboard, Some(log::LevelFilter::Trace));
//! assert_eq!(config.level_for(Subsystem::Clipboard), log::LevelFilter::Trace);
//!
//! // Subsystems without an override keep following the global level.
//! config.set_level(log::LevelFilter::Warn);
//! assert_eq!(config.level_for(Subsystem::Input), log::LevelFilter::Warn);
//! assert_eq!(config.level_for(Subsystem::Clipboard), log::LevelFilter::Trace);
//! ```

mod logger;

pub use logger::{InstallError, Logger, Mirror, install};

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use log::LevelFilter;

/// A part of Syntra whose verbosity can be tuned independently.
///
/// Variants map onto log target prefixes, so `log::debug!(target: "clipboard", …)`
/// and any record from a `syntra_*clipboard*` module resolve to
/// [`Subsystem::Clipboard`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Subsystem {
    /// Pointer and keyboard capture and emulation.
    Input,
    /// Peer discovery, handshakes, DTLS and the wire protocol.
    Network,
    /// Clipboard observation and synchronisation.
    Clipboard,
    /// File offers, chunked transfers and their state machines.
    Transfer,
    /// Clipboard history storage and peer reconciliation.
    History,
    /// The control socket between the daemon and its clients.
    Ipc,
    /// Presentation code in a dashboard process.
    Ui,
    /// Syntra code not attributable to a specific subsystem.
    Other,
    /// Records emitted by third-party dependencies.
    ///
    /// Kept separate because their idea of `info` is not ours: D-Bus and
    /// windowing crates narrate every message, which would drown the
    /// application's own output at the default level.
    External,
}

impl Subsystem {
    /// Every tunable subsystem, in a stable order suitable for a UI listing.
    pub const ALL: [Subsystem; 9] = [
        Subsystem::Input,
        Subsystem::Network,
        Subsystem::Clipboard,
        Subsystem::Transfer,
        Subsystem::History,
        Subsystem::Ipc,
        Subsystem::Ui,
        Subsystem::Other,
        Subsystem::External,
    ];

    /// Stable lowercase identifier used on the wire and in configuration.
    pub const fn as_str(self) -> &'static str {
        match self {
            Subsystem::Input => "input",
            Subsystem::Network => "network",
            Subsystem::Clipboard => "clipboard",
            Subsystem::Transfer => "transfer",
            Subsystem::History => "history",
            Subsystem::Ipc => "ipc",
            Subsystem::Ui => "ui",
            Subsystem::Other => "other",
            Subsystem::External => "external",
        }
    }

    /// Classifies a `log` target into a subsystem.
    ///
    /// Targets outside the `syntra` crates are [`Subsystem::External`]
    /// regardless of their spelling. That test comes first because
    /// dependency module paths collide with our vocabulary — `zbus::connection`
    /// contains "connect" but is not our networking code.
    ///
    /// Within Syntra, matching is substring-based on purpose: module paths
    /// differ between crates (`syntra_core::clipboard`,
    /// `syntra_plugin_clipboard`) yet belong to the same subsystem from an
    /// operator's point of view.
    pub fn classify(target: &str) -> Subsystem {
        // Ordered by specificity: `history_sync` mentions transfers, and
        // capture/emulation modules mention clipboard, so the narrower
        // subsystems must be tested first.
        const RULES: [(&str, Subsystem); 12] = [
            ("history", Subsystem::History),
            ("transfer", Subsystem::Transfer),
            ("clipboard", Subsystem::Clipboard),
            ("capture", Subsystem::Input),
            ("emulation", Subsystem::Input),
            ("input", Subsystem::Input),
            ("proto", Subsystem::Network),
            ("discovery", Subsystem::Network),
            ("dns", Subsystem::Network),
            ("connect", Subsystem::Network),
            ("ipc", Subsystem::Ipc),
            ("ui", Subsystem::Ui),
        ];
        let target = target.to_ascii_lowercase();
        if !target.starts_with("syntra") {
            return Subsystem::External;
        }
        for (needle, subsystem) in RULES {
            if target.contains(needle) {
                return subsystem;
            }
        }
        Subsystem::Other
    }
}

impl fmt::Display for Subsystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Returned when a string does not name a known subsystem.
#[derive(Debug, PartialEq, Eq)]
pub struct UnknownSubsystem(String);

impl fmt::Display for UnknownSubsystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown log subsystem: `{}`", self.0)
    }
}

impl std::error::Error for UnknownSubsystem {}

impl FromStr for Subsystem {
    type Err = UnknownSubsystem;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Subsystem::ALL
            .into_iter()
            .find(|subsystem| subsystem.as_str().eq_ignore_ascii_case(value))
            .ok_or_else(|| UnknownSubsystem(value.to_owned()))
    }
}

/// Sentinel stored for "no override; follow the global level".
const INHERIT: u8 = u8::MAX;

const fn encode(level: LevelFilter) -> u8 {
    level as u8
}

fn decode(raw: u8) -> LevelFilter {
    match raw {
        0 => LevelFilter::Off,
        1 => LevelFilter::Error,
        2 => LevelFilter::Warn,
        3 => LevelFilter::Info,
        4 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    }
}

/// Live log configuration shared between the logger and its controllers.
///
/// Cloning shares the same atomics, so a clone handed to an IPC handler
/// controls the installed logger.
#[derive(Clone)]
pub struct LogConfig {
    inner: Arc<Inner>,
}

struct Inner {
    global: AtomicU8,
    /// Indexed by `Subsystem as usize`; [`INHERIT`] means "follow global".
    overrides: [AtomicU8; Subsystem::ALL.len()],
}

impl Default for LogConfig {
    fn default() -> Self {
        Self::new(LevelFilter::Info)
    }
}

impl fmt::Debug for LogConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogConfig")
            .field("global", &self.level())
            .field("overrides", &self.overrides())
            .finish()
    }
}

impl LogConfig {
    /// Creates a configuration with `global` applied to every subsystem.
    ///
    /// [`Subsystem::External`] is the one exception: dependencies start at
    /// `warn`, because a default of `info` buries the application's own
    /// output under D-Bus and windowing chatter. An explicit
    /// `external=<level>` in a spec overrides this.
    pub fn new(global: LevelFilter) -> Self {
        let config = Self {
            inner: Arc::new(Inner {
                global: AtomicU8::new(encode(global)),
                overrides: std::array::from_fn(|_| AtomicU8::new(INHERIT)),
            }),
        };
        config.set_subsystem_level(Subsystem::External, Some(LevelFilter::Warn));
        config
    }

    /// Parses a specification such as `info,clipboard=trace,input=off`.
    ///
    /// The first bare level becomes the global level; `name=level` pairs
    /// become overrides. Unparseable entries are skipped rather than failing:
    /// a typo in an environment variable must not prevent a service from
    /// starting.
    pub fn parse(spec: &str) -> Self {
        let config = Self::new(LevelFilter::Info);
        for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            match entry.split_once('=') {
                Some((name, level)) => {
                    if let (Ok(subsystem), Ok(level)) =
                        (name.trim().parse::<Subsystem>(), level.trim().parse())
                    {
                        config.set_subsystem_level(subsystem, Some(level));
                    }
                }
                None => {
                    if let Ok(level) = entry.parse() {
                        config.set_level(level);
                    }
                }
            }
        }
        config
    }

    /// Reads the configuration from `key`, falling back to `default_spec`.
    pub fn from_env(key: &str, default_spec: &str) -> Self {
        let spec = std::env::var(key).unwrap_or_else(|_| default_spec.to_owned());
        Self::parse(&spec)
    }

    /// The level applied to subsystems without an override.
    pub fn level(&self) -> LevelFilter {
        decode(self.inner.global.load(Ordering::Relaxed))
    }

    /// Replaces the global level. Takes effect on the next record.
    pub fn set_level(&self, level: LevelFilter) {
        self.inner.global.store(encode(level), Ordering::Relaxed);
        self.refresh_max_level();
    }

    /// The level currently in force for `subsystem`.
    pub fn level_for(&self, subsystem: Subsystem) -> LevelFilter {
        match self.inner.overrides[subsystem as usize].load(Ordering::Relaxed) {
            INHERIT => self.level(),
            raw => decode(raw),
        }
    }

    /// Overrides one subsystem, or clears the override with `None`.
    pub fn set_subsystem_level(&self, subsystem: Subsystem, level: Option<LevelFilter>) {
        let encoded = level.map_or(INHERIT, encode);
        self.inner.overrides[subsystem as usize].store(encoded, Ordering::Relaxed);
        self.refresh_max_level();
    }

    /// Every explicit override, for display and for round-tripping over IPC.
    pub fn overrides(&self) -> Vec<(Subsystem, LevelFilter)> {
        Subsystem::ALL
            .into_iter()
            .filter_map(|subsystem| {
                match self.inner.overrides[subsystem as usize].load(Ordering::Relaxed) {
                    INHERIT => None,
                    raw => Some((subsystem, decode(raw))),
                }
            })
            .collect()
    }

    /// Renders the configuration in the syntax [`LogConfig::parse`] accepts.
    pub fn to_spec(&self) -> String {
        let mut spec = self.level().to_string().to_ascii_lowercase();
        for (subsystem, level) in self.overrides() {
            spec.push_str(&format!(
                ",{subsystem}={}",
                level.to_string().to_ascii_lowercase()
            ));
        }
        spec
    }

    /// Whether a record with `level` from `target` should be emitted.
    pub fn enabled(&self, target: &str, level: log::Level) -> bool {
        level <= self.level_for(Subsystem::classify(target))
    }

    /// Keeps `log`'s global fast path in sync with the loosest active level.
    ///
    /// `log` short-circuits on `max_level` before a logger is consulted, so
    /// without this a raised subsystem level would be silently dropped.
    fn refresh_max_level(&self) {
        let max = Subsystem::ALL
            .into_iter()
            .map(|subsystem| self.level_for(subsystem))
            .max()
            .unwrap_or(LevelFilter::Info);
        log::set_max_level(max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Operators tune one noisy subsystem while the rest stays quiet; an
    /// override must not leak into its neighbours.
    #[test]
    fn override_applies_to_one_subsystem_only() {
        let config = LogConfig::new(LevelFilter::Warn);
        config.set_subsystem_level(Subsystem::Clipboard, Some(LevelFilter::Trace));

        assert_eq!(config.level_for(Subsystem::Clipboard), LevelFilter::Trace);
        assert_eq!(config.level_for(Subsystem::Input), LevelFilter::Warn);
    }

    /// Subsystems without an override must track later global changes,
    /// otherwise raising the global level would appear to do nothing.
    #[test]
    fn cleared_override_follows_later_global_changes() {
        let config = LogConfig::new(LevelFilter::Warn);
        config.set_subsystem_level(Subsystem::Input, Some(LevelFilter::Off));
        config.set_subsystem_level(Subsystem::Input, None);

        config.set_level(LevelFilter::Debug);

        assert_eq!(config.level_for(Subsystem::Input), LevelFilter::Debug);
        // `External` keeps its built-in default; only `Input` was cleared.
        assert!(
            !config
                .overrides()
                .iter()
                .any(|(subsystem, _)| *subsystem == Subsystem::Input)
        );
    }

    /// `log` consults `max_level` before the logger, so a raised subsystem
    /// must lift the global ceiling or its records never arrive.
    #[test]
    fn raising_a_subsystem_lifts_the_global_ceiling() {
        let config = LogConfig::new(LevelFilter::Error);
        config.set_subsystem_level(Subsystem::Transfer, Some(LevelFilter::Trace));

        assert_eq!(log::max_level(), LevelFilter::Trace);
        assert!(config.enabled("syntra_core::transfer_manager", log::Level::Trace));
        assert!(!config.enabled("syntra_core::capture", log::Level::Trace));
    }

    /// The spec is the wire and configuration format; it must survive a
    /// round trip or a saved configuration would drift from the live one.
    #[test]
    fn spec_round_trips() {
        let config = LogConfig::new(LevelFilter::Warn);
        config.set_subsystem_level(Subsystem::Network, Some(LevelFilter::Debug));

        let restored = LogConfig::parse(&config.to_spec());

        assert_eq!(restored.level(), LevelFilter::Warn);
        assert_eq!(restored.level_for(Subsystem::Network), LevelFilter::Debug);
    }

    /// A typo in an environment variable must not stop a service from
    /// starting, so unparseable entries are skipped, not fatal.
    #[test]
    fn malformed_entries_are_skipped() {
        let config = LogConfig::parse("debug,nonsense=trace,clipboard=notalevel,ipc=warn");

        assert_eq!(config.level(), LevelFilter::Debug);
        assert_eq!(config.level_for(Subsystem::Ipc), LevelFilter::Warn);
        assert_eq!(config.level_for(Subsystem::Clipboard), LevelFilter::Debug);
    }

    /// Classification drives every override, so the overlapping names that
    /// motivated the rule ordering are pinned here.
    #[test]
    fn classification_prefers_the_narrower_subsystem() {
        assert_eq!(
            Subsystem::classify("syntra_core::history_sync"),
            Subsystem::History
        );
        assert_eq!(
            Subsystem::classify("syntra_core::clipboard"),
            Subsystem::Clipboard
        );
        assert_eq!(
            Subsystem::classify("syntra_input_capture::libei"),
            Subsystem::Input
        );
        assert_eq!(
            Subsystem::classify("some::vendor::crate"),
            Subsystem::External
        );
    }

    /// A dependency module path may contain our vocabulary: `zbus::connection`
    /// contains "connect". Misclassifying it as our networking code is what
    /// flooded the dashboard log, so the crate-prefix test must win.
    #[test]
    fn dependency_targets_are_external_despite_familiar_names() {
        assert_eq!(Subsystem::classify("zbus::connection"), Subsystem::External);
        assert_eq!(Subsystem::classify("winit::input"), Subsystem::External);
        assert_eq!(
            Subsystem::classify("syntra_core::connect"),
            Subsystem::Network
        );
    }

    /// Dependencies must be quiet by default but still reachable, otherwise
    /// diagnosing a D-Bus problem would require a rebuild.
    #[test]
    fn dependencies_are_quiet_by_default_yet_tunable() {
        let config = LogConfig::new(LevelFilter::Info);
        assert!(!config.enabled("zbus::connection", log::Level::Info));
        assert!(config.enabled("zbus::connection", log::Level::Warn));

        let verbose = LogConfig::parse("info,external=debug");
        assert!(verbose.enabled("zbus::connection", log::Level::Debug));
    }
}
