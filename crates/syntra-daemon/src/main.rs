//! Syntra daemon: the headless service that owns input capture, emulation,
//! peer transport, clipboard synchronisation, file transfers and history.
//!
//! This binary is installable as a system or user service and starts at boot.
//! It deliberately knows nothing about any user interface: clients — the
//! desktop dashboard, the CLI, and third-party plugins — reach it only over
//! the control socket described by [`syntra_api::paths`].

use std::future::Future;
use std::io::{self, Write};
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
    init_logging();
    if let Err(error) = run() {
        log::error!("{error}");
        process::exit(1);
    }
}

/// Installs a logger that never blocks the service event loop.
///
/// Records are handed to a dedicated writer thread and, on Unix, mirrored to
/// the diagnostics datagram socket so a dashboard can render a live log
/// without the daemon depending on it. A launcher or SSH pipe that stops
/// draining stderr must not stall input, network or IPC processing.
fn init_logging() {
    use env_logger::{Env, Target};

    let env = Env::default().filter_or(paths::ENV_LOG, "info");

    #[cfg(unix)]
    let diagnostics = std::os::unix::net::UnixDatagram::unbound()
        .and_then(|socket| {
            socket.set_nonblocking(true)?;
            Ok(socket)
        })
        .ok()
        .zip(paths::diagnostics_socket().ok());

    let (tx, rx) = std::sync::mpsc::sync_channel::<String>(256);
    let _ = std::thread::Builder::new()
        .name("syntra-log-output".into())
        .spawn(move || {
            let stderr = io::stderr();
            let mut output = stderr.lock();
            while let Ok(line) = rx.recv() {
                if output.write_all(line.as_bytes()).is_err() {
                    break;
                }
            }
        });

    let mut logger = env_logger::Builder::from_env(env);
    // The writer thread owns stderr; env_logger must not lock it as well.
    logger.target(Target::Pipe(Box::new(io::sink())));
    logger.format(move |buf, record| {
        let line = format!(
            "[{}][{}][{}] {}\n",
            buf.timestamp_millis(),
            record.level(),
            record.target(),
            record.args()
        );
        #[cfg(unix)]
        if let Some((socket, path)) = &diagnostics {
            let _ = socket.send_to(line.as_bytes(), path);
        }
        let _ = tx.try_send(line);
        Ok(())
    });
    logger.init();
}

fn run() -> Result<(), DaemonError> {
    let config = Config::new()?;
    match config.command() {
        Some(Command::TestEmulation(args)) => block_on(emulation_test::run(config, args)),
        Some(Command::TestCapture(args)) => block_on(capture_test::run(config, args)),
        Some(Command::Cli(args)) => block_on(syntra_cli::run(args)),
        // Running the service is the default: an installed unit invokes the
        // binary with no arguments.
        Some(Command::Daemon) | None => match block_on(serve(config)) {
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

async fn serve(config: Config) -> Result<(), ServiceError> {
    let release_bind = config.release_bind();
    let config_path = config.config_path().to_owned();
    let mut service = Service::new(config).await?;
    log::info!("using config: {config_path:?}");
    log::info!("press {release_bind:?} to release the pointer");
    service.run().await?;
    log::info!("daemon stopped");
    Ok(())
}
