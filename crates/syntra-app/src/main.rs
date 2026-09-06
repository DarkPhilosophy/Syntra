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
    // The dashboard writes to stderr only; the daemon owns the diagnostics
    // mirror that the in-app log viewer reads.
    let log_config = syntra_log::LogConfig::from_env(paths::ENV_LOG, "info");
    let _ = syntra_log::install(log_config, syntra_log::Mirror::None);

    // D-Bus and tray integrations expect a reactor while Slint owns the main
    // thread, so the runtime must outlive the window.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            log::error!("could not start the async runtime: {error}");
            process::exit(1);
        }
    };
    let _guard = runtime.enter();

    let startup = match parse_startup() {
        Ok(startup) => startup,
        Err(message) => {
            eprintln!("{message}");
            process::exit(2);
        }
    };

    ensure_daemon();
    let result = syntra_ui::app::run_with_startup(startup);
    // Stops only a daemon this process started; an installed service or one
    // the user launched keeps running.
    syntra_ui::platform::daemon::shutdown();
    if let Err(error) = result {
        log::error!("{error}");
        // The writer thread owns the output; flush before the process dies.
        log::logger().flush();
        process::exit(1);
    }
    log::logger().flush();
}

/// Parses the dashboard's only command-line option.
///
/// Kept hand-written rather than pulling in an argument parser: the
/// dashboard has one flag, and everything else is configured through the
/// daemon or the settings page.
fn parse_startup() -> Result<syntra_ui::app::StartupMode, String> {
    let mut startup = syntra_ui::app::StartupMode::Window;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--background" | "-b" => startup = syntra_ui::app::StartupMode::Background,
            "--help" | "-h" => {
                println!(
                    "Syntra dashboard\n\n\
                     Usage: syntra [OPTIONS]\n\n\
                     Options:\n  \
                       -b, --background  Start without a window, reachable from the tray\n  \
                       -h, --help        Show this message\n\n\
                     The background service is `syntra-daemon`; this binary is its dashboard."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown option `{other}`; try --help")),
        }
    }
    Ok(startup)
}

/// Makes a best effort to have a daemon available, without blocking start-up.
///
/// A daemon this process starts is owned by
/// [`syntra_ui::platform::daemon`], which stops it when the dashboard exits
/// and can start another later. That matters because the service may be
/// uninstalled while the window is open, and the user must still be able to
/// start one from the interface.
fn ensure_daemon() {
    if syntra_api::connect_with_timeout(PROBE_TIMEOUT).is_ok() {
        log::info!("attached to a running Syntra daemon");
        return;
    }
    if let Ok(status) = syntra_ui::platform::service::query() {
        if status.installed {
            if let Err(error) = syntra_ui::platform::service::start() {
                log::warn!("could not start the installed Syntra service: {error}");
            }
            return;
        }
    }
    // Not fatal: the dashboard opens regardless and offers to start one.
    if let Err(error) = syntra_ui::platform::daemon::start() {
        log::warn!("could not start a Syntra daemon: {error}");
    }
}
