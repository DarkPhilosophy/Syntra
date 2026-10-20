//! How a plugin process is started, contained and ended.
//!
//! A plugin is somebody else's program, so the daemon gives it no more than it
//! needs:
//!
//! * a **filtered environment**: only the variables a graphical or plugin
//!   program legitimately needs, never the daemon's whole environment, which
//!   may hold tokens and session secrets;
//! * its own **process group** (Unix), so ending the plugin ends every
//!   descendant it started, not just the process the daemon launched. On
//!   Windows there is no equivalent: job-object containment is not
//!   implemented ([`WINDOWS_JOB_CONTAINMENT`]), so only the launched process
//!   is ended there;
//! * **bounded diagnostics**: what it writes to its error stream is read in
//!   limited, sanitised lines kept in a small ring, never copied verbatim and
//!   in bulk into the daemon's log.
//!
//! None of this makes a plugin safe to run: approval is what decides whether it
//! runs at all. This is the containment of an approved plugin.

use std::{collections::VecDeque, ffi::OsString, time::Duration};

/// Environment variables passed through to a plugin, by exact name.
///
/// A graphical plugin needs to find the display and session it draws on; a
/// toolkit needs a home for caches and a locale. The list is closed: anything
/// not named here, or matching a prefix below, is not inherited.
const PASSED_VARIABLES: &[&str] = &[
    // Where files and programs live.
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "TMPDIR",
    // Language and rendering of text.
    "LANG",
    "LANGUAGE",
    "TZ",
    // Display servers, so a plugin can draw an overlay or read a clipboard.
    "DISPLAY",
    "XAUTHORITY",
    "WAYLAND_DISPLAY",
    "WAYLAND_SOCKET",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_ID",
    "XDG_DATA_DIRS",
    "XDG_CONFIG_DIRS",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    // The session bus, which portals and toolkits reach through.
    "DBUS_SESSION_BUS_ADDRESS",
    // Graphics stack selection.
    "XCURSOR_THEME",
    "XCURSOR_SIZE",
    "GDK_BACKEND",
    "QT_QPA_PLATFORM",
    "SLINT_BACKEND",
    "SLINT_STYLE",
    "LIBGL_ALWAYS_SOFTWARE",
    "MESA_LOADER_DRIVER_OVERRIDE",
    // Windows needs these to find its own system files.
    "SYSTEMROOT",
    "WINDIR",
    "APPDATA",
    "LOCALAPPDATA",
    "USERPROFILE",
    "TEMP",
    "TMP",
];

/// Prefixes passed through: locale categories and Syntra's own settings.
const PASSED_PREFIXES: &[&str] = &["LC_", "SYNTRA_PLUGIN_"];

/// Variables that are never passed even if a prefix would allow them.
///
/// Library-injection variables let one process run code inside another, so a
/// plugin must not inherit values that were meant for the daemon alone.
const NEVER_PASSED: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
];

/// Whether `name` is inherited by a plugin.
pub(crate) fn passes_variable(name: &str) -> bool {
    if NEVER_PASSED.contains(&name) {
        return false;
    }
    PASSED_VARIABLES.contains(&name)
        || PASSED_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// The environment a plugin starts with, from `source`.
///
/// Taking the source as a parameter keeps this a pure function the tests can
/// drive without touching the real environment.
pub(crate) fn filtered_environment(
    source: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    source
        .into_iter()
        .filter(|(name, _)| name.to_str().is_some_and(passes_variable))
        .collect()
}

/// How long a plugin gets to exit after being asked, before it is killed.
pub(crate) const GRACE: Duration = Duration::from_millis(1500);
/// How long the daemon waits for the kill to take effect and the process to
/// be reaped, so shutdown can never hang on one plugin.
pub(crate) const REAP: Duration = Duration::from_secs(3);

/// Most diagnostic lines kept per plugin.
pub(crate) const DIAGNOSTIC_LINES: usize = 20;
/// Longest diagnostic line kept, in characters.
pub(crate) const DIAGNOSTIC_WIDTH: usize = 200;

/// Makes one line of plugin output safe to keep and show.
///
/// Control characters (including escape sequences that could rewrite a
/// terminal) are dropped, the length is bounded, and nothing is parsed or
/// interpreted. The result is for a human to read; it is never fed back into
/// anything.
pub(crate) fn sanitize_line(line: &str) -> String {
    let mut text: String = line
        .chars()
        .filter(|character| !character.is_control() || *character == '\t')
        .map(|character| if character == '\t' { ' ' } else { character })
        .collect();
    let trimmed = text.trim().len();
    if trimmed != text.len() {
        text = text.trim().to_owned();
    }
    if text.chars().count() > DIAGNOSTIC_WIDTH {
        text = text.chars().take(DIAGNOSTIC_WIDTH).collect::<String>() + "…";
    }
    text
}

/// A small ring of a plugin's most recent diagnostic lines.
#[derive(Debug, Default, Clone)]
pub(crate) struct Diagnostics {
    lines: VecDeque<String>,
}

impl Diagnostics {
    /// Records one line, after sanitising it. Empty lines are dropped, and the
    /// oldest line makes room when the ring is full.
    pub(crate) fn push(&mut self, line: &str) {
        let line = sanitize_line(line);
        if line.is_empty() {
            return;
        }
        if self.lines.len() == DIAGNOSTIC_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// The recorded lines, oldest first.
    pub(crate) fn lines(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }

    /// Forgets everything, as when a plugin is started afresh.
    pub(crate) fn clear(&mut self) {
        self.lines.clear();
    }
}

/// Configures `command` so the plugin runs contained.
///
/// On Unix the child becomes the leader of a new process group, so a signal to
/// the group reaches everything it started. The caller must still call
/// [`terminate_group`] to end it. On other platforms this does nothing: there
/// is no Windows job-object containment (see [`WINDOWS_JOB_CONTAINMENT`]), so
/// only the process the daemon launched is ended, not its descendants.
pub(crate) fn contain(command: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = command;
    }
}

/// Sends `signal` to the whole process group led by `pid`.
///
/// `pid` must be a process this daemon launched with [`contain`]: a group id
/// equals its leader's pid only because the leader was made one. A pid that is
/// zero or does not fit is refused, so this can never signal the daemon's own
/// group or every process of the user.
#[cfg(unix)]
pub(crate) fn signal_group(pid: u32, signal: libc::c_int) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    if pid <= 1 {
        return;
    }
    // SAFETY: `kill` with a negative pid addresses the process group `-pid`;
    // it takes no pointers and has no memory-safety preconditions.
    unsafe {
        libc::kill(-pid, signal);
    }
}

