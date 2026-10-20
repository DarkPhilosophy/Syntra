//! The dashboard's connection to the daemon: connecting, reading events,
//! sending requests and reconnecting.
//!
//! This is *transport* only. It knows nothing about pages, projections or
//! callbacks; what an event means to the interface is decided elsewhere. The
//! boundary is the pair of traits in [`crate::bridge`], so the interface does
//! not depend on how events arrive. Every platform, Android included (where
//! the service shares the process but still listens on the same Unix control
//! socket), uses this socket transport; the detached source exists only so
//! the interface can run without a daemon, as in tests and previews.
//!
//! A dashboard must outlive the daemon: the reader is optional, and
//! [`EventSource::reconnect`] is the single place that establishes or
//! re-establishes the stream.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use syntra_api::{FrontendEvent, FrontendEventReader, FrontendRequest, FrontendRequestWriter};

use crate::bridge::{EventSource, RequestSink, TransportError};

/// Delay before the first reconnection attempt.
const RECONNECT_MIN: Duration = Duration::from_millis(250);
/// Upper bound on the reconnection backoff.
const RECONNECT_MAX: Duration = Duration::from_secs(5);

/// The shared, replaceable writer half of a connection.
type SharedWriter = Arc<Mutex<Option<FrontendRequestWriter>>>;

/// The delay that follows `delay` when an attempt fails: doubled, up to the
/// cap, so a daemon that is down is retried promptly at first and then gently.
fn next_delay(delay: Duration) -> Duration {
    (delay * 2).min(RECONNECT_MAX)
}

/// Daemon connection that may be absent.
pub(super) struct IpcEventSource {
    reader: Mutex<Option<FrontendEventReader>>,
    writer: SharedWriter,
}

impl IpcEventSource {
    /// Creates a source with no connection yet; the dashboard opens regardless.
    pub(super) fn detached() -> Self {
        Self {
            reader: Mutex::new(None),
            writer: Arc::new(Mutex::new(None)),
        }
    }

    /// Wraps a connection that is already established.
    pub(super) fn attached(reader: FrontendEventReader, writer: FrontendRequestWriter) -> Self {
        Self {
            reader: Mutex::new(Some(reader)),
            writer: Arc::new(Mutex::new(Some(writer))),
        }
    }

    /// The request half paired with this source.
    ///
    /// Both halves share one writer slot, so a reconnect that replaces the
    /// stream is seen by requests sent afterwards, and a lost stream empties
    /// the slot for both.
    pub(super) fn sink(&self) -> IpcRequestSink {
        IpcRequestSink(Arc::clone(&self.writer))
    }
}

impl EventSource for IpcEventSource {
    fn recv(&self) -> Result<FrontendEvent, TransportError> {
        let mut reader = self
            .reader
            .lock()
            .map_err(|_| TransportError::Disconnected)?;
        let result = reader
            .as_mut()
            .ok_or(TransportError::Disconnected)?
            .next_event()
            .ok_or(TransportError::Disconnected)?
            .map_err(|error| {
                log::error!("daemon event stream failed: {error}");
                TransportError::Disconnected
            });
        if result.is_err() {
            *reader = None;
            if let Ok(mut writer) = self.writer.lock() {
                *writer = None;
            }
        }
        result
    }

    /// Blocks with exponential backoff until a daemon accepts the connection.
    ///
    /// Returning `false` would end the pump thread and strand the dashboard
    /// offline for the rest of the session, so this retries indefinitely and
    /// only gives up if the freshly opened stream cannot be primed.
    fn reconnect(&self) -> bool {
        let mut delay = RECONNECT_MIN;
        loop {
            if let Ok((reader, mut writer)) = syntra_api::connect() {
                // Prime the new stream: without a resynchronisation the
                // dashboard would keep rendering pre-restart state.
                if writer.request(FrontendRequest::Sync).is_err()
                    || writer.request(FrontendRequest::Enumerate()).is_err()
                {
                    return false;
                }
                let (Ok(mut current_reader), Ok(mut current_writer)) =
                    (self.reader.lock(), self.writer.lock())
                else {
                    return false;
                };
                *current_reader = Some(reader);
                *current_writer = Some(writer);
                log::info!("attached to the Syntra daemon");
                return true;
            }
            std::thread::sleep(delay);
            delay = next_delay(delay);
        }
    }
}

/// The request half of the connection.
pub(super) struct IpcRequestSink(SharedWriter);

impl RequestSink for IpcRequestSink {
    fn send(&self, request: FrontendRequest) -> Result<(), TransportError> {
        self.0
            .lock()
            .map_err(|_| TransportError::Disconnected)?
            .as_mut()
            .ok_or(TransportError::Disconnected)?
            .request(request)
            .map_err(|error| {
                log::error!("frontend IPC request failed: {error}");
                TransportError::Disconnected
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_starts_prompt_doubles_and_stops_at_the_cap() {
        let mut delay = RECONNECT_MIN;
        let mut seen = vec![delay];
        for _ in 0..8 {
            delay = next_delay(delay);
            seen.push(delay);
        }
        assert_eq!(seen[0], Duration::from_millis(250));
        assert_eq!(seen[1], Duration::from_millis(500));
        assert_eq!(seen[2], Duration::from_secs(1));
        assert!(
            seen.windows(2).all(|pair| pair[1] >= pair[0]),
            "never shrinks"
        );
        assert!(
            seen.iter().all(|delay| *delay <= RECONNECT_MAX),
            "never exceeds the cap"
        );
        assert_eq!(*seen.last().unwrap(), RECONNECT_MAX);
    }

    /// With no connection the source reports a disconnect instead of blocking
    /// or panicking, and a request cannot be sent; the dashboard stays open.
    #[test]
    fn a_detached_transport_reports_disconnection_without_failing() {
        let source = IpcEventSource::detached();
        assert!(matches!(source.recv(), Err(TransportError::Disconnected)));
        let sink = source.sink();
        assert!(matches!(
            sink.send(FrontendRequest::QueryPlugins),
            Err(TransportError::Disconnected)
        ));
    }
}
