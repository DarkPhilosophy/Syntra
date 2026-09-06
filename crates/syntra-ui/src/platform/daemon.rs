//! Starting and stopping a daemon from the dashboard.
//!
//! The dashboard is a client, not a host: it never runs the service in its own
//! process. It can, however, launch one, which is what happens when you open
//! the dashboard on a machine with no installed service.
//!
//! That capability used to exist only at start-up, so a user who uninstalled
//! the service while the dashboard was open was left with a window that could
//! never reach a daemon again and no way to start one. Everything needed to
//! launch a daemon therefore lives here, callable at any time.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::Duration;

use thiserror::Error;

use crate::platform::install;

/// How long to wait for a freshly started daemon to accept a connection.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// Interval between connection attempts while waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Why a daemon could not be started or stopped.
#[derive(Debug, Error)]
pub enum DaemonControlError {
    /// No daemon executable could be found.
    #[error(
        "syntra-daemon was not found; install Syntra, or place the daemon \
         next to the application"
    )]
    NotFound,
    /// The process could not be spawned.
    #[error("could not start the service: {0}")]
    Spawn(#[from] std::io::Error),
    /// The process started and then exited before answering.
    ///
    /// Carries what the daemon itself said, which is nearly always the
    /// actionable part: a port already in use, a missing permission, or a
    /// configuration it could not read.
    #[error("the service exited immediately ({status}){}",
        if detail.is_empty() { String::new() } else { format!(": {detail}") })]
    Exited {
        /// Exit status as reported by the operating system.
        status: String,
        /// Most useful line the daemon printed before exiting.
        detail: String,
    },
    /// The process started but never accepted a connection.
    #[error("the service started but did not become reachable within {0:?}")]
    NeverReady(Duration),
}

/// A daemon this process started, stopped when the dashboard exits.
///
/// A daemon started by an init system, or by hand, is never owned here and
/// keeps running after the dashboard closes.
static OWNED: Mutex<Option<Child>> = Mutex::new(None);

/// Locates a daemon executable to run.
///
/// The installed copy is preferred over the one beside the running binary, so
/// a dashboard launched from a build tree still starts the daemon the user
/// actually installed, rather than a stale sibling.
pub fn executable() -> Option<PathBuf> {
    let installed = install::status().ok().map(|paths| paths.daemon);
    let sibling = std::env::current_exe().ok().and_then(|path| {
        path.parent()
            .map(|dir| dir.join(install::DAEMON_EXECUTABLE))
    });
    [installed, sibling]
        .into_iter()
        .flatten()
        .find(|path| path.is_file())
}

/// Whether a daemon is currently answering the control socket.
pub fn is_running() -> bool {
    syntra_api::connect_with_timeout(Duration::from_millis(150)).is_ok()
}

