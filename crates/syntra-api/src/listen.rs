use futures::{Stream, StreamExt, stream::SelectAll};
#[cfg(unix)]
use std::path::PathBuf;
use std::{
    io::ErrorKind,
    pin::Pin,
    task::{Context, Poll},
};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
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
    #[cfg(unix)]
    tx_streams: Vec<WriteHalf<UnixStream>>,
    #[cfg(windows)]
    tx_streams: Vec<WriteHalf<TcpStream>>,
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
    pub async fn broadcast(&mut self, notify: FrontendEvent) {
        // encode event
        let mut json = serde_json::to_string(&notify).unwrap();
        json.push('\n');

        let mut keep = vec![];
        // TODO do simultaneously
        for tx in self.tx_streams.iter_mut() {
            // write len + payload
            if tx.write_all(json.as_bytes()).await.is_err() {
                keep.push(false);
                continue;
            }
            keep.push(true);
        }

        // could not find a better solution because async
        let mut keep = keep.into_iter();
        self.tx_streams.retain(|_| keep.next().unwrap());
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
            self.tx_streams.push(tx);
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

    /// A second daemon must never unlink the endpoint of one that is alive.
    ///
    /// Deleting it leaves the first daemon listening on an inode no client
    /// can reach: the process keeps running and every dashboard reports "no
    /// service is reachable" for as long as it lives.
    #[tokio::test]
    async fn a_listening_socket_is_never_removed() {
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
        let directory = std::env::temp_dir().join("syntra-listen-stale");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("daemon.sock");
        // A plain file stands in for the remains of a crashed daemon: nothing
        // is listening, so connecting is refused.
        std::fs::write(&path, b"").unwrap();
        unsafe { std::env::set_var(crate::paths::ENV_DAEMON_SOCKET, &path) };

        let listener = AsyncFrontendListener::new().await;

        assert!(
            listener.is_ok(),
            "an abandoned socket must not block a fresh listener"
        );
        unsafe { std::env::remove_var(crate::paths::ENV_DAEMON_SOCKET) };
        let _ = std::fs::remove_dir_all(&directory);
    }
}
