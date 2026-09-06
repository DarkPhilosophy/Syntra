//! Exclusive ownership and activation forwarding for the desktop frontend.
//!
//! This endpoint is deliberately distinct from the daemon IPC endpoint. The
//! callback runs on the listener thread; callers must marshal it to Slint.

use fs2::FileExt;
#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
#[cfg(windows)]
use interprocess::local_socket::GenericNamespaced;
use interprocess::local_socket::{ListenerOptions, Stream, prelude::*};
use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const ACTIVATE: u8 = 1;
const RETRIES: usize = 20;
const RETRY_DELAY: Duration = Duration::from_millis(10);
const CLIENT_READ_TIMEOUT: Duration = Duration::from_millis(50);

pub enum InstanceOutcome {
    Primary(PrimaryInstanceGuard),
    /// The existing primary was successfully sent an activation request.
    Existing,
}

impl fmt::Debug for InstanceOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Primary(_) => f.write_str("Primary(..)"),
            Self::Existing => f.write_str("Existing"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SingleInstanceError {
    #[error("single frontend instances are unsupported on this platform")]
    Unsupported,
    #[error("could not determine the per-user frontend runtime directory: {0}")]
    UserDirectory(String),
    #[error("could not open the frontend ownership lock: {0}")]
    Lock(#[source] io::Error),
    #[error("the existing frontend owns the lock but could not be activated: {0}")]
    Activation(#[source] io::Error),
    #[error("could not construct the frontend activation endpoint name: {0}")]
    InvalidName(#[source] io::Error),
    #[error("could not bind the frontend activation endpoint: {0}")]
    Bind(#[source] io::Error),
    #[error("could not start the frontend activation listener: {0}")]
    ListenerThread(#[source] io::Error),
}

pub struct PrimaryInstanceGuard {
    endpoint: Endpoint,
    stopping: Arc<AtomicBool>,
    listener: Option<JoinHandle<()>>,
    // Ownership is the OS lock on this open file, not the file's existence.
    _ownership: File,
}

impl fmt::Debug for PrimaryInstanceGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrimaryInstanceGuard")
            .finish_non_exhaustive()
    }
}

impl Drop for PrimaryInstanceGuard {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = notify(&self.endpoint);
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
        #[cfg(unix)]
        let _ = remove_stale_socket(&self.endpoint.socket_path);
        // `_ownership` unlocks only after the listener has relinquished its name.
    }
}

/// Acquires this user's frontend instance or activates its current owner.
///
/// `on_activate` runs serially on a worker thread and may run multiple times.
/// It must marshal UI work with `slint::invoke_from_event_loop` rather than
/// manipulating a Slint component directly.
pub fn acquire<F>(on_activate: F) -> Result<InstanceOutcome, SingleInstanceError>
where
    F: FnMut() + Send + 'static,
{
    acquire_at(default_user_directory()?, on_activate)
}

fn acquire_at<F>(directory: PathBuf, on_activate: F) -> Result<InstanceOutcome, SingleInstanceError>
where
    F: FnMut() + Send + 'static,
{
    fs::create_dir_all(&directory).map_err(SingleInstanceError::Lock)?;
    let endpoint = Endpoint::new(&directory)?;
    let lock_path = directory.join("syntra-frontend.lock");
    let ownership = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(SingleInstanceError::Lock)?;

    match ownership.try_lock_exclusive() {
        Ok(()) => start_primary(endpoint, ownership, on_activate),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            notify_with_retry(&endpoint).map_err(SingleInstanceError::Activation)?;
            Ok(InstanceOutcome::Existing)
        }
        Err(error) => Err(SingleInstanceError::Lock(error)),
    }
}

fn start_primary<F>(
    endpoint: Endpoint,
    ownership: File,
    mut on_activate: F,
) -> Result<InstanceOutcome, SingleInstanceError>
where
    F: FnMut() + Send + 'static,
{
    // Holding the ownership lock makes stale-socket removal race-free. Refuse
    // to unlink anything except the fixed path when it is actually a socket.
    #[cfg(unix)]
    remove_stale_socket(&endpoint.socket_path).map_err(SingleInstanceError::Bind)?;

    let listener = ListenerOptions::new()
        .name(endpoint.name()?)
        .try_overwrite(false)
        .create_sync()
        .map_err(SingleInstanceError::Bind)?;
    #[cfg(unix)]
    let listener = {
        let mut listener = listener;
        listener.do_not_reclaim_name_on_drop();
        listener
    };
    let stopping = Arc::new(AtomicBool::new(false));
    let listener_stopping = Arc::clone(&stopping);
    let listener = thread::Builder::new()
        .name("syntra-activation-listener".into())
        .spawn(move || {
            while let Ok(mut connection) = listener.accept() {
                if listener_stopping.load(Ordering::Acquire) {
                    break;
                }
                // An idle or malicious client must not prevent guard teardown.
                // A bounded read also returns the listener to `accept` promptly.
                if connection
                    .set_recv_timeout(Some(CLIENT_READ_TIMEOUT))
                    .is_err()
                {
                    continue;
                }
                let mut command = [0];
                if connection.read_exact(&mut command).is_ok()
                    && command[0] == ACTIVATE
                    && !listener_stopping.load(Ordering::Acquire)
                {
                    on_activate();
                }
            }
        })
        .map_err(SingleInstanceError::ListenerThread)?;

    Ok(InstanceOutcome::Primary(PrimaryInstanceGuard {
        endpoint,
        stopping,
        listener: Some(listener),
        _ownership: ownership,
    }))
}

#[cfg(unix)]
fn remove_stale_socket(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;

    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(path),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("refusing to remove non-socket endpoint {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[derive(Clone)]
struct Endpoint {
    #[cfg(unix)]
    socket_path: PathBuf,
    #[cfg(windows)]
    pipe_name: String,
}

impl Endpoint {
    fn new(directory: &Path) -> Result<Self, SingleInstanceError> {
        #[cfg(unix)]
        return Ok(Self {
            socket_path: directory.join("syntra-frontend.sock"),
        });
        #[cfg(windows)]
        return Ok(Self {
            pipe_name: format!(
                "syntra-frontend-{:016x}",
                fnv1a64(directory.as_os_str().to_string_lossy().as_bytes())
            ),
        });
        #[cfg(not(any(unix, windows)))]
        {
            let _ = directory;
            Err(SingleInstanceError::Unsupported)
        }
    }

    fn name(&self) -> Result<interprocess::local_socket::Name<'static>, SingleInstanceError> {
        #[cfg(unix)]
        return self
            .socket_path
            .clone()
            .to_fs_name::<GenericFilePath>()
            .map_err(SingleInstanceError::InvalidName);
        #[cfg(windows)]
        return self
            .pipe_name
            .clone()
            .to_ns_name::<GenericNamespaced>()
            .map_err(SingleInstanceError::InvalidName);
        #[cfg(not(any(unix, windows)))]
        Err(SingleInstanceError::Unsupported)
    }
}

fn notify(endpoint: &Endpoint) -> io::Result<()> {
    let name = endpoint.name().map_err(io::Error::other)?;
    let mut stream = Stream::connect(name)?;
    stream.write_all(&[ACTIVATE])
}

fn notify_with_retry(endpoint: &Endpoint) -> io::Result<()> {
    let mut last_error = None;
    for _ in 0..RETRIES {
        match notify(endpoint) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        thread::sleep(RETRY_DELAY);
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("activation retry loop did not run")))
}

#[cfg(all(unix, not(target_os = "android")))]
fn default_user_directory() -> Result<PathBuf, SingleInstanceError> {
    lan_mouse_ipc::default_socket_path()
        .map_err(|error| SingleInstanceError::UserDirectory(error.to_string()))?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| SingleInstanceError::UserDirectory("daemon socket has no parent".into()))
}

#[cfg(windows)]
fn default_user_directory() -> Result<PathBuf, SingleInstanceError> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|path| path.join("Syntra"))
        .ok_or_else(|| SingleInstanceError::UserDirectory("LOCALAPPDATA is unset".into()))
}

