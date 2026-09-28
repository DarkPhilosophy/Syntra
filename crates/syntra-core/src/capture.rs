use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    future::Future,
    rc::Rc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use local_channel::mpsc::{Receiver, Sender, channel};
use syntra_input_capture::{
    CaptureCreationError, CaptureError, CaptureEvent, CaptureHandle, InputCapture,
    InputCaptureError, Position,
};
use syntra_input_event::{Event, KeyboardEvent, PointerEvent, scancode};
use syntra_proto::{MAX_CLIPBOARD_CHUNK_SIZE, ProtoEvent};
use tokio::task::{JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

use crate::connect::SyntraConnection;

/// How often a lost peer transport is retried while idle.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) struct Capture {
    cancellation_token: CancellationToken,
    request_tx: Sender<CaptureRequest>,
    task: JoinHandle<()>,
    event_rx: Receiver<ICaptureEvent>,
}

pub(crate) enum ICaptureEvent {
    /// Move the local pointer a little inside from this edge.
    StepInside(Position),
    /// Put the local pointer just inside this edge (it came home through it).
    PlacePointer(Position),
    /// The pointer of the device controlling this one reached the edge of
    /// another device: ask that controller to enter it directly.
    HandOn {
        controller: String,
        target: String,
        side: syntra_proto::Position,
    },
    Clipboard {
        handle: CaptureHandle,
        fingerprint: String,
        event: ProtoEvent,
    },
    /// Certificate identity bound to the outgoing DTLS transport.
    PeerAuthenticated {
        handle: CaptureHandle,
        fingerprint: String,
    },
    FileClipboard {
        handle: CaptureHandle,
        mime_type: String,
        value: String,
    },
    /// The transport to a peer died: transfers bound to it are dead too.
    PeerLost(CaptureHandle),
    /// A Pong or transport loss changed the outgoing client's live state.
    PeerStateChanged(CaptureHandle),
    /// a client was entered
    CaptureBegin(CaptureHandle),
    /// capture disabled
    CaptureDisabled,
    /// Capture became available, carrying the backend that was selected.
    CaptureEnabled(String),
    /// A (new) client was entered.
    /// In contrast to [`ICaptureEvent::CaptureBegin`] this
    /// event is only triggered when the capture was
    /// explicitly released in the meantime by
    /// either the remote client leaving its device region,
    /// a new device entering the screen or the release bind.
    ClientEntered(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureType {
    /// a normal input capture
    Default,
    /// A capture only interested in [`CaptureEvent::Begin`] events.
    /// The capture is released immediately, if there is no
    /// Default capture at the same position.
    EnterOnly,
}

#[derive(Debug)]
enum CaptureRequest {
    /// capture must release the mouse
    Release,
    /// The device controlling this one: its certificate and the edge of this
    /// screen its pointer entered through. `None` when nobody controls it.
    SetController(Option<(String, Position)>),
    /// Turn multi-hop on or off.
    SetMultiHop(bool),
    /// add a capture client
    Create(CaptureHandle, Position, CaptureType),
    /// destory a capture client
    Destroy(CaptureHandle),
    /// reenable input capture
    Reenable,
    /// Switch to another capture backend (`None`: pick automatically).
    SetBackend(Option<syntra_input_capture::Backend>),
    /// globally enable or disable outgoing capture
    SetInputSharing(bool),
    /// set release bind
    SetReleaseBind(Vec<scancode::Linux>),
    /// send a protocol event without tying it to input readiness
    SendProto {
        handle: CaptureHandle,
        event: ProtoEvent,
    },
    SendClipboard {
        handle: CaptureHandle,
        transfer_id: u64,
        data: Vec<u8>,
        image_dimensions: Option<(u32, u32)>,
    },
}

impl Capture {
    pub(crate) fn new(
        backend: Option<syntra_input_capture::Backend>,
        conn: SyntraConnection,
        release_bind: Vec<scancode::Linux>,
        last_entry: EntryMark,
    ) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let cancellation_token = CancellationToken::new();
        let capture_task = CaptureTask {
            active_client: None,
            ack_deadline: None,
            held_until_ack: Vec::new(),
            entering_via: None,
            controller: None,
            last_entry,
            multi_hop: Default::default(),
            backend,
            restart_now: false,
            cancellation_token: cancellation_token.clone(),
            enabled_captures: Default::default(),
            captures: Default::default(),
            conn,
            event_tx,
            request_rx,
            release_bind: Rc::new(RefCell::new(release_bind)),
            input_sharing: true,
            state: Default::default(),
        };
        let task = spawn_local(capture_task.run());
        Self {
            cancellation_token,
            request_tx,
            task,
            event_rx,
        }
    }

    pub(crate) fn set_backend(&self, backend: Option<syntra_input_capture::Backend>) {
        let _ = self.request_tx.send(CaptureRequest::SetBackend(backend));
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(CaptureRequest::Reenable)
            .expect("channel closed");
    }

    pub(crate) fn set_multi_hop(&self, enabled: bool) {
        let _ = self.request_tx.send(CaptureRequest::SetMultiHop(enabled));
    }

    /// Tells capture which device's pointer is on this screen, if any.
    pub(crate) fn set_controller(&self, controller: Option<(String, Position)>) {
        let _ = self
            .request_tx
            .send(CaptureRequest::SetController(controller));
    }

    pub(crate) fn set_input_sharing(&self, enabled: bool) {
        self.request_tx
            .send(CaptureRequest::SetInputSharing(enabled))
            .expect("channel closed");
    }

    pub(crate) async fn terminate(&mut self) {
        self.cancellation_token.cancel();
        log::debug!("terminating capture");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }

    pub(crate) fn create(
        &self,
        handle: CaptureHandle,
        pos: syntra_api::Position,
        capture_type: CaptureType,
    ) {
        let pos = capture_pos(pos);
        self.request_tx
            .send(CaptureRequest::Create(handle, pos, capture_type))
            .expect("channel closed");
    }

    pub(crate) fn destroy(&self, handle: CaptureHandle) {
        self.request_tx
            .send(CaptureRequest::Destroy(handle))
            .expect("channel closed");
    }

    pub(crate) fn release(&self) {
        self.request_tx
            .send(CaptureRequest::Release)
            .expect("channel closed");
    }

    pub(crate) async fn event(&mut self) -> ICaptureEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    pub(crate) fn set_release_bind(&mut self, bind: Vec<scancode::Linux>) {
        let _ = self.request_tx.send(CaptureRequest::SetReleaseBind(bind));
    }

    pub(crate) fn send_clipboard(
        &self,
        handle: CaptureHandle,
        transfer_id: u64,
        data: Vec<u8>,
        image_dimensions: Option<(u32, u32)>,
    ) {
        self.request_tx
            .send(CaptureRequest::SendClipboard {
                handle,
                transfer_id,
                data,
                image_dimensions,
            })
            .expect("channel closed");
    }
    pub(crate) fn send_proto(&self, handle: CaptureHandle, event: ProtoEvent) {
        self.request_tx
            .send(CaptureRequest::SendProto { handle, event })
            .expect("channel closed");
    }
}

/// debounce a statement `$st`, i.e. the statement is executed only if the
/// time since the previous execution is at least `$dur`.
/// `$prev` is used to keep track of this timestamp
/// Releases capture, leaving the pointer inside `landing` when given.
async fn release_with(
    capture: &mut InputCapture,
    landing: Option<Position>,
) -> Result<bool, CaptureError> {
    match landing {
        Some(edge) => capture.release_at(edge).await,
        None => capture.release().await.map(|()| false),
    }
}

/// How long after an entry its own edge is not treated as leaving again.
const ENTRY_GRACE: Duration = Duration::from_millis(400);

/// Shared between the emulation and capture tasks (same local thread): the
/// edge a peer's pointer last entered through and when. Set synchronously
/// before the pointer is placed, so capture never races the service.
pub(crate) type EntryMark = Rc<std::cell::Cell<Option<(Position, Instant)>>>;

macro_rules! debounce {
    ($prev:ident, $dur:expr, $st:stmt) => {
        let exec = match $prev.get() {
            None => true,
            Some(instant) if instant.elapsed() > $dur => true,
            _ => false,
        };
        if exec {
            $prev.replace(Some(Instant::now()));
            $st
        }
    };
}

struct CaptureTask {
    active_client: Option<CaptureHandle>,
    ack_deadline: Option<Instant>,
    /// Set while another device controls this one (its pointer is here).
    /// Only then does an edge shared with that device mean "hand off"
    /// instead of "enter the device on this side".
    controller: Option<(String, Position)>,
    /// Edge and time a peer's pointer last entered this screen, set by the
    /// emulation task before it places the pointer against that edge.
    last_entry: EntryMark,
    /// Multi-hop enabled (off by default until proven on every route).
    multi_hop: Rc<std::cell::Cell<bool>>,
    /// After a handoff: certificate of the device the pointer came through,
    /// named in every enter until the new peer acknowledges.
    entering_via: Option<String>,
    /// Clicks and keys pressed before the peer acknowledged entry. They are
    /// replayed once it does; dropping them lost every first click.
    held_until_ack: Vec<Event>,
    backend: Option<syntra_input_capture::Backend>,
    /// The backend was changed: start it again at once.
    restart_now: bool,
    cancellation_token: CancellationToken,
    enabled_captures: HashSet<CaptureHandle>,
    captures: Vec<(CaptureHandle, Position, CaptureType)>,
    conn: SyntraConnection,
    event_tx: Sender<ICaptureEvent>,
    release_bind: Rc<RefCell<Vec<scancode::Linux>>>,
    request_rx: Receiver<CaptureRequest>,
    input_sharing: bool,
    state: State,
}

impl CaptureTask {
    fn add_capture(&mut self, handle: CaptureHandle, pos: Position, capture_type: CaptureType) {
        self.captures.push((handle, pos, capture_type));
    }

