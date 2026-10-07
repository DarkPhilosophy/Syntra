use futures::{Stream, StreamExt, stream::SelectAll};
#[cfg(unix)]
use std::path::PathBuf;
use std::{
    io::ErrorKind,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf};
use tokio::sync::mpsc;
use tokio_stream::wrappers::LinesStream;

#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(windows)]
use tokio::net::TcpListener;
#[cfg(windows)]
use tokio::net::TcpStream;

use crate::{FrontendEvent, FrontendRequest, IpcError, IpcListenerCreationError};

/// Public reader or writer for the frontend IPC stream.
pub struct AsyncFrontendListener {
    #[cfg(windows)]
    listener: TcpListener,
    #[cfg(unix)]
    listener: UnixListener,
    #[cfg(unix)]
    socket_path: PathBuf,
    #[cfg(unix)]
    line_streams: SelectAll<LinesStream<BufReader<ReadHalf<UnixStream>>>>,
    #[cfg(windows)]
    line_streams: SelectAll<LinesStream<BufReader<ReadHalf<TcpStream>>>>,
    tx_streams: Vec<mpsc::Sender<Arc<str>>>,
}

/// How many events one client may have waiting before it is considered to
/// have stopped reading. A dashboard drains these within milliseconds, so a
/// queue this deep is only ever reached by a client that has died or hung.
const CLIENT_QUEUE: usize = 512;

/// How long the writer waits on one socket write before giving up on the
/// client, so a hung client frees its resources instead of lingering.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Starts the task that writes one client's events, and returns its queue.
///
/// The task ends when the client stops accepting data or the queue is
/// dropped, which closes the socket's write half.
fn spawn_writer<W>(mut tx: W) -> mpsc::Sender<Arc<str>>
where
    W: AsyncWrite + Unpin + 'static,
{
    let (queue, mut lines) = mpsc::channel::<Arc<str>>(CLIENT_QUEUE);
    tokio::task::spawn_local(async move {
        while let Some(line) = lines.recv().await {
            match tokio::time::timeout(WRITE_TIMEOUT, tx.write_all(line.as_bytes())).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        let _ = tx.shutdown().await;
    });
    queue
}

impl AsyncFrontendListener {
    /// Creates a listener for frontend IPC connections.
    pub async fn new() -> Result<Self, IpcListenerCreationError> {
        #[cfg(unix)]
        let (socket_path, listener) = {
            let socket_path = crate::default_socket_path()?;

            if socket_path.exists() {
                // Probe before touching it: the file may belong to a daemon
                // that is alive and listening.
                match UnixStream::connect(&socket_path).await {
                    // Somebody answered, so a daemon is already running.
                    Ok(_) => return Err(IpcListenerCreationError::AlreadyRunning),
                    // Nobody is bound to it. Only this error proves the file
                    // is abandoned; a timeout or a permission problem means
                    // the socket may well be live, and deleting it would
                    // unlink a listening daemon's endpoint and leave it
                    // running yet permanently unreachable.
                    Err(error) if error.kind() == ErrorKind::ConnectionRefused => {
                        log::debug!("{socket_path:?}: {error} - removing left behind socket");
                        let _ = std::fs::remove_file(&socket_path);
                    }
                    Err(error) => {
                        log::warn!(
                            "{socket_path:?}: {error} - refusing to remove a socket that may be in use"
                        );
                        return Err(IpcListenerCreationError::AlreadyRunning);
                    }
                }
            }
            let listener = match UnixListener::bind(&socket_path) {
                Ok(ls) => ls,
                // some other syntra instance has bound the socket in the meantime
                Err(e) if e.kind() == ErrorKind::AddrInUse => {
                    return Err(IpcListenerCreationError::AlreadyRunning);
                }
                Err(e) => return Err(IpcListenerCreationError::Bind(e)),
            };
            (socket_path, listener)
        };

        #[cfg(windows)]
        let listener = match TcpListener::bind("127.0.0.1:5252").await {
            Ok(ls) => ls,
            // some other syntra instance has bound the socket in the meantime
            Err(e) if e.kind() == ErrorKind::AddrInUse => {
                return Err(IpcListenerCreationError::AlreadyRunning);
            }
            Err(e) => return Err(IpcListenerCreationError::Bind(e)),
        };

        let adapter = Self {
            listener,
            #[cfg(unix)]
            socket_path,
            line_streams: SelectAll::new(),
            tx_streams: vec![],
        };

        Ok(adapter)
    }

    /// Broadcasts one authoritative event to connected frontend clients.
    ///
    /// Each client has its own bounded queue and writer, so a client that
    /// stops reading fills only its own queue. An unbounded or serial write
    /// would wait on it for ever, holding up every client after it and,
    /// through them, the whole event stream.
    pub async fn broadcast(&mut self, notify: FrontendEvent) {
        // encode event once for every client
        let mut json = serde_json::to_string(&notify).unwrap();
        json.push('\n');
        let line: Arc<str> = json.into();

        // Only queue: nothing here waits on a socket. A client whose queue is
        // full has stopped reading and one whose writer ended has gone; both
        // are dropped, and neither can delay the others or the service.
        self.tx_streams
            .retain(|queue| match queue.try_send(line.clone()) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    log::warn!("dropping a frontend client that stopped reading");
                    false
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            });
    }
}

