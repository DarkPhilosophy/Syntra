use crate::clipboard::ClipboardContent;
use crate::config::local_commit;
use crate::listen::{LanMouseListener, ListenEvent, ListenerCreationError};
use futures::StreamExt;
use input_emulation::{EmulationHandle, InputEmulation, InputEmulationError};
use input_event::Event;
use lan_mouse_proto::{MAX_CLIPBOARD_CHUNK_SIZE, Position, ProtoEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::Cell,
    collections::HashMap,
    net::SocketAddr,
    rc::Rc,
    time::{Duration, Instant},
};
use tokio::{
    select,
    task::{JoinHandle, spawn_local},
};

/// emulation handling events received from a listener
pub(crate) struct Emulation {
    task: JoinHandle<()>,
    request_tx: Sender<EmulationRequest>,
    event_rx: Receiver<EmulationEvent>,
}

pub(crate) enum EmulationEvent {
    Connected {
        addr: SocketAddr,
        fingerprint: String,
    },
    ConnectionAttempt {
        fingerprint: String,
    },
    /// new connection
    Entered {
        /// address of the connection
        addr: SocketAddr,
        /// position of the connection
        pos: lan_mouse_ipc::Position,
        /// certificate fingerprint of the connection
        fingerprint: String,
    },
    /// connection closed
    Disconnected {
        addr: SocketAddr,
    },
    /// the port of the listener has changed
    PortChanged(Result<u16, ListenerCreationError>),
    /// emulation was disabled
    EmulationDisabled,
    /// emulation was enabled
    EmulationEnabled,
    /// capture should be released
    ReleaseNotify,
    /// peer sent us a Hello with its build commit hash. Used to
    /// populate `client_manager.peer_commit` from the listen side
    /// too — without this, peer-version visibility silently fails
    /// whenever the outgoing connection in the *other* direction is
    /// broken (one-way setups, asymmetric NAT, peer's TCP listener
    /// down). The connect-side path stays as the primary source;
    /// this is the defensive fallback.
    PeerHello {
        addr: SocketAddr,
        commit: [u8; 8],
    },
    Clipboard(ClipboardContent),
    /// File-clipboard protocol traffic received from an authenticated listener peer.
    ClipboardProtocol {
        addr: SocketAddr,
        event: ProtoEvent,
    },
}

enum EmulationRequest {
    Reenable,
    Release(SocketAddr),
    ChangePort(u16),
    CaptureReady(bool),
    SendProto { addr: SocketAddr, event: ProtoEvent },
    Terminate,
}

impl Emulation {
    pub(crate) fn new(
        backend: Option<input_emulation::Backend>,
        listener: LanMouseListener,
    ) -> Self {
        let emulation_proxy = EmulationProxy::new(backend);
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let emulation_task = ListenTask {
            listener,
            emulation_proxy,
            request_rx,
            event_tx,
            capture_ready: false,
        };
        let task = spawn_local(emulation_task.run());
        Self {
            task,
            request_tx,
            event_rx,
        }
    }

    pub(crate) fn send_leave_event(&self, addr: SocketAddr) {
        self.request_tx
            .send(EmulationRequest::Release(addr))
            .expect("channel closed");
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(EmulationRequest::Reenable)
            .expect("channel closed");
    }

    pub(crate) fn request_port_change(&self, port: u16) {
        self.request_tx
            .send(EmulationRequest::ChangePort(port))
            .expect("channel closed")
    }

    pub(crate) fn set_capture_ready(&self, ready: bool) {
        self.request_tx
            .send(EmulationRequest::CaptureReady(ready))
            .expect("channel closed");
    }

    pub(crate) fn send_proto(&self, addr: SocketAddr, event: ProtoEvent) {
        self.request_tx
            .send(EmulationRequest::SendProto { addr, event })
            .expect("channel closed");
    }

