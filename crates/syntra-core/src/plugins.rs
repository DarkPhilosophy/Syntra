//! The daemon's plugin registry.
//!
//! Plugins are separate processes that add optional capabilities. The daemon
//! is the master: it owns the list, decides what is installed, and starts or
//! stops the processes. Clients render this state and ask for changes; they
//! never touch a plugin process themselves.
//!
//! Plugins are **discovered from manifests**, not hard-coded. A manifest is a
//! JSON file matching `plugins/manifest.schema.json` that declares the
//! executable plus the provenance a user needs in order to decide whether to
//! trust it: author, source, homepage, licence, version. A plugin sees
//! clipboard contents and file paths, so that information is part of the
//! contract rather than a README.
//!
//! Two directories are searched, in order:
//!
//! 1. next to the daemon executable — where bundled plugins live;
//! 2. `<config>/plugins` — where a user drops third-party plugins.
//!
//! A user manifest with the same id as a bundled one wins, so a plugin can be
//! replaced without touching the installation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use syntra_api::{PluginHealth, PluginStatus};
use syntra_plugin_api::PluginMetadata;

/// Identifier of the bundled clipboard plugin.
pub(crate) const CLIPBOARD_PLUGIN_ID: &str = "clipboard";
/// Identifier of the bundled FUSE plugin.
pub(crate) const FUSE_PLUGIN_ID: &str = "fuse";

/// Manifest version this daemon understands.
const SUPPORTED_MANIFEST_VERSION: u32 = 1;

/// Plugin protocol version this daemon speaks.
///
/// Mirrors [`syntra_plugin_api::PROTOCOL_VERSION`]. A plugin declaring a
/// different one is still listed, so the user can see why it misbehaves
/// instead of finding an unexplained absence.
const SUPPORTED_PROTOCOL_VERSION: u32 = syntra_plugin_api::PROTOCOL_VERSION;

/// How long a launched plugin may take to complete its handshake.
///
/// Past this the process exists but is not answering, which is a different
/// and more useful statement than "starting" repeated indefinitely.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Restarts within a session after which a plugin is treated as unhealthy
/// rather than merely restarting.
const CRASH_LOOP_RESTARTS: u32 = 3;

/// A plugin manifest as found on disk.
#[derive(Debug, Default, Deserialize)]
struct Manifest {
    manifest_version: u32,
    #[serde(default)]
    protocol_version: u32,
    id: String,
    name: String,
    executable: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    update_url: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    bundled: bool,
    /// Started only when something needs it, rather than kept running.
    #[serde(default)]
    on_demand: bool,
    #[serde(default)]
    capabilities: Capabilities,
}

/// The capability block of a manifest; only the parts the daemon surfaces.
#[derive(Debug, Default, Deserialize)]
struct Capabilities {
    #[serde(default)]
    mime_types: Vec<String>,
}

/// A discovered plugin and where it came from.
struct Discovered {
    manifest: Manifest,
    /// Build the plugin reported at handshake; empty until it connects.
    build_fingerprint: String,
    manifest_path: PathBuf,
    executable: PathBuf,
}

/// Tracks discovered plugins and which the user has switched on.
pub(crate) struct PluginRegistry {
    /// Discovery order is display order.
    plugins: Vec<Discovered>,
    /// User preference per plugin id. Absent means enabled.
    disabled: HashMap<String, bool>,
    /// Ids whose process has completed its handshake.
    running: HashMap<String, bool>,
    /// Ids whose process has been launched but has not handshaken yet.
    /// When a launch was observed, so a start that never completes cannot
    /// stay pending for ever.
    starting: HashMap<String, Instant>,
    /// Restart count per id for this session.
    restarts: HashMap<String, u32>,
    /// Most recent failure reported for a plugin.
    failures: HashMap<String, String>,
    /// Process id of the running plugin, so the interface can prove the
    /// entry corresponds to a real process rather than a file on disk.
    pids: HashMap<String, u32>,
}

