use env_logger::{Env, Target};
use input_capture::InputCaptureError;
use input_emulation::InputEmulationError;
use lan_mouse::{
    capture_test,
    config::{self, Command, Config, ConfigError},
    emulation_test,
    service::{Service, ServiceError},
};
use lan_mouse_cli::CliError;
use lan_mouse_ipc::{IpcError, IpcListenerCreationError};
#[cfg(feature = "slint-ui")]
use std::process::Child;
#[cfg(feature = "slint-ui")]
use std::time::Duration;
use std::{
    future::Future,
    io::{self, Write},
    process,
};
#[cfg(unix)]
use std::{os::unix::net::UnixDatagram, path::PathBuf};
use thiserror::Error;
use tokio::task::LocalSet;

#[derive(Debug, Error)]
enum LanMouseError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    IpcError(#[from] IpcError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Capture(#[from] InputCaptureError),
    #[error(transparent)]
    Emulation(#[from] InputEmulationError),
    #[cfg(feature = "slint-ui")]
    #[error("slint frontend: {0}")]
    Slint(String),
    #[error(transparent)]
    Cli(#[from] CliError),
}

fn main() {
    let env = Env::default().filter_or(
        "LAN_MOUSE_LOG_LEVEL",
        "info/clipboard|Clipboard|file transfer|CopyManifest|RemoteManifest|RangeRequest|RangeResponse|MountReady|PublishFileClipboard|PasteDestination|Cancelled|Completed|Unmounted",
    );
    #[cfg(unix)]
    let console = UnixDatagram::unbound()
        .and_then(|socket| {
            socket.set_nonblocking(true)?;
            Ok(socket)
        })
        .ok();
    #[cfg(unix)]
    let console_path = clipboard_console_path();
    // A launcher/SSH pipe may stop draining stderr. Logging must never block
    // the service's single-threaded input, network, or IPC event loop.
    let (stderr_tx, stderr_rx) = std::sync::mpsc::sync_channel::<String>(256);
    let _ = std::thread::Builder::new()
        .name("syntra-log-output".into())
        .spawn(move || {
            let stderr = io::stderr();
            let mut output = stderr.lock();
            while let Ok(line) = stderr_rx.recv() {
                if output.write_all(line.as_bytes()).is_err() {
                    break;
                }
            }
        });
    let mut logger = env_logger::Builder::from_env(env);
    // The formatter queues output itself; env_logger must not also lock stderr.
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
        if let Some(console) = &console {
            let _ = console.send_to(line.as_bytes(), &console_path);
        }
        let _ = stderr_tx.try_send(line);
        Ok(())
    });
    logger.init();

    if let Err(e) = run() {
        log::error!("{e}");
        process::exit(1);
    }
}

#[cfg(unix)]
fn clipboard_console_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("lan-mouse-clipboard-console.sock")
}

fn run() -> Result<(), LanMouseError> {
    let config = config::Config::new()?;
    match config.command() {
        Some(command) => match command {
            Command::TestEmulation(args) => run_async(emulation_test::run(config, args))?,
            Command::TestCapture(args) => run_async(capture_test::run(config, args))?,
            Command::Cli(cli_args) => run_async(lan_mouse_cli::run(cli_args))?,
            Command::Daemon => {
                // if daemon is specified we run the service
                match run_async(run_service(config)) {
                    Err(LanMouseError::Service(ServiceError::IpcListen(
                        IpcListenerCreationError::AlreadyRunning,
                    ))) => log::info!("service already running!"),
                    r => r?,
                }
            }
        },
        None => {
            //  otherwise start the service as a child process and
            //  run a frontend
            #[cfg(feature = "slint-ui")]
            {
                // D-Bus dependencies share Tokio through workspace feature unification.
                // Keep their reactor alive while Slint owns the main-thread event loop.
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?;
                let _runtime_context = runtime.enter();
                let mut owned_service = None;
                let connection =
                    match lan_mouse_ipc::connect_with_timeout(Duration::from_millis(150)) {
                        Ok(connection) => Ok(connection),
                        Err(_) => {
                            if lan_mouse_ui::platform::service::query()
                                .is_ok_and(|status| status.installed)
                            {
                                lan_mouse_ui::platform::service::start()
                                    .map_err(|error| LanMouseError::Slint(error.to_string()))?;
                            } else {
                                owned_service = Some(start_service()?);
                            }
                            lan_mouse_ipc::connect_with_timeout(Duration::from_secs(10))
                        }
                    };
                let res = connection
                    .map_err(|error| LanMouseError::Slint(error.to_string()))
                    .and_then(|(reader, writer)| {
                        lan_mouse_ui::app::run_with_ipc(reader, writer)
                            .map_err(|error| LanMouseError::Slint(error.to_string()))
                    });
                let stop = match owned_service.as_mut() {
                    Some(child) => stop_service(child),
                    None => Ok(()),
                };
                res?;
                stop?;
            }
            #[cfg(not(feature = "slint-ui"))]
            {
                match run_async(run_service(config)) {
                    Err(LanMouseError::Service(ServiceError::IpcListen(
                        IpcListenerCreationError::AlreadyRunning,
                    ))) => log::info!("service already running!"),
                    r => r?,
                }
            }
        }
    }

    Ok(())
}

#[cfg(feature = "slint-ui")]
fn stop_service(service: &mut Child) -> Result<(), io::Error> {
    if service.try_wait()?.is_some() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        let pid = service.id() as libc::pid_t;
        unsafe {
            libc::kill(pid, libc::SIGINT);
        }
    }
    #[cfg(not(unix))]
    {
        service.kill()?;
    }
    let _ = service.wait()?;
    Ok(())
}

fn run_async<F, E>(f: F) -> Result<(), LanMouseError>
where
    F: Future<Output = Result<(), E>>,
    LanMouseError: From<E>,
{
    // create single threaded tokio runtime
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    // run async event loop
    Ok(runtime.block_on(LocalSet::new().run_until(f))?)
}

#[cfg(feature = "slint-ui")]
fn start_service() -> Result<Child, io::Error> {
    let child = process::Command::new(std::env::current_exe()?)
        .args(std::env::args().skip(1))
        .arg("daemon")
        .spawn()?;
    Ok(child)
}

async fn run_service(config: Config) -> Result<(), ServiceError> {
    let release_bind = config.release_bind();
    let config_path = config.config_path().to_owned();
    let mut service = Service::new(config).await?;
    log::info!("using config: {config_path:?}");
    log::info!("Press {release_bind:?} to release the mouse");
    service.run().await?;
    log::info!("service exited!");
    Ok(())
}