    pub(crate) async fn event(&mut self) -> EmulationEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    /// wait for termination
    pub(crate) async fn terminate(&mut self) {
        log::debug!("terminating emulation");
        self.request_tx
            .send(EmulationRequest::Terminate)
            .expect("channel closed");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }
}

struct ListenTask {
    listener: LanMouseListener,
    emulation_proxy: EmulationProxy,
    request_rx: Receiver<EmulationRequest>,
    event_tx: Sender<EmulationEvent>,
    capture_ready: bool,
}

#[derive(Debug)]
enum ClipboardTransferKind {
    Text,
    Image { width: u32, height: u32 },
}

#[derive(Debug)]
struct ClipboardTransfer {
    kind: ClipboardTransferKind,
    next: u32,
    chunks: u32,
    total_len: usize,
    bytes: Vec<u8>,
}

impl ListenTask {
    async fn run(mut self) {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        let mut last_response = HashMap::new();
        let mut clipboard_transfers: HashMap<(SocketAddr, u64), ClipboardTransfer> = HashMap::new();
        let mut rejected_connections = HashMap::new();
        loop {
            select! {
                e = self.listener.next() => {match e {
                    Some(ListenEvent::Msg { event, addr }) => {
                        log::trace!("{event} <-<-<-<-<- {addr}");
                        last_response.insert(addr, Instant::now());
                        match event {
                            ProtoEvent::Enter(pos) => {
                                if !self.emulation_proxy.emulation_ready.get()
                                    || !self.capture_ready
                                {
                                    log::warn!(
                                        "rejecting entry from {addr}: remote input is unavailable"
                                    );
                                    self.emulation_proxy.remove(addr);
                                    self.listener.reply(addr, ProtoEvent::Leave(0)).await;
                                    continue;
                                }
                                if let Some(fingerprint) =
                                    self.listener.get_certificate_fingerprint(addr).await
                                {
                                    log::info!("accepting entry from {addr}");
                                    self.event_tx
                                        .send(EmulationEvent::ReleaseNotify)
                                        .expect("channel closed");
                                    self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                                    self.event_tx
                                        .send(EmulationEvent::Entered {
                                            addr,
                                            pos: to_ipc_pos(pos),
                                            fingerprint,
                                        })
                                        .expect("channel closed");
                                } else {
                                    self.listener.reply(addr, ProtoEvent::Leave(0)).await;
                                }
                            }
                            ProtoEvent::Leave(_) => {
                                self.emulation_proxy.remove(addr);
                                self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                            }
                            ProtoEvent::Input(event) => self.emulation_proxy.consume(event, addr),
                            ProtoEvent::Ping => {
                                self.listener
                                    .reply(
                                        addr,
                                        ProtoEvent::Pong(
                                            self.emulation_proxy.emulation_ready.get()
                                                && self.capture_ready,
                                        ),
                                    )
                                    .await
                            }
                            // Peer's version handshake. Echo our own
                            // commit back so the peer's connect-side
                            // receive_loop populates its `peer_commit`,
                            // AND publish a PeerHello upward so our
                            // service can populate ours from the listen
                            // side too — the connect side is the primary
                            // path, but if the outbound direction is
                            // broken (one-way setup, NAT, peer's TCP
                            // listener down) the version display would
                            // otherwise silently say "unknown" while
                            // the peer is in fact happily talking to us.
                            ProtoEvent::Hello { commit } => {
                                self.listener.reply(addr, ProtoEvent::Hello { commit: local_commit() }).await;
                                self.event_tx.send(EmulationEvent::PeerHello { addr, commit }).expect("channel closed");
                            }
                            ProtoEvent::ClipboardStart { transfer_id, total_len, chunks } => {
                                clipboard_transfers.insert(
                                    (addr, transfer_id),
                                    ClipboardTransfer {
                                        kind: ClipboardTransferKind::Text,
                                        next: 0,
                                        chunks,
                                        total_len: total_len as usize,
                                        bytes: Vec::with_capacity(total_len as usize),
                                    },
                                );
                            }
                            ProtoEvent::ClipboardImageStart { transfer_id, width, height, total_len, chunks } => {
                                clipboard_transfers.insert(
                                    (addr, transfer_id),
                                    ClipboardTransfer {
                                        kind: ClipboardTransferKind::Image { width, height },
                                        next: 0,
                                        chunks,
                                        total_len: total_len as usize,
                                        bytes: Vec::with_capacity(total_len as usize),
                                    },
                                );
                            }
                            ProtoEvent::ClipboardChunk { transfer_id, index, data } => {
                                let key = (addr, transfer_id);
                                let mut complete = false;
                                if let Some(transfer) = clipboard_transfers.get_mut(&key) {
                                    if index == transfer.next
                                        && data.len() <= MAX_CLIPBOARD_CHUNK_SIZE
                                        && transfer.bytes.len() + data.len() <= transfer.total_len
                                    {
                                        transfer.bytes.extend_from_slice(&data);
                                        transfer.next += 1;
                                        complete = transfer.next == transfer.chunks
                                            && transfer.bytes.len() == transfer.total_len;
                                        if transfer.next == transfer.chunks && !complete {
                                            clipboard_transfers.remove(&key);
                                        }
                                    } else {
                                        clipboard_transfers.remove(&key);
                                    }
                                }
                                if complete {
                                    if let Some(transfer) = clipboard_transfers.remove(&key) {
                                        let content = match transfer.kind {
                                            ClipboardTransferKind::Text => match String::from_utf8(transfer.bytes) {
                                                Ok(text) => Some(ClipboardContent::Text(text)),
                                                Err(e) => {
                                                    log::warn!("ignoring invalid UTF-8 clipboard text: {e}");
                                                    None
                                                }
                                            },
                                            ClipboardTransferKind::Image { width, height } => {
                                                Some(ClipboardContent::Image { width, height, rgba: transfer.bytes })
                                            }
                                        };
                                        if let Some(content) = content {
                                            self.event_tx.send(EmulationEvent::Clipboard(content)).expect("channel closed");
                                        }
                                    }
                                }
                            }
                            event @ (
                                ProtoEvent::ClipboardCapabilities(_)
                                | ProtoEvent::ClipboardManifest { .. }
                                | ProtoEvent::ClipboardFileRequest { .. }
                                | ProtoEvent::ClipboardFileChunk { .. }
                                | ProtoEvent::ClipboardFileComplete { .. }
                                | ProtoEvent::ClipboardTransferCancel { .. }
                                | ProtoEvent::ClipboardTransferProgress { .. }
                            ) => {
                                self.event_tx
                                    .send(EmulationEvent::ClipboardProtocol { addr, event })
                                    .expect("channel closed");
                            }
                            _ => {}
                        }
                    }
                    Some(ListenEvent::Accept { addr, fingerprint }) => {
                        self.event_tx.send(EmulationEvent::Connected { addr, fingerprint }).expect("channel closed");
                    }
                    Some(ListenEvent::Rejected { fingerprint }) => {
                        if rejected_connections.insert(fingerprint.clone(), Instant::now())
                            .is_none_or(|i| i.elapsed() >= Duration::from_secs(2)) {
                                self.event_tx.send(EmulationEvent::ConnectionAttempt { fingerprint }).expect("channel closed");
                            }
                    }
                    None => break
                }}
                event = self.emulation_proxy.event() => {
                    self.event_tx.send(event).expect("channel closed");
                }
                request = self.request_rx.recv() => match request.expect("channel closed") {
                    // reenable emulation
                    EmulationRequest::Reenable => self.emulation_proxy.reenable(),
                    // notify the other end that we hit a barrier (should release capture)
                    EmulationRequest::Release(addr) => self.listener.reply(addr, ProtoEvent::Leave(0)).await,
                    EmulationRequest::ChangePort(port) => {
                        self.listener.request_port_change(port);
                        let result = self.listener.port_changed().await;
                        self.event_tx.send(EmulationEvent::PortChanged(result)).expect("channel closed");
                    }
                    EmulationRequest::CaptureReady(ready) => {
                        self.capture_ready = ready;
                    }
                    EmulationRequest::SendProto { addr, event } => {
                        self.listener.reply(addr, event).await;
                    }
                    EmulationRequest::Terminate => break,
                },
                _ = interval.tick() => {
                    last_response.retain(|&addr,instant| {
                        if instant.elapsed() > Duration::from_secs(1) {
                            log::warn!("releasing keys: {addr} not responding!");
                            self.emulation_proxy.remove(addr);
                            self.event_tx.send(EmulationEvent::Disconnected { addr }).expect("channel closed");
                            false
                        } else {
                            true
                        }
                    });
                }
            }
        }
        self.listener.terminate().await;
        self.emulation_proxy.terminate().await;
    }
}