#[cfg(unix)]
impl Drop for AsyncFrontendListener {
    fn drop(&mut self) {
        log::debug!("remove socket: {:?}", self.socket_path);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

impl Stream for AsyncFrontendListener {
    type Item = Result<FrontendRequest, IpcError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Poll::Ready(Some(Ok(l))) = self.line_streams.poll_next_unpin(cx) {
            let request = serde_json::from_str(l.as_str()).map_err(|e| e.into());
            return Poll::Ready(Some(request));
        }
        let mut sync = false;
        while let Poll::Ready(Ok((stream, _))) = self.listener.poll_accept(cx) {
            let (rx, tx) = tokio::io::split(stream);
            let buf_reader = BufReader::new(rx);
            let lines = buf_reader.lines();
            let lines = LinesStream::new(lines);
            self.line_streams.push(lines);
            self.tx_streams.push(spawn_writer(tx));
            sync = true;
        }
        if sync {
            Poll::Ready(Some(Ok(FrontendRequest::Sync)))
        } else {
            Poll::Pending
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    // Tests here point the daemon socket at their own path through one
    // process-wide environment variable, so they share one lock.
    use crate::paths::ENV_LOCK;

    /// A second daemon must never unlink the endpoint of one that is alive.
    ///
    /// Deleting it leaves the first daemon listening on an inode no client
    /// can reach: the process keeps running and every dashboard reports "no
    /// service is reachable" for as long as it lives.
    #[tokio::test]
    async fn a_listening_socket_is_never_removed() {
        let _env = ENV_LOCK.lock().await;
        let directory = std::env::temp_dir().join("syntra-listen-live");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("daemon.sock");
        // SAFETY: single-threaded test; the variable is restored below.
        unsafe { std::env::set_var(crate::paths::ENV_DAEMON_SOCKET, &path) };

        let first = AsyncFrontendListener::new().await.expect("first listener");
        let second = AsyncFrontendListener::new().await;

        assert!(
            matches!(second, Err(IpcListenerCreationError::AlreadyRunning)),
            "a running daemon must be detected"
        );
        assert!(path.exists(), "the live socket must still exist");
        drop(first);
        unsafe { std::env::remove_var(crate::paths::ENV_DAEMON_SOCKET) };
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A socket left behind by a dead daemon must be replaced, or the service
    /// could never start again after a crash.
    #[tokio::test]
    async fn an_abandoned_socket_is_replaced() {
        let _env = ENV_LOCK.lock().await;
        let directory = std::env::temp_dir().join("syntra-listen-stale");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("daemon.sock");
        // The remains of a crashed daemon: a socket file nobody listens on.
        // (A plain file is not a socket; macOS refuses it differently.)
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        unsafe { std::env::set_var(crate::paths::ENV_DAEMON_SOCKET, &path) };

        let listener = AsyncFrontendListener::new().await;

        assert!(
            listener.is_ok(),
            "an abandoned socket must not block a fresh listener"
        );
        unsafe { std::env::remove_var(crate::paths::ENV_DAEMON_SOCKET) };
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A client that connects and never reads must not hold anyone up.
    ///
    /// This is what froze the dashboard's updates: one dead client filled its
    /// socket buffer, and a write to it waited for ever, ahead of every
    /// client after it. The healthy client must receive every event, the
    /// broadcasts must not wait, and the dead client must be dropped.
    #[tokio::test(flavor = "current_thread")]
    async fn a_client_that_never_reads_delays_nobody() {
        use tokio::io::AsyncBufReadExt;
        let _env = ENV_LOCK.lock().await;
        let directory = std::env::temp_dir().join("syntra-listen-slow");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("daemon.sock");
        unsafe { std::env::set_var(crate::paths::ENV_DAEMON_SOCKET, &path) };

        tokio::task::LocalSet::new()
            .run_until(async {
                let mut listener = AsyncFrontendListener::new().await.expect("listener");
                // The dead client connects first, so it is ahead in the list.
                let dead = UnixStream::connect(&path).await.expect("dead client");
                let healthy = UnixStream::connect(&path).await.expect("healthy client");
                // Accepting happens when the listener is polled.
                let _ = futures::poll!(listener.next());
                assert_eq!(listener.tx_streams.len(), 2);

                // Far more than a socket buffer and a client queue can hold.
                let events = 1500;
                let big = "x".repeat(2_000);
                let reader = tokio::task::spawn_local(async move {
                    let mut lines = BufReader::new(healthy).lines();
                    let mut seen = 0;
                    while let Ok(Some(_)) = lines.next_line().await {
                        seen += 1;
                        if seen == events {
                            break;
                        }
                    }
                    seen
                });

                let started = std::time::Instant::now();
                for _ in 0..events {
                    listener
                        .broadcast(FrontendEvent::LogSpec(big.clone()))
                        .await;
                    // Let the healthy client's reader and the writers run.
                    tokio::task::yield_now().await;
                }
                let took = started.elapsed();

                assert!(
                    took < std::time::Duration::from_secs(5),
                    "broadcasting waited on a client that does not read: {took:?}"
                );
                assert_eq!(
                    reader.await.unwrap(),
                    events,
                    "the healthy client lost events"
                );
                assert_eq!(
                    listener.tx_streams.len(),
                    1,
                    "the client that never read must have been dropped"
                );
                drop(dead);
            })
            .await;
        unsafe { std::env::remove_var(crate::paths::ENV_DAEMON_SOCKET) };
        let _ = std::fs::remove_dir_all(&directory);
    }
}
