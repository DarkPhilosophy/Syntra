use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const MAX_DIAGNOSTIC_ENTRIES: usize = 2_000;
const SOCKET_NAME: &str = "syntra-diagnostics.sock";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveDiagnostic {
    pub timestamp: String,
    pub level: String,
    pub stage: String,
    pub direction: String,
    pub correlation: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticFilter {
    pub level: String,
    pub stage: String,
    pub direction: String,
    pub query: String,
    pub paused: bool,
    pub follow_tail: bool,
}

impl Default for DiagnosticFilter {
    fn default() -> Self {
        Self {
            level: "all".into(),
            stage: "all".into(),
            direction: "all".into(),
            query: String::new(),
            paused: false,
            follow_tail: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct DiagnosticStore {
    entries: VecDeque<LiveDiagnostic>,
    paused_entries: Option<Vec<LiveDiagnostic>>,
    pub filter: DiagnosticFilter,
    pub stream_status: String,
    pub last_error: Option<String>,
}

impl DiagnosticStore {
    pub fn unavailable(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            stream_status: "Unavailable".into(),
            last_error: Some(message),
            ..Self::default()
        }
    }

    pub fn push(&mut self, entry: LiveDiagnostic) {
        self.entries.push_back(entry);
        while self.entries.len() > MAX_DIAGNOSTIC_ENTRIES {
            self.entries.pop_front();
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        if let Some(entries) = &mut self.paused_entries {
            entries.clear();
        }
    }

    pub fn set_paused(&mut self, paused: bool) {
        if paused == self.filter.paused {
            return;
        }
        self.filter.paused = paused;
        self.paused_entries = paused.then(|| self.entries.iter().cloned().collect());
    }

    /// Most recent entries matching the active filter, newest last.
    ///
    /// `limit` bounds the result because the view only ever shows a window
    /// of rows: projecting all [`MAX_DIAGNOSTIC_ENTRIES`] on every arriving
    /// record made the UI thread do O(n) work per log line, which froze the
    /// window under normal network chatter.
    pub fn filtered(&self, limit: usize) -> Vec<LiveDiagnostic> {
        let query = self.filter.query.trim().to_lowercase();
        let matches = |entry: &LiveDiagnostic| {
            (self.filter.level == "all" || entry.level == self.filter.level)
                && (self.filter.stage == "all" || entry.stage == self.filter.stage)
                && (self.filter.direction == "all" || entry.direction == self.filter.direction)
                && (query.is_empty()
                    || entry.timestamp.to_lowercase().contains(&query)
                    || entry.level.to_lowercase().contains(&query)
                    || entry.stage.to_lowercase().contains(&query)
                    || entry.direction.to_lowercase().contains(&query)
                    || entry.correlation.to_lowercase().contains(&query)
                    || entry.message.to_lowercase().contains(&query))
        };
        // Walk from the newest backwards so the scan stops as soon as the
        // window is full, instead of filtering the whole ring buffer.
        let mut newest_first: Vec<LiveDiagnostic> = match &self.paused_entries {
            Some(entries) => entries
                .iter()
                .rev()
                .filter(|entry| matches(entry))
                .take(limit)
                .cloned()
                .collect(),
            None => self
                .entries
                .iter()
                .rev()
                .filter(|entry| matches(entry))
                .take(limit)
                .cloned()
                .collect(),
        };
        newest_first.reverse();
        newest_first
    }
}

pub fn parse_record(line: &str) -> Option<LiveDiagnostic> {
    let (timestamp, rest) = take_bracketed(line.trim())?;
    let (level, rest) = take_bracketed(rest)?;
    let (target, message) = take_bracketed(rest)?;
    let message = message.trim().to_owned();
    let level = match level.to_ascii_lowercase().as_str() {
        "warn" | "warning" => "warning".to_owned(),
        value => value.to_owned(),
    };
    let stage = metadata_value(&message, "stage")
        .unwrap_or_else(|| classify_stage(target, &message))
        .to_ascii_lowercase();
    let direction = metadata_value(&message, "direction")
        .map(normalize_direction)
        .unwrap_or_default();
    let correlation = metadata_value(&message, "correlation")
        .or_else(|| metadata_value(&message, "transfer"))
        .unwrap_or_default();
    Some(LiveDiagnostic {
        timestamp: timestamp.to_owned(),
        level,
        stage,
        direction,
        correlation,
        message,
    })
}

fn take_bracketed(value: &str) -> Option<(&str, &str)> {
    let value = value.strip_prefix('[')?;
    let end = value.find(']')?;
    Some((&value[..end], &value[end + 1..]))
}

fn metadata_value(message: &str, key: &str) -> Option<String> {
    message.split_whitespace().find_map(|word| {
        let (candidate, value) = word.split_once('=')?;
        if candidate.eq_ignore_ascii_case(key) {
            Some(value.trim_matches(|c: char| ",;)]}".contains(c)).to_owned())
        } else {
            None
        }
    })
}

fn normalize_direction(value: String) -> String {
    match value.to_ascii_lowercase().as_str() {
        "inbound" | "incoming" => "incoming".into(),
        "outbound" | "outgoing" => "outgoing".into(),
        "local" => "local".into(),
        other => other.into(),
    }
}

fn classify_stage(target: &str, message: &str) -> String {
    let haystack = format!("{target} {message}").to_ascii_lowercase();
    if haystack.contains("clipboard")
        || haystack.contains("manifest")
        || haystack.contains("selection")
        || haystack.contains("paste")
    {
        "clipboard".into()
    } else if haystack.contains("capture") || haystack.contains("emulation") {
        "capture".into()
    } else if haystack.contains("authoriz") || haystack.contains("fingerprint") {
        "authorization".into()
    } else {
        "network".into()
    }
}

pub fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(SOCKET_NAME)
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::os::unix::net::UnixDatagram;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    #[derive(Clone, Copy)]
    struct FileIdentity {
        device: u64,
        inode: u64,
    }

    pub struct LiveDiagnosticReceiver {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
        path: PathBuf,
        identity: FileIdentity,
    }

    impl LiveDiagnosticReceiver {
        pub fn start(
            store: Arc<Mutex<DiagnosticStore>>,
            notify: impl Fn() + Send + 'static,
        ) -> io::Result<Self> {
            let path = socket_path();
            let socket = bind_without_displacing_listener(&path)?;
            socket.set_read_timeout(Some(Duration::from_millis(100)))?;
            let metadata = fs::symlink_metadata(&path)?;
            let identity = FileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            };
            if let Ok(mut store) = store.lock() {
                store.stream_status = "Live".into();
                store.last_error = None;
            }
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = Arc::clone(&stop);
            let thread = thread::spawn(move || {
                let mut buffer = [0_u8; 65_536];
                while !thread_stop.load(Ordering::Relaxed) {
                    match socket.recv(&mut buffer) {
                        Ok(length) => {
                            let mut changed = false;
                            for line in String::from_utf8_lossy(&buffer[..length]).lines() {
                                if let Some(entry) = parse_record(line) {
                                    let paused = match store.lock() {
                                        Ok(mut store) => {
                                            store.push(entry);
                                            store.filter.paused
                                        }
                                        Err(_) => return,
                                    };
                                    changed |= !paused;
                                }
                            }
                            if changed {
                                notify();
                            }
                        }
                        Err(_error)
                            if matches!(
                                _error.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                            ) => {}
                        Err(error) => {
                            if let Ok(mut store) = store.lock() {
                                store.stream_status = "Unavailable".into();
                                store.last_error =
                                    Some(format!("Live diagnostic stream failed: {error}"));
                            }
                            notify();
                            break;
                        }
                    }
                }
            });
            Ok(Self {
                stop,
                thread: Some(thread),
                path,
                identity,
            })
        }
    }

    impl Drop for LiveDiagnosticReceiver {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            if let Ok(metadata) = fs::symlink_metadata(&self.path) {
                if metadata.file_type().is_socket()
                    && metadata.dev() == self.identity.device
                    && metadata.ino() == self.identity.inode
                {
                    let _ = fs::remove_file(&self.path);
                }
            }
        }
    }

    fn bind_without_displacing_listener(path: &Path) -> io::Result<UnixDatagram> {
        match UnixDatagram::bind(path) {
            Ok(socket) => return Ok(socket),
            Err(error)
                if !matches!(
                    error.kind(),
                    io::ErrorKind::AddrInUse | io::ErrorKind::AlreadyExists
                ) =>
            {
                return Err(error);
            }
            Err(_) => {}
        }

        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_socket()
            || metadata.uid() != fs::metadata(path.parent().unwrap_or(Path::new(".")))?.uid()
        {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "diagnostic socket path is not a stale socket owned by this user",
            ));
        }
        let probe = UnixDatagram::unbound()?.send_to(&[], path);
        if !matches!(
            probe,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::NotFound
                        | io::ErrorKind::AddrNotAvailable
                )
        ) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "another live diagnostic listener owns the socket",
            ));
        }
        let current = fs::symlink_metadata(path)?;
        if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "diagnostic socket changed while checking ownership",
            ));
        }
        fs::remove_file(path)?;
        UnixDatagram::bind(path)
    }

    pub use LiveDiagnosticReceiver as PlatformReceiver;
}