    fn remove_capture(&mut self, handle: CaptureHandle) {
        self.captures.retain(|&(h, ..)| handle != h);
    }

    /// Whether a `Begin` on `handle` must be left to the controlling peer's
    /// multi-hop hand-off instead of entering the device here. Only while
    /// multi-hop is on, another device controls this screen, and the edge
    /// faces a device known to be another than that controller. Towards the
    /// controller itself, or a device not identified yet, this is a normal
    /// entry: an unknown certificate must never block direct switching.
    /// Whether input comes from the phone's own touchpad screen.
    fn is_touchpad(&self) -> bool {
        #[cfg(target_os = "android")]
        {
            self.backend == Some(syntra_input_capture::Backend::Touchpad)
        }
        #[cfg(not(target_os = "android"))]
        {
            false
        }
    }

    /// Device that captured input goes to. After a hand-off the capture still
    /// reports the barrier it began on (the device in the middle), but the
    /// pointer belongs to the device it was handed on to. Input without an
    /// entered device goes nowhere.
    fn input_target(&self, handle: CaptureHandle, begin: bool) -> Option<CaptureHandle> {
        match self.active_client {
            Some(active) => Some(active),
            None if begin => Some(handle),
            None => None,
        }
    }

    /// Whether a peer's pointer entered through `pos` a moment ago: touching
    /// that edge then comes from placing the pointer there, not from the user
    /// going back.
    fn just_entered_through(&self, pos: Position) -> bool {
        matches!(self.last_entry.get(), Some((edge, at)) if edge == pos && at.elapsed() < ENTRY_GRACE)
    }

    /// Whether `handle` is the device whose pointer is on this screen, and
    /// this screen is not controlling anything itself.
    fn returns_to_controller(&self, handle: CaptureHandle) -> bool {
        let Some((controller, _)) = self.controller.as_ref() else {
            return false;
        };
        self.active_client.is_none()
            && self.conn.client_fingerprint(handle).as_deref() == Some(controller.as_str())
    }

    fn defer_to_handoff(&self, handle: CaptureHandle) -> bool {
        let Some((controller, entered)) = self.controller.as_ref() else {
            return false;
        };
        // Any edge towards a device other than the controller: the owner
        // enters it itself, keeping its identity along the chain.
        let _ = entered;
        self.multi_hop.get()
            && self.active_client.is_none()
            && matches!(self.conn.client_fingerprint(handle), Some(fingerprint) if fingerprint != *controller)
    }

    fn is_default_capture_at(&self, pos: Position) -> bool {
        self.captures
            .iter()
            .any(|&(_, p, t)| p == pos && t == CaptureType::Default)
    }

    fn get_pos(&self, handle: CaptureHandle) -> Position {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .expect("no such capture")
            .1
    }

    fn get_type(&self, handle: CaptureHandle) -> CaptureType {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .expect("no such capture")
            .2
    }

    /// Tell the service that every transfer bound to this peer is dead.
    fn notify_peer_lost(&self, handle: CaptureHandle) {
        log::warn!("peer {handle} lost: dropping transfers bound to it");
        self.event_tx
            .send(ICaptureEvent::PeerLost(handle))
            .expect("channel closed");
    }