/// Executables Syntra ships with.
///
/// The only thing the daemon states about its own plugins is which file to
/// run: it cannot discover a binary it was built to launch. Everything else
/// — name, description, author, whether it is one-shot — is read from that
/// binary, so the daemon holds no second copy that could disagree with it.
const BUILTIN_EXECUTABLES: [(&str, &str); 2] = [
    (CLIPBOARD_PLUGIN_ID, "syntra-plugin-clipboard"),
    (FUSE_PLUGIN_ID, "syntra-plugin-fuse"),
];

/// Applies a plugin's own declaration over whatever is currently held.
///
/// Empty fields mean "not declared", so a plugin written against the earlier
/// message shape still contributes what it does say. Behavioural facts are
/// always taken from the plugin: only it knows whether it is one-shot.
fn apply_declared(manifest: &mut Manifest, declared: PluginMetadata) {
    if !declared.description.is_empty() {
        manifest.description = declared.description;
    }
    if !declared.version.is_empty() {
        manifest.version = declared.version;
    }
    if !declared.author.is_empty() {
        manifest.author = declared.author;
    }
    if declared.homepage.is_some() {
        manifest.homepage = declared.homepage;
    }
    if declared.source.is_some() {
        manifest.source = declared.source;
    }
    if declared.update_url.is_some() {
        manifest.update_url = declared.update_url;
    }
    if declared.license.is_some() {
        manifest.license = declared.license;
    }
    manifest.on_demand = declared.on_demand;
    manifest.bundled = declared.bundled;
}

fn builtin(directory: &Path) -> Vec<Discovered> {
    BUILTIN_EXECUTABLES
        .into_iter()
        .map(|(id, executable)| {
            let executable = if cfg!(windows) {
                format!("{executable}.exe")
            } else {
                executable.to_owned()
            };
            let path = directory.join(&executable);
            let mut manifest = Manifest {
                manifest_version: SUPPORTED_MANIFEST_VERSION,
                protocol_version: SUPPORTED_PROTOCOL_VERSION,
                id: id.to_owned(),
                executable,
                ..Manifest::default()
            };
            let mut fingerprint = String::new();
            // Ask the binary to describe itself. A plugin that has never run
            // would otherwise be listed with no description at all, and an
            // on-demand one would be indistinguishable from a stopped one.
            if let Some((name, declared)) = describe(&path) {
                manifest.name = name;
                fingerprint = declared.build_fingerprint.clone();
                apply_declared(&mut manifest, declared);
            }
            Discovered {
                manifest,
                // No manifest file backs a built-in entry.
                manifest_path: PathBuf::new(),
                executable: path,
                build_fingerprint: fingerprint,
            }
        })
        .collect()
}

/// Runs a plugin with `--describe` and reads the declaration it prints.
///
/// Cheap and bounded: the plugin prints one line and exits. Any failure is
/// silent, since a plugin that is not installed simply has nothing to say.
fn describe(executable: &Path) -> Option<(String, PluginMetadata)> {
    if !executable.is_file() {
        return None;
    }
    let output = std::process::Command::new(executable)
        .arg("--describe")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let line = String::from_utf8(output.stdout).ok()?;
    match syntra_plugin_api::Message::decode_line(line.trim_end()) {
        Ok(syntra_plugin_api::Message::Hello { name, metadata, .. }) => {
            Some((name, metadata.unwrap_or_default()))
        }
        _ => None,
    }
}