/// Asks the whole group to end, politely.
#[cfg(unix)]
pub(crate) fn terminate_group(pid: u32) {
    signal_group(pid, libc::SIGTERM);
}

/// Ends the whole group, without asking.
#[cfg(unix)]
pub(crate) fn kill_group(pid: u32) {
    signal_group(pid, libc::SIGKILL);
}

/// Whether a plugin's descendants are ended with it on this platform.
///
/// `false` where there is no process group: Windows job-object containment is
/// **not implemented** (it needs the Win32 job-object calls, which this crate
/// does not link). There the daemon ends only the process it launched, and a
/// descendant a plugin started may outlive it. Nothing reports this at run
/// time; it is a stated limit of the Windows build.
pub(crate) const WINDOWS_JOB_CONTAINMENT: bool = false;

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    fn names(filtered: &[(OsString, OsString)]) -> Vec<String> {
        filtered
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_variables_a_graphical_plugin_needs_are_passed() {
        let filtered = filtered_environment(env(&[
            ("DISPLAY", ":0"),
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
            ("HOME", "/home/u"),
            ("PATH", "/usr/bin"),
            ("LC_ALL", "C"),
            ("SYNTRA_PLUGIN_THEME", "dark"),
        ]));
        assert_eq!(filtered.len(), 8, "{:?}", names(&filtered));
    }

    #[test]
    fn secrets_and_injection_variables_are_not_passed() {
        let filtered = filtered_environment(env(&[
            ("AWS_SECRET_ACCESS_KEY", "x"),
            ("GITHUB_TOKEN", "x"),
            ("SSH_AUTH_SOCK", "/tmp/agent"),
            ("LD_PRELOAD", "/evil.so"),
            ("LD_LIBRARY_PATH", "/evil"),
            ("DYLD_INSERT_LIBRARIES", "/evil"),
            ("SYNTRA_SECRET", "x"),
            ("DISPLAY", ":0"),
        ]));
        assert_eq!(names(&filtered), ["DISPLAY"]);
    }

    /// A prefix must not let an injection variable through by a side door.
    #[test]
    fn the_deny_list_wins_over_a_prefix() {
        assert!(!passes_variable("LD_PRELOAD"));
        assert!(passes_variable("LC_MESSAGES"));
        assert!(!passes_variable("LCX"));
    }

    #[test]
    fn diagnostics_are_sanitised_bounded_and_oldest_first() {
        let mut diagnostics = Diagnostics::default();
        diagnostics.push("\u{1b}[31mred\u{1b}[0m text\twith tab");
        diagnostics.push("   ");
        diagnostics.push(&"x".repeat(DIAGNOSTIC_WIDTH * 3));
        let lines = diagnostics.lines();
        assert_eq!(lines.len(), 2, "blank lines are dropped");
        assert_eq!(lines[0], "[31mred[0m text with tab");
        assert!(!lines[0].contains('\u{1b}'), "no escape characters survive");
        assert!(lines[1].chars().count() <= DIAGNOSTIC_WIDTH + 1);

        for number in 0..(DIAGNOSTIC_LINES * 2) {
            diagnostics.push(&format!("line {number}"));
        }
        let lines = diagnostics.lines();
        assert_eq!(lines.len(), DIAGNOSTIC_LINES);
        assert_eq!(
            lines.last().unwrap(),
            &format!("line {}", DIAGNOSTIC_LINES * 2 - 1)
        );
        diagnostics.clear();
        assert!(diagnostics.lines().is_empty());
    }

    /// The group signal must refuse the pids that would hit the wrong
    /// processes: 0 is the caller's own group, 1 is init.
    #[cfg(unix)]
    #[test]
    fn signalling_refuses_pids_that_would_reach_unrelated_processes() {
        // These must be no-ops; if they were not, this test process would be
        // signalled and the run would end.
        signal_group(0, libc::SIGKILL);
        signal_group(1, libc::SIGKILL);
        signal_group(u32::MAX, libc::SIGKILL);
    }
}