#[cfg(unix)]
pub use unix::PlatformReceiver as LiveDiagnosticReceiver;

#[cfg(not(unix))]
pub struct LiveDiagnosticReceiver;

#[cfg(not(unix))]
impl LiveDiagnosticReceiver {
    pub fn start(
        _store: Arc<Mutex<DiagnosticStore>>,
        _notify: impl Fn() + Send + 'static,
    ) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "live diagnostics require Unix datagram sockets",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_logger_record_and_preserves_source_metadata() {
        let entry = parse_record(
            "[2026-09-05T12:34:56.789Z][WARN][clipboard::transfer] stage=clipboard direction=outbound correlation=abc completed",
        )
        .unwrap();
        assert_eq!(entry.timestamp, "2026-09-05T12:34:56.789Z");
        assert_eq!(entry.level, "warning");
        assert_eq!(entry.stage, "clipboard");
        assert_eq!(entry.direction, "outgoing");
        assert_eq!(entry.correlation, "abc");
        assert_eq!(
            entry.message,
            "stage=clipboard direction=outbound correlation=abc completed"
        );
    }

    #[test]
    fn bounded_history_evicts_oldest_and_filters_all_metadata() {
        let mut store = DiagnosticStore::default();
        for index in 0..=MAX_DIAGNOSTIC_ENTRIES {
            store.push(LiveDiagnostic {
                timestamp: index.to_string(),
                level: if index % 2 == 0 { "info" } else { "error" }.into(),
                stage: "clipboard".into(),
                direction: "incoming".into(),
                correlation: format!("transfer-{index}"),
                message: "RangeResponse".into(),
            });
        }
        assert_eq!(store.entries.len(), MAX_DIAGNOSTIC_ENTRIES);
        assert_eq!(store.entries.front().unwrap().timestamp, "1");
        store.filter.level = "error".into();
        store.filter.query = "TRANSFER-19".into();
        let filtered = store.filtered(MAX_DIAGNOSTIC_ENTRIES);
        assert!(!filtered.is_empty());
        assert!(filtered.iter().all(|entry| {
            entry.level == "error" && entry.correlation.to_lowercase().contains("transfer-19")
        }));
    }

    /// The view asks for a window, not the whole ring buffer: projecting
    /// everything on each arriving record is what froze the UI thread.
    #[test]
    fn filtering_returns_the_newest_entries_up_to_the_limit() {
        let mut store = DiagnosticStore::default();
        for index in 0..50 {
            store.push(LiveDiagnostic {
                timestamp: index.to_string(),
                level: "info".into(),
                stage: "network".into(),
                direction: "".into(),
                correlation: String::new(),
                message: "tick".into(),
            });
        }

        let window = store.filtered(10);

        assert_eq!(window.len(), 10);
        // Oldest first within the window, ending at the newest record.
        assert_eq!(window.first().unwrap().timestamp, "40");
        assert_eq!(window.last().unwrap().timestamp, "49");
    }

    #[test]
    fn malformed_or_unstructured_lines_are_not_projected() {
        assert!(parse_record("ordinary stderr text").is_none());
        assert!(parse_record("[time][INFO] missing target").is_none());
    }
}
