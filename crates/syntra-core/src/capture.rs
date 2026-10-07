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
    /// The captured local pointer came back through an edge.
    PointerReturned { edge: Position, along: Option<f32> },
    /// How hard an edge is being pushed, from 0.0 to 1.0; 0.0 ends it.
    EdgePressure {
        edge: Position,
        amount: f32,
        along: Option<f32>,
    },
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
    /// Peers have a cursor of their own on this screen.
    SetSeparatePeerPointers(bool),
    /// How hard an edge must be pushed before the pointer passes on.
    SetEdgePressure(f64),
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
        entry_along: EntryAlong,
    ) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let cancellation_token = CancellationToken::new();
        let capture_task = CaptureTask {
            active_client: None,
            ack_deadline: None,
            held_until_ack: Vec::new(),
            entering_via: None,
            entry_along: None,
            controller: None,
            last_entry,
            landing_along: entry_along,
            multi_hop: Default::default(),
            separate_peer_pointers: false,
            edge_pressure: 0.0,
            last_pressure_report: None,
            last_pressure_amount: 0.0,
            pressure: None,
            last_pass: None,
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

    pub(crate) fn set_separate_peer_pointers(&self, enabled: bool) {
        let _ = self
            .request_tx
            .send(CaptureRequest::SetSeparatePeerPointers(enabled));
    }

    pub(crate) fn set_edge_pressure(&self, amount: f64) {
        let _ = self
            .request_tx
            .send(CaptureRequest::SetEdgePressure(amount));
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
/// Releases capture, leaving the pointer inside `landing` when given, at the
/// share `along` of that edge when one is given too.
async fn release_with(
    capture: &mut InputCapture,
    landing: Option<Position>,
    along: Option<f32>,
) -> Result<bool, CaptureError> {
    match landing {
        Some(edge) => capture.release_at_along(edge, along).await,
        None => capture.release().await.map(|()| false),
    }
}

/// The outcome of one motion event while an edge push is being measured.
#[derive(Debug, PartialEq)]
enum Pressure {
    Building(f64),
    Reached,
    Backed,
}

/// How far back, in motion units, the pointer may be pulled from the edge
/// before it is let go. Smaller jitter, the bounce off the barrier included,
/// must not end the push: the barrier only reports a touch again after the
/// pointer has really left the edge and come back.
const PRESSURE_BACK_SLACK: f64 = 40.0;

/// A second `Begin` this soon after the last update is the barrier reporting
/// one touch twice; anything later is a new touch.
const DUPLICATE_TOUCH: Duration = Duration::from_millis(50);

/// How fast a push that is not kept up leaks away, in motion units per
/// second: idle at the edge lets pressure go, pressing builds it again.
const PRESSURE_LEAK_PER_SECOND: f64 = 100.0;

/// Smallest change of the reported amount that is always sent, however soon
/// after the last one: about one step of a gentle push.
const PRESSURE_REPORT_STEP: f32 = 0.08;

/// How long small changes of the reported amount are held back.
const PRESSURE_REPORT_INTERVAL: Duration = Duration::from_millis(33);

/// Whether a new `amount` is worth telling the service about.
///
/// Small changes are limited by time, so a mouse making hundreds of motions a
/// second does not make hundreds of messages. A large one is always sent: a
/// firm push climbs from nothing to the full amount in a motion or two, and a
/// rule based on time alone dropped the one that mattered, which is why the
/// glow showed only a faint start or nothing at all. The first report, with
/// nothing before it, is always sent.
fn should_report_pressure(since_last: Option<Duration>, last: f32, amount: f32) -> bool {
    match since_last {
        None => true,
        Some(gap) => {
            gap >= PRESSURE_REPORT_INTERVAL || (amount - last).abs() >= PRESSURE_REPORT_STEP
        }
    }
}

/// Adds one motion event to the push against `edge`. Only movement out of
/// the screen builds it up; movement back takes it away, down to
/// `-PRESSURE_BACK_SLACK`, where the pointer is let go.
fn advance_pressure(
    so_far: f64,
    idle: Duration,
    edge: Position,
    dx: f64,
    dy: f64,
    needed: f64,
) -> Pressure {
    let so_far = if so_far > 0.0 {
        (so_far - PRESSURE_LEAK_PER_SECOND * idle.as_secs_f64()).max(0.0)
    } else {
        so_far
    };
    let push = match edge {
        Position::Left => -dx,
        Position::Right => dx,
        Position::Top => -dy,
        Position::Bottom => dy,
    };
    if !push.is_finite() {
        return Pressure::Building(so_far);
    }
    let total = (so_far + push).max(-PRESSURE_BACK_SLACK);
    if total >= needed {
        Pressure::Reached
    } else if total <= -PRESSURE_BACK_SLACK {
        Pressure::Backed
    } else {
        Pressure::Building(total)
    }
}

/// Default push against an edge before the pointer passes on, in motion
/// units (a gentle push moves about 8 per event, a firm one 30 or more).
///
/// Stays on: this is the rule MSI to HP already follows, and the returns
/// (HP to MSI, ROG to MSI) are brought to it.
pub(crate) const DEFAULT_EDGE_PRESSURE: u32 = 80;

/// How long after an entry its own edge is not treated as leaving again.
const ENTRY_GRACE: Duration = Duration::from_millis(400);

/// Shared between the emulation and capture tasks (same local thread): the
/// edge a peer's pointer last entered through and when. Set synchronously
/// before the pointer is placed, so capture never races the service.
pub(crate) type EntryMark = Rc<std::cell::Cell<Option<(Position, Instant)>>>;

/// Where along its edge the pointer of the last entry came in, as a share of
/// the edge from 0.0 to 1.0. Shared like the mark above, but set on every
/// entry whatever the backend places, and read once by the release that lets
/// the cursor go to meet it.
pub(crate) type EntryAlong = Rc<std::cell::Cell<Option<f32>>>;

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
    /// Independent pointers are on and the compositor draws each peer's own
    /// cursor: this screen's shared pointer then belongs to this device.
    separate_peer_pointers: bool,
    /// Push, in motion units, needed against an edge before the pointer
    /// passes on. Zero passes on at the first touch.
    edge_pressure: f64,
    /// Edge push being measured: the capture handle, what has built up and
    /// when it was last updated (idle time leaks it away).
    pressure: Option<(CaptureHandle, f64, Instant)>,
    /// When pressure was last reported to the service, so a burst of motion
    /// events does not become a burst of messages.
    last_pressure_report: Option<Instant>,
    /// The amount last reported, so a large change can be told from a small
    /// one.
    last_pressure_amount: f32,
    /// Edge the owner's pointer was last passed on through, and when.
    last_pass: Option<(Position, Instant)>,
    /// After a handoff: certificate of the device the pointer came through,
    /// named in every enter until the new peer acknowledges.
    entering_via: Option<String>,
    /// Where along the edge the pointer came in, as a fraction, from the
    /// capture backend. Named in every enter until the peer acknowledges, so
    /// the repeats carry it too.
    entry_along: Option<f32>,
    /// Where along the edge a peer's pointer came in, as a share of the edge,
    /// for the one release that lets the cursor go to meet it. Taken by that
    /// release, so it never outlives it.
    landing_along: EntryAlong,
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

    /// Whether a pointer entered this screen a moment ago, through any edge.
    /// Until the placement has taken effect the cursor can still sit against
    /// the opposite edge, where it was left last time; a touch there then is
    /// that stale position, and passing it on sent the pointer straight
    /// through this screen and back to where it came from, in a loop.
    fn just_entered(&self) -> bool {
        matches!(self.last_entry.get(), Some((_, at)) if at.elapsed() < ENTRY_GRACE)
    }

    /// Whether the owner's pointer was passed on through `pos` a moment ago.
    fn passed_on_just_now(&self, pos: Position) -> bool {
        matches!(self.last_pass, Some((edge, at)) if edge == pos && at.elapsed() < ENTRY_GRACE)
    }

    /// One line that says where each identity's pointer is, as this device
    /// sees it: who this screen sends to, who controls this screen, and
    /// whether a push is being measured. Compare the lines of all devices.
    fn where_is_the_pointer(&self, why: &str) {
        let short = |fingerprint: &str| fingerprint.chars().take(8).collect::<String>();
        log::info!(
            "[where] {why}: this device sends to client {:?} ({}); controlled by {}; push {}",
            self.active_client,
            if matches!(self.state, State::Sending) {
                "sending"
            } else {
                "waiting for ack"
            },
            self.controller
                .as_ref()
                .map(|(fingerprint, edge)| format!("{} via {edge:?}", short(fingerprint)))
                .unwrap_or_else(|| "nobody".into()),
            match self.pressure {
                Some((handle, so_far, _)) => format!("armed on client {handle} at {so_far:.0}"),
                None => "none".into(),
            },
        );
    }

    /// Clipboard and history traffic for a peer. A peer that is not
    /// connected gets nothing: history sync queues thousands of records, and
    /// failing and logging each one on the capture task starved the keepalive
    /// replies of every other peer, closing their connections too.
    async fn send_proto_event(&self, handle: CaptureHandle, event: ProtoEvent) {
        if !self.conn.peer_connected(handle) {
            log::trace!("dropping a protocol event for peer {handle}: not connected");
            return;
        }
        if let Err(error) = self.conn.send(event, handle).await {
            log::warn!("failed to send clipboard protocol event: {error}");
        }
    }

    /// Whether `handle` is the device whose pointer is on this screen, and
    /// this screen is not controlling anything itself.
    fn returns_to_controller(&self, handle: CaptureHandle) -> bool {
        if self.separate_peer_pointers {
            return false;
        }
        let Some((controller, _)) = self.controller.as_ref() else {
            return false;
        };
        self.active_client.is_none()
            && self.conn.client_fingerprint(handle).as_deref() == Some(controller.as_str())
    }

    fn defer_to_handoff(&self, handle: CaptureHandle) -> bool {
        // A peer with a cursor of its own leaves through its own edge
        // handling: this screen's pointer is never the peer's.
        if self.separate_peer_pointers {
            return false;
        }
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
            CaptureRequest::SetController(controller) => {
                self.controller = controller;
                self.clear_pressure();
            }
            CaptureRequest::SetMultiHop(enabled) => self.multi_hop.set(enabled),
            CaptureRequest::SetSeparatePeerPointers(enabled) => {
                self.separate_peer_pointers = enabled;
            }
            CaptureRequest::SetEdgePressure(amount) => self.edge_pressure = amount,
            CaptureRequest::SetInputSharing(enabled) => self.input_sharing = enabled,
            CaptureRequest::SetReleaseBind(bind) => {
                self.release_bind.borrow_mut().clone_from(&bind);
            }
            CaptureRequest::SendProto { handle, event } => {
                self.send_proto_event(handle, event).await;
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
                            self.entry_along = None;
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
                        // A pointer just entered this screen: let the compositor
                        // put the cursor on the entry edge. Released with no
                        // edge it puts it back where the last capture began,
                        // the edge the pointer last left through, undoing the
                        // placement and sometimes leaving it on the wrong edge.
                        let landing = match self.last_entry.get() {
                            Some((edge, at)) if at.elapsed() < ENTRY_GRACE => Some(edge),
                            _ => None,
                        };
                        self.clear_pressure();
                        self.where_is_the_pointer("capture released");
                        let placed = self.release_capture_at(capture, landing).await?;
                        if let Some(edge) = landing {
                            self.last_entry.set(Some((edge, Instant::now())));
                            // A peer's pointer arrived while ours was captured, or
                            // the compositor did not place it: place it ourselves.
                            if was_sending || !placed {
                                self.event_tx
                                    .send(ICaptureEvent::PlacePointer(edge))
                                    .expect("channel closed");
                            }
                        }
                    }
                    CaptureRequest::SetController(controller) => {
                        let changed = controller.as_ref().map(|c| &c.0)
                            != self.controller.as_ref().map(|c| &c.0);
                        self.controller = controller;
                        // A push was being measured with the capture held. Once
                        // who controls this screen changes, nobody is waiting for
                        // that push any more: the pointer would stay trapped at
                        // the barrier with nowhere to go.
                        if changed && self.pressure.is_some() {
                            log::info!("[route] edge push dropped: control of this screen changed");
                            self.release_capture(capture).await?;
                        }
                    }
                    CaptureRequest::SetMultiHop(enabled) => self.multi_hop.set(enabled),
                    CaptureRequest::SetSeparatePeerPointers(enabled) => {
                        self.separate_peer_pointers = enabled;
                    }
                    CaptureRequest::SetEdgePressure(amount) => self.edge_pressure = amount,
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
                        self.send_proto_event(handle, event).await;
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
        let mut pressure_met = false;
        let mut event = match event {
            CaptureEvent::Entry { along } => {
                // Not input to forward: where along the edge the pointer is
                // about to enter. Kept before `input_target` could drop it, for
                // the `Enter` that `Begin` is about to turn into.
                self.entry_along = Some(along);
                return Ok(());
            }
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

        // A push is being measured on this edge: motion adds to it or takes
        // it back, and nothing else reaches the peer until it is enough.
        if let Some((pending, so_far, since)) = self.pressure {
            if pending != handle {
                // The same touch is reported by every barrier on this edge:
                // the outgoing barrier and the incoming one sit side by side.
                // Only the outgoing one is being measured; letting the other
                // cancel it dropped the push while the capture stayed held,
                // and every later event of the measured barrier was thrown
                // away: the pointer stuck at the edge, out of reach.
                if self.captures.iter().any(|&(h, ..)| h == pending)
                    && self.get_pos(handle) == self.get_pos(pending)
                {
                    return Ok(());
                }
                // A different edge, or the measured barrier is gone: the push
                // is over, and the capture held for it must be let go.
                self.release_capture(capture).await?;
                return Ok(());
            } else if let CaptureEvent::Input(Event::Pointer(PointerEvent::Motion {
                dx, dy, ..
            })) = &event
            {
                match advance_pressure(
                    so_far,
                    since.elapsed(),
                    self.get_pos(handle),
                    *dx,
                    *dy,
                    self.edge_pressure,
                ) {
                    Pressure::Building(total) => {
                        self.pressure = Some((handle, total, Instant::now()));
                        self.report_pressure(handle, total, false);
                        return Ok(());
                    }
                    Pressure::Reached => {
                        log::info!("[route] edge pressure reached: passing on");
                        // The push that carried it over is shown at full before
                        // it is cleared: passing on ends it in the same breath.
                        if let Some((handle, ..)) = self.pressure {
                            self.report_pressure_peak(handle);
                        }
                        self.clear_pressure();
                        event = CaptureEvent::Begin;
                        pressure_met = true;
                    }
                    Pressure::Backed => {
                        log::info!("[route] edge pressure released: the pointer moved back");
                        self.clear_pressure();
                        capture.release().await?;
                        return Ok(());
                    }
                }
            } else if event == CaptureEvent::Begin && since.elapsed() >= DUPLICATE_TOUCH {
                // A new touch of the edge: whatever was built up belongs to an
                // earlier push. It must not carry over, or every try after
                // the first would need less than the one before.
                log::info!("[route] edge pressure restarted from zero (fresh touch)");
                self.clear_pressure();
            } else {
                return Ok(());
            }
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
        if event == CaptureEvent::Begin {
            log::info!(
                "[route] edge touch {:?} (last entry: {:?})",
                self.get_pos(handle),
                self.last_entry
                    .get()
                    .map(|(edge, at)| (edge, at.elapsed().as_millis())),
            );
        }
        if event == CaptureEvent::Begin && !self.is_touchpad() && self.just_entered() {
            log::info!("ignoring the edge the pointer just entered through");
            log::info!(
                "[route] IGNORE {:?} edge touch right after entry",
                self.get_pos(handle)
            );
            // Let go of the capture that the touch started, and put the cursor
            // on the entry edge. Released without an edge the cursor is left
            // 1 px inside the edge that was touched: right where the next
            // push touches it again, so the pointer never got away from it.
            let landing = self.last_entry.get().map(|(edge, _)| edge);
            if !self.release_capture_at(capture, landing).await? {
                if let Some(edge) = landing {
                    self.event_tx
                        .send(ICaptureEvent::PlacePointer(edge))
                        .expect("channel closed");
                }
            }
            return Ok(());
        }

        // The service must not hear of a touch that has not yet been pushed
        // hard enough: it would hand control on at once.
        let arm_pressure = event == CaptureEvent::Begin
            && !pressure_met
            // The phone's control pad is not a pointer pushed against an edge:
            // it hands control on at once.
            && !cfg!(target_os = "android")
            && !self.is_touchpad()
            && self.edge_pressure > 0.0
            && self.get_type(handle) == CaptureType::Default
            && !self.returns_to_controller(handle)
            && !self.passed_on_just_now(self.get_pos(handle));
        if event == CaptureEvent::Begin && !arm_pressure {
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
            self.where_is_the_pointer("pointer back to its owner");
            // The same push touches the edge again a moment later: that touch
            // must neither arm a push nor send this screen's pointer on.
            self.last_pass = Some((self.get_pos(handle), Instant::now()));
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
        // One gesture can deliver more than one touch of the same edge before
        // the service has answered the pass-on. The second must not send this
        // screen's own pointer as well: that is two identities in one push.
        if event == CaptureEvent::Begin && self.passed_on_just_now(self.get_pos(handle)) {
            log::info!(
                "[route] IGNORE {:?} edge touch right after passing the pointer on",
                self.get_pos(handle)
            );
            capture.release().await?;
            return Ok(());
        }
        // Edge pressure: touching the edge is not enough to leave. The push
        // outwards is added up, and only when it reaches the configured
        // amount does the pointer pass on. Resting on the edge adds nothing.
        if arm_pressure {
            log::info!(
                "[route] edge pressure armed at the {:?} edge (needs {:.0})",
                self.get_pos(handle),
                self.edge_pressure
            );
            self.where_is_the_pointer("edge pressure armed");
            self.pressure = Some((handle, 0.0, Instant::now()));
            return Ok(());
        }
        if event == CaptureEvent::Begin && !self.is_touchpad() && self.defer_to_handoff(handle) {
            self.last_pass = Some((self.get_pos(handle), Instant::now()));
            capture.release().await?;
            if let (Some((controller, _)), Some(target)) = (
                self.controller.clone(),
                self.conn.client_fingerprint(handle),
            ) {
                let side = to_proto_pos(self.get_pos(handle));
                log::info!(
                    "[route] PASS owner's pointer on to client {handle} through the {side} edge"
                );
                self.where_is_the_pointer("owner's pointer passed on");
                self.event_tx
                    .send(ICaptureEvent::HandOn {
                        controller,
                        target,
                        side,
                    })
                    .expect("channel closed");
                // The shared pointer is left against the barrier it just
                // crossed: move it off, or the next touch of that edge would
                // send this screen's own pointer through as well.
                self.event_tx
                    .send(ICaptureEvent::StepInside(self.get_pos(handle)))
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
            self.where_is_the_pointer("own pointer leaves");
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
                self.enter_event(opposite_pos)
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
            CaptureEvent::Entry { .. } => unreachable!("entry events return above"),
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
        match (&self.entering_via, self.entry_along) {
            (Some(via), _) => ProtoEvent::EnterVia {
                side,
                via: via.clone(),
            },
            // The backend knew where along the edge the pointer came in: a
            // fraction, so it means the same on a screen of any resolution.
            (None, Some(along)) => ProtoEvent::EnterAt { side, along },
            (None, None) => ProtoEvent::Enter(side),
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

    /// Reports how hard the edge of `handle` is being pushed, `0.0` to `1.0`.
    ///
    /// One pointer motion is one call, and a mouse makes hundreds, so small
    /// changes are limited to about thirty a second. A large change is never
    /// held back (see [`should_report_pressure`]): a firm push goes from
    /// nothing to the full amount within a couple of motions, and the one that
    /// carries it up used to fall inside the interval and be dropped, so the
    /// glow saw a faint start and then the end, and showed nothing. The end of
    /// a push is not limited either (`force`).
    fn report_pressure(&mut self, handle: CaptureHandle, so_far: f64, force: bool) {
        let now = Instant::now();
        let need = self.edge_pressure;
        let amount = if need > 0.0 {
            (so_far / need).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        let since_last = self
            .last_pressure_report
            .map(|last| now.duration_since(last));
        if !force && !should_report_pressure(since_last, self.last_pressure_amount, amount) {
            return;
        }
        self.last_pressure_report = Some(now);
        self.last_pressure_amount = amount;
        let _ = self.event_tx.send(ICaptureEvent::EdgePressure {
            edge: self.get_pos(handle),
            amount,
            along: None,
        });
    }

    /// Reports that the push reached the full amount, before it ends.
    ///
    /// Passing on ends the push at once, and the end is reported as nothing,
    /// so without this the full amount that caused it was never shown.
    fn report_pressure_peak(&mut self, handle: CaptureHandle) {
        let need = self.edge_pressure;
        self.report_pressure(handle, need, true);
    }

    /// Ends a push, telling the service so the glow it drew goes out.
    ///
    /// Every path that abandons a push goes through here rather than setting
    /// the field itself: a path that forgot would leave an edge lit for good.
    fn clear_pressure(&mut self) {
        if let Some((handle, ..)) = self.pressure.take() {
            self.report_pressure(handle, 0.0, true);
        }
    }

    async fn release_capture(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        self.clear_pressure();
        self.where_is_the_pointer("capture released");
        self.release_capture_at(capture, None).await.map(|_| ())
    }

    /// Releases capture; with `landing`, the pointer is left inside that edge.
    /// Returns whether the capture backend placed the pointer.
    async fn release_capture_at(
        &mut self,
        capture: &mut InputCapture,
        landing: Option<Position>,
    ) -> Result<bool, CaptureError> {
        // The share of the edge to land at belongs to this one release only.
        let along = self.landing_along.take();
        self.ack_deadline = None;
        self.held_until_ack.clear();
        self.state = State::WaitingForAck;
        let Some(handle) = self.active_client.take() else {
            return release_with(capture, landing, along).await;
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
        let released = release_with(capture, landing, along).await;
        if released.is_ok() {
            if let Some(edge) = landing {
                self.event_tx
                    .send(ICaptureEvent::PointerReturned { edge, along })
                    .expect("channel closed");
            }
        }

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

/// The API's name for an edge of the screen, from the capture crate's.
pub(crate) fn api_pos(pos: syntra_input_capture::Position) -> syntra_api::Position {
    match pos {
        syntra_input_capture::Position::Left => syntra_api::Position::Left,
        syntra_input_capture::Position::Right => syntra_api::Position::Right,
        syntra_input_capture::Position::Top => syntra_api::Position::Top,
        syntra_input_capture::Position::Bottom => syntra_api::Position::Bottom,
    }
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
            entry_along: None,
            landing_along: EntryAlong::default(),
            controller: None,
            last_entry: EntryMark::default(),
            multi_hop: Default::default(),
            separate_peer_pointers: false,
            edge_pressure: 0.0,
            last_pressure_report: None,
            last_pressure_amount: 0.0,
            pressure: None,
            last_pass: None,
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
    async fn local_return_reports_edge_and_position_once() {
        let (mut task, _requests, mut events) = capture_task();
        let mut capture = InputCapture::new(Some(syntra_input_capture::Backend::Dummy))
            .await
            .unwrap();
        task.active_client = Some(7);
        task.landing_along.set(Some(0.35));
        task.release_capture_at(&mut capture, Some(Position::Right))
            .await
            .unwrap();
        assert!(
            matches!(events.next().await, Some(ICaptureEvent::PointerReturned {
            edge: Position::Right, along: Some(along),
        }) if along == 0.35)
        );
        assert_eq!(task.landing_along.get(), None);
        task.release_capture_at(&mut capture, Some(Position::Right))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), events.next())
                .await
                .is_err()
        );
        task.active_client = Some(7);
        task.release_capture_at(&mut capture, None).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), events.next())
                .await
                .is_err()
        );
        capture.terminate().await.unwrap();
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

    /// With a cursor of its own per peer, this screen's pointer is this
    /// device's: touching an edge sends this device on, never the peer.
    #[test]
    fn a_peer_with_its_own_cursor_is_never_passed_on_by_the_local_pointer() {
        let (mut task, handles) = capture_task_with_peers(&["hp"]);
        let hp = handles[0];
        task.add_capture(hp, Position::Left, CaptureType::Default);
        task.controller = Some(("msi".into(), Position::Left));
        task.multi_hop.set(true);
        assert!(task.defer_to_handoff(hp));
        task.separate_peer_pointers = true;
        assert!(!task.defer_to_handoff(hp));
        assert!(!task.returns_to_controller(hp));
    }

    /// One push can touch the same edge twice within milliseconds; after the
    /// pointer was passed on the second touch is ignored, later ones count.
    /// Pushing out of the screen builds up; resting adds nothing; pulling
    /// back takes it away and finally lets the pointer go.
    #[test]
    fn edge_pressure_adds_up_pushes_and_takes_back_pulls() {
        let need = 3.0;
        // Left edge: outwards is negative x.
        let left =
            |so_far, dx| advance_pressure(so_far, Duration::ZERO, Position::Left, dx, 0.0, need);
        assert_eq!(left(0.0, -1.0), Pressure::Building(1.0), "one dot");
        assert_eq!(left(1.0, -1.0), Pressure::Building(2.0), "two dots");
        assert_eq!(left(2.0, -1.0), Pressure::Reached, "the third passes");
        assert_eq!(left(2.9, -0.2), Pressure::Reached, "3.1 is enough");
        // Resting or sliding along the edge builds nothing and does not let go.
        assert_eq!(left(1.0, 0.0), Pressure::Building(1.0));
        assert_eq!(
            advance_pressure(1.0, Duration::ZERO, Position::Left, 0.0, 5.0, need),
            Pressure::Building(1.0),
            "sliding along the edge"
        );
        // Pulling back takes pressure away, but small jitter does not end
        // the push: the pointer is let go only after a real pull back.
        assert_eq!(left(2.0, 1.0), Pressure::Building(1.0));
        assert_eq!(left(0.0, 1.0), Pressure::Building(-1.0), "jitter at rest");
        assert_eq!(left(-39.5, 1.0), Pressure::Backed, "really pulled back");
        // A gentle attempt never spoils a later firm one: the push can
        // still be completed after any amount of gentle tries and jitter.
        let mut total = 0.0;
        for dx in [-8.0, 3.0, -8.0, 2.0, -8.0] {
            total = match advance_pressure(total, Duration::ZERO, Position::Left, dx, 0.0, 80.0) {
                Pressure::Building(t) => t,
                other => panic!("gentle pushes must keep building, got {other:?}"),
            };
        }
        assert_eq!(
            advance_pressure(total, Duration::ZERO, Position::Left, -80.0, 0.0, 80.0),
            Pressure::Reached,
            "a firm push after gentle ones passes"
        );
        // Each edge counts only its own outwards direction.
        assert_eq!(
            advance_pressure(0.0, Duration::ZERO, Position::Right, 4.0, 0.0, 10.0),
            Pressure::Building(4.0)
        );
        assert_eq!(
            advance_pressure(0.0, Duration::ZERO, Position::Top, 0.0, -4.0, 10.0),
            Pressure::Building(4.0)
        );
        assert_eq!(
            advance_pressure(0.0, Duration::ZERO, Position::Bottom, 0.0, 4.0, 10.0),
            Pressure::Building(4.0)
        );
    }

    /// A push that is not kept up leaks away; pushing again builds it anew.
    #[test]
    fn edge_pressure_leaks_while_idle() {
        let after = |so_far, secs: f64, dx| {
            advance_pressure(
                so_far,
                Duration::from_secs_f64(secs),
                Position::Left,
                dx,
                0.0,
                80.0,
            )
        };
        // 60 built up, half a second of rest: most of it is gone.
        let Pressure::Building(left) = after(60.0, 0.5, 0.0) else {
            panic!("resting must not pass on");
        };
        assert!(left < 20.0, "left {left}");
        // The same rest does not stop a firm push from completing.
        assert_eq!(after(60.0, 0.5, -70.0), Pressure::Reached);
        // No rest, no leak.
        assert_eq!(after(60.0, 0.0, -10.0), Pressure::Building(70.0));
        // Leaking never digs below zero on its own.
        assert_eq!(after(5.0, 10.0, 0.0), Pressure::Building(0.0));
    }

    /// A firm push climbs from nothing to nearly the full amount within a
    /// motion or two, a few milliseconds apart. The report of that climb is
    /// the one that matters, and it used to be thrown away because it came
    /// inside the interval after the first faint one.
    #[test]
    fn a_large_jump_of_pressure_is_reported_even_right_after_a_small_one() {
        let just_after = Some(Duration::from_millis(5));
        assert!(
            should_report_pressure(just_after, 0.10, 0.95),
            "the climb to nearly full was dropped"
        );
        assert!(
            should_report_pressure(just_after, 0.95, 0.10),
            "so was a fall back"
        );
    }

    /// A mouse makes hundreds of motions a second; small changes inside the
    /// interval must still be held back or each would become a message.
    #[test]
    fn small_changes_of_pressure_are_still_limited_in_time() {
        let just_after = Some(Duration::from_millis(5));
        assert!(!should_report_pressure(just_after, 0.30, 0.34));
        assert!(should_report_pressure(
            Some(Duration::from_millis(40)),
            0.30,
            0.34
        ));
    }

    /// With nothing reported yet the first amount always goes out, however
    /// small, or a gentle push would never start the glow.
    #[test]
    fn the_first_pressure_report_is_always_sent() {
        assert!(should_report_pressure(None, 0.0, 0.05));
    }

    #[test]
    fn a_second_touch_right_after_a_pass_on_is_ignored() {
        let (mut task, _requests, _events) = capture_task();
        assert!(!task.passed_on_just_now(Position::Left), "nothing passed");
        task.last_pass = Some((Position::Left, Instant::now()));
        assert!(task.passed_on_just_now(Position::Left));
        assert!(!task.passed_on_just_now(Position::Right), "other edge");
        task.last_pass = Some((Position::Left, Instant::now() - ENTRY_GRACE));
        assert!(
            !task.passed_on_just_now(Position::Left),
            "later: user again"
        );
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