/// Starts a daemon and waits until it accepts a connection.
///
/// Returns without starting anything if one is already answering, so pressing
/// the control twice cannot produce two daemons.
pub fn start() -> Result<(), DaemonControlError> {
    if is_running() {
        return Ok(());
    }
    let executable = executable().ok_or(DaemonControlError::NotFound)?;
    log::info!("starting service: {}", executable.display());
    // stderr is captured so a daemon that dies immediately can say why.
    // Reporting only "did not become reachable" leaves the user with nothing
    // to act on, when the daemon usually knows exactly what went wrong.
    let mut child = Command::new(&executable)
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let stderr = child.stderr.take();

    // Report readiness rather than optimism: the interface must not claim the
    // service is up before it can be reached.
    let deadline = std::time::Instant::now() + START_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if is_running() {
            if let Ok(mut owned) = OWNED.lock() {
                *owned = Some(child);
            }
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(DaemonControlError::Exited {
                status: status.to_string(),
                detail: read_failure(stderr),
            });
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    // Alive but not answering: keep it owned so it is not orphaned.
    if let Ok(mut owned) = OWNED.lock() {
        *owned = Some(child);
    }
    Err(DaemonControlError::NeverReady(START_TIMEOUT))
}

/// Extracts the most useful line the daemon printed before dying.
///
/// The last error line is preferred over the whole log, which is mostly
/// start-up noise that would bury the reason.
fn read_failure(stderr: Option<std::process::ChildStderr>) -> String {
    use std::io::Read;
    let Some(mut stderr) = stderr else {
        return String::new();
    };
    let mut output = String::new();
    let _ = stderr.read_to_string(&mut output);
    output
        .lines()
        .rev()
        .find(|line| line.contains("ERROR") || line.contains("error"))
        .or_else(|| output.lines().next_back())
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Daemon processes that are running but not answering the control socket.
///
/// A daemon can end up alive yet unreachable — its socket unlinked, or the
/// process wedged — and while it lives it holds the peer port, so nothing
/// else can start. Without a way to see and stop it the user is stuck.
///
/// Only processes owned by this user, executing a file named exactly like
/// our daemon, are reported; an unrelated program can never be listed.
#[cfg(target_os = "linux")]
pub fn unreachable_daemons() -> Vec<u32> {
    if is_running() {
        return Vec::new();
    }
    let own = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == own {
            continue;
        }
        // Resolving the executable also proves the process is ours: reading
        // another user's link fails with a permission error.
        let Ok(executable) = std::fs::read_link(format!("/proc/{pid}/exe")) else {
            continue;
        };
        if executable.file_name().and_then(|name| name.to_str()) == Some(install::DAEMON_EXECUTABLE)
        {
            found.push(pid);
        }
    }
    found
}

/// Daemon processes that are running but not answering the control socket.
#[cfg(not(target_os = "linux"))]
pub fn unreachable_daemons() -> Vec<u32> {
    Vec::new()
}

/// Stops every daemon that is running but not answering.
///
/// Returns how many were signalled. SIGTERM rather than SIGKILL, so the
/// daemon releases pressed keys and grabbed devices on its way out.
#[cfg(target_os = "linux")]
pub fn stop_unreachable() -> usize {
    let daemons = unreachable_daemons();
    for pid in &daemons {
        log::info!("stopping unreachable service with pid {pid}");
        // SAFETY: the pid was just read from /proc and belongs to this user,
        // proven by resolving its executable link.
        unsafe { libc::kill(*pid as libc::pid_t, libc::SIGTERM) };
    }
    daemons.len()
}

/// Stops every daemon that is running but not answering.
#[cfg(not(target_os = "linux"))]
pub fn stop_unreachable() -> usize {
    0
}

/// Stops a daemon this process started.
///
/// Returns `false` when no daemon is owned here, which means the running one
/// belongs to an init system or to the user and must be stopped the same way
/// it was started.
pub fn stop_owned() -> bool {
    let Ok(mut owned) = OWNED.lock() else {
        return false;
    };
    let Some(mut child) = owned.take() else {
        return false;
    };
    terminate(&mut child);
    true
}

/// Whether the running daemon was started by this dashboard.
pub fn owns_running_daemon() -> bool {
    OWNED.lock().map(|owned| owned.is_some()).unwrap_or(false)
}

/// Adopts a daemon started before the interface came up.
///
/// Called once at start-up so the guard covers a daemon spawned during
/// launch, which otherwise would not be stopped when the dashboard exits.
pub fn adopt(child: Child) {
    if let Ok(mut owned) = OWNED.lock() {
        *owned = Some(child);
    }
}

/// Stops an owned daemon, if any. Called as the dashboard exits.
pub fn shutdown() {
    stop_owned();
}

fn terminate(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    #[cfg(unix)]
    {
        // SIGINT lets the daemon release pressed keys and grabbed devices; a
        // hard kill would leave modifiers stuck on the remote machine.
        // SAFETY: the pid belongs to a child this process owns and has not
        // reaped, so it cannot have been recycled.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let _ = child.wait();
}