impl PluginRegistry {
    /// Builds the registry: built-in plugins first, then discovered manifests.
    ///
    /// Bundled plugins are known intrinsically, not found on disk. Shipping
    /// their description as a loose file meant it could be missing, stale or
    /// hand-edited, and the plugin was then reported wrongly for as long as
    /// the file survived — a one-shot plugin read as merely stopped because
    /// an installed file predated the field that says it is one-shot.
    ///
    /// A manifest beside the daemon therefore does NOT override a built-in.
    /// That directory is ours: anything there is a leftover of an earlier
    /// install, and letting it win reintroduces the very staleness this
    /// removes.
    ///
    /// Manifests remain how a THIRD-PARTY plugin is discovered, since the
    /// daemon cannot know about one it was not built with. A manifest in the
    /// user directory may still replace a built-in entry, because putting
    /// one there is a deliberate substitution rather than an accident.
    ///
    /// Whatever the source, the plugin's own handshake supersedes it.
    pub(crate) fn discover(daemon_executable: &Path, user_directory: Option<PathBuf>) -> Self {
        let directory = daemon_executable
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let mut plugins: Vec<Discovered> = builtin(&directory);
        let builtin_ids: Vec<String> = plugins
            .iter()
            .map(|plugin| plugin.manifest.id.clone())
            .collect();

        for (directory, may_replace_builtin) in [(Some(directory), false), (user_directory, true)] {
            let Some(directory) = directory else { continue };
            for discovered in read_directory(&directory) {
                let id = discovered.manifest.id.clone();
                if !may_replace_builtin && builtin_ids.contains(&id) {
                    log::debug!("ignoring redundant manifest for built-in plugin `{id}`");
                    continue;
                }
                if let Some(existing) = plugins.iter_mut().find(|plugin| plugin.manifest.id == id) {
                    *existing = discovered;
                } else {
                    plugins.push(discovered);
                }
            }
        }

        Self {
            plugins,
            disabled: HashMap::new(),
            running: HashMap::new(),
            starting: HashMap::new(),
            restarts: HashMap::new(),
            failures: HashMap::new(),
            pids: HashMap::new(),
        }
    }

    /// Absolute path of a plugin's executable, whether or not it exists.
    pub(crate) fn executable(&self, id: &str) -> Option<PathBuf> {
        self.plugins
            .iter()
            .find(|plugin| plugin.manifest.id == id)
            .map(|plugin| plugin.executable.clone())
    }

    /// Whether the user has this plugin switched on.
    ///
    /// Unknown ids are reported as disabled, so a stale or hostile client
    /// cannot make the daemon launch something it never discovered.
    pub(crate) fn is_enabled(&self, id: &str) -> bool {
        self.plugins.iter().any(|plugin| plugin.manifest.id == id)
            && !self.disabled.get(id).copied().unwrap_or(false)
    }

    /// Records a user preference. Returns `false` for an unknown id.
    pub(crate) fn set_enabled(&mut self, id: &str, enabled: bool) -> bool {
        if !self.plugins.iter().any(|plugin| plugin.manifest.id == id) {
            return false;
        }
        self.disabled.insert(id.to_owned(), !enabled);
        if !enabled {
            self.running.insert(id.to_owned(), false);
        }
        true
    }

    /// Records that a plugin's process has become ready, or has stopped.
    pub(crate) fn set_running(&mut self, id: &str, running: bool) {
        self.running.insert(id.to_owned(), running);
        if running {
            self.starting.remove(id);
            self.failures.remove(id);
        }
    }

    /// Records that a plugin's process has been launched, with its pid.
    ///
    /// Only the supervisor calls this, and only once a process genuinely
    /// exists. Health is derived from observed processes, never from an
    /// intention to start one.
    pub(crate) fn set_starting(&mut self, id: &str, pid: u32) {
        self.starting.insert(id.to_owned(), Instant::now());
        self.running.insert(id.to_owned(), false);
        self.pids.insert(id.to_owned(), pid);
    }

    /// Records that a plugin stopped because it was asked to.
    ///
    /// Clears the process state without a failure, so the interface shows a
    /// stopped plugin rather than a broken one.
    pub(crate) fn set_stopped(&mut self, id: &str) {
        self.running.insert(id.to_owned(), false);
        self.starting.remove(id);
        self.failures.remove(id);
        self.pids.remove(id);
    }

    /// Replaces a plugin's description with what the plugin itself declared.
    ///
    /// A manifest file is only how a plugin is discovered. Once the process
    /// speaks, its own description wins: a file beside a binary can be
    /// stale, hand-edited or absent, and a plugin described by a stale file
    /// is reported wrongly for as long as that file survives. This is what
    /// made a one-shot plugin read as perpetually starting.
    pub(crate) fn adopt_declared(&mut self, id: &str, declared: PluginMetadata) {
        let Some(plugin) = self
            .plugins
            .iter_mut()
            .find(|plugin| plugin.manifest.id == id)
        else {
            return;
        };
        plugin.build_fingerprint = declared.build_fingerprint.clone();
        apply_declared(&mut plugin.manifest, declared);
    }