/// proxy handling the actual input emulation,
/// discarding events when it is disabled
pub(crate) struct EmulationProxy {
    emulation_active: Rc<Cell<bool>>,
    emulation_ready: Rc<Cell<bool>>,
    exit_requested: Rc<Cell<bool>>,
    request_tx: Sender<ProxyRequest>,
    event_rx: Receiver<EmulationEvent>,
    task: JoinHandle<()>,
}

enum ProxyRequest {
    Input(Event, SocketAddr),
    Remove(SocketAddr),
    Terminate,
    Reenable,
}

impl EmulationProxy {
    fn new(backend: Option<input_emulation::Backend>) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let emulation_active = Rc::new(Cell::new(false));
        let emulation_ready = Rc::new(Cell::new(false));
        let exit_requested = Rc::new(Cell::new(false));
        let emulation_task = EmulationTask {
            backend,
            emulation_ready: emulation_ready.clone(),
            exit_requested: exit_requested.clone(),
            request_rx,
            event_tx,
            handles: Default::default(),
            next_id: 0,
        };
        let task = spawn_local(emulation_task.run());
        Self {
            emulation_active,
            emulation_ready,
            exit_requested,
            request_tx,
            task,
            event_rx,
        }
    }

    async fn event(&mut self) -> EmulationEvent {
        let event = self.event_rx.recv().await.expect("channel closed");
        if let EmulationEvent::EmulationEnabled = event {
            self.emulation_active.replace(true);
        }
        if let EmulationEvent::EmulationDisabled = event {
            self.emulation_active.replace(false);
        }
        event
    }

    fn consume(&self, event: Event, addr: SocketAddr) {
        // ignore events if emulation is currently disabled
        if self.emulation_active.get() {
            self.request_tx
                .send(ProxyRequest::Input(event, addr))
                .expect("channel closed");
        }
    }

    fn remove(&self, addr: SocketAddr) {
        self.request_tx
            .send(ProxyRequest::Remove(addr))
            .expect("channel closed");
    }

    fn reenable(&self) {
        self.request_tx
            .send(ProxyRequest::Reenable)
            .expect("channel closed");
    }

    async fn terminate(&mut self) {
        self.exit_requested.replace(true);
        self.request_tx
            .send(ProxyRequest::Terminate)
            .expect("channel closed");
        let _ = (&mut self.task).await;
    }
}

