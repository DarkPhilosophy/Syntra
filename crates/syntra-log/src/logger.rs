//! Installation of the process-wide logger.

use std::io::{self, Write};
#[cfg(unix)]
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

/// A record handed to the writer thread.
enum Entry {
    /// A formatted line to emit.
    Line(String),
    /// A barrier: the writer acknowledges once every earlier line is written.
    Flush(SyncSender<()>),
}

/// A logger whose levels can be changed while the process runs.
pub struct Logger {
    config: LogConfig,
    sink: SyncSender<Entry>,
    /// Another logger that also receives every record that passes the filter
    /// (Android's logcat: stderr goes nowhere there).
    tee: Option<Box<dyn Log>>,
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
        if let Some(tee) = &self.tee {
            tee.log(record);
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
        let _ = self.sink.try_send(Entry::Line(line));
    }

    /// Blocks until every queued record has been written.
    ///
    /// Handing records to a writer thread means a process that exits
    /// immediately after logging would lose them — precisely the fatal error
    /// that explains why it exited. Call this before terminating.
    fn flush(&self) {
        if let Some(tee) = &self.tee {
            tee.flush();
        }
        let (ack, done) = sync_channel(0);
        if self.sink.send(Entry::Flush(ack)).is_ok() {
            let _ = done.recv();
        }
    }
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
    install_with_tee(config, mirror, None)
}

/// [`install`], additionally forwarding every record to `tee`.
///
/// # Errors
///
/// Returns [`InstallError`] if a logger was already installed.
pub fn install_with_tee(
    config: LogConfig,
    mirror: Mirror,
    tee: Option<Box<dyn Log>>,
) -> Result<LogConfig, InstallError> {
    let (sink, records) = sync_channel::<Entry>(QUEUE_DEPTH);

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

            // Acquire the stderr lock per write, never across the loop.
            // Holding it for the thread's lifetime deadlocks anything else
            // that writes to stderr directly: Slint logs image-decode
            // failures that way, and the UI thread would block forever
            // before its first frame.
            let stderr = io::stderr();
            let mut stderr_available = true;
            while let Ok(entry) = records.recv() {
                let line = match entry {
                    Entry::Line(line) => line,
                    Entry::Flush(ack) => {
                        if stderr_available {
                            let _ = stderr.lock().flush();
                        }
                        // The sender is waiting on this; a dropped ack simply
                        // releases it.
                        let _ = ack.send(());
                        continue;
                    }
                };
                #[cfg(unix)]
                if let Some((socket, path)) = &datagram {
                    let _ = socket.send_to(line.as_bytes(), path);
                }
                if stderr_available && stderr.lock().write_all(line.as_bytes()).is_err() {
                    // Losing the launcher's stderr must not disable the IPC log mirror.
                    stderr_available = false;
                }
            }
        });

    let logger = Logger {
        config: config.clone(),
        sink,
        tee,
    };
    log::set_boxed_logger(Box::new(logger))?;
    // Publish the ceiling only after the logger exists, so no record slips
    // through against a default filter.
    config.set_level(config.level());
    Ok(config)
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn mirror_survives_closed_stderr() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixDatagram;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        const CHILD_SOCKET: &str = "SYNTRA_LOG_TEST_SOCKET";
        if let Some(path) = std::env::var_os(CHILD_SOCKET) {
            // The parent closes stderr before releasing this handshake.
            std::io::stdin().read_exact(&mut [0]).unwrap();
            let config = crate::LogConfig::default();
            config.set_level(log::LevelFilter::Info);
            super::install(config, super::Mirror::Datagram(path.into())).unwrap();
            log::info!(target: "syntra", "before stderr failure");
            log::logger().flush();
            log::info!(target: "syntra", "after stderr failure");
            log::logger().flush();
            return;
        }

        let path =
            std::env::temp_dir().join(format!("syntra-log-test-{}.sock", std::process::id()));
        let socket = UnixDatagram::bind(&path).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "logger::tests::mirror_survives_closed_stderr",
                "--nocapture",
            ])
            .env(CHILD_SOCKET, &path)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        drop(child.stderr.take());
        child.stdin.take().unwrap().write_all(&[1]).unwrap();
        let result = (|| -> std::io::Result<bool> {
            let mut buffer = [0; 4096];
            for _ in 0..2 {
                let count = socket.recv(&mut buffer)?;
                if String::from_utf8_lossy(&buffer[..count]).contains("after stderr failure") {
                    return Ok(true);
                }
            }
            Ok(false)
        })();
        if !matches!(result, Ok(true)) {
            let _ = child.kill();
        }
        let status = child.wait().unwrap();
        drop(socket);
        std::fs::remove_file(path).unwrap();
        assert!(status.success());
        assert!(result.unwrap(), "the log mirror stopped with stderr");
    }

    /// A tee sees the records that pass the filter and none that do not.
    #[test]
    fn tee_receives_only_records_that_pass_the_filter() {
        use log::{Level, Log, Metadata, Record};
        use std::sync::mpsc::{Sender, channel, sync_channel};

        struct Recorder(Sender<String>);
        impl Log for Recorder {
            fn enabled(&self, _: &Metadata<'_>) -> bool {
                true
            }
            fn log(&self, record: &Record<'_>) {
                let _ = self.0.send(record.args().to_string());
            }
            fn flush(&self) {}
        }

        let (seen_tx, seen) = channel();
        let (sink, _records) = sync_channel(8);
        let logger = super::Logger {
            config: crate::LogConfig::new(log::LevelFilter::Warn),
            sink,
            tee: Some(Box::new(Recorder(seen_tx))),
        };
        logger.log(
            &Record::builder()
                .args(format_args!("kept"))
                .level(Level::Warn)
                .target("syntra")
                .build(),
        );
        logger.log(
            &Record::builder()
                .args(format_args!("dropped"))
                .level(Level::Info)
                .target("syntra")
                .build(),
        );
        assert_eq!(seen.try_iter().collect::<Vec<_>>(), vec!["kept".to_owned()]);
    }
}
