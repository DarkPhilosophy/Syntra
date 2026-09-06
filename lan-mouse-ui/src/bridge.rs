use lan_mouse_ipc::{ClientHandle, ClipboardTransferId, FrontendEvent, FrontendRequest, Position};
use std::sync::mpsc::{self, Receiver, Sender};
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiIntent {
    Activate(ClientHandle, bool),
    Create,
    ChangePort(u16),
    Delete(ClientHandle),
    Enumerate,
    DiscoverPeers,
    ResolveDns(ClientHandle),
    UpdateHostname(ClientHandle, Option<String>),
    UpdatePort(ClientHandle, u16),
    UpdatePosition(ClientHandle, Position),
    UpdateFixIps(ClientHandle, Vec<std::net::IpAddr>),
    EnableCapture,
    EnableEmulation,
    Sync,
    Authorize(String, String),
    RemoveAuthorized(String),
    UpdateEnterHook(ClientHandle, Option<String>),
    SaveConfiguration,
    SetClipboardText(bool),
    SetClipboardImage(bool),
    SetClipboardFiles(bool),
    CancelTransfer(ClipboardTransferId),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentError {
    EmptyField(&'static str),
    InvalidPort,
    InvalidTransferId,
}
impl UiIntent {
    pub fn into_request(self) -> Result<FrontendRequest, IntentError> {
        use UiIntent::*;
        Ok(match self {
            Activate(h, b) => FrontendRequest::Activate(h, b),
            Create => FrontendRequest::Create,
            ChangePort(0) | UpdatePort(_, 0) => return Err(IntentError::InvalidPort),
            ChangePort(p) => FrontendRequest::ChangePort(p),
            Delete(h) => FrontendRequest::Delete(h),
            Enumerate => FrontendRequest::Enumerate(),
            DiscoverPeers => FrontendRequest::DiscoverPeers,
            ResolveDns(h) => FrontendRequest::ResolveDns(h),
            UpdateHostname(h, v) => {
                FrontendRequest::UpdateHostname(h, v.filter(|x| !x.trim().is_empty()))
            }
            UpdatePort(h, p) => FrontendRequest::UpdatePort(h, p),
            UpdatePosition(h, p) => FrontendRequest::UpdatePosition(h, p),
            UpdateFixIps(h, ips) => FrontendRequest::UpdateFixIps(h, ips),
            EnableCapture => FrontendRequest::EnableCapture,
            EnableEmulation => FrontendRequest::EnableEmulation,
            Sync => FrontendRequest::Sync,
            Authorize(d, f) if !d.trim().is_empty() && !f.trim().is_empty() => {
                FrontendRequest::AuthorizeKey(d, f)
            }
            Authorize(_, _) => return Err(IntentError::EmptyField("authorization")),
            RemoveAuthorized(f) if !f.trim().is_empty() => FrontendRequest::RemoveAuthorizedKey(f),
            RemoveAuthorized(_) => return Err(IntentError::EmptyField("fingerprint")),
            UpdateEnterHook(h, v) => {
                FrontendRequest::UpdateEnterHook(h, v.filter(|x| !x.trim().is_empty()))
            }
            SaveConfiguration => FrontendRequest::SaveConfiguration,
            SetClipboardText(v) => FrontendRequest::SetClipboardText(v),
            SetClipboardImage(v) => FrontendRequest::SetClipboardImage(v),
            SetClipboardFiles(v) => FrontendRequest::SetClipboardFiles(v),
            CancelTransfer(0) => return Err(IntentError::InvalidTransferId),
            CancelTransfer(id) => FrontendRequest::CancelClipboardTransfer(id),
        })
    }
}
pub trait EventSource {
    fn recv(&self) -> Result<FrontendEvent, TransportError>;
    fn reconnect(&self) -> bool {
        false
    }
}
pub trait RequestSink {
    fn send(&self, request: FrontendRequest) -> Result<(), TransportError>;
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    Disconnected,
}
pub struct MemoryEventSource(Receiver<FrontendEvent>);
impl EventSource for MemoryEventSource {
    fn recv(&self) -> Result<FrontendEvent, TransportError> {
        self.0.recv().map_err(|_| TransportError::Disconnected)
    }
}
#[derive(Clone)]
pub struct MemoryRequestSink(Sender<FrontendRequest>);
impl RequestSink for MemoryRequestSink {
    fn send(&self, r: FrontendRequest) -> Result<(), TransportError> {
        self.0.send(r).map_err(|_| TransportError::Disconnected)
    }
}
pub fn memory_event_channel() -> (Sender<FrontendEvent>, MemoryEventSource) {
    let (tx, rx) = mpsc::channel();
    (tx, MemoryEventSource(rx))
}
pub fn memory_request_channel() -> (MemoryRequestSink, Receiver<FrontendRequest>) {
    let (tx, rx) = mpsc::channel();
    (MemoryRequestSink(tx), rx)
}

/// Pump daemon events on a blocking worker until the IPC stream closes.
pub fn spawn_event_pump<S, F>(source: S, mut deliver: F) -> std::thread::JoinHandle<()>
where
    S: EventSource + Send + 'static,
    F: FnMut(FrontendEvent) + Send + 'static,
{
    std::thread::spawn(move || {
        while let Ok(event) = source.recv() {
            deliver(event);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_and_rejects() {
        assert_eq!(
            UiIntent::ChangePort(9).into_request(),
            Ok(FrontendRequest::ChangePort(9))
        );
        assert_eq!(
            UiIntent::ChangePort(0).into_request(),
            Err(IntentError::InvalidPort)
        );
    }
    #[test]
    fn fifo() {
        let (sink, rx) = memory_request_channel();
        sink.send(FrontendRequest::Create).unwrap();
        sink.send(FrontendRequest::Sync).unwrap();
        assert_eq!(rx.recv().unwrap(), FrontendRequest::Create);
        assert_eq!(rx.recv().unwrap(), FrontendRequest::Sync);
    }
}

/// Forward events from the concrete IPC reader to a Slint/event-loop callback.
pub fn spawn_ipc_event_pump<F>(
    mut reader: lan_mouse_ipc::FrontendEventReader,
    mut deliver: F,
) -> std::thread::JoinHandle<()>
where
    F: FnMut(FrontendEvent) + Send + 'static,
{
    std::thread::spawn(move || {
        while let Some(result) = reader.next_event() {
            match result {
                Ok(event) => deliver(event),
                Err(error) => {
                    log::error!("frontend IPC event pump stopped: {error}");
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod live_transport_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn event_pump_delivers_each_event_in_order_until_disconnect() {
        let (sender, source) = memory_event_channel();
        sender.send(FrontendEvent::Error("first".into())).unwrap();
        sender.send(FrontendEvent::Error("second".into())).unwrap();
        drop(sender);

        let delivered = Arc::new(Mutex::new(Vec::new()));
        let delivered_by_pump = Arc::clone(&delivered);
        let pump = spawn_event_pump(source, move |event| {
            delivered_by_pump.lock().unwrap().push(event);
        });
        pump.join().unwrap();

        let delivered = delivered.lock().unwrap();
        let messages = delivered
            .iter()
            .map(|event| match event {
                FrontendEvent::Error(message) => message.as_str(),
                _ => panic!("unexpected event"),
            })
            .collect::<Vec<_>>();
        assert_eq!(messages, ["first", "second"]);
    }
}
