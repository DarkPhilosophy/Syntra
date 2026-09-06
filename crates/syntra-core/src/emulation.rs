use crate::clipboard::ClipboardContent;
use crate::config::local_commit;
use crate::listen::{LanMouseListener, ListenEvent, ListenerCreationError};
use futures::StreamExt;
use image::GenericImageView;
use syntra_input_emulation::{EmulationHandle, InputEmulation, InputEmulationError};
use syntra_input_event::{Event, PointerEvent};
use syntra_store::MAX_IMAGE_BYTES as MAX_HISTORY_IMAGE_BYTES;
use syntra_proto::{MAX_CLIPBOARD_CHUNK_SIZE, Position, ProtoEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    net::SocketAddr,
    rc::Rc,
    time::{Duration, Instant},
};
use tokio::{
    select,
    sync::oneshot,
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
        pos: syntra_api::Position,
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
    Clipboard {
        addr: SocketAddr,
        transfer_id: u64,
        content: ClipboardContent,
    },
    FileClipboard {
        mime_type: String,
        value: String,
    },
    NativeClipboard(ClipboardContent),
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
    SetInputSharing(bool),
    SendProto {
        addr: SocketAddr,
        event: ProtoEvent,
    },
    PublishFileClipboard {
        contents: Vec<(String, Vec<u8>)>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Terminate,
}

impl Emulation {
    pub(crate) fn new(
        backend: Option<syntra_input_emulation::Backend>,
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
            input_sharing: true,
            active_inputs: HashSet::new(),
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

    pub(crate) fn set_input_sharing(&self, enabled: bool) {
        self.request_tx
            .send(EmulationRequest::SetInputSharing(enabled))
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
    pub(crate) fn publish_file_clipboard(&self, contents: Vec<(String, Vec<u8>)>) {
        let (reply, result) = oneshot::channel();
        if self
            .request_tx
            .send(EmulationRequest::PublishFileClipboard { contents, reply })
            .is_err()
        {
            log::warn!("cannot publish file clipboard: emulation task stopped");
            return;
        }
        tokio::task::spawn_local(async move {
            match result.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => log::warn!("cannot publish file clipboard: {error}"),
                Err(_) => log::warn!("cannot publish file clipboard: emulation task stopped"),
            }
        });
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
    input_sharing: bool,
    active_inputs: HashSet<SocketAddr>,
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
                                if !self.input_sharing
                                    || !self.emulation_proxy.emulation_ready.get()
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
                                    self.active_inputs.insert(addr);
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
                                self.active_inputs.remove(&addr);
                                self.emulation_proxy.remove(addr);
                                self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                            }
                            ProtoEvent::Input(event) => {
                                if self.input_sharing && self.active_inputs.contains(&addr) {
                                    self.emulation_proxy.consume(event, addr);
                                }
                            }
                            ProtoEvent::Ping => {
                                self.listener
                                    .reply(
                                        addr,
                                        ProtoEvent::Pong(
                                            self.input_sharing
                                                && self.emulation_proxy.emulation_ready.get()
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
                                            self.event_tx.send(EmulationEvent::Clipboard { addr, transfer_id, content }).expect("channel closed");
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
                                | ProtoEvent::ManualFileOffer { .. }
                                | ProtoEvent::ManualFileDecision { .. }
                                | ProtoEvent::ManualFileRequest { .. }
                                | ProtoEvent::ManualFileChunk { .. }
                                | ProtoEvent::ManualFileComplete { .. }
                                | ProtoEvent::ManualFileResult { .. }
                                | ProtoEvent::ManualFileCancel { .. }
                                | ProtoEvent::HistorySyncRequest { .. }
                                | ProtoEvent::HistoryRecordStart { .. }
                                | ProtoEvent::HistoryRecordChunk { .. }
                                | ProtoEvent::HistorySyncPageEnd { .. }
                                | ProtoEvent::HistoryClearBoundaryRequest { .. }
                                | ProtoEvent::HistoryClearBoundary { .. }
                                | ProtoEvent::HistoryClearApply { .. }
                                | ProtoEvent::HistoryClearAck { .. }
                                | ProtoEvent::ProfileStart { .. }
                                | ProtoEvent::ProfileChunk { .. }
                                | ProtoEvent::ProfileRequest { .. }
                                | ProtoEvent::ProfileChanged
                            ) => {
                                self.event_tx
                                    .send(EmulationEvent::ClipboardProtocol { addr, event })
                                    .expect("channel closed");
                            }
                            _ => {}
                        }
                    }
                    Some(ListenEvent::Accept { addr, fingerprint }) => {
                        last_response.insert(addr, Instant::now());
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
                    EmulationRequest::SetInputSharing(enabled) => {
                        self.input_sharing = enabled;
                        self.emulation_proxy.set_input_sharing(enabled);
                        if !enabled {
                            for addr in self.active_inputs.drain() {
                                self.emulation_proxy.remove(addr);
                                self.listener.reply(addr, ProtoEvent::Leave(0)).await;
                            }
                        }
                    }
                    EmulationRequest::SendProto { addr, event } => {
                        self.listener.reply(addr, event).await;
                    }
                    EmulationRequest::PublishFileClipboard { contents, reply } => {
                        let result = self.emulation_proxy.publish_file_clipboard(contents).await;
                        let _ = reply.send(result);
                    }
                    EmulationRequest::Terminate => break,
                },
                _ = interval.tick() => {
                    last_response.retain(|&addr,instant| {
                        if instant.elapsed() > Duration::from_secs(1) {
                            log::warn!("releasing keys: {addr} not responding!");
                            self.active_inputs.remove(&addr);
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
    input_sharing: Rc<Cell<bool>>,
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
    SetInputSharing(bool),
    PublishFileClipboard {
        contents: Vec<(String, Vec<u8>)>,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

impl EmulationProxy {
    fn new(backend: Option<syntra_input_emulation::Backend>) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let emulation_active = Rc::new(Cell::new(false));
        let emulation_ready = Rc::new(Cell::new(false));
        let input_sharing = Rc::new(Cell::new(true));
        let exit_requested = Rc::new(Cell::new(false));
        let emulation_task = EmulationTask {
            backend,
            emulation_ready: emulation_ready.clone(),
            input_sharing: input_sharing.clone(),
            exit_requested: exit_requested.clone(),
            request_rx,
            event_tx,
            handles: Default::default(),
            pressed_buttons: Default::default(),
            next_id: 0,
        };
        let task = spawn_local(emulation_task.run());
        Self {
            emulation_active,
            emulation_ready,
            input_sharing,
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
        if self.emulation_active.get() && self.input_sharing.get() {
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
    fn set_input_sharing(&self, enabled: bool) {
        self.input_sharing.set(enabled);
        self.request_tx
            .send(ProxyRequest::SetInputSharing(enabled))
            .expect("channel closed");
    }

    fn reenable(&self) {
        self.request_tx
            .send(ProxyRequest::Reenable)
            .expect("channel closed");
    }
    async fn publish_file_clipboard(&self, contents: Vec<(String, Vec<u8>)>) -> Result<(), String> {
        let (reply, result) = oneshot::channel();
        self.request_tx
            .send(ProxyRequest::PublishFileClipboard { contents, reply })
            .map_err(|_| "emulation task stopped".to_string())?;
        result
            .await
            .map_err(|_| "emulation task stopped".to_string())?
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
    backend: Option<syntra_input_emulation::Backend>,
    emulation_ready: Rc<Cell<bool>>,
    input_sharing: Rc<Cell<bool>>,
    exit_requested: Rc<Cell<bool>>,
    request_rx: Receiver<ProxyRequest>,
    event_tx: Sender<EmulationEvent>,
    handles: HashMap<SocketAddr, EmulationHandle>,
    pressed_buttons: HashMap<SocketAddr, HashSet<u32>>,
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
                    ProxyRequest::SetInputSharing(enabled) => {
                        if !enabled {
                            self.handles.clear();
                            self.pressed_buttons.clear();
                        }
                    }
                    ProxyRequest::PublishFileClipboard { reply, .. } => {
                        let _ = reply.send(Err("emulation inactive".to_string()));
                    }
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
        if !self.input_sharing.get() {
            self.handles.clear();
            self.pressed_buttons.clear();
            return Ok(());
        }
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
                        if self.input_sharing.get() {
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
                            if let Event::Pointer(PointerEvent::Button { button, state, .. }) = event {
                                let buttons = self.pressed_buttons.entry(addr).or_default();
                                if state == 0 {
                                    buttons.remove(&button);
                                } else {
                                    buttons.insert(button);
                                }
                            }
                            emulation.consume(event, handle).await?;
                        }
                    },
                    ProxyRequest::Remove(addr) => {
                        self.release_client(emulation, addr).await?;
                    }
                    ProxyRequest::SetInputSharing(enabled) => {
                        if !enabled {
                            let addrs = self.handles.keys().copied().collect::<Vec<_>>();
                            for addr in addrs {
                                self.release_client(emulation, addr).await?;
                            }
                        }
                    }
                    ProxyRequest::Terminate => break Ok(()),
                    ProxyRequest::Reenable => continue,
                    ProxyRequest::PublishFileClipboard { contents, reply } => {
                        let result = emulation
                            .set_file_clipboard(contents)
                            .await
                            .map_err(|error| error.to_string());
                        let _ = reply.send(result);
                    },
                },
                clipboard = emulation.clipboard_event() => {
                    if let Some((mime_type, data)) = clipboard {
                        if matches!(
                            mime_type.as_str(),
                            "image/png" | "image/jpeg" | "image/jpg"
                        ) {
                            const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
                            if data.len() > MAX_IMAGE_BYTES {
                                log::warn!("native image clipboard exceeds {MAX_IMAGE_BYTES} bytes");
                                continue;
                            }
                            let mime_type = mime_type.clone();
                            let decoded = tokio::task::spawn_blocking(move || {
                                decode_native_image(&mime_type, data)
                            }).await;
                            match decoded {
                                Ok(Ok((width, height, rgba))) => self.event_tx
                                    .send(EmulationEvent::NativeClipboard(
                                        ClipboardContent::Image { width, height, rgba },
                                    ))
                                    .expect("channel closed"),
                                Ok(Err(error)) => log::warn!("native image clipboard decode failed: {error}"),
                                Err(error) => log::warn!("native image clipboard decoder task failed: {error}"),
                            }
                        } else if matches!(
                            mime_type.as_str(),
                            "text/plain;charset=utf-8" | "text/plain" | "UTF8_STRING"
                        ) {
                            match String::from_utf8(data) {
                                Ok(text) => self.event_tx
                                    .send(EmulationEvent::NativeClipboard(ClipboardContent::Text(text)))
                                    .expect("channel closed"),
                                Err(error) => log::warn!("native text clipboard is not UTF-8: {error}"),
                            }
                        } else {
                            match String::from_utf8(data) {
                                Ok(value) => self.event_tx
                                    .send(EmulationEvent::FileClipboard { mime_type, value })
                                    .expect("channel closed"),
                                Err(error) => log::warn!("native file clipboard is not UTF-8: {error}"),
                            }
                        }
                    }
                },
                _ = health_check.tick() => {
                    if !emulation.healthy() {
                        log::warn!("input emulation backend transport was lost");
                        break Err(syntra_input_emulation::EmulationError::EndOfStream.into());
                    }
                },
            }
        }
    }

    async fn release_client(
        &mut self,
        emulation: &mut InputEmulation,
        addr: SocketAddr,
    ) -> Result<(), InputEmulationError> {
        let Some(handle) = self.handles.remove(&addr) else {
            self.pressed_buttons.remove(&addr);
            return Ok(());
        };
        let mut release_result = Ok(());
        for button in self.pressed_buttons.remove(&addr).unwrap_or_default() {
            if let Err(error) = emulation
                .consume(
                    Event::Pointer(PointerEvent::Button {
                        time: 0,
                        button,
                        state: 0,
                    }),
                    handle,
                )
                .await
            {
                if release_result.is_ok() {
                    release_result = Err(error);
                }
            }
        }
        emulation.destroy(handle).await;
        release_result.map_err(Into::into)
    }
}

fn to_ipc_pos(pos: Position) -> syntra_api::Position {
    match pos {
        Position::Left => syntra_api::Position::Left,
        Position::Right => syntra_api::Position::Right,
        Position::Top => syntra_api::Position::Top,
        Position::Bottom => syntra_api::Position::Bottom,
    }
}

async fn wait_for_termination(rx: &mut Receiver<ProxyRequest>) {
    loop {
        match rx.recv().await.expect("channel closed") {
            ProxyRequest::Terminate => return,
            ProxyRequest::Input(_, _) => continue,
            ProxyRequest::Remove(_) => continue,
            ProxyRequest::SetInputSharing(_) => continue,
            ProxyRequest::Reenable => continue,
            ProxyRequest::PublishFileClipboard { reply, .. } => {
                let _ = reply.send(Err("emulation task stopped".to_string()));
            }
        }
    }
}
fn decode_native_image(mime_type: &str, data: Vec<u8>) -> Result<(u32, u32, Vec<u8>), String> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(data));
    reader.set_format(match mime_type {
        "image/png" => image::ImageFormat::Png,
        _ => image::ImageFormat::Jpeg,
    });
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|error| error.to_string())?;
    let (width, height) = decoded.dimensions();
    let rgba_bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "native image dimensions overflow".to_owned())?;
    if rgba_bytes > MAX_HISTORY_IMAGE_BYTES as u64 {
        return Err(format!(
            "native image RGBA payload exceeds {} bytes",
            MAX_HISTORY_IMAGE_BYTES
        ));
    }
    Ok((width, height, decoded.into_rgba8().into_raw()))
}

#[cfg(test)]
mod tests {
    use super::decode_native_image;
    use image::{ImageBuffer, ImageEncoder, Rgba};

    #[test]
    fn decodes_png_exact_pixels() {
        let pixels = [Rgba([1, 2, 3, 4]), Rgba([5, 6, 7, 8])];
        let raw = pixels.iter().flat_map(|pixel| pixel.0).collect::<Vec<u8>>();
        let mut encoded = Vec::new();
        image::codecs::png::PngEncoder::new(&mut encoded)
            .write_image(&raw, 2, 1, image::ColorType::Rgba8.into())
            .unwrap();
        let (width, height, rgba) = decode_native_image("image/png", encoded).unwrap();
        assert_eq!((width, height), (2, 1));
        assert_eq!(rgba, pixels.iter().flat_map(|p| p.0).collect::<Vec<_>>());
    }

    #[test]
    fn rejects_declared_oversize_rgba_payload() {
        let image = ImageBuffer::from_pixel(4097, 2049, Rgba([0, 0, 0, 0]));
        let mut encoded = Vec::new();
        image::codecs::png::PngEncoder::new(&mut encoded)
            .write_image(image.as_raw(), 4097, 2049, image::ColorType::Rgba8.into())
            .unwrap();
        assert!(decode_native_image("image/png", encoded).is_err());
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