    /// Whether this plugin only runs while something needs it.
    pub(crate) fn is_on_demand(&self, id: &str) -> bool {
        self.plugins
            .iter()
            .any(|plugin| plugin.manifest.id == id && plugin.manifest.on_demand)
    }

    /// Records that a plugin's process ended.
    ///
    /// For an on-demand plugin an exit is the normal end of its work, not a
    /// fault: the FUSE helper serves one transfer and leaves. Recording that
    /// as a failure left a red state and a "Starting" badge that nothing
    /// could ever clear, because nothing relaunches it until the next
    /// transfer.
    pub(crate) fn set_exited(&mut self, id: &str, reason: impl Into<String>) {
        if self.is_on_demand(id) {
            self.set_stopped(id);
            return;
        }
        self.set_failed(id, reason);
    }

    /// Records a failure reported for a plugin.
    pub(crate) fn set_failed(&mut self, id: &str, reason: impl Into<String>) {
        self.failures.insert(id.to_owned(), reason.into());
        self.running.insert(id.to_owned(), false);
        self.starting.remove(id);
        // The process is gone; keeping its pid would advertise a dead one.
        self.pids.remove(id);
    }

    /// Counts a restart requested by a client, used to spot a crash loop.
    ///
    /// The pid is cleared rather than kept: the old process is on its way out
    /// and the supervisor reports the new one when it launches, so showing
    /// the previous pid meanwhile would be a lie.
    /// Counts a restart requested by a client, used to spot a crash loop.
    ///
    /// This records only that the old process is going away. It must NOT
    /// claim the plugin is starting: an on-demand plugin is not relaunched
    /// until something needs it, and marking an intention as a state left it
    /// reading "Starting" forever with nothing able to clear the flag.
    pub(crate) fn record_restart(&mut self, id: &str) {
        *self.restarts.entry(id.to_owned()).or_default() += 1;
        self.starting.remove(id);
        self.running.insert(id.to_owned(), false);
        self.pids.remove(id);
        self.failures.remove(id);
    }

    /// Health of one plugin, derived from its connection to the daemon.
    ///
    /// `Healthy` means the plugin completed its handshake over the stdio
    /// contract and is answering. It is never inferred from an executable
    /// existing on disk: a file that is present but never connects is not a
    /// working plugin, and reporting it as one is exactly the lie this state
    /// machine exists to prevent. `installed` only distinguishes "declared
    /// but absent" from "declared and launchable".
    fn health(&self, id: &str, installed: bool, enabled: bool, on_demand: bool) -> PluginHealth {
        if !enabled {
            return PluginHealth::Disabled;
        }
        if self.failures.contains_key(id) {
            return PluginHealth::Failed;
        }
        // Answering the handshake is the only evidence of health.
        if self.running.get(id).copied().unwrap_or(false) {
            let restarts = self.restarts.get(id).copied().unwrap_or(0);
            // A plugin that keeps coming back answers, but does not work.
            if restarts >= CRASH_LOOP_RESTARTS {
                return PluginHealth::Unresponsive;
            }
            return PluginHealth::Healthy;
        }
        if let Some(since) = self.starting.get(id) {
            // A launch that never handshakes must not stay pending for ever;
            // past the deadline the process exists but is not answering.
            return if since.elapsed() < HANDSHAKE_TIMEOUT {
                PluginHealth::Starting
            } else {
                PluginHealth::Unresponsive
            };
        }
        if !installed {
            return PluginHealth::NotInstalled;
        }
        if on_demand {
            return PluginHealth::OnDemand;
        }
        PluginHealth::Stopped
    }