    /// Re-establish transports for peers whose connection died.
    /// [`SyntraConnection::connect`] is a no-op while a peer is connected.
    async fn reconnect_lost_peers(&self) {
        for &(handle, _, kind) in &self.captures {
            if kind == CaptureType::Default && !self.conn.peer_connected(handle) {
                self.conn.connect(handle).await;
            }
        }
    }

    async fn handle_inactive_request(&mut self, request: CaptureRequest) -> bool {
        match request {
            CaptureRequest::Reenable => return true,
            CaptureRequest::SetBackend(backend) => {
                self.backend = backend;
                self.restart_now = true;
                return true;
            }
            CaptureRequest::Create(handle, position, kind) => {
                self.add_capture(handle, position, kind);
                if kind == CaptureType::Default {
                    self.conn.connect(handle).await;
                }
            }
            CaptureRequest::Destroy(handle) => self.remove_capture(handle),
            CaptureRequest::Release => {}
            CaptureRequest::SetController(controller) => self.controller = controller,
            CaptureRequest::SetMultiHop(enabled) => self.multi_hop.set(enabled),
            CaptureRequest::SetInputSharing(enabled) => self.input_sharing = enabled,
            CaptureRequest::SetReleaseBind(bind) => {
                self.release_bind.borrow_mut().clone_from(&bind);
            }
            CaptureRequest::SendProto { handle, event } => {
                if let Err(error) = self.conn.send(event, handle).await {
                    log::warn!("failed to send clipboard protocol event: {error}");
                }
            }
            CaptureRequest::SendClipboard {
                handle,
                transfer_id,
                data,
                image_dimensions,
            } => {
                self.send_clipboard_transfer(handle, transfer_id, data, image_dimensions)
                    .await;
            }
        }
        false
    }

    fn handle_inactive_event(&self, handle: CaptureHandle, fingerprint: String, event: ProtoEvent) {
        self.event_tx
            .send(ICaptureEvent::PeerAuthenticated {
                handle,
                fingerprint: fingerprint.clone(),
            })
            .expect("channel closed");
        if Self::is_forwarded_protocol(&event) {
            self.event_tx
                .send(ICaptureEvent::Clipboard {
                    handle,
                    fingerprint,
                    event,
                })
                .expect("channel closed");
        } else if matches!(&event, ProtoEvent::Pong(_)) {
            self.event_tx
                .send(ICaptureEvent::PeerStateChanged(handle))
                .expect("channel closed");
            if matches!(&event, ProtoEvent::Pong(false)) && !self.conn.peer_connected(handle) {
                self.notify_peer_lost(handle);
            }
        }
    }

    async fn run(mut self) {
        loop {
            if let Err(error) = self.do_capture().await {
                log::warn!("input capture exited: {error}");
            }
            if std::mem::take(&mut self.restart_now) {
                log::info!("switching input capture backend");
                continue;
            }
            let mut reconnect = reconnect_timer();
            loop {
                tokio::select! {
                    request = self.request_rx.recv() => {
                        if self.handle_inactive_request(request.expect("channel closed")).await {
                            break;
                        }
                    }
                    (handle, fingerprint, event) = self.conn.recv() => {
                        self.handle_inactive_event(handle, fingerprint, event)
                    },
                    _ = reconnect.tick() => self.reconnect_lost_peers().await,
                    _ = self.cancellation_token.cancelled() => return,
                }
            }
        }
    }

    async fn initialize_capture<F>(
        &mut self,
        mut create: impl FnMut() -> F,
    ) -> Result<Option<InputCapture>, CaptureCreationError>
    where
        F: Future<Output = Result<InputCapture, CaptureCreationError>>,
    {
        // A portal permission dialog must not block peer transport or clipboard traffic.
        let initialization = create();
        let deadline = tokio::time::sleep(crate::INPUT_INITIALIZATION_TIMEOUT);
        tokio::pin!(initialization, deadline);
        let mut reconnect = reconnect_timer();
        loop {
            tokio::select! {
                result = &mut initialization => return result.map(Some),
                request = self.request_rx.recv() => {
                    if self.handle_inactive_request(request.expect("channel closed")).await {
                        if self.restart_now {
                            return Ok(None);
                        }
                        log::info!("restarting pending input capture initialization");
                        initialization.set(create());
                        deadline.as_mut().reset(tokio::time::Instant::now() + crate::INPUT_INITIALIZATION_TIMEOUT);
                    }
                }
                (handle, fingerprint, event) = self.conn.recv() => {
                    self.handle_inactive_event(handle, fingerprint, event)
                },
                _ = &mut deadline => {
                    log::warn!("input capture initialization timed out; retry when the desktop is ready");
                    return Ok(None);
                }
                _ = reconnect.tick() => self.reconnect_lost_peers().await,
                _ = self.cancellation_token.cancelled() => return Ok(None),
            }
        }
    }

    async fn do_capture(&mut self) -> Result<(), InputCaptureError> {
        let backend = self.backend;
        let Some(mut capture) = self
            .initialize_capture(|| InputCapture::new(backend))
            .await?
        else {
            return Ok(());
        };

        let _capture_guard = DropGuard::new(
            self.event_tx.clone(),
            ICaptureEvent::CaptureEnabled(capture.backend().to_string()),
            ICaptureEvent::CaptureDisabled,
        );

        /* create barriers only for peers that confirmed remote input */
        self.enabled_captures.clear();
        let r = self.sync_captures(&mut capture).await;
        if let Err(e) = r {
            capture.terminate().await?;
            return Err(e.into());
        }

        let r = self.do_capture_session(&mut capture).await;

        // FIXME replace with async drop when stabilized
        capture.terminate().await?;

        r
    }

    async fn sync_captures(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        let captures = self.captures.clone();
        for (handle, pos, capture_type) in captures {
            // Keep configured barriers stable for the lifetime of the portal
            // session. GNOME requires a new approval whenever barriers are
            // removed and recreated. Readiness is enforced on Begin below:
            // an unavailable destination is released locally without sending.
            let should_exist = self.input_sharing;
            log::debug!(
                "peer gate handle={handle} type={capture_type:?} ready={} barrier={} active={}",
                self.conn.remote_ready(handle),
                self.enabled_captures.contains(&handle),
                self.active_client == Some(handle)
            );
            if should_exist && self.enabled_captures.insert(handle) {
                if let Err(error) = capture.create(handle, pos).await {
                    self.enabled_captures.remove(&handle);
                    return Err(error);
                }
                log::info!("peer gate opened handle={handle}; capture barrier created");
            } else if !should_exist && self.enabled_captures.contains(&handle) {
                // Release while the active barrier still exists. Destroying it
                // first can leave the compositor capture session without a
                // valid activation to release, trapping the pointer in limbo.
                if self.active_client == Some(handle) {
                    self.release_capture(capture).await?;
                }
                self.enabled_captures.remove(&handle);
                capture.destroy(handle).await?;
                log::info!(
                    "peer gate closed handle={handle}; capture released and barrier destroyed"
                );
            }
        }
        Ok(())
    }