struct EmulationTask {
    backend: Option<input_emulation::Backend>,
    emulation_ready: Rc<Cell<bool>>,
    exit_requested: Rc<Cell<bool>>,
    request_rx: Receiver<ProxyRequest>,
    event_tx: Sender<EmulationEvent>,
    handles: HashMap<SocketAddr, EmulationHandle>,
    next_id: EmulationHandle,
}

impl EmulationTask {
    async fn run(mut self) {
        loop {
            if let Err(e) = self.do_emulation().await {
                log::warn!("input emulation exited: {e}");
            }
            if self.exit_requested.get() {
                break;
            }
            // wait for reenable request
            loop {
                match self.request_rx.recv().await.expect("channel closed") {
                    ProxyRequest::Reenable => break,
                    ProxyRequest::Terminate => return,
                    ProxyRequest::Input(..) => { /* emulation inactive => ignore */ }
                    ProxyRequest::Remove(..) => { /* emulation inactive => ignore */ }
                }
            }
        }
    }

    async fn do_emulation(&mut self) -> Result<(), InputEmulationError> {
        log::info!("creating input emulation ...");
        let mut emulation = tokio::select! {
            r = InputEmulation::new(self.backend) => r?,
            // allow termination event while requesting input emulation
            _ = wait_for_termination(&mut self.request_rx) => return Ok(()),
        };

        // The portal being approved is not enough: only advertise readiness
        // after the backend has completed initialization and can consume input.
        self.emulation_ready.set(true);
        let _emulation_guard = ReadyGuard::new(self.emulation_ready.clone(), self.event_tx.clone());

        // create active handles
        if let Err(e) = self.create_clients(&mut emulation).await {
            emulation.terminate().await;
            return Err(e);
        }

        let res = self.do_emulation_session(&mut emulation).await;
        // FIXME replace with async drop when stabilized
        emulation.terminate().await;
        res
    }

