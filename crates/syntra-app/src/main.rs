//! Syntra desktop application: the dashboard client.
//!
//! The window always opens, whether or not a daemon is reachable. When no
//! daemon answers the control socket the dashboard runs in a degraded state
//! and keeps retrying in the background; it attaches as soon as one appears
//! and survives daemon restarts.
//!
//! This binary is a pure client. It links the presentation crate and the IPC
//! contract, never the daemon core, which keeps the dashboard and the service
//! independently installable.

use std::process;
use std::time::Duration;

use syntra_api::paths;

/// How long to wait for an already-running daemon before deciding to start one.
const PROBE_TIMEOUT: Duration = Duration::from_millis(150);

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().filter_or(paths::ENV_LOG, "info"))
        .init();

    // D-Bus and tray integrations expect a reactor while Slint owns the main
    // thread, so the runtime must outlive the window.
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            log::error!("could not start the async runtime: {error}");
            process::exit(1);
        }
    };
    let _guard = runtime.enter();

    let daemon = ensure_daemon();
    if let Err(error) = syntra_ui::app::run() {
        log::error!("{error}");
        process::exit(1);
    }
    drop(daemon);
}

/// Makes a best effort to have a daemon available, without ever blocking startup.
///
/// Returns a guard that stops the daemon on exit only when this process
/// started it. An installed service, or a daemon started by the user, keeps
/// running after the dashboard closes.
fn ensure_daemon() -> Option<OwnedDaemon> {
    if syntra_api::connect_with_timeout(PROBE_TIMEOUT).is_ok() {
        log::info!("attached to a running Syntra daemon");
        return None;
    }
    match syntra_ui::platform::service::query() {
        Ok(status) if status.installed => {
            if let Err(error) = syntra_ui::platform::service::start() {
                log::warn!("could not start the installed Syntra service: {error}");
            }
            None
        }
        _ => match spawn_daemon() {
            Ok(child) => Some(OwnedDaemon(child)),
            Err(error) => {
                // A missing daemon is not fatal: the dashboard still opens and
                // reports the disconnected state to the user.
                log::warn!("could not start a Syntra daemon: {error}");
                None
            }
        },
    }
}

fn spawn_daemon() -> std::io::Result<std::process::Child> {
    let executable = std::env::current_exe()?
        .parent()
        .map(|dir| dir.join(DAEMON_EXECUTABLE))
        .filter(|path| path.exists())
        .unwrap_or_else(|| DAEMON_EXECUTABLE.into());
    log::info!("starting daemon: {}", executable.display());
    process::Command::new(executable).spawn()
}

#[cfg(windows)]
const DAEMON_EXECUTABLE: &str = "syntra-daemon.exe";
#[cfg(not(windows))]
const DAEMON_EXECUTABLE: &str = "syntra-daemon";

/// Stops a daemon that this process started, leaving foreign ones untouched.
struct OwnedDaemon(std::process::Child);

impl Drop for OwnedDaemon {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(Some(_))) {
            return;
        }
        #[cfg(unix)]
        {
            // SIGINT lets the daemon release pressed keys and grabbed devices;
            // a hard kill would leave modifiers stuck on the remote machine.
            // SAFETY: the pid belongs to a child this process owns and has not
            // reaped, so it cannot have been recycled.
            unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGINT) };
        }
        #[cfg(not(unix))]
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