    async fn do_capture_session(
        &mut self,
        capture: &mut InputCapture,
    ) -> Result<(), InputCaptureError> {
        let mut reconnect = reconnect_timer();
        loop {
            tokio::select! {
                event = capture.next() => match event {
                    Some(event) => self.handle_capture_event(capture, event?).await?,
                    None => return Ok(()),
                },
                (handle, fingerprint, event) = self.conn.recv() => {
                    self.event_tx
                        .send(ICaptureEvent::PeerAuthenticated {
                            handle,
                            fingerprint: fingerprint.clone(),
                        })
                        .expect("channel closed");
                    let is_clipboard = Self::is_forwarded_protocol(&event);
                    if is_clipboard {
                        self.event_tx
                            .send(ICaptureEvent::Clipboard { handle, fingerprint, event })
                            .expect("channel closed");
                        continue;
                    }
                    if matches!(&event, ProtoEvent::Pong(_)) {
                        self.event_tx
                            .send(ICaptureEvent::PeerStateChanged(handle))
                            .expect("channel closed");
                    }
                    if matches!(&event, ProtoEvent::Pong(false)) && !self.conn.peer_connected(handle) {
                        self.notify_peer_lost(handle);
                    }
                    if self.active_client != Some(handle) {
                        self.sync_captures(capture).await?;
                        continue;
                    }
                    match event {
                        ProtoEvent::Ack(_) if self.state == State::WaitingForAck => {
                            log::info!("client {handle} acknowledged entry");
                            self.entering_via = None;
                            self.state = State::Sending;
                            self.ack_deadline = None;
                            for event in std::mem::take(&mut self.held_until_ack) {
                                let _ = self.conn.send(ProtoEvent::Input(event), handle).await;
                            }
                        }
                        ProtoEvent::Pong(false) | ProtoEvent::Leave(_) => {
                            log::info!("releasing capture: remote input unavailable");
                            self.release_capture(capture).await?;
                        }
                        // The device in the middle decides to pass the pointer
                        // on; this owner only checks it trusts the target.
                        ProtoEvent::Handoff { target, side } if self.state == State::Sending => {
                            self.handoff(capture, handle, &target, side).await?;
                        }
                        // A hand-off this machine cannot follow (multi-hop off
                        // here, or entry not confirmed yet): the device in the
                        // middle has already let go, so bring the pointer home
                        // instead of leaving it on no screen at all.
                        ProtoEvent::Handoff { .. } => {
                            log::info!("hand-off from client {handle} refused; taking the pointer home");
                            let edge = self.get_pos(handle);
                            self.release_capture(capture).await?;
                            // The local pointer still rests on the edge that
                            // led to that device: step it back inside, or it
                            // re-enters at once and loops.
                            self.event_tx
                                .send(ICaptureEvent::StepInside(edge))
                                .expect("channel closed");
                        }
                        _ => {}
                    }
                    self.sync_captures(capture).await?;
                },
                _ = reconnect.tick() => self.reconnect_lost_peers().await,
                _ = async {
                    if let Some(deadline) = self.ack_deadline {
                        tokio::time::sleep_until(deadline.into()).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                }, if self.ack_deadline.is_some() => {
                    log::warn!("releasing capture: peer did not acknowledge entry");
                    self.release_capture(capture).await?;
                },
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    CaptureRequest::Reenable => { /* already active */ },
                    CaptureRequest::SetBackend(backend) => {
                        self.backend = backend;
                        self.restart_now = true;
                        self.release_capture(capture).await?;
                        break;
                    }
                    CaptureRequest::Release => {
                        let was_sending = self.active_client.is_some();
                        self.release_capture(capture).await?;
                        // A peer's pointer arrived while ours was captured:
                        // placing it could not move the cursor then. Place
                        // it again now that the cursor is free.
                        if let Some((edge, at)) = self.last_entry.get() {
                            if was_sending && at.elapsed() < ENTRY_GRACE {
                                self.last_entry.set(Some((edge, Instant::now())));
                                self.event_tx
                                    .send(ICaptureEvent::PlacePointer(edge))
                                    .expect("channel closed");
                            }
                        }
                    }
                    CaptureRequest::SetController(controller) => self.controller = controller,
                    CaptureRequest::SetMultiHop(enabled) => self.multi_hop.set(enabled),
                    CaptureRequest::SetInputSharing(enabled) => {
                        self.input_sharing = enabled;
                        if !enabled {
                            self.release_capture(capture).await?;
                        }
                        self.sync_captures(capture).await?;
                    }
                    CaptureRequest::Create(h, p, t) => {
                        self.add_capture(h, p, t);
                        if t == CaptureType::Default {
                            self.conn.connect(h).await;
                        }
                        self.sync_captures(capture).await?;
                    }
                    CaptureRequest::Destroy(h) => {
                        self.remove_capture(h);
                        if self.enabled_captures.contains(&h) {
                            if self.active_client == Some(h) {
                                self.release_capture(capture).await?;
                            }
                            self.enabled_captures.remove(&h);
                            capture.destroy(h).await?;
                        }
                    }
                    CaptureRequest::SetReleaseBind(bind) => {
                        self.release_bind.borrow_mut().clone_from(&bind);
                    }
                    CaptureRequest::SendProto { handle, event } => {
                        if let Err(e) = self.conn.send(event, handle).await {
                            log::warn!("failed to send clipboard protocol event: {e}");
                        }
                    }
                    CaptureRequest::SendClipboard { handle, transfer_id, data, image_dimensions } => {
                        self.send_clipboard_transfer(handle, transfer_id, data, image_dimensions).await;
                    }
                },
                _ = self.cancellation_token.cancelled() => {
                    self.release_capture(capture).await?;
                    break;
                },
            }
        }
        Ok(())
    }