    async fn create_clients(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<(), InputEmulationError> {
        for handle in self.handles.values() {
            tokio::select! {
                _ = emulation.create(*handle) => {},
                _ = wait_for_termination(&mut self.request_rx) => return Ok(()),
            }
        }
        Ok(())
    }

    async fn do_emulation_session(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<(), InputEmulationError> {
        let mut health_check = tokio::time::interval(Duration::from_millis(100));
        health_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    ProxyRequest::Input(event, addr) => {
                        let handle = match self.handles.get(&addr) {
                            Some(&handle) => handle,
                            None => {
                                let handle = self.next_id;
                                self.next_id += 1;
                                emulation.create(handle).await;
                                self.handles.insert(addr, handle);
                                handle
                            }
                        };
                        emulation.consume(event, handle).await?;
                    },
                    ProxyRequest::Remove(addr) => {
                        if let Some(handle) = self.handles.remove(&addr) {
                            emulation.destroy(handle).await;
                        }
                    }
                    ProxyRequest::Terminate => break Ok(()),
                    ProxyRequest::Reenable => continue,
                },
                _ = health_check.tick() => {
                    if !emulation.healthy() {
                        log::warn!("input emulation backend transport was lost");
                        break Err(input_emulation::EmulationError::EndOfStream.into());
                    }
                },
            }
        }
    }
}

fn to_ipc_pos(pos: Position) -> lan_mouse_ipc::Position {
    match pos {
        Position::Left => lan_mouse_ipc::Position::Left,
        Position::Right => lan_mouse_ipc::Position::Right,
        Position::Top => lan_mouse_ipc::Position::Top,
        Position::Bottom => lan_mouse_ipc::Position::Bottom,
    }
}

async fn wait_for_termination(rx: &mut Receiver<ProxyRequest>) {
    loop {
        match rx.recv().await.expect("channel closed") {
            ProxyRequest::Terminate => return,
            ProxyRequest::Input(_, _) => continue,
            ProxyRequest::Remove(_) => continue,
            ProxyRequest::Reenable => continue,
        }
    }
}

struct ReadyGuard {
    ready: Rc<Cell<bool>>,
    event_tx: Sender<EmulationEvent>,
}

impl ReadyGuard {
    fn new(ready: Rc<Cell<bool>>, event_tx: Sender<EmulationEvent>) -> Self {
        event_tx
            .send(EmulationEvent::EmulationEnabled)
            .expect("channel closed");
        Self { ready, event_tx }
    }
}

impl Drop for ReadyGuard {
    fn drop(&mut self) {
        self.ready.set(false);
        self.event_tx
            .send(EmulationEvent::EmulationDisabled)
            .expect("channel closed");
    }
}
