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
    /// capture disabled
    CaptureEnabled,
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
    /// add a capture client
    Create(CaptureHandle, Position, CaptureType),
    /// destory a capture client
    Destroy(CaptureHandle),
    /// reenable input capture
    Reenable,
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
    ) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let cancellation_token = CancellationToken::new();
        let capture_task = CaptureTask {
            active_client: None,
            ack_deadline: None,
            backend,
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

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(CaptureRequest::Reenable)
            .expect("channel closed");
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
        let pos = to_capture_pos(pos);
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
    backend: Option<syntra_input_capture::Backend>,
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
            CaptureRequest::Create(handle, position, kind) => {
                self.add_capture(handle, position, kind);
                if kind == CaptureType::Default {
                    self.conn.connect(handle).await;
                }
            }
            CaptureRequest::Destroy(handle) => self.remove_capture(handle),
            CaptureRequest::Release => {}
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
                    _ = tokio::time::sleep(RECONNECT_INTERVAL) => self.reconnect_lost_peers().await,
                    _ = self.cancellation_token.cancelled() => return,
                }
            }
        }
    }

    async fn initialize_capture(
        &mut self,
        initialization: impl Future<Output = Result<InputCapture, CaptureCreationError>>,
    ) -> Result<Option<InputCapture>, CaptureCreationError> {
        // A portal permission dialog must not block peer transport or clipboard traffic.
        tokio::pin!(initialization);
        loop {
            tokio::select! {
                result = &mut initialization => return result.map(Some),
                request = self.request_rx.recv() => {
                    // Reenable while initialization is pending must not open a second dialog.
                    self.handle_inactive_request(request.expect("channel closed")).await;
                }
                (handle, fingerprint, event) = self.conn.recv() => {
                    self.handle_inactive_event(handle, fingerprint, event)
                },
                _ = tokio::time::sleep(RECONNECT_INTERVAL) => self.reconnect_lost_peers().await,
                _ = self.cancellation_token.cancelled() => return Ok(None),
            }
        }
    }

    async fn do_capture(&mut self) -> Result<(), InputCaptureError> {
        let Some(mut capture) = self
            .initialize_capture(InputCapture::new(self.backend))
            .await?
        else {
            return Ok(());
        };

        let _capture_guard = DropGuard::new(
            self.event_tx.clone(),
            ICaptureEvent::CaptureEnabled,
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
                            self.state = State::Sending;
                            self.ack_deadline = None;
                        }
                        ProtoEvent::Pong(false) | ProtoEvent::Leave(_) => {
                            log::info!("releasing capture: remote input unavailable");
                            self.release_capture(capture).await?;
                        }
                        _ => {}
                    }
                    self.sync_captures(capture).await?;
                },
                _ = tokio::time::sleep(RECONNECT_INTERVAL) => self.reconnect_lost_peers().await,
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
                    CaptureRequest::Release => self.release_capture(capture).await?,
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

        // activated a new client
        if event == CaptureEvent::Begin && Some(handle) != self.active_client {
            self.state = State::WaitingForAck;
            self.ack_deadline = Some(Instant::now() + Duration::from_millis(750));
            self.active_client.replace(handle);
            self.event_tx
                .send(ICaptureEvent::ClientEntered(handle))
                .expect("channel closed");
        }

        let opposite_pos = to_proto_pos(self.get_pos(handle).opposite());

        let event = match event {
            CaptureEvent::Begin => ProtoEvent::Enter(opposite_pos),
            CaptureEvent::Input(e) => match self.state {
                // connection not acknowledged, repeat `Enter` event
                State::WaitingForAck => ProtoEvent::Enter(opposite_pos),
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

    async fn release_capture(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        self.ack_deadline = None;
        self.state = State::WaitingForAck;
        // If we have an active client, notify them we're leaving
        if let Some(handle) = self.active_client.take() {
            // Synthesize key-up events for every key still held in the
            // capture's pressed_keys set BEFORE sending Leave. Without
            // this, pressing the release-bind chord (typically all four
            // modifiers) leaves the peer with phantom held modifiers:
            // the down events were forwarded while capture was active,
            // but the matching up events arrive after the local tap
            // flips to passthrough and never reach the peer. The peer
            // then runs every subsequent keystroke through those held
            // mods until its watchdog times out (1+ s) or our Leave
            // arrives — and Leave can be lost over UDP/DTLS.
            for key in capture.take_pressed_keys() {
                let key_up = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: key as u32,
                    state: 0,
                }));
                if let Err(e) = self.conn.send(key_up, handle).await {
                    log::warn!("failed to send key-up to client {handle}: {e}");
                }
            }
            for button in capture.take_pressed_buttons() {
                let button_up = ProtoEvent::Input(Event::Pointer(PointerEvent::Button {
                    time: 0,
                    button,
                    state: 0,
                }));
                if let Err(e) = self.conn.send(button_up, handle).await {
                    log::warn!("failed to send button-up to client {handle}: {e}");
                }
            }
            // Reset the modifier mask too. The peer's input-emulation
            // layer keeps a separate XKB-style modifier state that's
            // updated by KeyboardEvent::Modifiers, distinct from the
            // pressed_keys set drained above. Without this, an
            // already-locked CapsLock would survive the release.
            let mods_zero = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            }));
            if let Err(e) = self.conn.send(mods_zero, handle).await {
                log::warn!("failed to reset modifiers on client {handle}: {e}");
            }

            log::info!("sending Leave event to client {handle}");
            if let Err(e) = self.conn.send(ProtoEvent::Leave(0), handle).await {
                log::warn!("failed to send Leave to client {handle}: {e}");
            }
        }
        capture.release().await
    }
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

fn to_capture_pos(pos: syntra_api::Position) -> syntra_input_capture::Position {
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

    #[tokio::test(flavor = "current_thread")]
    async fn pending_permission_keeps_control_requests_responsive() {
        let (requests, request_rx) = channel();
        let (event_tx, _events) = channel();
        let cancellation_token = CancellationToken::new();
        let release_bind = Rc::new(RefCell::new(Vec::new()));
        let mut task = CaptureTask {
            active_client: None,
            ack_deadline: None,
            backend: None,
            cancellation_token: cancellation_token.clone(),
            enabled_captures: HashSet::new(),
            captures: vec![(7, Position::Left, CaptureType::Default)],
            conn: SyntraConnection::new(
                Certificate::generate_self_signed(["ignored".to_owned()]).unwrap(),
                ClientManager::default(),
            ),
            event_tx,
            request_rx,
            release_bind: Rc::clone(&release_bind),
            input_sharing: true,
            state: State::default(),
        };
        requests.send(CaptureRequest::Destroy(7)).unwrap();
        requests
            .send(CaptureRequest::SetInputSharing(false))
            .unwrap();
        requests
            .send(CaptureRequest::SetReleaseBind(vec![
                scancode::Linux::KeyLeftCtrl,
            ]))
            .unwrap();
        let initialization = task.initialize_capture(std::future::pending());
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
}
