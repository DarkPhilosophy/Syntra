use crate::clipboard::ClipboardContent;
use crate::config::local_commit;
use crate::listen::{ListenEvent, ListenerCreationError, SyntraListener};
use futures::StreamExt;
use image::GenericImageView;
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    future::Future,
    net::SocketAddr,
    rc::Rc,
    time::{Duration, Instant},
};
use syntra_input_emulation::{
    EmulationCreationError, EmulationHandle, InputEmulation, InputEmulationError,
};
use syntra_input_event::{Event, PointerEvent};
use syntra_proto::{MAX_CLIPBOARD_CHUNK_SIZE, Position, ProtoEvent};
use syntra_store::MAX_IMAGE_BYTES as MAX_HISTORY_IMAGE_BYTES;
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

#[cfg(target_os = "android")]
fn capture_pos(side: Position) -> syntra_input_capture::Position {
    match side {
        Position::Left => syntra_input_capture::Position::Left,
        Position::Right => syntra_input_capture::Position::Right,
        Position::Top => syntra_input_capture::Position::Top,
        Position::Bottom => syntra_input_capture::Position::Bottom,
    }
}

/// Whether leaving through `edge` returns towards a peer that entered
/// from `entered`: `Enter` carries the side of this screen the peer is on.
fn edge_matches(entered: Position, edge: syntra_input_emulation::PointerEdge) -> bool {
    use syntra_input_emulation::PointerEdge;
    matches!(
        (entered, edge),
        (Position::Left, PointerEdge::Left)
            | (Position::Right, PointerEdge::Right)
            | (Position::Top, PointerEdge::Top)
            | (Position::Bottom, PointerEdge::Bottom)
    )
}

pub(crate) enum EmulationEvent {
    /// No peer's pointer is on this screen any more.
    NoLongerControlled,
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
    /// Emulation became available, carrying the backend that was selected.
    ///
    /// The name reaches the interface: which backend won decides what the
    /// user must grant permission for, and "running" alone explains nothing.
    EmulationEnabled(String),
    /// capture should be released
    ReleaseNotify,
    /// A peer's independent cursor was pushed past a desktop edge. Handled
    /// inside the listen task, never forwarded to the service.
    PointerEdge {
        addr: SocketAddr,
        edge: syntra_input_emulation::PointerEdge,
    },
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
    /// Clipboard content replayed from an authenticated peer.
    ///
    /// Neither the peer address nor the wire transfer id is carried: the
    /// content is applied to the local clipboard regardless of which peer
    /// sent it, and routing was already decided upstream.
    Clipboard {
        content: ClipboardContent,
    },
    FileClipboard {
        mime_type: String,
        value: String,
    },
    /// Native publication failed or the backend has no clipboard; the
    /// service publishes through its own clipboard path instead.
    ClipboardFallback(ClipboardFallback),
    NativeClipboard(ClipboardContent),
    /// File-clipboard protocol traffic received from an authenticated listener peer.
    ClipboardProtocol {
        addr: SocketAddr,
        event: ProtoEvent,
    },
}

/// Where a clipboard goes when the emulation backend cannot publish it.
///
/// Kernel-level backends such as uinput have no clipboard at all, so every
/// publication must name a working alternative rather than be dropped.
#[derive(Debug)]
pub(crate) enum ClipboardFallback {
    /// Hand file URIs to the clipboard adapter plugin.
    Adapter(syntra_plugin_api::PublishFileClipboard),
    /// Write through the service's desktop clipboard.
    Local(ClipboardContent),
}

enum EmulationRequest {
    Reenable,
    Release(SocketAddr),
    ChangePort(u16),
    CaptureReady(bool),
    SetInputSharing(bool),
    SetIndependentPointers(bool),
    /// Allow carrying a controlling device's pointer on to unpaired devices.
    SetHopBypass(bool),
    /// Enable passing the pointer on to further devices (multi-hop).
    SetMultiHop(bool),
    /// Move the local pointer a little inside from this edge.
    StepInside(syntra_input_capture::Position),
    /// Where each configured peer sits on this screen, by address, and
    /// which certificate sits on each side.
    SetPeerSides(
        HashMap<std::net::IpAddr, Position>,
        HashMap<Position, String>,
    ),
    SendProto {
        addr: SocketAddr,
        event: ProtoEvent,
    },
    PublishFileClipboard {
        contents: Vec<(String, Vec<u8>)>,
        fallback: ClipboardFallback,
    },
    Terminate,
}