    /// Snapshot for the interface, in discovery order.
    pub(crate) fn snapshot(&self) -> Vec<PluginStatus> {
        self.plugins
            .iter()
            .map(|plugin| {
                let installed = plugin.executable.is_file();
                let enabled = self.is_enabled(&plugin.manifest.id);
                PluginStatus {
                    id: plugin.manifest.id.clone(),
                    name: plugin.manifest.name.clone(),
                    description: plugin.manifest.description.clone(),
                    version: plugin.manifest.version.clone(),
                    protocol_version: plugin.manifest.protocol_version,
                    supported_protocol_version: SUPPORTED_PROTOCOL_VERSION,
                    health: self.health(
                        &plugin.manifest.id,
                        installed,
                        enabled,
                        plugin.manifest.on_demand,
                    ),
                    pid: self.pids.get(&plugin.manifest.id).copied(),
                    restarts: self.restarts.get(&plugin.manifest.id).copied().unwrap_or(0),
                    author: plugin.manifest.author.clone(),
                    homepage: plugin.manifest.homepage.clone(),
                    source: plugin.manifest.source.clone(),
                    update_url: plugin.manifest.update_url.clone(),
                    license: plugin.manifest.license.clone(),
                    mime_types: plugin.manifest.capabilities.mime_types.clone(),
                    bundled: plugin.manifest.bundled,
                    manifest_path: plugin.manifest_path.display().to_string(),
                    build_fingerprint: plugin.build_fingerprint.clone(),
                    daemon_build_fingerprint: syntra_plugin_api::BUILD_FINGERPRINT.to_owned(),
                    executable: plugin.executable.display().to_string(),
                    installed,
                    enabled,
                    running: installed
                        && enabled
                        && self
                            .running
                            .get(&plugin.manifest.id)
                            .copied()
                            .unwrap_or(false),
                    error: self.failures.get(&plugin.manifest.id).cloned().or_else(|| {
                        (!installed).then(|| {
                            format!(
                                "{} was not found; the plugin is declared but not installed",
                                plugin.executable.display()
                            )
                        })
                    }),
                }
            })
            .collect()
    }
}
/// Reads every `*.json` manifest in one directory.
fn read_directory(directory: &Path) -> Vec<Discovered> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        match read_manifest(&path) {
            Ok(discovered) => found.push(discovered),
            Err(error) => log::warn!("ignoring plugin manifest {}: {error}", path.display()),
        }
    }
    // Directory order is arbitrary; sort so the list does not reshuffle
    // between runs.
    found.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
    found
}