#[cfg(not(any(windows, all(unix, not(target_os = "android")))))]
fn default_user_directory() -> Result<PathBuf, SingleInstanceError> {
    Err(SingleInstanceError::Unsupported)
}

#[cfg(windows)]
const fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        index += 1;
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Barrier,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };

    fn directory(test: &str) -> PathBuf {
        std::env::temp_dir().join(format!("syntra-instance-{test}-{}", std::process::id()))
    }

    #[test]
    fn existing_instance_is_activated() {
        let directory = directory("activation");
        let (tx, rx) = mpsc::channel();
        let first = acquire_at(directory.clone(), move || tx.send(()).unwrap()).unwrap();
        assert!(matches!(first, InstanceOutcome::Primary(_)));
        assert!(matches!(
            acquire_at(directory, || {}).unwrap(),
            InstanceOutcome::Existing
        ));
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn ownership_is_reacquired_after_drop() {
        let directory = directory("reacquire");
        let InstanceOutcome::Primary(first) = acquire_at(directory.clone(), || {}).unwrap() else {
            panic!("unexpected existing instance");
        };
        drop(first);
        assert!(matches!(
            acquire_at(directory, || {}).unwrap(),
            InstanceOutcome::Primary(_)
        ));
    }

    #[test]
    fn idle_client_cannot_block_guard_drop() {
        let directory = directory("idle-client");
        let InstanceOutcome::Primary(guard) = acquire_at(directory, || {}).unwrap() else {
            panic!("unexpected existing instance");
        };
        let idle_connection = Stream::connect(guard.endpoint.name().unwrap()).unwrap();
        let (dropped_tx, dropped_rx) = mpsc::channel();
        thread::spawn(move || {
            drop(guard);
            dropped_tx.send(()).unwrap();
        });

        dropped_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(idle_connection);
    }

    #[test]
    fn simultaneous_attempts_have_one_primary() {
        let directory = directory("simultaneous");
        let barrier = Arc::new(Barrier::new(2));
        let primaries = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let directory = directory.clone();
                let barrier = Arc::clone(&barrier);
                let primaries = Arc::clone(&primaries);
                thread::spawn(move || {
                    barrier.wait();
                    if let InstanceOutcome::Primary(guard) = acquire_at(directory, || {}).unwrap() {
                        primaries.fetch_add(1, Ordering::Relaxed);
                        thread::sleep(Duration::from_millis(100));
                        drop(guard);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(primaries.load(Ordering::Relaxed), 1);
    }
}