    fn is_forwarded_protocol(event: &ProtoEvent) -> bool {
        matches!(
            event,
            ProtoEvent::ClipboardStart { .. }
                | ProtoEvent::ClipboardImageStart { .. }
                | ProtoEvent::ClipboardChunk { .. }
                | ProtoEvent::ClipboardCapabilities(_)
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
        )
    }
    async fn send_clipboard_transfer(
        &mut self,
        handle: CaptureHandle,
        transfer_id: u64,
        data: Vec<u8>,
        image_dimensions: Option<(u32, u32)>,
    ) {
        let Ok(total_len) = u32::try_from(data.len()) else {
            log::warn!("clipboard payload is too large");
            return;
        };
        let chunks = total_len.div_ceil(MAX_CLIPBOARD_CHUNK_SIZE as u32);
        let start = match image_dimensions {
            Some((width, height)) => ProtoEvent::ClipboardImageStart {
                transfer_id,
                width,
                height,
                total_len,
                chunks,
            },
            None => ProtoEvent::ClipboardStart {
                transfer_id,
                total_len,
                chunks,
            },
        };
        if let Err(e) = self.conn.send(start, handle).await {
            log::warn!("failed to send clipboard start to client {handle}: {e}");
            return;
        }
        for (index, chunk) in data.chunks(MAX_CLIPBOARD_CHUNK_SIZE).enumerate() {
            let event = ProtoEvent::ClipboardChunk {
                transfer_id,
                index: index as u32,
                data: chunk.to_vec(),
            };
            if let Err(e) = self.conn.send(event, handle).await {
                log::warn!("failed to send clipboard chunk to client {handle}: {e}");
                return;
            }
            // DTLS preserves message boundaries but does not provide reliable
            // delivery. Avoid overflowing the receiver's UDP socket when an
            // uncompressed image expands into hundreds of datagrams.
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    async fn handle_capture_event(
        &mut self,
        capture: &mut InputCapture,
        event: (CaptureHandle, CaptureEvent),
    ) -> Result<(), CaptureError> {
        let (handle, event) = event;
        let event = match event {
            CaptureEvent::Clipboard { mime_type, data } => {
                match String::from_utf8(data) {
                    Ok(value) => self
                        .event_tx
                        .send(ICaptureEvent::FileClipboard {
                            handle,
                            mime_type,
                            value,
                        })
                        .expect("channel closed"),
                    Err(error) => log::warn!("native file clipboard is not UTF-8: {error}"),
                }
                return Ok(());
            }
            event => event,
        };
        log::info!(
            "capture event handle={handle} event={event:?} ready={} barrier={} active={}",
            self.conn.remote_ready(handle),
            self.enabled_captures.contains(&handle),
            self.active_client == Some(handle)
        );
        // A barrier removed a moment ago can still deliver queued events; its
        // position and kind are gone, so they belong to nobody now.
        if !self.captures.iter().any(|&(h, ..)| h == handle) {
            log::debug!("dropping event from removed capture {handle}");
            if event == CaptureEvent::Begin {
                capture.release().await?;
            }
            return Ok(());
        }

        if capture.keys_pressed(&self.release_bind.borrow()) {
            log::info!("releasing capture: release-bind pressed");
            return self.release_capture(capture).await;
        }

        if event == CaptureEvent::Begin
            && self.get_type(handle) == CaptureType::Default
            && !self.conn.remote_ready(handle)
        {
            log::warn!("rejecting capture: client {handle} cannot accept remote input");
            capture.release().await?;
            return Ok(());
        }

        // Default and EnterOnly barriers on the entry edge alike: releasing
        // an EnterOnly one would tell the service the pointer left back.
        if event == CaptureEvent::Begin
            && !self.is_touchpad()
            && self.just_entered_through(self.get_pos(handle))
        {
            log::info!("ignoring the edge the pointer just entered through");
            log::info!(
                "[route] IGNORE {:?} edge touch right after entry",
                self.get_pos(handle)
            );
            capture.release().await?;
            return Ok(());
        }

        if event == CaptureEvent::Begin {
            self.event_tx
                .send(ICaptureEvent::CaptureBegin(handle))
                .expect("channel closed");
        }

        // enter only capture (for incoming connections)
        if self.get_type(handle) == CaptureType::EnterOnly {
            // if there is no active outgoing connection at the current capture,
            // we release the capture
            if !self.is_default_capture_at(self.get_pos(handle)) {
                log::info!("releasing capture: no active client at this position");
                capture.release().await?;
            }
            // we dont care about events from incoming handles except for releasing the capture
            return Ok(());
        }

        // The pointer of the device controlling this one went back towards
        // it: the incoming barrier on that edge hands control back. Entering
        // it from here as well would make this device a second owner.
        if event == CaptureEvent::Begin && !self.is_touchpad() && self.returns_to_controller(handle)
        {
            log::info!("[route] BACK pointer returns to its owner, client {handle}");
            capture.release().await?;
            // The owner may have arrived through another device, so no
            // incoming barrier sits on this edge to hand control back: ask
            // the owner to take its pointer home through this edge.
            if let Some((controller, _)) = self.controller.clone() {
                self.event_tx
                    .send(ICaptureEvent::HandOn {
                        target: controller.clone(),
                        controller,
                        side: to_proto_pos(self.get_pos(handle)),
                    })
                    .expect("channel closed");
            }
            return Ok(());
        }

        // The shared pointer here belongs to a peer that controls us. At an
        // edge towards another device, the peer enters that device itself
        // (multi-hop Handoff); taking the pointer over here as well would
        // enter it twice.
        // The phone's own touchpad is this device's input, never a peer's
        // pointer passing through: it is not deferred.
        if event == CaptureEvent::Begin && !self.is_touchpad() && self.defer_to_handoff(handle) {
            capture.release().await?;
            if let (Some((controller, _)), Some(target)) = (
                self.controller.clone(),
                self.conn.client_fingerprint(handle),
            ) {
                let side = to_proto_pos(self.get_pos(handle));
                log::info!(
                    "[route] PASS owner's pointer on to client {handle} through the {side} edge"
                );
                self.event_tx
                    .send(ICaptureEvent::HandOn {
                        controller,
                        target,
                        side,
                    })
                    .expect("channel closed");
            }
            return Ok(());
        }

        // activated a new client
        if event == CaptureEvent::Begin && Some(handle) != self.active_client {
            log::info!(
                "[route] OUT own pointer leaves through the {:?} edge -> client {handle}",
                self.get_pos(handle)
            );
            self.state = State::WaitingForAck;
            self.ack_deadline = Some(Instant::now() + Duration::from_millis(750));
            self.active_client.replace(handle);
            self.event_tx
                .send(ICaptureEvent::ClientEntered(handle))
                .expect("channel closed");
        }

        let Some(handle) = self.input_target(handle, event == CaptureEvent::Begin) else {
            // Input still queued in the backend after this screen released
            // capture (for example after passing the pointer on): nothing is
            // entered, and turning it into an Enter would make this device a
            // second owner of the next screen.
            log::debug!("dropping input after release (capture {handle})");
            return Ok(());
        };
        let opposite_pos = to_proto_pos(self.get_pos(handle).opposite());

        let event = match event {
            CaptureEvent::Begin => {
                // A fresh entry from our own screen, not a handoff.
                self.entering_via = None;
                ProtoEvent::Enter(opposite_pos)
            }
            CaptureEvent::Input(e) => match self.state {
                // connection not acknowledged, repeat `Enter` event; keep
                // discrete presses so they still arrive once it is.
                State::WaitingForAck => {
                    if matches!(
                        e,
                        Event::Pointer(PointerEvent::Button { .. })
                            | Event::Keyboard(KeyboardEvent::Key { .. })
                    ) && self.held_until_ack.len() < MAX_HELD_UNTIL_ACK
                    {
                        self.held_until_ack.push(e);
                    }
                    self.enter_event(opposite_pos)
                }
                State::Sending => ProtoEvent::Input(e),
            },
            CaptureEvent::Clipboard { .. } => unreachable!("clipboard events return above"),
        };

        if let Err(e) = self.conn.send(event, handle).await {
            const DUR: Duration = Duration::from_millis(500);
            debounce!(PREV_LOG, DUR, log::warn!("releasing capture: {e}"));
            self.release_capture(capture).await?;
        }
        Ok(())
    }

    /// Enter event for the current entry: plain, or naming the device the
    /// pointer comes through after a handoff.
    fn enter_event(&self, side: syntra_proto::Position) -> ProtoEvent {
        match &self.entering_via {
            Some(via) => ProtoEvent::EnterVia {
                side,
                via: via.clone(),
            },
            None => ProtoEvent::Enter(side),
        }
    }

    async fn send_enter(&mut self, handle: CaptureHandle, side: syntra_proto::Position) {
        let event = self.enter_event(side);
        let _ = self.conn.send(event, handle).await;
    }

    /// Multi-hop: the device we control pushed our pointer on to the device
    /// with certificate `target`. Enter that device directly if we trust it
    /// (or trust-through-hops is allowed); otherwise take the pointer home.
    async fn handoff(
        &mut self,
        capture: &mut InputCapture,
        from: CaptureHandle,
        target: &str,
        side: syntra_proto::Position,
    ) -> Result<(), CaptureError> {
        // The chain came back round to this machine (for example
        // MSI -> phone -> HP -> MSI): the pointer is home.
        if target == self.conn.own_fingerprint() {
            log::info!("hand-off from client {from} leads back here; taking the pointer home");
            log::info!(
                "[route] HOME pointer returns through the {:?} edge (client {from})",
                self.get_pos(from)
            );
            // It comes in from the device it was handed on by, so it appears
            // on that device's edge, not where it left this screen. Placing
            // it touches that edge: capture must not take it as leaving.
            let edge = self.get_pos(from);
            self.last_entry.set(Some((edge, Instant::now())));
            // The portal restores the pointer where it left this screen
            // unless told otherwise; a separate move afterwards loses the
            // race with that restore and leaves it on the wrong edge.
            if !self.release_capture_at(capture, Some(edge)).await? {
                self.event_tx
                    .send(ICaptureEvent::PlacePointer(edge))
                    .expect("channel closed");
            }
            return Ok(());
        }
        let next = self
            .captures
            .iter()
            .find(|(handle, _, kind)| {
                *kind == CaptureType::Default
                    && *handle != from
                    && self.conn.client_fingerprint(*handle).as_deref() == Some(target)
            })
            .map(|(handle, ..)| *handle);
        let Some(next) = next else {
            // Pairing is the trust boundary: never enter a device this
            // machine has not authorised. The middle device keeps the
            // pointer (or carries it on itself if its user allowed that).
            log::warn!(
                "handoff from client {from} refused: the next device is not paired with this one"
            );
            let _ = self
                .conn
                .send(
                    ProtoEvent::Handoff {
                        target: target.to_owned(),
                        side,
                    },
                    from,
                )
                .await;
            return Ok(());
        };
        if !self.conn.remote_ready(next) {
            log::warn!("handoff from client {from} refused: client {next} cannot accept input");
            let _ = self
                .conn
                .send(
                    ProtoEvent::Handoff {
                        target: target.to_owned(),
                        side,
                    },
                    from,
                )
                .await;
            return Ok(());
        }
        log::info!("handing off from client {from} to client {next}");
        log::info!(
            "[route] HOP client {from} ({:?} edge) -> client {next} ({:?} edge)",
            self.get_pos(from),
            self.get_pos(next)
        );
        // Clean up the device we are leaving without giving up the capture.
        let _ = self.conn.send(ProtoEvent::Leave(0), from).await;
        self.state = State::WaitingForAck;
        self.ack_deadline = Some(Instant::now() + Duration::from_millis(750));
        self.held_until_ack.clear();
        self.active_client = Some(next);
        self.event_tx
            .send(ICaptureEvent::ClientEntered(next))
            .expect("channel closed");
        // Name the device the pointer comes through, so the receiver places
        // it on that device's side of its own screen.
        let enter = to_proto_pos(self.get_pos(next).opposite());
        self.entering_via = self.conn.client_fingerprint(from);
        self.send_enter(next, enter).await;
        Ok(())
    }

    async fn release_capture(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        self.release_capture_at(capture, None).await.map(|_| ())
    }

    /// Releases capture; with `landing`, the pointer is left inside that edge.
    /// Returns whether the capture backend placed the pointer.
    async fn release_capture_at(
        &mut self,
        capture: &mut InputCapture,
        landing: Option<Position>,
    ) -> Result<bool, CaptureError> {
        self.ack_deadline = None;
        self.held_until_ack.clear();
        self.state = State::WaitingForAck;
        let Some(handle) = self.active_client.take() else {
            return release_with(capture, landing).await;
        };
        // Snapshot what is still held BEFORE releasing: release clears it.
        // Without synthesized key-ups, pressing the release-bind chord
        // (typically all four modifiers) leaves the peer with phantom held
        // modifiers: the down events were forwarded while capture was
        // active, but the matching up events arrive after the local tap
        // flips to passthrough and never reach the peer.
        let keys = capture.take_pressed_keys();
        let buttons = capture.take_pressed_buttons();
        // Give the pointer back locally first. Remote cleanup goes over the
        // network and must never keep the user's own desktop captured.
        let released = release_with(capture, landing).await;

        let mut events = Vec::with_capacity(keys.len() + buttons.len() + 2);
        events.extend(keys.into_iter().map(|key| {
            ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: key as u32,
                state: 0,
            }))
        }));
        events.extend(buttons.into_iter().map(|button| {
            ProtoEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button,
                state: 0,
            }))
        }));
        // Reset the modifier mask too: the peer keeps XKB-style modifier
        // state separate from pressed keys, so a locked CapsLock would
        // otherwise survive the release.
        events.push(ProtoEvent::Input(Event::Keyboard(
            KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            },
        )));
        events.push(ProtoEvent::Leave(0));
        log::info!("sending Leave event to client {handle}");
        let conn = &self.conn;
        let cleanup = async {
            for event in events {
                if let Err(e) = conn.send(event, handle).await {
                    log::warn!("failed to send release cleanup to client {handle}: {e}");
                }
            }
        };
        if tokio::time::timeout(REMOTE_RELEASE_TIMEOUT, cleanup)
            .await
            .is_err()
        {
            log::warn!(
                "release cleanup for client {handle} timed out; its watchdog will reset input"
            );
        }
        released
    }
}

