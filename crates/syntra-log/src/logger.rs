//! Installation of the process-wide logger.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc::{SyncSender, sync_channel};

use log::{Log, Metadata, Record};

use crate::{LogConfig, Subsystem};

/// Bounded queue depth for the writer thread.
///
/// A burst larger than this drops records rather than blocking the caller:
/// losing log lines is always preferable to stalling input forwarding.
const QUEUE_DEPTH: usize = 1024;

/// Where a logger sends formatted records in addition to stderr.
#[derive(Debug, Default)]
pub enum Mirror {
    /// Write to stderr only.
    #[default]
    None,
    /// Also send each record to a Unix datagram socket.
    ///
    /// This is how the daemon publishes a live log without depending on any
    /// dashboard: readers come and go, and an absent reader costs one failed
    /// non-blocking `send_to`.
    #[cfg(unix)]
    Datagram(PathBuf),
}

/// A logger whose levels can be changed while the process runs.
pub struct Logger {
    config: LogConfig,
    sink: SyncSender<String>,
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.config.enabled(metadata.target(), metadata.level())
    }

    fn log(&self, record: &Record<'_>) {
        // `log` only consults the global ceiling, so the per-subsystem filter
        // has to be applied here as well.
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "[{}][{:<5}][{}] {}\n",
            humantime::format_rfc3339_millis(std::time::SystemTime::now()),
            record.level(),
            Subsystem::classify(record.target()),
            record.args()
        );
        // Never block: a full queue means the consumer is wedged, and the
        // caller may be the input hot path.
        let _ = self.sink.try_send(line);
    }

    fn flush(&self) {}
}

/// Failure to install the process-wide logger.
#[derive(Debug, thiserror::Error)]
#[error("a logger has already been installed for this process")]
pub struct InstallError(#[from] log::SetLoggerError);

/// Installs the process-wide logger and returns its live configuration.
///
/// The returned [`LogConfig`] shares state with the installed logger, so
/// changing a level through it takes effect immediately. Hand it to whatever
/// exposes log control — for the daemon, the API request handler.
///
/// # Errors
///
/// Returns [`InstallError`] if a logger was already installed.
pub fn install(config: LogConfig, mirror: Mirror) -> Result<LogConfig, InstallError> {
    let (sink, records) = sync_channel::<String>(QUEUE_DEPTH);

    // A dedicated thread owns stderr so that a launcher or SSH session which
    // stops draining the pipe cannot stall the service event loop.
    let _ = std::thread::Builder::new()
        .name("syntra-log".into())
        .spawn(move || {
            #[cfg(unix)]
            let datagram = match &mirror {
                Mirror::Datagram(path) => std::os::unix::net::UnixDatagram::unbound()
                    .and_then(|socket| {
                        socket.set_nonblocking(true)?;
                        Ok(socket)
                    })
                    .ok()
                    .map(|socket| (socket, path.clone())),
                Mirror::None => None,
            };
            #[cfg(not(unix))]
            let _ = &mirror;

            let stderr = io::stderr();
            let mut output = stderr.lock();
            while let Ok(line) = records.recv() {
                #[cfg(unix)]
                if let Some((socket, path)) = &datagram {
                    let _ = socket.send_to(line.as_bytes(), path);
                }
                if output.write_all(line.as_bytes()).is_err() {
                    break;
                }
            }
        });

    let logger = Logger {
        config: config.clone(),
        sink,
    };
    log::set_boxed_logger(Box::new(logger))?;
    // Publish the ceiling only after the logger exists, so no record slips
    // through against a default filter.
    config.set_level(config.level());
    Ok(config)
}