/// Parses one manifest and resolves its executable relative to the manifest.
fn read_manifest(path: &Path) -> Result<Discovered, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let manifest: Manifest = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    if manifest.manifest_version != SUPPORTED_MANIFEST_VERSION {
        return Err(format!(
            "manifest version {} is not supported (expected {SUPPORTED_MANIFEST_VERSION})",
            manifest.manifest_version
        ));
    }
    if manifest.id.is_empty() || manifest.executable.is_empty() {
        return Err("manifest must declare a non-empty id and executable".into());
    }
    // Resolve relative to the manifest so a plugin directory is relocatable.
    let executable = if Path::new(&manifest.executable).is_absolute() {
        PathBuf::from(&manifest.executable)
    } else {
        path.parent()
            .unwrap_or(Path::new("."))
            .join(&manifest.executable)
    };
    Ok(Discovered {
        manifest,
        build_fingerprint: String::new(),
        manifest_path: path.to_path_buf(),
        executable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes an executable that answers `--describe` like a real plugin.
    ///
    /// A stub that prints nothing is not a plugin: the daemon reads every
    /// description from the binary, so a test using an inert file would be
    /// asserting against an absence rather than against behaviour.
    fn describing_stub(directory: &Path, executable: &str, id: &str, on_demand: bool) {
        let hello = format!(
            r#"{{"type":"hello","data":{{"protocol_version":1,"adapter_id":"{id}",
               "name":"Stub {id}","capabilities":{{"clipboard_read":true,"paste":true,
               "cancel":true,"requires_live_mount":true,"mime_types":["text/uri-list"]}},
               "metadata":{{"description":"stub","version":"9.9.9","author":"Test",
               "bundled":true,"on_demand":{on_demand},"build_fingerprint":"test"}}}}}}"#
        )
        .replace('\n', "")
        .replace("               ", "");
        let path = directory.join(executable);
        std::fs::write(&path, format!("#!/bin/sh\nprintf '%s' '{hello}'\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// The registry always contains the built-in plugins, so tests select the
    /// entry they mean rather than relying on position or total count.
    fn entry(registry: &PluginRegistry, id: &str) -> PluginStatus {
        registry
            .snapshot()
            .into_iter()
            .find(|plugin| plugin.id == id)
            .unwrap_or_else(|| panic!("no plugin with id {id}"))
    }

    fn write(directory: &Path, name: &str, contents: &str) {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(directory.join(name), contents).unwrap();
    }

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("syntra-plugins-{name}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    const THIRD_PARTY_ID: &str = "acme-scanner";

    const BUNDLED_MANIFEST: &str = r#"{
        "manifest_version": 1,
        "protocol_version": 1,
        "id": "acme-scanner",
        "name": "Acme scanner",
        "version": "1.0.0",
        "author": "Syntra",
        "source": "https://example.invalid/src",
        "bundled": true,
        "executable": "syntra-plugin-clipboard",
        "capabilities": { "clipboard_read": true, "paste": true, "cancel": true,
                          "mime_types": ["text/uri-list"] }
    }"#;

    /// Metadata is the whole point of the manifest: a user decides whether to
    /// trust a plugin from it, so it must reach the interface intact.
    #[test]
    fn manifest_metadata_reaches_the_snapshot() {
        let directory = temp_dir("metadata");
        write(&directory, "clipboard.json", BUNDLED_MANIFEST);

        let registry = PluginRegistry::discover(&directory.join("syntra-daemon"), None);
        let plugin = entry(&registry, THIRD_PARTY_ID);

        assert_eq!(plugin.id, THIRD_PARTY_ID);
        assert_eq!(plugin.version, "1.0.0");
        assert_eq!(plugin.author, "Syntra");
        assert_eq!(
            plugin.source.as_deref(),
            Some("https://example.invalid/src")
        );
        assert_eq!(plugin.mime_types, vec!["text/uri-list".to_owned()]);
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// One malformed third-party file must not stop the daemon or hide the
    /// plugins that are fine.
    #[test]
    fn a_malformed_manifest_is_skipped_not_fatal() {
        let directory = temp_dir("malformed");
        write(&directory, "clipboard.json", BUNDLED_MANIFEST);
        write(&directory, "broken.json", "{ not json");

        let registry = PluginRegistry::discover(&directory.join("syntra-daemon"), None);

        // The good manifest still describes its plugin; the broken file is
        // simply absent from the result.
        assert_eq!(entry(&registry, THIRD_PARTY_ID).author, "Syntra");
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A manifest from a future version must be ignored rather than
    /// half-understood.
    #[test]
    fn an_unsupported_manifest_version_is_rejected() {
        let directory = temp_dir("version");
        write(
            &directory,
            "future.json",
            &BUNDLED_MANIFEST.replace("\"manifest_version\": 1", "\"manifest_version\": 99"),
        );

        let registry = PluginRegistry::discover(&directory.join("syntra-daemon"), None);

        // Rejected outright: a manifest from a future version is not
        // half-understood, so the plugin is simply not discovered.
        assert!(
            !registry
                .snapshot()
                .iter()
                .any(|plugin| plugin.id == THIRD_PARTY_ID)
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A user copy replaces the bundled plugin of the same id, so a plugin can
    /// be swapped without modifying the installation.
    #[test]
    fn a_user_manifest_overrides_the_bundled_one_with_the_same_id() {
        let bundled = temp_dir("override-bundled");
        let user = temp_dir("override-user");
        write(&bundled, "clipboard.json", BUNDLED_MANIFEST);
        write(
            &user,
            "clipboard.json",
            &BUNDLED_MANIFEST
                .replace("\"author\": \"Syntra\"", "\"author\": \"Third party\"")
                .replace("\"bundled\": true", "\"bundled\": false"),
        );

        let registry = PluginRegistry::discover(&bundled.join("syntra-daemon"), Some(user.clone()));
        let clipboard = entry(&registry, THIRD_PARTY_ID);

        assert_eq!(
            registry
                .snapshot()
                .iter()
                .filter(|plugin| plugin.id == THIRD_PARTY_ID)
                .count(),
            1,
            "the id must not appear twice"
        );
        assert_eq!(clipboard.author, "Third party");
        assert!(!clipboard.bundled);
        std::fs::remove_dir_all(bundled).unwrap();
        std::fs::remove_dir_all(user).unwrap();
    }

    /// An id the daemon never discovered must not be launchable, otherwise a
    /// client could name an arbitrary executable.
    #[test]
    fn unknown_plugins_are_neither_enabled_nor_settable() {
        let mut registry = PluginRegistry::discover(Path::new("/nonexistent/syntra-daemon"), None);

        assert!(!registry.is_enabled("something-else"));
        assert!(!registry.set_enabled("something-else", true));
    }

    /// Disabling must clear the running flag, or the interface would show a
    /// switched-off plugin as still running.
    #[test]
    fn disabling_reports_the_plugin_as_stopped() {
        let directory = temp_dir("disable");
        write(&directory, "clipboard.json", BUNDLED_MANIFEST);
        let mut registry = PluginRegistry::discover(&directory.join("syntra-daemon"), None);
        registry.set_running(CLIPBOARD_PLUGIN_ID, true);

        assert!(registry.set_enabled(CLIPBOARD_PLUGIN_ID, false));

        let clipboard = entry(&registry, CLIPBOARD_PLUGIN_ID);
        assert!(!clipboard.enabled);
        assert!(!clipboard.running);
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// An executable sitting on disk is not a working plugin. Health must
    /// come from the plugin answering the daemon, never from a file being
    /// present, or the interface would claim a capability that does not work.
    #[test]
    fn an_installed_executable_alone_is_never_reported_healthy() {
        let directory = temp_dir("presence-is-not-health");
        write(&directory, "clipboard.json", BUNDLED_MANIFEST);
        // Create the executable the manifest names, so it counts as installed.
        describing_stub(
            &directory,
            "syntra-plugin-clipboard",
            "gtk-clipboard",
            false,
        );

        let registry = PluginRegistry::discover(&directory.join("syntra-daemon"), None);
        let plugin = entry(&registry, THIRD_PARTY_ID);

        assert!(plugin.installed, "precondition: the executable exists");
        assert!(!plugin.running);
        assert_eq!(plugin.health, PluginHealth::Stopped);
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Restarting an on-demand plugin must not leave it claiming to start.
    /// Nothing relaunches it until a transfer needs it, so a "starting" flag
    /// set from intention rather than an observed process stuck for ever.
    #[test]
    fn restarting_an_on_demand_plugin_returns_it_to_on_demand() {
        let directory = temp_dir("on-demand-restart");
        write(
            &directory,
            "fuse.json",
            &BUNDLED_MANIFEST
                .replace("\"id\": \"acme-scanner\"", "\"id\": \"fuse\"")
                .replace(
                    "\"bundled\": true",
                    "\"bundled\": true, \"on_demand\": true",
                )
                .replace("syntra-plugin-clipboard", "syntra-plugin-fuse"),
        );
        describing_stub(&directory, "syntra-plugin-fuse", "fuse", true);
        let mut registry = PluginRegistry::discover(&directory.join("syntra-daemon"), None);

        registry.record_restart(FUSE_PLUGIN_ID);

        let plugin = entry(&registry, FUSE_PLUGIN_ID);
        assert_eq!(plugin.health, PluginHealth::OnDemand);
        assert!(plugin.pid.is_none(), "a stopped process has no pid");
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A one-shot plugin exiting has finished its work, not failed. Treating
    /// it as a fault left a red state and a badge nothing could clear,
    /// because nothing relaunches it until the next transfer.
    #[test]
    fn an_on_demand_plugin_exiting_is_not_a_failure() {
        let directory = temp_dir("on-demand-exit");
        write(
            &directory,
            "fuse.json",
            &BUNDLED_MANIFEST
                .replace("\"id\": \"acme-scanner\"", "\"id\": \"fuse\"")
                .replace(
                    "\"bundled\": true",
                    "\"bundled\": true, \"on_demand\": true",
                )
                .replace("syntra-plugin-clipboard", "syntra-plugin-fuse"),
        );
        describing_stub(&directory, "syntra-plugin-fuse", "fuse", true);
        let mut registry = PluginRegistry::discover(&directory.join("syntra-daemon"), None);

        registry.set_starting(FUSE_PLUGIN_ID, 4242);
        registry.set_exited(FUSE_PLUGIN_ID, "exited with status 0");

        let plugin = entry(&registry, FUSE_PLUGIN_ID);
        assert_eq!(plugin.health, PluginHealth::OnDemand);
        assert!(plugin.error.is_none(), "a normal exit is not an error");
        assert!(plugin.pid.is_none());
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A persistent plugin exiting IS a failure, and must keep saying why.
    #[test]
    fn a_persistent_plugin_exiting_is_reported_as_failed() {
        let directory = temp_dir("persistent-exit");
        write(&directory, "clipboard.json", BUNDLED_MANIFEST);
        describing_stub(
            &directory,
            "syntra-plugin-clipboard",
            "gtk-clipboard",
            false,
        );
        let mut registry = PluginRegistry::discover(&directory.join("syntra-daemon"), None);

        registry.set_exited(CLIPBOARD_PLUGIN_ID, "killed by signal 9");

        let plugin = entry(&registry, CLIPBOARD_PLUGIN_ID);
        assert_eq!(plugin.health, PluginHealth::Failed);
        assert_eq!(plugin.error.as_deref(), Some("killed by signal 9"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A leftover manifest beside the daemon must not override what the
    /// daemon knows about its own plugins. An installed file that predated
    /// `on_demand` made a one-shot plugin report as merely stopped, which is
    /// exactly the staleness built-ins exist to remove.
    #[test]
    fn a_manifest_beside_the_daemon_cannot_override_a_builtin() {
        let directory = temp_dir("builtin-not-overridden");
        // An older install: same id, no on_demand field.
        write(
            &directory,
            "syntra-plugin-fuse.json",
            r#"{
                "manifest_version": 1,
                "protocol_version": 1,
                "id": "fuse",
                "name": "Stale name",
                "executable": "syntra-plugin-fuse",
                "capabilities": { "clipboard_read": false, "paste": true,
                                  "cancel": true, "mime_types": ["text/uri-list"] }
            }"#,
        );

        describing_stub(&directory, "syntra-plugin-fuse", "fuse", true);

        let registry = PluginRegistry::discover(&directory.join("syntra-daemon"), None);
        let fuse = entry(&registry, FUSE_PLUGIN_ID);

        assert_eq!(fuse.health, PluginHealth::OnDemand);
        assert_ne!(fuse.name, "Stale name");
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A manifest the user placed deliberately still substitutes a built-in,
    /// which is how a plugin is replaced without touching the installation.
    #[test]
    fn a_user_manifest_still_substitutes_a_builtin() {
        let install = temp_dir("substitute-install");
        let user = temp_dir("substitute-user");
        write(
            &user,
            "fuse.json",
            r#"{
                "manifest_version": 1,
                "protocol_version": 1,
                "id": "fuse",
                "name": "My own receiver",
                "executable": "my-receiver",
                "capabilities": { "clipboard_read": false, "paste": true,
                                  "cancel": true, "mime_types": ["text/uri-list"] }
            }"#,
        );

        let registry = PluginRegistry::discover(&install.join("syntra-daemon"), Some(user.clone()));
        let fuse = entry(&registry, FUSE_PLUGIN_ID);

        assert_eq!(fuse.name, "My own receiver");
        std::fs::remove_dir_all(install).unwrap();
        std::fs::remove_dir_all(user).unwrap();
    }
}