/// Presses kept while waiting for the peer to acknowledge entry.
const MAX_HELD_UNTIL_ACK: usize = 64;

/// Upper bound for sending key-ups and `Leave` to a peer on release. The
/// peer's own heartbeat watchdog covers anything that does not arrive.
const REMOTE_RELEASE_TIMEOUT: Duration = Duration::from_millis(500);

fn reconnect_timer() -> tokio::time::Interval {
    // A timer inside select! would restart whenever traffic wins another branch.
    let mut timer = tokio::time::interval_at(
        tokio::time::Instant::now() + RECONNECT_INTERVAL,
        RECONNECT_INTERVAL,
    );
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    timer
}

thread_local! {
    static PREV_LOG: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    WaitingForAck,
    Sending,
}

pub(crate) fn capture_pos(pos: syntra_api::Position) -> syntra_input_capture::Position {
    match pos {
        syntra_api::Position::Left => syntra_input_capture::Position::Left,
        syntra_api::Position::Right => syntra_input_capture::Position::Right,
        syntra_api::Position::Top => syntra_input_capture::Position::Top,
        syntra_api::Position::Bottom => syntra_input_capture::Position::Bottom,
    }
}

fn to_proto_pos(pos: syntra_input_capture::Position) -> syntra_proto::Position {
    match pos {
        syntra_input_capture::Position::Left => syntra_proto::Position::Left,
        syntra_input_capture::Position::Right => syntra_proto::Position::Right,
        syntra_input_capture::Position::Top => syntra_proto::Position::Top,
        syntra_input_capture::Position::Bottom => syntra_proto::Position::Bottom,
    }
}