impl Emulation {
    pub(crate) fn new(
        backend: Option<syntra_input_emulation::Backend>,
        listener: SyntraListener,
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
            independent_pointers: false,
            input_sharing: true,
            active_inputs: HashSet::new(),
            entered_from: HashMap::new(),
            peer_sides: HashMap::new(),
            peer_beyond: HashMap::new(),
            peer_fingerprints: HashMap::new(),
            hop_bypass: false,
            multi_hop: false,
            relay: None,
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

    pub(crate) fn set_independent_pointers(&self, enabled: bool) {
        self.request_tx
            .send(EmulationRequest::SetIndependentPointers(enabled))
            .expect("channel closed");
    }

    /// Updates where each configured peer sits on this screen. An entering
    /// peer's pointer appears on, and leaves through, the side configured
    /// here: the other machine's own map may disagree.
    pub(crate) fn set_peer_sides(
        &self,
        sides: HashMap<std::net::IpAddr, Position>,
        beyond: HashMap<Position, String>,
    ) {
        self.request_tx
            .send(EmulationRequest::SetPeerSides(sides, beyond))
            .expect("channel closed");
    }

    pub(crate) fn step_inside(&self, edge: syntra_input_capture::Position) {
        let _ = self.request_tx.send(EmulationRequest::StepInside(edge));
    }

    pub(crate) fn set_multi_hop(&self, enabled: bool) {
        self.request_tx
            .send(EmulationRequest::SetMultiHop(enabled))
            .expect("channel closed");
    }

    pub(crate) fn set_hop_bypass(&self, enabled: bool) {
        self.request_tx
            .send(EmulationRequest::SetHopBypass(enabled))
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
    pub(crate) fn publish_file_clipboard(
        &self,
        contents: Vec<(String, Vec<u8>)>,
        fallback: ClipboardFallback,
    ) {
        if self
            .request_tx
            .send(EmulationRequest::PublishFileClipboard { contents, fallback })
            .is_err()
        {
            log::warn!("cannot publish file clipboard: emulation task stopped");
        }
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
    listener: SyntraListener,
    emulation_proxy: EmulationProxy,
    request_rx: Receiver<EmulationRequest>,
    event_tx: Sender<EmulationEvent>,
    capture_ready: bool,
    /// Peers drive their own cursor, which returns control by itself.
    independent_pointers: bool,
    input_sharing: bool,
    active_inputs: HashSet<SocketAddr>,
    /// Side of this screen each active peer entered from.
    entered_from: HashMap<SocketAddr, Position>,
    /// Side of this screen each configured peer sits on, from local config.
    peer_sides: HashMap<std::net::IpAddr, Position>,
    /// Certificate of the configured device on each side of this screen.
    peer_beyond: HashMap<Position, String>,
    /// Allow a device controlling this one to reach the next device through
    /// this one even when the two are not paired (off by default).
    hop_bypass: bool,
    /// Multi-hop enabled: pass the pointer on through edges facing other
    /// devices. Off by default.
    multi_hop: bool,
    /// Bypass in progress: input from this peer is carried on through the
    /// given edge by this device.
    relay: Option<(SocketAddr, Position)>,
    /// Certificate of each connected peer, for telling a return from a hop.
    peer_fingerprints: HashMap<SocketAddr, String>,
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
    started: Instant,
}

impl ClipboardTransfer {
    fn new(kind: ClipboardTransferKind, total_len: usize, chunks: u32) -> Self {
        Self {
            kind,
            next: 0,
            chunks,
            total_len,
            // Grow with received data rather than trusting the declared size:
            // a start record alone must not reserve up to the protocol maximum.
            bytes: Vec::with_capacity(total_len.min(CLIPBOARD_INITIAL_CAPACITY)),
            started: Instant::now(),
        }
    }
}

/// Initial buffer for an inbound clipboard; larger payloads grow as chunks arrive.
const CLIPBOARD_INITIAL_CAPACITY: usize = 1024 * 1024;
/// Incomplete inbound clipboards are discarded after this long.
const CLIPBOARD_TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);

/// Starts reassembling a clipboard from `addr`. A clipboard is latest-wins,
/// so any unfinished earlier transfer from the same peer is superseded; this
/// bounds reassembly state to one buffer per peer.
fn start_clipboard_transfer(
    transfers: &mut HashMap<(SocketAddr, u64), ClipboardTransfer>,
    addr: SocketAddr,
    transfer_id: u64,
    transfer: ClipboardTransfer,
) {
    transfers.retain(|(peer, _), _| *peer != addr);
    transfers.insert((addr, transfer_id), transfer);
}

fn input_accepted(
    sharing: bool,
    emulation_ready: bool,
    capture_ready: bool,
    independent: bool,
) -> bool {
    sharing && emulation_ready && (capture_ready || independent)
}

/// Silence after which a peer is considered gone. Must exceed the sender's
/// own ping window (4 pings, 500 ms apart) so both sides agree.
const PEER_SILENCE_TIMEOUT: Duration = Duration::from_secs(3);

impl ListenTask {
    /// Whether a peer may drive input here. Local capture is only needed to
    /// hand the shared pointer back; an independent pointer returns control
    /// through its own edges, so a capture backend that is still starting or
    /// re-initialising must not bounce the peer out mid-movement.
    /// Stops treating `addr` as controlling this screen, and tells the
    /// service when no peer controls it any more.
    fn end_control(&mut self, addr: SocketAddr) {
        if self.active_inputs.remove(&addr) && self.active_inputs.is_empty() {
            let _ = self.event_tx.send(EmulationEvent::NoLongerControlled);
        }
    }

    /// A peer asks to move its pointer onto this screen. `through` is the
    /// side of the device it arrived through after a handoff; otherwise our
    /// own map's side for the peer wins over the side it assumed.
    async fn handle_enter(&mut self, addr: SocketAddr, pos: Position, through: Option<Position>) {
        if !self.accepts_input() {
            log::warn!("rejecting entry from {addr}: remote input is unavailable");
            self.emulation_proxy.remove(addr);
            self.listener.reply(addr, ProtoEvent::Leave(0)).await;
            return;
        }
        let Some(fingerprint) = self.listener.get_certificate_fingerprint(addr).await else {
            self.listener.reply(addr, ProtoEvent::Leave(0)).await;
            return;
        };
        log::info!("accepting entry from {addr}");
        self.active_inputs.insert(addr);
        let side = through
            .or_else(|| self.peer_sides.get(&addr.ip()).copied())
            .unwrap_or(pos);
        self.entered_from.insert(addr, side);
        // Phones draw the peer's pointer and put it on the entry side.
        // Desktops keep their pointer where it is unless multi-hop is on.
        if cfg!(target_os = "android") || self.multi_hop {
            self.emulation_proxy.place_pointer(side);
        }
        self.event_tx
            .send(EmulationEvent::ReleaseNotify)
            .expect("channel closed");
        self.listener.reply(addr, ProtoEvent::Ack(0)).await;
        self.event_tx
            .send(EmulationEvent::Entered {
                addr,
                pos: to_ipc_pos(side),
                fingerprint,
            })
            .expect("channel closed");
    }

    /// Multi-hop: the pointer of `addr` was pushed past `edge` of this
    /// screen towards another configured device. Ask the machine that owns
    /// the pointer to take it back and enter that device directly; it checks
    /// that it trusts the target itself.
    async fn start_handoff(
        &mut self,
        addr: SocketAddr,
        edge: syntra_input_emulation::PointerEdge,
    ) -> bool {
        use syntra_input_emulation::PointerEdge;
        let side = match edge {
            PointerEdge::Left => Position::Left,
            PointerEdge::Right => Position::Right,
            PointerEdge::Top => Position::Top,
            PointerEdge::Bottom => Position::Bottom,
        };
        if !self.multi_hop || !self.active_inputs.contains(&addr) {
            return false;
        }
        let Some(target) = self.peer_beyond.get(&side).cloned() else {
            return false;
        };
        if self.peer_fingerprints.get(&addr) == Some(&target) {
            // That side is the pointer's own machine: a normal return.
            return false;
        }
        log::info!("handing the pointer of {addr} on through the {side} edge");
        self.end_control(addr);
        self.entered_from.remove(&addr);
        self.emulation_proxy.remove(addr);
        // The target sees the pointer arrive from the side this device
        // occupies in its own layout; the owner resolves that.
        self.listener
            .reply(addr, ProtoEvent::Handoff { target, side })
            .await;
        true
    }

    /// Called when the owner answers our handoff with its own `Handoff`:
    /// it does not trust the next device. With the bypass enabled a phone
    /// carries the pointer on through its own touchpad capture; otherwise
    /// the pointer comes back here.
    fn handoff_refused(&mut self, addr: SocketAddr, target: &str, side: Position) {
        let _ = target;
        #[cfg(target_os = "android")]
        if self.hop_bypass && self.relay.is_none() {
            log::info!(
                "carrying the pointer of {addr} on through the {side} edge (pairing bypass)"
            );
            self.active_inputs.insert(addr);
            self.relay = Some((addr, side));
            syntra_input_capture::touchpad::sender().begin(capture_pos(side));
            return;
        }
        log::info!("pointer of {addr} stays here: the next device is not paired with it");
        self.active_inputs.insert(addr);
        self.entered_from.insert(
            addr,
            self.peer_sides.get(&addr.ip()).copied().unwrap_or(side),
        );
        self.emulation_proxy.place_pointer(side);
    }

    /// Forwards input while carrying a pointer on; false means move ours.
    fn relay_input(&mut self, addr: SocketAddr, event: Event) -> bool {
        #[cfg(target_os = "android")]
        {
            let Some((source, side)) = self.relay else {
                return false;
            };
            if source != addr {
                return false;
            }
            if !syntra_input_capture::touchpad::is_active() {
                log::info!("pointer of {addr} came back through the {side} edge");
                self.relay = None;
                self.emulation_proxy.place_pointer(side);
                return false;
            }
            syntra_input_capture::touchpad::sender().input(capture_pos(side), event);
            true
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = (addr, event);
            false
        }
    }

    fn accepts_input(&self) -> bool {
        input_accepted(
            self.input_sharing,
            self.emulation_proxy.emulation_ready.get(),
            self.capture_ready,
            self.independent_pointers,
        )
    }

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
                            ProtoEvent::EnterVia { side, via } => {
                                // Arrived through `via` after a handoff: our
                                // own map says on which side that device sits.
                                let through = self
                                    .peer_beyond
                                    .iter()
                                    .find(|(_, fingerprint)| **fingerprint == via)
                                    .map(|(side, _)| *side);
                                self.handle_enter(addr, side, through).await;
                            }
                            ProtoEvent::Enter(pos) => {
                                self.handle_enter(addr, pos, None).await;
                            }
                            ProtoEvent::Leave(_) => {
                                self.end_control(addr);
                                self.emulation_proxy.remove(addr);
                                self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                            }
                            ProtoEvent::Input(event) => {
                                if self.input_sharing && self.active_inputs.contains(&addr) && !self.relay_input(addr, event) {
                                    self.emulation_proxy.consume(event, addr);
                                }
                            }
                            // The owner could not enter the next device (not
                            // paired): carry the pointer on ourselves if the
                            // user allowed it, otherwise it stays here.
                            ProtoEvent::Handoff { target, side } => {
                                self.handoff_refused(addr, &target, side);
                            }
                            ProtoEvent::Ping => {
                                self.listener
                                    .reply(
                                        addr,
                                        ProtoEvent::Pong(self.accepts_input()),
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
                                start_clipboard_transfer(
                                    &mut clipboard_transfers,
                                    addr,
                                    transfer_id,
                                    ClipboardTransfer::new(ClipboardTransferKind::Text, total_len as usize, chunks),
                                );
                            }
                            ProtoEvent::ClipboardImageStart { transfer_id, width, height, total_len, chunks } => {
                                start_clipboard_transfer(
                                    &mut clipboard_transfers,
                                    addr,
                                    transfer_id,
                                    ClipboardTransfer::new(
                                        ClipboardTransferKind::Image { width, height },
                                        total_len as usize,
                                        chunks,
                                    ),
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
                                            self.event_tx.send(EmulationEvent::Clipboard { content }).expect("channel closed");
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
                        self.peer_fingerprints.insert(addr, fingerprint.clone());
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
                    if let EmulationEvent::PointerEdge { addr, edge } = event {
                        if self.start_handoff(addr, edge).await {
                            continue;
                        }
                        // An independent cursor is invisible to local
                        // capture, so its edges must hand control back here.
                        if self.active_inputs.contains(&addr)
                            && self.entered_from.get(&addr).is_some_and(|pos| edge_matches(*pos, edge))
                        {
                            log::info!("independent pointer of {addr} left through the {edge:?} edge");
                            self.end_control(addr);
                            self.emulation_proxy.remove(addr);
                            self.listener.reply(addr, ProtoEvent::Leave(0)).await;
                        }
                        continue;
                    }
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
                    EmulationRequest::SetIndependentPointers(enabled) => {
                        self.independent_pointers = enabled;
                        self.emulation_proxy.set_independent_pointers(enabled);
                    }
                    EmulationRequest::SetHopBypass(enabled) => {
                        self.hop_bypass = enabled;
                    }
                    EmulationRequest::SetMultiHop(enabled) => {
                        self.multi_hop = enabled;
                    }
                    EmulationRequest::StepInside(edge) => {
                        self.emulation_proxy.step_inside(match edge {
                            syntra_input_capture::Position::Left => syntra_input_emulation::PointerEdge::Left,
                            syntra_input_capture::Position::Right => syntra_input_emulation::PointerEdge::Right,
                            syntra_input_capture::Position::Top => syntra_input_emulation::PointerEdge::Top,
                            syntra_input_capture::Position::Bottom => syntra_input_emulation::PointerEdge::Bottom,
                        });
                    }
                    EmulationRequest::SetPeerSides(sides, beyond) => {
                        self.peer_sides = sides;
                        self.peer_beyond = beyond;
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
                    EmulationRequest::PublishFileClipboard { contents, fallback } => {
                        if let Err(error) = self.emulation_proxy.publish_file_clipboard(contents).await {
                            log::warn!("native clipboard publication failed: {error}");
                            let _ = self.event_tx.send(EmulationEvent::ClipboardFallback(fallback));
                        }
                    }
                    EmulationRequest::Terminate => break,
                },
                _ = interval.tick() => {
                    // Peers ping every 500 ms and give up after 2 s without
                    // an answer; expiring sooner dropped peers over a couple
                    // of lost datagrams while they kept sending input.
                    let mut expired = Vec::new();
                    last_response.retain(|&addr, instant: &mut Instant| {
                        let alive = instant.elapsed() <= PEER_SILENCE_TIMEOUT;
                        if !alive {
                            expired.push(addr);
                        }
                        alive
                    });
                    for addr in expired {
                        log::warn!("releasing keys: {addr} not responding!");
                        let was_active = self.active_inputs.contains(&addr);
                        self.end_control(addr);
                        self.entered_from.remove(&addr);
                        self.peer_fingerprints.remove(&addr);
                        if self.relay.is_some_and(|(source, _)| source == addr) {
                            self.relay = None;
                        }
                        self.emulation_proxy.remove(addr);
                        self.event_tx.send(EmulationEvent::Disconnected { addr }).expect("channel closed");
                        // Its input is ignored from now on; if it is still
                        // alive it must release capture instead of driving a
                        // pointer that no longer moves.
                        if was_active {
                            self.listener.reply(addr, ProtoEvent::Leave(0)).await;
                        }
                    }
                    clipboard_transfers
                        .retain(|_, transfer| transfer.started.elapsed() < CLIPBOARD_TRANSFER_TIMEOUT);
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
    independent_pointers: Rc<Cell<bool>>,
    exit_requested: Rc<Cell<bool>>,
    request_tx: Sender<ProxyRequest>,
    event_rx: Receiver<EmulationEvent>,
    task: JoinHandle<()>,
}

enum ProxyRequest {
    Input(Event, SocketAddr),
    /// Put the pointer on the given side of this screen (entry point).
    PlacePointer(Position),
    /// Move the pointer a little inside from this edge.
    StepInside(syntra_input_emulation::PointerEdge),
    Remove(SocketAddr),
    Terminate,
    Reenable,
    SetInputSharing(bool),
    /// Applied to the live backend; `independent_pointers` also keeps the
    /// value for backends created later.
    SetIndependentPointers(bool),
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
        let independent_pointers = Rc::new(Cell::new(false));
        let exit_requested = Rc::new(Cell::new(false));
        let emulation_task = EmulationTask {
            backend,
            emulation_ready: emulation_ready.clone(),
            input_sharing: input_sharing.clone(),
            independent_pointers: independent_pointers.clone(),
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
            independent_pointers,
            exit_requested,
            request_tx,
            task,
            event_rx,
        }
    }

    async fn event(&mut self) -> EmulationEvent {
        let event = self.event_rx.recv().await.expect("channel closed");
        if let EmulationEvent::EmulationEnabled(_) = event {
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
    fn step_inside(&self, edge: syntra_input_emulation::PointerEdge) {
        let _ = self.request_tx.send(ProxyRequest::StepInside(edge));
    }

    fn place_pointer(&self, side: Position) {
        let _ = self.request_tx.send(ProxyRequest::PlacePointer(side));
    }

    fn set_independent_pointers(&self, enabled: bool) {
        self.independent_pointers.set(enabled);
        self.request_tx
            .send(ProxyRequest::SetIndependentPointers(enabled))
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
    independent_pointers: Rc<Cell<bool>>,
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
                    // Kept in the shared cell; applied on the next backend.
                    ProxyRequest::SetIndependentPointers(_)
                    | ProxyRequest::PlacePointer(_)
                    | ProxyRequest::StepInside(_) => {}
                    ProxyRequest::PublishFileClipboard { reply, .. } => {
                        let _ = reply.send(Err("emulation inactive".to_string()));
                    }
                }
            }
        }
    }

    async fn initialize_emulation<F>(
        &mut self,
        mut create: impl FnMut() -> F,
    ) -> Result<Option<InputEmulation>, EmulationCreationError>
    where
        F: Future<Output = Result<InputEmulation, EmulationCreationError>>,
    {
        let initialization = create();
        let deadline = tokio::time::sleep(crate::INPUT_INITIALIZATION_TIMEOUT);
        tokio::pin!(initialization, deadline);
        loop {
            tokio::select! {
                result = &mut initialization => return result.map(Some),
                _ = &mut deadline => {
                    log::warn!("input emulation initialization timed out; retry when the desktop is ready");
                    return Ok(None);
                }
                request = self.request_rx.recv() => match request.expect("channel closed") {
                    ProxyRequest::Reenable => {
                        log::info!("restarting pending input emulation initialization");
                        initialization.set(create());
                        deadline.as_mut().reset(tokio::time::Instant::now() + crate::INPUT_INITIALIZATION_TIMEOUT);
                    }
                    ProxyRequest::Terminate => {
                        self.exit_requested.set(true);
                        return Ok(None);
                    }
                    ProxyRequest::SetInputSharing(enabled) => {
                        self.input_sharing.set(enabled);
                        if !enabled {
                            self.handles.clear();
                            self.pressed_buttons.clear();
                        }
                    }
                    ProxyRequest::Remove(addr) => {
                        self.handles.remove(&addr);
                        self.pressed_buttons.remove(&addr);
                    }
                    ProxyRequest::Input(..)
                    | ProxyRequest::SetIndependentPointers(_)
                    | ProxyRequest::PlacePointer(_)
                    | ProxyRequest::StepInside(_) => {}
                    ProxyRequest::PublishFileClipboard { reply, .. } => {
                        let _ = reply.send(Err("emulation not ready".to_string()));
                    }
                },
            }
        }
    }

    async fn do_emulation(&mut self) -> Result<(), InputEmulationError> {
        log::info!("creating input emulation ...");
        let backend = self.backend;
        let Some(mut emulation) = self
            .initialize_emulation(|| InputEmulation::new(backend))
            .await?
        else {
            return Ok(());
        };

        emulation.set_independent_pointers(self.independent_pointers.get());

        // The portal being approved is not enough: only advertise readiness
        // after the backend has completed initialization and can consume input.
        self.emulation_ready.set(true);
        let _emulation_guard = ReadyGuard::new(
            self.emulation_ready.clone(),
            self.event_tx.clone(),
            emulation.backend().to_string(),
        );

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

        // Cleared once the backend's clipboard source is exhausted, so an
        // absent clipboard is polled once rather than on every wake-up.
        let mut clipboard_active = true;

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
                            if let Some(edge) = emulation.take_pointer_edge(handle) {
                                let _ = self.event_tx.send(EmulationEvent::PointerEdge { addr, edge });
                            }
                        }
                    },
                    ProxyRequest::Remove(addr) => {
                        self.release_client(emulation, addr).await?;
                    }
                    ProxyRequest::SetIndependentPointers(enabled) => {
                        emulation.set_independent_pointers(enabled);
                    }
                    ProxyRequest::StepInside(edge) => emulation.step_inside(edge),
                    ProxyRequest::PlacePointer(side) => {
                        emulation.place_pointer(match side {
                            Position::Left => syntra_input_emulation::PointerEdge::Left,
                            Position::Right => syntra_input_emulation::PointerEdge::Right,
                            Position::Top => syntra_input_emulation::PointerEdge::Top,
                            Position::Bottom => syntra_input_emulation::PointerEdge::Bottom,
                        });
                    }
                    ProxyRequest::SetInputSharing(enabled) => {
                        if !enabled {
                            let addrs = self.handles.keys().copied().collect::<Vec<_>>();
                            let mut release_result = Ok(());
                            for addr in addrs {
                                let result = self.release_client(emulation, addr).await;
                                if release_result.is_ok() {
                                    release_result = result;
                                }
                            }
                            release_result?;
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
                // Guarded: a backend without clipboard support resolves this
                // immediately with `None`, and `select!` would poll it again
                // at once, spinning the daemon at 100% CPU for the lifetime
                // of the session. An exhausted source is polled once.
                clipboard = emulation.clipboard_event(), if clipboard_active => {
                    let Some((mime_type, data)) = clipboard else {
                        log::debug!("clipboard source ended; no longer polling it");
                        clipboard_active = false;
                        continue;
                    };
                    {
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
            ProxyRequest::SetInputSharing(_)
            | ProxyRequest::SetIndependentPointers(_)
            | ProxyRequest::PlacePointer(_)
            | ProxyRequest::StepInside(_) => continue,
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

struct ReadyGuard {
    ready: Rc<Cell<bool>>,
    event_tx: Sender<EmulationEvent>,
}

impl ReadyGuard {
    fn new(ready: Rc<Cell<bool>>, event_tx: Sender<EmulationEvent>, backend: String) -> Self {
        event_tx
            .send(EmulationEvent::EmulationEnabled(backend))
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

#[cfg(test)]
mod tests {
    use super::{
        ClipboardTransfer, ClipboardTransferKind, HashMap, SocketAddr, decode_native_image,
        start_clipboard_transfer,
    };

    /// Capture flapping on this machine must not eject a peer that drives
    /// its own pointer, but still gates the shared pointer, whose way back
    /// depends on it.
    #[test]
    fn capture_readiness_only_gates_the_shared_pointer() {
        use super::input_accepted;
        assert!(input_accepted(true, true, false, true));
        assert!(!input_accepted(true, true, false, false));
        assert!(input_accepted(true, true, true, false));
        assert!(
            !input_accepted(true, false, true, true),
            "emulation must be ready"
        );
        assert!(
            !input_accepted(false, true, true, true),
            "sharing must be on"
        );
    }

    /// A peer that starts transfer after transfer without finishing any must
    /// hold at most one reassembly buffer, and none reserved at the declared
    /// maximum size.
    #[test]
    fn clipboard_starts_are_bounded_to_one_buffer_per_peer() {
        let mut transfers = HashMap::new();
        let peer: SocketAddr = "10.0.0.2:4242".parse().unwrap();
        let other: SocketAddr = "10.0.0.3:4242".parse().unwrap();
        for id in 0..1_000 {
            start_clipboard_transfer(
                &mut transfers,
                peer,
                id,
                ClipboardTransfer::new(ClipboardTransferKind::Text, 64 * 1024 * 1024, 1),
            );
        }
        start_clipboard_transfer(
            &mut transfers,
            other,
            7,
            ClipboardTransfer::new(ClipboardTransferKind::Text, 10, 1),
        );
        assert_eq!(transfers.len(), 2);
        assert!(transfers.contains_key(&(peer, 999)));
        assert!(transfers.contains_key(&(other, 7)));
        assert!(
            transfers
                .values()
                .all(|t| t.bytes.capacity() <= 1024 * 1024)
        );
    }
    use image::{ImageBuffer, ImageEncoder, Rgba};

    fn emulation_task() -> (
        super::EmulationTask,
        super::Sender<super::ProxyRequest>,
        super::Receiver<super::EmulationEvent>,
    ) {
        let (requests, request_rx) = super::channel();
        let (event_tx, events) = super::channel();
        let task = super::EmulationTask {
            backend: None,
            emulation_ready: Default::default(),
            input_sharing: super::Rc::new(super::Cell::new(true)),
            independent_pointers: Default::default(),
            exit_requested: Default::default(),
            request_rx,
            event_tx,
            handles: Default::default(),
            pressed_buttons: Default::default(),
            next_id: 0,
        };
        (task, requests, events)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retry_replaces_stalled_emulation_and_drops_old_attempt() {
        let (mut task, requests, _events) = emulation_task();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
        let mut first = Some((started_tx, dropped_tx));
        let initialization = task.initialize_emulation(|| {
            let first = first.take();
            async move {
                if let Some((started, held)) = first {
                    let _held = held;
                    started.send(()).unwrap();
                    std::future::pending().await
                } else {
                    super::InputEmulation::new(Some(syntra_input_emulation::Backend::Dummy)).await
                }
            }
        });
        let retry = async {
            started_rx.await.unwrap();
            requests.send(super::ProxyRequest::Reenable).unwrap();
        };
        let (result, ()) = tokio::time::timeout(super::Duration::from_secs(2), async {
            tokio::join!(initialization, retry)
        })
        .await
        .expect("Retry left emulation waiting on the old initialization");
        let mut emulation = result.unwrap().expect("replacement emulation unavailable");
        assert!(dropped_rx.await.is_err(), "old initialization was retained");
        emulation.terminate().await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn stalled_emulation_times_out_and_can_initialize_again() {
        let (mut task, _requests, _events) = emulation_task();
        let start = tokio::time::Instant::now();
        let result = task
            .initialize_emulation(std::future::pending)
            .await
            .unwrap();
        assert!(result.is_none());
        assert_eq!(start.elapsed(), crate::INPUT_INITIALIZATION_TIMEOUT);
        let mut emulation = task
            .initialize_emulation(|| {
                super::InputEmulation::new(Some(syntra_input_emulation::Backend::Dummy))
            })
            .await
            .unwrap()
            .expect("emulation stayed stuck after its deadline");
        emulation.terminate().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stalled_emulation_keeps_stop_and_clipboard_responsive() {
        let (mut task, requests, _events) = emulation_task();
        let addr = "127.0.0.1:1".parse().unwrap();
        task.handles.insert(addr, 0);
        task.pressed_buttons.insert(addr, [1].into_iter().collect());
        let (reply, response) = tokio::sync::oneshot::channel();
        requests
            .send(super::ProxyRequest::SetInputSharing(false))
            .unwrap();
        requests
            .send(super::ProxyRequest::PublishFileClipboard {
                contents: Vec::new(),
                reply,
            })
            .unwrap();
        let initialization = task.initialize_emulation(std::future::pending);
        let stop = async {
            assert!(response.await.unwrap().is_err());
            requests.send(super::ProxyRequest::Terminate).unwrap();
        };
        let (result, ()) = tokio::time::timeout(super::Duration::from_secs(2), async {
            tokio::join!(initialization, stop)
        })
        .await
        .expect("pending initialization blocked shutdown or clipboard fallback");
        assert!(result.unwrap().is_none());
        assert!(task.exit_requested.get());
        assert!(!task.input_sharing.get());
        assert!(task.handles.is_empty());
        assert!(task.pressed_buttons.is_empty());
        assert!(!task.emulation_ready.get());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unavailable_native_clipboard_preserves_file_offer_for_adapter() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let listener = super::SyntraListener::new(
                    0,
                    webrtc_dtls::crypto::Certificate::generate_self_signed(["test".into()])
                        .unwrap(),
                    Default::default(),
                )
                .await
                .unwrap();
                let mut emulation =
                    super::Emulation::new(Some(syntra_input_emulation::Backend::Dummy), listener);
                let publication = syntra_plugin_api::PublishFileClipboard {
                    transfer_id: "clipboard-fallback".into(),
                    operation: syntra_plugin_api::Operation::Move,
                    uris: vec!["file:///tmp/clipboard-test/empty%20file".into()],
                };
                emulation.publish_file_clipboard(
                    vec![(
                        "text/uri-list".into(),
                        publication.uris[0].as_bytes().to_vec(),
                    )],
                    super::ClipboardFallback::Adapter(publication.clone()),
                );
                let fallback = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    loop {
                        if let super::EmulationEvent::ClipboardFallback(
                            super::ClipboardFallback::Adapter(value),
                        ) = emulation.event().await
                        {
                            break value;
                        }
                    }
                })
                .await;
                emulation.terminate().await;
                let fallback = fallback.expect("file offer disappeared after native rejection");
                assert_eq!(fallback.transfer_id, publication.transfer_id);
                assert_eq!(fallback.uris, publication.uris);
                assert!(matches!(
                    fallback.operation,
                    syntra_plugin_api::Operation::Move
                ));
            })
            .await;
    }

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
