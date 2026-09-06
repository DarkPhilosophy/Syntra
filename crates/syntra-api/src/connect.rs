use crate::{ConnectionError, FrontendEvent, FrontendRequest, IpcError};
use std::{
    cmp::min,
    io::{self, BufReader, LineWriter, Lines, prelude::*},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::net::UnixStream;

#[cfg(windows)]
use std::net::TcpStream;

/// Public reader or writer for the frontend IPC stream.
pub struct FrontendEventReader {
    #[cfg(unix)]
    lines: Lines<BufReader<UnixStream>>,
    #[cfg(windows)]
    lines: Lines<BufReader<TcpStream>>,
}

/// Public reader or writer for the frontend IPC stream.
pub struct FrontendRequestWriter {
    #[cfg(unix)]
    line_writer: LineWriter<UnixStream>,
    #[cfg(windows)]
    line_writer: LineWriter<TcpStream>,
}

impl FrontendEventReader {
    /// Reads the next daemon event from the IPC stream.
    pub fn next_event(&mut self) -> Option<Result<FrontendEvent, IpcError>> {
        match self.lines.next()? {
            Err(e) => Some(Err(e.into())),
            Ok(l) => Some(serde_json::from_str(l.as_str()).map_err(|e| e.into())),
        }
    }
}

impl FrontendRequestWriter {
    /// Sends one client request to the daemon.
    pub fn request(&mut self, request: FrontendRequest) -> Result<(), io::Error> {
        let mut json = serde_json::to_string(&request).unwrap();
        log::debug!("requesting: {json}");
        json.push('\n');
        self.line_writer.write_all(json.as_bytes())?;
        Ok(())
    }
}

/// Opens a blocking frontend IPC connection to the daemon.
pub fn connect() -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    connect_inner(None)
}

/// Opens a blocking frontend IPC connection, failing if the timeout expires.
pub fn connect_with_timeout(
    timeout: Duration,
) -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    connect_inner(Some(timeout))
}

fn connect_inner(
    timeout: Option<Duration>,
) -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    let rx = wait_for_service(timeout)?;
    let tx = rx.try_clone()?;
    let buf_reader = BufReader::new(rx);
    let lines = buf_reader.lines();
    let line_writer = LineWriter::new(tx);
    let reader = FrontendEventReader { lines };
    let writer = FrontendRequestWriter { line_writer };
    Ok((reader, writer))
}

/// wait for the syntra socket to come online
#[cfg(unix)]
fn wait_for_service(timeout: Option<Duration>) -> Result<UnixStream, ConnectionError> {
    let socket_path = crate::default_socket_path()?;
    wait_for_service_with(timeout, || UnixStream::connect(&socket_path))
}

#[cfg(windows)]
fn wait_for_service(timeout: Option<Duration>) -> Result<TcpStream, ConnectionError> {
    wait_for_service_with(timeout, || TcpStream::connect("127.0.0.1:5252"))
}

fn wait_for_service_with<S>(
    timeout: Option<Duration>,
    mut connect: impl FnMut() -> io::Result<S>,
) -> Result<S, ConnectionError> {
    let started = Instant::now();
    let mut backoff = Duration::from_millis(10);
    loop {
        if timeout.is_some_and(|limit| started.elapsed() >= limit) {
            return Err(ConnectionError::Timeout);
        }
        if let Ok(stream) = connect() {
            return Ok(stream);
        }

        let delay = exponential_back_off(&mut backoff);
        if let Some(timeout) = timeout {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(ConnectionError::Timeout);
            }
            thread::sleep(min(delay, remaining));
        } else {
            thread::sleep(delay);
        }
    }
}

fn exponential_back_off(duration: &mut Duration) -> Duration {
    let new = duration.saturating_mul(2);
    *duration = min(new, Duration::from_secs(1));
    *duration
}