struct DropGuard<T> {
    tx: Sender<T>,
    on_drop: Option<T>,
}

impl<T> DropGuard<T> {
    fn new(tx: Sender<T>, on_new: T, on_drop: T) -> Self {
        tx.send(on_new).expect("channel closed");
        let on_drop = Some(on_drop);
        Self { tx, on_drop }
    }
}

impl<T> Drop for DropGuard<T> {
    fn drop(&mut self) {
        self.tx
            .send(self.on_drop.take().expect("item"))
            .expect("channel closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ClientManager;
    use webrtc_dtls::crypto::Certificate;

    fn capture_task() -> (CaptureTask, Sender<CaptureRequest>, Receiver<ICaptureEvent>) {
        let (requests, request_rx) = channel();
        let (event_tx, events) = channel();
        let task = CaptureTask {
            active_client: None,
            ack_deadline: None,
            held_until_ack: Vec::new(),
            entering_via: None,
            controller: None,
            last_entry: EntryMark::default(),
            multi_hop: Default::default(),
            backend: None,
            restart_now: false,
            cancellation_token: CancellationToken::new(),
            enabled_captures: HashSet::new(),
            captures: Vec::new(),
            conn: SyntraConnection::new(
                Certificate::generate_self_signed(["ignored".to_owned()]).unwrap(),
                ClientManager::default(),
            ),
            event_tx,
            request_rx,
            release_bind: Rc::new(RefCell::new(Vec::new())),
            input_sharing: true,
            state: State::default(),
        };
        (task, requests, events)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retry_replaces_stalled_capture_and_drops_old_attempt() {
        let (mut task, requests, _events) = capture_task();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
        let mut first = Some((started_tx, dropped_tx));
        let initialization = task.initialize_capture(|| {
            let first = first.take();
            async move {
                if let Some((started, held)) = first {
                    let _held = held;
                    started.send(()).unwrap();
                    std::future::pending().await
                } else {
                    InputCapture::new(Some(syntra_input_capture::Backend::Dummy)).await
                }
            }
        });
        let retry = async {
            started_rx.await.unwrap();
            requests.send(CaptureRequest::Reenable).unwrap();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(initialization, retry)
        })
        .await
        .expect("Retry left capture waiting on the old initialization");
        let mut capture = result.unwrap().expect("replacement capture unavailable");
        assert!(dropped_rx.await.is_err(), "old initialization was retained");
        capture.terminate().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn stalled_capture_times_out_and_can_initialize_again() {
        let (mut task, _requests, _events) = capture_task();
        let start = tokio::time::Instant::now();
        let result = task.initialize_capture(std::future::pending).await.unwrap();
        assert!(result.is_none());
        assert_eq!(start.elapsed(), crate::INPUT_INITIALIZATION_TIMEOUT);
        let mut capture = task
            .initialize_capture(|| InputCapture::new(Some(syntra_input_capture::Backend::Dummy)))
            .await
            .unwrap()
            .expect("capture stayed stuck after its deadline");
        capture.terminate().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reconnect_deadline_survives_other_events() {
        let mut reconnect = reconnect_timer();
        tokio::time::timeout(RECONNECT_INTERVAL * 2, async {
            loop {
                tokio::select! {
                    biased;
                    _ = reconnect.tick() => break,
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            }
        })
        .await
        .expect("traffic postponed the reconnection deadline");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pending_permission_keeps_control_requests_responsive() {
        let (mut task, requests, _events) = capture_task();
        let cancellation_token = task.cancellation_token.clone();
        let release_bind = Rc::clone(&task.release_bind);
        task.captures
            .push((7, Position::Left, CaptureType::Default));
        requests.send(CaptureRequest::Destroy(7)).unwrap();
        requests
            .send(CaptureRequest::SetInputSharing(false))
            .unwrap();
        requests
            .send(CaptureRequest::SetReleaseBind(vec![
                scancode::Linux::KeyLeftCtrl,
            ]))
            .unwrap();
        let initialization = task.initialize_capture(std::future::pending);
        let observe = async {
            while release_bind.borrow().is_empty() {
                tokio::task::yield_now().await;
            }
            cancellation_token.cancel();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(initialization, observe)
        })
        .await
        .expect("permission wait blocked the request channel");
        assert!(result.unwrap().is_none());
        assert!(
            task.captures.is_empty(),
            "Stop must apply before permission resolves"
        );
        assert!(!task.input_sharing);
        assert_eq!(*release_bind.borrow(), vec![scancode::Linux::KeyLeftCtrl]);
    }

    /// Capture task whose connection knows `peers` as configured clients
    /// with the given certificates.
    fn capture_task_with_peers(peers: &[&str]) -> (CaptureTask, Vec<CaptureHandle>) {
        let (mut task, _requests, _events) = capture_task();
        let manager = ClientManager::default();
        let handles = peers
            .iter()
            .map(|fingerprint| {
                let handle = manager.add_client();
                manager.set_peer_fingerprint(handle, (*fingerprint).to_owned());
                handle
            })
            .collect();
        task.conn = SyntraConnection::new(
            Certificate::generate_self_signed(["ignored".to_owned()]).unwrap(),
            manager,
        );
        (task, handles)
    }

    /// Direct switching must never wait for a hand-off: a device that was
    /// controlled by its neighbour enters that neighbour again at the shared
    /// edge, with multi-hop on or off, and once nobody controls it.
    #[test]
    fn returning_to_the_controlling_device_is_never_deferred() {
        let (mut task, handles) = capture_task_with_peers(&["phone"]);
        let phone = handles[0];
        task.add_capture(phone, Position::Right, CaptureType::Default);
        task.add_capture(
            crate::client::ENTER_HANDLE_BEGIN,
            Position::Right,
            CaptureType::EnterOnly,
        );
        task.controller = Some(("phone".into(), Position::Right));

        task.multi_hop.set(false);
        assert!(!task.defer_to_handoff(phone), "multi-hop off: never defer");
        task.multi_hop.set(true);
        assert!(
            !task.defer_to_handoff(phone),
            "the controller itself: return"
        );
        task.controller = None;
        assert!(
            !task.defer_to_handoff(phone),
            "not controlled: enter directly"
        );
    }

    /// Going back to the device in control hands control back (its incoming
    /// barrier sends Leave); this screen must not enter it as a second owner.
    #[test]
    fn going_back_to_the_controller_does_not_enter_it() {
        let (mut task, handles) = capture_task_with_peers(&["msi", "phone"]);
        let (msi, phone) = (handles[0], handles[1]);
        task.add_capture(msi, Position::Right, CaptureType::Default);
        task.add_capture(phone, Position::Left, CaptureType::Default);
        task.controller = Some(("msi".into(), Position::Right));
        assert!(task.returns_to_controller(msi));
        assert!(!task.returns_to_controller(phone), "another device");
        task.active_client = Some(phone);
        assert!(
            !task.returns_to_controller(msi),
            "already sending elsewhere"
        );
        task.active_client = None;
        task.controller = None;
        assert!(!task.returns_to_controller(msi), "not controlled");
    }

    /// A device whose certificate is not known yet (its outgoing connection
    /// has not authenticated) must still be entered directly: blocking on an
    /// unknown identity is what froze switching.
    #[test]
    fn an_unidentified_device_is_entered_directly() {
        let (mut task, _requests, _events) = capture_task();
        let manager = ClientManager::default();
        let phone = manager.add_client();
        task.conn = SyntraConnection::new(
            Certificate::generate_self_signed(["ignored".to_owned()]).unwrap(),
            manager,
        );
        task.add_capture(phone, Position::Right, CaptureType::Default);
        task.add_capture(
            crate::client::ENTER_HANDLE_BEGIN,
            Position::Right,
            CaptureType::EnterOnly,
        );
        task.controller = Some(("phone".into(), Position::Right));
        task.multi_hop.set(true);
        assert!(!task.defer_to_handoff(phone));
    }

    /// With multi-hop on and another device in control, every edge towards a
    /// third device is left to the owner: it enters that device itself and
    /// keeps its identity. Without multi-hop this screen enters it directly.
    #[test]
    fn edges_towards_other_devices_go_to_the_owner() {
        let (mut task, handles) = capture_task_with_peers(&["hp", "phone"]);
        let (hp, phone) = (handles[0], handles[1]);
        task.add_capture(hp, Position::Left, CaptureType::Default);
        task.add_capture(phone, Position::Right, CaptureType::Default);
        // The phone controlled this screen earlier: its barrier stays.
        task.add_capture(
            crate::client::ENTER_HANDLE_BEGIN,
            Position::Right,
            CaptureType::EnterOnly,
        );
        // MSI now controls it, entered through the left edge.
        task.controller = Some(("msi".into(), Position::Left));
        task.multi_hop.set(true);
        assert!(task.defer_to_handoff(hp), "another device: owner enters it");
        assert!(task.defer_to_handoff(phone), "any other edge too");
        task.multi_hop.set(false);
        assert!(!task.defer_to_handoff(hp));
    }

    /// Placing the pointer next to the edge it entered through touches that
    /// edge at once; only later is that edge the user going back.
    #[test]
    fn the_entry_edge_is_ignored_right_after_entry() {
        let (task, _requests, _events) = capture_task();
        assert!(
            !task.just_entered_through(Position::Right),
            "nobody entered"
        );
        task.last_entry.set(Some((Position::Right, Instant::now())));
        assert!(task.just_entered_through(Position::Right));
        assert!(
            !task.just_entered_through(Position::Left),
            "other edges: normal"
        );
        task.last_entry
            .set(Some((Position::Right, Instant::now() - ENTRY_GRACE)));
        assert!(
            !task.just_entered_through(Position::Right),
            "later: going back"
        );
    }

    /// Input queued before a release must not re-enter anything: only a new
    /// Begin enters a device.
    #[test]
    fn input_after_release_goes_nowhere() {
        let (mut task, handles) = capture_task_with_peers(&["hp", "phone"]);
        let (hp, phone) = (handles[0], handles[1]);
        assert_eq!(task.input_target(phone, false), None, "released: dropped");
        assert_eq!(task.input_target(phone, true), Some(phone), "Begin enters");
        task.active_client = Some(hp);
        assert_eq!(task.input_target(phone, false), Some(hp), "after hand-off");
    }

    /// Choosing another backend restarts capture with it at once.
    #[tokio::test(flavor = "current_thread")]
    async fn choosing_a_backend_restarts_capture_with_it() {
        let (mut task, _requests, _events) = capture_task();
        let restart = task
            .handle_inactive_request(CaptureRequest::SetBackend(Some(
                syntra_input_capture::Backend::Dummy,
            )))
            .await;
        assert!(restart);
        assert!(task.restart_now);
        assert_eq!(task.backend, Some(syntra_input_capture::Backend::Dummy));
    }
}
