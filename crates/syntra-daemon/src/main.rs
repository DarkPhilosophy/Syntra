//! Syntra daemon: the headless service that owns input capture, emulation,
//! peer transport, clipboard synchronisation, file transfers and history.
//!
//! This binary is installable as a system or user service and starts at boot.
//! It deliberately knows nothing about any user interface: clients — the
//! desktop dashboard, the CLI, and third-party plugins — reach it only over
//! the control socket described by [`syntra_api::paths`].

use std::future::Future;
use std::io;
use std::process;

use syntra_api::{IpcError, IpcListenerCreationError, paths};
use syntra_core::config::{Command, Config, ConfigError};
use syntra_core::service::{Service, ServiceError};
use syntra_core::{capture_test, emulation_test};
use syntra_input_capture::InputCaptureError;
use syntra_input_emulation::InputEmulationError;
use thiserror::Error;
use tokio::task::LocalSet;

/// Any fatal condition that terminates the daemon.
#[derive(Debug, Error)]
enum DaemonError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    Ipc(#[from] IpcError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Capture(#[from] InputCaptureError),
    #[error(transparent)]
    Emulation(#[from] InputEmulationError),
    #[error(transparent)]
    Cli(#[from] syntra_cli::CliError),
}

fn main() {
    let log_config = init_logging();
    if let Err(error) = run(log_config) {
        log::error!("{error}");
        // The writer thread owns the output, so the record would be lost if
        // the process exited first.
        log::logger().flush();
        process::exit(1);
    }
    log::logger().flush();
}

/// Installs the shared logger and returns its live configuration.
///
/// Records are mirrored to the diagnostics socket so a dashboard can render a
/// live log; the daemon neither knows nor cares whether anyone is listening.
/// The returned handle is what lets a client retune levels at runtime.
fn init_logging() -> syntra_log::LogConfig {
    #[cfg(unix)]
    let mirror = paths::diagnostics_socket()
        .map(syntra_log::Mirror::Datagram)
        .unwrap_or(syntra_log::Mirror::None);
    #[cfg(not(unix))]
    let mirror = syntra_log::Mirror::None;

    let config = syntra_log::LogConfig::from_env(paths::ENV_LOG, "info");
    syntra_log::install(config.clone(), mirror).expect("no logger is installed yet");
    config
}

fn run(log_config: syntra_log::LogConfig) -> Result<(), DaemonError> {
    let config = Config::new()?;
    match config.command() {
        Some(Command::TestEmulation(args)) => block_on(emulation_test::run(config, args)),
        Some(Command::TestCapture(args)) => block_on(capture_test::run(config, args)),
        Some(Command::Cli(args)) => block_on(syntra_cli::run(args)),
        // Running the service is the default: an installed unit invokes the
        // binary with no arguments.
        Some(Command::Daemon) | None => match block_on(serve(config, log_config)) {
            Err(DaemonError::Service(ServiceError::IpcListen(
                IpcListenerCreationError::AlreadyRunning,
            ))) => {
                log::info!("another Syntra daemon already owns the control socket");
                Ok(())
            }
            result => result,
        },
    }
}

/// Drives a future to completion on a current-thread runtime.
///
/// The service is intentionally single-threaded: capture, emulation and IPC
/// state is `!Send` and shared without locks, so a `LocalSet` is required.
fn block_on<F, E>(future: F) -> Result<(), DaemonError>
where
    F: Future<Output = Result<(), E>>,
    DaemonError: From<E>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    Ok(runtime.block_on(LocalSet::new().run_until(future))?)
}

async fn serve(config: Config, log_config: syntra_log::LogConfig) -> Result<(), ServiceError> {
    let release_bind = config.release_bind();
    let config_path = config.config_path().to_owned();
    let mut service = Service::new(config, log_config).await?;
    log::info!("using config: {config_path:?}");
    log::info!("press {release_bind:?} to release the pointer");
    service.run().await?;
    log::info!("daemon stopped");
    Ok(())
}
