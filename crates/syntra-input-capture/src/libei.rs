use ashpd::{
    desktop::{
        PersistMode, Session,
        clipboard::{Clipboard, RequestClipboardOptions},
        input_capture::{
            Activated, ActivatedBarrier, Barrier, BarrierID, Capabilities, CreateSession2Options,
            InputCapture, Region, ReleaseOptions, StartOptions, Zones,
        },
    },
    enumflags2::BitFlags,
};
use async_trait::async_trait;
use futures::{FutureExt, StreamExt};
use reis::{
    ei::{self, handshake::ContextType},
    event::{Connection, DeviceCapability, EiEvent},
    tokio::EiConvertEventStream,
};
use std::{
    cell::Cell,
    collections::HashMap,
    env, fs, io,
    num::NonZeroU32,
    os::unix::net::UnixStream,
    path::PathBuf,
    pin::Pin,
    rc::Rc,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};
use tokio::{
    io::AsyncReadExt,
    sync::{
        Notify,
        mpsc::{self, Receiver, Sender},
    },
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use futures_core::Stream;

use syntra_input_event::Event;

use crate::CaptureEvent;

use super::{
    Capture as SyntraInputCapture, Position,
    error::{CaptureError, LibeiCaptureCreationError},
};

/* there is a bug in xdg-remote-desktop-portal-gnome / mutter that
 * prevents receiving further events after a session has been disabled once.
 * Therefore the session needs to be recreated when the barriers are updated */

/* mutter also kills the session whenever ei devices come and go, so there the
 * whole session has to be torn down and recreated on every device change.
 * Elsewhere that is pure overhead: each restart costs a CreateSession +
 * ConnectToEIS round trip, and compositors that keep per-session state around
 * (hyprland leaks a keymap fd per eis session, see hyprwm/Hyprland) can be
 * driven out of file descriptors by the churn.
 * Set LM_RESTART_SESSION_ON_DEVICE_CHANGE=1/0 to override the default. */
static RESTART_SESSION_ON_DEVICE_CHANGE: LazyLock<bool> =
    LazyLock::new(restart_session_on_device_change);

fn restart_session_on_device_change() -> bool {
    match env::var("LM_RESTART_SESSION_ON_DEVICE_CHANGE").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => env::var("XDG_CURRENT_DESKTOP")
            .is_ok_and(|desktops| desktops.to_uppercase().split(':').any(|d| d == "GNOME")),
    }
}

/// events that necessitate restarting the capture session
#[derive(Clone, Copy, Debug)]
enum LibeiNotifyEvent {
    Create(Position),
    Destroy(Position),
}

/// How long to wait for a capture session to acknowledge a release request.
///
/// Past this the session is assumed wedged and the portal session is torn
/// down: leaving the pointer captured is worse than losing the session.
const RELEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Cross-task handshake for handing the pointer back to the local desktop.
///
/// Releasing spans two tasks: whoever asks, and the capture session that
/// must acknowledge. Keeping the three primitives together makes the
/// ordering rule expressible as a method rather than a comment repeated at
/// each call site.
#[derive(Clone, Default)]
struct ReleaseSignal {
    /// Raised to ask the capture session to hand control back.
    requested: Arc<Notify>,
    /// Whether a session currently holds the pointer.
    capturing: Arc<AtomicBool>,
    /// Raised once the session has actually let go.
    completed: Arc<Notify>,
    /// Edge to leave the pointer at on the next release, instead of the
    /// edge the capture began on.
    landing: Arc<std::sync::Mutex<Option<Position>>>,
}

impl ReleaseSignal {
    /// Whether a capture session currently holds the pointer.
    fn is_capturing(&self) -> bool {
        self.capturing.load(Ordering::SeqCst)
    }

    /// Records whether a session holds the pointer.
    fn set_capturing(&self, capturing: bool) {
        self.capturing.store(capturing, Ordering::SeqCst);
    }

    /// Resolves when a release has been requested.
    fn requested(&self) -> impl std::future::Future<Output = ()> + '_ {
        self.requested.notified()
    }

    fn set_landing(&self, landing: Option<Position>) {
        *self.landing.lock().expect("landing lock") = landing;
    }

    fn landing(&self) -> Option<Position> {
        *self.landing.lock().expect("landing lock")
    }

    /// Marks the pointer released and wakes whoever is waiting.
    fn complete(&self) {
        self.set_capturing(false);
        self.completed.notify_waiters();
    }

    /// Asks the session to release and waits for it, up to `timeout`.
    ///
    /// The completion future is created *before* the capturing check on
    /// purpose: subscribing afterwards could miss a notification sent in
    /// between, and the caller would wait for the full timeout.
    ///
    /// Returns `false` if the session did not acknowledge in time.
    async fn release(&self, timeout: std::time::Duration) -> bool {
        let completed = self.completed.notified();
        tokio::pin!(completed);
        completed.as_mut().enable();
        if !self.is_capturing() {
            return true;
        }
        // Keep a permit if the session is still publishing its Begin event.
        self.requested.notify_one();
        tokio::time::timeout(timeout, completed).await.is_ok()
    }
}

#[allow(dead_code)]
pub struct LibeiInputCapture {
    syntra_input_capture: Pin<Box<InputCapture>>,
    capture_task: JoinHandle<Result<(), CaptureError>>,
    event_rx: Receiver<(Position, CaptureEvent)>,
    notify_capture: Sender<LibeiNotifyEvent>,
    release: ReleaseSignal,
    cancellation_token: CancellationToken,
    terminated: bool,
}

/// returns (start pos, end pos), inclusive
fn pos_to_barrier(r: &Region, pos: Position) -> (i32, i32, i32, i32) {
    barrier_line(
        (
            r.x_offset(),
            r.y_offset(),
            r.width() as i32,
            r.height() as i32,
        ),
        pos,
    )
}

#[cfg(test)]
fn pos_to_barrier_for_test(screen: (i32, i32, i32, i32), pos: Position) -> (i32, i32, i32, i32) {
    barrier_line(screen, pos)
}

fn barrier_line((x, y, w, h): (i32, i32, i32, i32), pos: Position) -> (i32, i32, i32, i32) {
    match pos {
        Position::Left => (x, y, x, y + h - 1),
        Position::Right => (x + w, y, x + w, y + h - 1),
        Position::Top => (x, y, x + w - 1, y),
        Position::Bottom => (x, y + h, x + w - 1, y + h),
    }
}

/// Ashpd does not expose fields
#[derive(Clone, Copy, Debug)]
struct ICBarrier {
    barrier_id: BarrierID,
    position: (i32, i32, i32, i32),
}

impl ICBarrier {
    fn new(barrier_id: BarrierID, position: (i32, i32, i32, i32)) -> Self {
        Self {
            barrier_id,
            position,
        }
    }
}

impl From<ICBarrier> for Barrier {
    fn from(barrier: ICBarrier) -> Self {
        Barrier::new(barrier.barrier_id, barrier.position)
    }
}

fn select_barriers(
    zones: &Zones,
    clients: &[Position],
    next_barrier_id: &mut NonZeroU32,
) -> (Vec<ICBarrier>, HashMap<BarrierID, Position>) {
    let mut pos_for_barrier = HashMap::new();
    let mut barriers: Vec<ICBarrier> = vec![];

    for pos in clients {
        let mut client_barriers = zones
            .regions()
            .iter()
            .map(|r| {
                let id = *next_barrier_id;
                *next_barrier_id = next_barrier_id
                    .checked_add(1)
                    .expect("barrier id out of range");
                let position = pos_to_barrier(r, *pos);
                pos_for_barrier.insert(id, *pos);
                ICBarrier::new(id, position)
            })
            .collect();
        barriers.append(&mut client_barriers);
    }
    (barriers, pos_for_barrier)
}

async fn update_barriers(
    syntra_input_capture: &InputCapture,
    session: &Session<InputCapture>,
    active_clients: &[Position],
    next_barrier_id: &mut NonZeroU32,
) -> Result<(Vec<ICBarrier>, HashMap<BarrierID, Position>), ashpd::Error> {
    let zones = syntra_input_capture
        .zones(session, Default::default())
        .await?
        .response()?;
    log::debug!("zones: {zones:?}");

    let (barriers, id_map) = select_barriers(&zones, active_clients, next_barrier_id);
    log::debug!("barriers: {barriers:?}");
    log::debug!("client for barrier id: {id_map:?}");

    let ashpd_barriers: Vec<Barrier> = barriers.iter().copied().map(|b| b.into()).collect();
    let response = syntra_input_capture
        .set_pointer_barriers(
            session,
            &ashpd_barriers,
            zones.zone_set(),
            Default::default(),
        )
        .await?;
    let response = response.response()?;
    log::debug!("{response:?}");
    Ok((barriers, id_map))
}

fn restore_token_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("syntra/input-capture.token"))
}

fn read_restore_token() -> Option<String> {
    let path = restore_token_path()?;
    match fs::read_to_string(path) {
        Ok(token) => {
            let token = token.trim().to_string();
            (!token.is_empty()).then_some(token)
        }
        Err(error) => {
            log::debug!("unable to read input-capture restore token: {error}");
            None
        }
    }
}

fn write_restore_token(token: &str) {
    let Some(path) = restore_token_path() else {
        log::debug!("configuration directory unavailable; restore token not saved");
        return;
    };
    let result = path
        .parent()
        .ok_or_else(|| io::Error::other("restore token path has no parent"))
        .and_then(fs::create_dir_all)
        .and_then(|()| fs::write(path, token));
    if let Err(error) = result {
        log::debug!("failed to persist input-capture restore token: {error}");
    }
}
async fn create_session(
    syntra_input_capture: &InputCapture,
) -> std::result::Result<(Session<InputCapture>, BitFlags<Capabilities>, bool), ashpd::Error> {
    log::debug!("creating input capture session");
    let session = match syntra_input_capture
        .create_session2(CreateSession2Options::default())
        .await
    {
        Ok(session) => session,
        Err(error) => {
            log::warn!("CreateSession2 unavailable, clipboard integration disabled: {error}");
            let options = ashpd::desktop::input_capture::CreateSessionOptions::default()
                .set_capabilities(
                    Capabilities::Keyboard | Capabilities::Pointer | Capabilities::Touchscreen,
                );
            let (session, capabilities) =
                syntra_input_capture.create_session(None, options).await?;
            return Ok((session, capabilities, false));
        }
    };
    let clipboard_requested =
        match Clipboard::with_connection(syntra_input_capture.connection().clone()).await {
            Ok(clipboard) => clipboard
                .request(&session, RequestClipboardOptions::default())
                .await
                .is_ok(),
            Err(error) => {
                log::warn!("clipboard portal unavailable: {error}");
                false
            }
        };
    let options = StartOptions::default()
        .set_capabilities(
            Capabilities::Keyboard | Capabilities::Pointer | Capabilities::Touchscreen,
        )
        .set_restore_token(read_restore_token())
        .set_persist_mode(PersistMode::ExplicitlyRevoked);
    let response = syntra_input_capture.start(&session, None, options).await?;
    let response = response.response()?;
    if let Some(token) = response.restore_token() {
        write_restore_token(token);
    }
    let clipboard_enabled = clipboard_requested && response.is_clipboard_enabled();
    log::info!("native clipboard portal enabled={clipboard_enabled}");
    Ok((session, response.capabilities(), clipboard_enabled))
}

async fn connect_to_eis(
    syntra_input_capture: &InputCapture,
    session: &Session<InputCapture>,
) -> Result<(ei::Context, Connection, EiConvertEventStream), CaptureError> {
    log::debug!("connect_to_eis");
    let fd = syntra_input_capture
        .connect_to_eis(session, Default::default())
        .await?;

    // create unix stream from fd
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true)?;

    // create ei context
    let context = ei::Context::new(stream)?;
    let (conn, event_stream) = context
        .handshake_tokio("io.syntra.Syntra", ContextType::Receiver)
        .await?;

    Ok((context, conn, event_stream))
}

async fn libei_event_handler(
    ei_event_stream: &mut EiConvertEventStream,
    context: &ei::Context,
    event_tx: Sender<(Position, CaptureEvent)>,
    release_session: Arc<Notify>,
    current_pos: Rc<Cell<Option<Position>>>,
) -> Result<(), CaptureError> {
    loop {
        let ei_event = ei_event_stream
            .next()
            .await
            .ok_or(CaptureError::EndOfStream)??;
        log::trace!("from ei: {ei_event:?}");
        let client = current_pos.get();
        handle_ei_event(ei_event, client, context, &event_tx, &release_session).await?;
    }
}

impl LibeiInputCapture {
    pub async fn new() -> std::result::Result<Self, LibeiCaptureCreationError> {
        // A cancelled initialization must disconnect its portal client, closing
        // pending dialogs/sessions without disturbing the other input backend.
        let connection = ashpd::zbus::Connection::session()
            .await
            .map_err(ashpd::Error::from)?;
        let syntra_input_capture = Box::pin(InputCapture::with_connection(connection).await?);
        let input_capture_ptr = syntra_input_capture.as_ref().get_ref() as *const InputCapture;
        let first_session = create_session(unsafe { &*input_capture_ptr }).await?;

        let (event_tx, event_rx) = mpsc::channel(1);
        let (notify_capture, notify_rx) = mpsc::channel(1);
        let release = ReleaseSignal::default();

        let cancellation_token = CancellationToken::new();

        let capture = do_capture(
            input_capture_ptr,
            notify_rx,
            release.clone(),
            first_session,
            event_tx,
            cancellation_token.clone(),
        );
        let capture_task = tokio::task::spawn_local(capture);

        let producer = Self {
            syntra_input_capture,
            event_rx,
            capture_task,
            notify_capture,
            release,
            cancellation_token,
            terminated: false,
        };

        Ok(producer)
    }
}

async fn do_capture(
    syntra_input_capture: *const InputCapture,
    mut capture_event: Receiver<LibeiNotifyEvent>,
    release: ReleaseSignal,
    session: (Session<InputCapture>, BitFlags<Capabilities>, bool),
    event_tx: Sender<(Position, CaptureEvent)>,
    cancellation_token: CancellationToken,
) -> Result<(), CaptureError> {
    let (mut session, _, clipboard_enabled) = session;
    // SAFETY: the backend retains the pinned portal until this task is joined.
    let portal = unsafe { &*syntra_input_capture };
    let result = async {
        // A peer/barrier change is not a new permission request. Keep the
        // portal session and EIS transport until this local backend terminates.
        let (context, _connection, mut events) = connect_to_eis(portal, &session).await?;
        let mut active_clients = Vec::new();
        let mut next_barrier_id = NonZeroU32::new(1).expect("id must be non-zero");
        let mut zones_changed = portal.receive_zones_changed().await?;
        loop {
            let cancel_session = CancellationToken::new();
            let cancel_update = CancellationToken::new();
            let mut update = None;
            let mut update_stream_closed = false;
            let wait_for_update = async {
                tokio::select! {
                    _ = cancellation_token.cancelled() => {},
                    _ = cancel_update.cancelled() => {},
                    event = zones_changed.next() => { update_stream_closed = event.is_none(); },
                    event = capture_event.recv() => { update_stream_closed = event.is_none(); update = event; },
                }
                cancel_session.cancel();
            };
            if active_clients.is_empty() {
                wait_for_update.await;
            } else {
                let capture = do_capture_session(
                    PortalSession {
                        portal,
                        session: &mut session,
                        clipboard_enabled,
                        context: &context,
                        events: &mut events,
                    },
                    &event_tx,
                    &active_clients,
                    &mut next_barrier_id,
                    &release,
                    (cancel_session.clone(), cancel_update.clone()),
                );
                let (result, ()) = tokio::join!(capture, wait_for_update);
                let disabled = portal.disable(&session, Default::default()).await;
                result?;
                disabled?;
            }
            while let Some(event) = update.take() {
                match event {
                    LibeiNotifyEvent::Create(position) => {
                        if !active_clients.contains(&position) {
                            active_clients.push(position);
                        }
                    }
                    LibeiNotifyEvent::Destroy(position) => {
                        active_clients.retain(|p| *p != position)
                    }
                }
                update = capture_event.try_recv().ok();
            }
            if update_stream_closed { return Err(CaptureError::EndOfStream); }
            if cancellation_token.is_cancelled() {
                break Ok(());
            }
        }
    }
    .await;
    release.complete();
    if let Err(error) = session.close().await {
        log::warn!("could not close input capture session: {error}");
    }
    result
}

const FILE_CLIPBOARD_MIME_TYPES: [&str; 2] = ["x-special/gnome-copied-files", "text/uri-list"];
const MAX_FILE_CLIPBOARD_BYTES: u64 = 1024 * 1024;

async fn read_portal_file_clipboard(
    clipboard: &Clipboard,
    session: &Session<InputCapture>,
    mime_types: &[String],
) -> Option<(String, Vec<u8>)> {
    let mime_type = FILE_CLIPBOARD_MIME_TYPES
        .iter()
        .find(|candidate| mime_types.iter().any(|mime| mime == **candidate))?;
    let fd = match clipboard.selection_read(session, mime_type).await {
        Ok(fd) => fd,
        Err(error) => {
            log::warn!("native clipboard read failed for {mime_type}: {error}");
            return None;
        }
    };
    let fd: std::os::fd::OwnedFd = fd.into();
    let file = std::fs::File::from(fd);
    let mut file = tokio::fs::File::from_std(file).take(MAX_FILE_CLIPBOARD_BYTES + 1);
    let mut data = Vec::new();
    if let Err(error) = file.read_to_end(&mut data).await {
        log::warn!("native clipboard data read failed: {error}");
        return None;
    }
    if data.len() as u64 > MAX_FILE_CLIPBOARD_BYTES {
        log::warn!("native clipboard selection exceeds size limit");
        return None;
    }
    Some(((*mime_type).to_string(), data))
}

/// The portal handle and the session opened on it, which are always used
/// together and share a lifetime.
struct PortalSession<'a> {
    portal: &'a InputCapture,
    session: &'a mut Session<InputCapture>,
    /// Whether this session also observes the clipboard.
    clipboard_enabled: bool,
    context: &'a ei::Context,
    events: &'a mut EiConvertEventStream,
}

async fn do_capture_session(
    portal_session: PortalSession<'_>,
    event_tx: &Sender<(Position, CaptureEvent)>,
    active_clients: &[Position],
    next_barrier_id: &mut NonZeroU32,
    release: &ReleaseSignal,
    cancel: (CancellationToken, CancellationToken),
) -> Result<(), CaptureError> {
    let PortalSession {
        portal: syntra_input_capture,
        session,
        clipboard_enabled,
        context,
        events: ei_event_stream,
    } = portal_session;
    let (cancel_session, cancel_update) = cancel;
    // current client
    let current_pos = Rc::new(Cell::new(None));

    // set barriers
    let (barriers, pos_for_barrier_id) = update_barriers(
        syntra_input_capture,
        session,
        active_clients,
        next_barrier_id,
    )
    .await?;

    // Subscribe before enabling: the compositor may activate immediately.
    let mut activated = syntra_input_capture.receive_activated().await?;
    let mut deactivated = syntra_input_capture.receive_deactivated().await?;
    log::debug!("enabling session");
    syntra_input_capture
        .enable(session, Default::default())
        .await?;

    // cancellation token to release session
    let release_session = Arc::new(Notify::new());

    // async event task
    let cancel_ei_handler = CancellationToken::new();
    let event_chan = event_tx.clone();
    let pos = current_pos.clone();
    let cancel_session_clone = cancel_session.clone();
    let release_session_clone = release_session.clone();
    let cancel_ei_handler_clone = cancel_ei_handler.clone();
    let ei_task = async move {
        tokio::select! {
            r = libei_event_handler(
                ei_event_stream,
                context,
                event_chan,
                release_session_clone,
                pos,
            ) => {
                log::debug!("libei exited: {r:?} cancelling session task");
                cancel_session_clone.cancel();
                r
            }
            _ = cancel_ei_handler_clone.cancelled() => Ok(()),
        }
    };

    let capture_session_task = async {
        let _stop_ei = cancel_ei_handler.clone().drop_guard();
        let _stop_updates = cancel_update.clone().drop_guard();
        let clipboard = if clipboard_enabled {
            Some(Clipboard::new().await?)
        } else {
            None
        };
        let mut clipboard_changed = match clipboard.as_ref() {
            Some(clipboard) => Some(Box::pin(
                clipboard
                    .receive_selection_owner_changed::<InputCapture>()
                    .await?,
            )),
            None => None,
        };
        let mut ei_devices_changed = false;
        loop {
            tokio::select! {
                activated = activated.next() => {
                    let activated = activated.ok_or(CaptureError::ActivationClosed)?;
                    log::debug!("activated: {activated:?}");

                    // get barrier id from activation
                    let barrier_id = match activated.barrier_id() {
                        Some(ActivatedBarrier::Barrier(id)) => id,
                        // workaround for KDE plasma not reporting barrier ids
                        Some(ActivatedBarrier::UnknownBarrier) | None => find_corresponding_client(&barriers, activated.cursor_position().expect("no cursor position reported by compositor")),
                    };

                    // find client corresponding to barrier
                    let pos = match pos_for_barrier_id.get(&barrier_id) {
                        Some(id) => *id,
                        None => {
                            // A signal for a retired barrier can remain queued
                            // across an update. Never release a different activation.
                            log::debug!("ignoring activation for retired barrier {barrier_id}");
                            continue;
                        },
                    };
                    current_pos.replace(Some(pos));
                    release.set_capturing(true);

                    // client entered => send event
                    event_tx.send((pos, CaptureEvent::Begin)).await.expect("no channel");

                    let mut compositor_released = false;
                    loop {
                        tokio::select! {
                            event = deactivated.next() => {
                                let Some(event) = event else { return Err(CaptureError::ActivationClosed) };
                                if event.session_handle() == activated.session_handle()
                                    && event.activation_id() == activated.activation_id() {
                                    compositor_released = true;
                                    break;
                                }
                            },
                            _ = release.requested() => {
                                log::debug!("release session requested");
                                break;
                            },
                            _ = release_session.notified() => {
                                log::debug!("ei devices changed");
                                ei_devices_changed = true;
                                break;
                            },
                            _ = cancel_session.cancelled() => {
                                log::debug!("session cancel requested");
                                break;
                            },
                            changed = async {
                                match clipboard_changed.as_mut() {
                                    Some(stream) => stream.as_mut().next().await,
                                    None => None,
                                }
                            }, if clipboard_changed.is_some() => {
                                let Some((_changed_session, changed)) = changed else {
                                    clipboard_changed = None;
                                    continue;
                                };
                                if changed.session_is_owner() == Some(true) {
                                    continue;
                                }
                                let Some(clipboard) = clipboard.as_ref() else {
                                    continue;
                                };
                                if let Some((mime_type, data)) = read_portal_file_clipboard(
                                    clipboard,
                                    session,
                                    changed.mime_types(),
                                ).await {
                                    log::info!(
                                        "native file clipboard detected: mime={mime_type} bytes={}",
                                        data.len()
                                    );
                                    event_tx
                                        .send((pos, CaptureEvent::Clipboard { mime_type, data }))
                                        .await
                                        .expect("no channel");
                                }
                            },
                        }
                    }

                    let release_result = if compositor_released {
                        Ok(())
                    } else {
                        let landing = release
                            .landing()
                            .and_then(|edge| landing_point(&barriers, &pos_for_barrier_id, edge, activated.cursor_position()?));
                        let result = release_capture(syntra_input_capture, session, &activated, pos, landing).await;
                        if let Err(error) = result {
                            // Release can race a compositor-initiated deactivation.
                            // Only an authoritative signal for this activation can
                            // turn the failed request into successful cleanup.
                            let confirmation = tokio::time::timeout(std::time::Duration::from_millis(250), async {
                                while let Some(event) = deactivated.next().await {
                                    if event.session_handle() == activated.session_handle()
                                        && event.activation_id() == activated.activation_id() {
                                        return true;
                                    }
                                }
                                false
                            }).await;
                            if matches!(confirmation, Ok(true)) { Ok(()) } else { Err(error) }
                        } else {
                            Ok(())
                        }
                    };
                    current_pos.replace(None);
                    release.complete();
                    release_result?;

                }
                _ = release.requested() => { /* capture release -> we are not capturing anyway, so ignore */
                    log::debug!("release session requested");
                },
                _ = release_session.notified() => { /* release session */
                    log::debug!("ei devices changed");
                    ei_devices_changed = true;
                },
                _ = cancel_session.cancelled() => { /* kill session notify */
                    log::debug!("session cancel requested");
                    break
                },
            }
            if ei_devices_changed {
                /* for whatever reason, GNOME seems to kill the session
                 * as soon as devices are added or removed, so we need
                 * to cancel */
                break;
            }
        }
        // cancel libei task
        log::debug!("session exited: killing libei task");
        cancel_ei_handler.cancel();
        Ok::<(), CaptureError>(())
    };

    let (a, b) = tokio::join!(ei_task, capture_session_task);

    cancel_update.cancel();
    // Unblock a concurrent release even if the portal/EIS session died
    // before it could process the explicit release request.
    release.complete();

    log::debug!("both session and ei task finished!");
    a?;
    b?;

    Ok(())
}

async fn release_capture(
    syntra_input_capture: &InputCapture,
    session: &Session<InputCapture>,
    activated: &Activated,
    current_pos: Position,
    landing: Option<(f64, f64)>,
) -> Result<(), CaptureError> {
    if let Some(activation_id) = activated.activation_id() {
        log::debug!("releasing input capture {activation_id}");
    }
    let (x, y) = activated
        .cursor_position()
        .expect("compositor did not report cursor position!");
    log::debug!("client entered @ ({x}, {y})");
    let (dx, dy) = match current_pos {
        // offset cursor position to not enter again immediately
        Position::Left => (1., 0.),
        Position::Right => (-1., 0.),
        Position::Top => (0., 1.),
        Position::Bottom => (0., -1.),
    };
    // release 1px to the right of the entered zone, unless the pointer comes
    // back through another edge
    let cursor_position = landing.unwrap_or((x as f64 + dx, y as f64 + dy));
    log::debug!("releasing the pointer at {cursor_position:?}");
    let release_options = ReleaseOptions::default()
        .set_activation_id(activated.activation_id())
        .set_cursor_position(Some(cursor_position));
    syntra_input_capture
        .release(session, release_options)
        .await?;
    Ok(())
}

/// A point just inside `landing`, level with `cursor`. Multi-monitor setups
/// have a barrier per output; the outermost one is the desktop's edge, and
/// among those the one nearest the cursor.
fn landing_point(
    barriers: &[ICBarrier],
    pos_for_barrier_id: &HashMap<BarrierID, Position>,
    landing: Position,
    cursor: (f32, f32),
) -> Option<(f64, f64)> {
    let (cx, cy) = (cursor.0 as f64, cursor.1 as f64);
    barriers
        .iter()
        .filter(|b| pos_for_barrier_id.get(&b.barrier_id) == Some(&landing))
        .map(|b| {
            let (x1, y1, x2, y2) = b.position;
            let (x1, y1, x2, y2) = (x1 as f64, y1 as f64, x2 as f64, y2 as f64);
            // Same insets as a normal release: barriers on the right and
            // bottom sit one pixel outside the screen.
            let point = match landing {
                Position::Left => (x1 + 1., cy.clamp(y1, y2)),
                Position::Right => (x1 - 2., cy.clamp(y1, y2)),
                Position::Top => (cx.clamp(x1, x2), y1 + 1.),
                Position::Bottom => (cx.clamp(x1, x2), y1 - 2.),
            };
            let outward = match landing {
                Position::Left => -x1,
                Position::Right => x1,
                Position::Top => -y1,
                Position::Bottom => y1,
            };
            let distance = (point.0 - cx).powi(2) + (point.1 - cy).powi(2);
            (point, outward, distance)
        })
        .max_by(|a, b| a.1.total_cmp(&b.1).then(b.2.total_cmp(&a.2)))
        .map(|(point, ..)| point)
}

fn find_corresponding_client(barriers: &[ICBarrier], pos: (f32, f32)) -> BarrierID {
    barriers
        .iter()
        .copied()
        .min_by_key(|b| {
            let (x1, y1, x2, y2) = b.position;
            let (x1, y1, x2, y2) = (x1 as f32, y1 as f32, x2 as f32, y2 as f32);
            distance_to_line(((x1, y1), (x2, y2)), pos) as i32
        })
        .expect("could not find barrier corresponding to client")
        .barrier_id
}

fn distance_to_line(line: ((f32, f32), (f32, f32)), p: (f32, f32)) -> f32 {
    let ((x1, y1), (x2, y2)) = line;
    let (x0, y0) = p;
    /*
     * we use the fact that for the triangle spanned by the line and p,
     * the height of the triangle is the desired distance and can be calculated by
     * h = 2A / b with b being the line_length and
     */
    let double_triangle_area = ((y2 - y1) * x0 - (x2 - x1) * y0 + x2 * y1 - y2 * x1).abs();
    let line_length = ((y2 - y1).powf(2.0) + (x2 - x1).powf(2.0)).sqrt();
    let distance = double_triangle_area / line_length;
    log::debug!("distance to line({line:?}, {p:?}) = {distance}");
    distance
}

async fn handle_ei_event(
    ei_event: EiEvent,
    current_client: Option<Position>,
    context: &ei::Context,
    event_tx: &Sender<(Position, CaptureEvent)>,
    release_session: &Notify,
) -> Result<(), CaptureError> {
    let all_capabilities = DeviceCapability::Pointer
        | DeviceCapability::PointerAbsolute
        | DeviceCapability::Keyboard
        | DeviceCapability::Touch
        | DeviceCapability::Scroll
        | DeviceCapability::Button;
    match ei_event {
        EiEvent::SeatAdded(s) => {
            s.seat.bind_capabilities(all_capabilities);
            context.flush().map_err(|e| io::Error::new(e.kind(), e))?;
        }
        EiEvent::SeatRemoved(_) => {
            log::debug!("releasing session: {ei_event:?}");
            release_session.notify_waiters();
        }
        /* EiEvent::DeviceAdded(_) | */
        EiEvent::DeviceRemoved(_) => {
            if *RESTART_SESSION_ON_DEVICE_CHANGE {
                log::debug!("releasing session: {ei_event:?}");
                release_session.notify_waiters();
            } else {
                log::debug!("ignoring device change: {ei_event:?}");
            }
        }
        EiEvent::DevicePaused(_) | EiEvent::DeviceResumed(_) => {}
        EiEvent::DeviceStartEmulating(_) => log::debug!("START EMULATING"),
        EiEvent::DeviceStopEmulating(_) => log::debug!("STOP EMULATING"),
        EiEvent::Disconnected(d) => {
            return Err(CaptureError::Disconnected(format!("{:?}", d.reason)));
        }
        _ => {
            if let Some(pos) = current_client {
                for event in Event::from_ei_event(ei_event) {
                    event_tx
                        .send((pos, CaptureEvent::Input(event)))
                        .await
                        .expect("no channel");
                }
            }
        }
    }
    Ok(())
}

#[async_trait(?Send)]
impl SyntraInputCapture for LibeiInputCapture {
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
        let _ = self
            .notify_capture
            .send(LibeiNotifyEvent::Create(pos))
            .await;
        Ok(())
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        let _ = self
            .notify_capture
            .send(LibeiNotifyEvent::Destroy(pos))
            .await;
        Ok(())
    }

    async fn release_at(&mut self, landing: Position) -> Result<bool, CaptureError> {
        self.release.set_landing(Some(landing));
        let result = self.release().await;
        self.release.set_landing(None);
        result.map(|()| true)
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        if !self.release.release(RELEASE_TIMEOUT).await {
            // A session that will not hand the pointer back would strand the
            // local desktop, so tear the portal session down instead.
            log::error!("input capture release timed out; terminating portal session");
            self.release.set_capturing(false);
            self.cancellation_token.cancel();
        }
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.cancellation_token.cancel();
        let task = &mut self.capture_task;
        log::debug!("waiting for capture to terminate...");
        let res = if !task.is_finished() {
            task.await.expect("libei task panic")
        } else {
            Ok(())
        };
        self.terminated = true;
        log::debug!("done!");
        res
    }
}

impl Drop for LibeiInputCapture {
    fn drop(&mut self) {
        if !self.terminated {
            /* this workaround is needed until async drop is stabilized */
            panic!("LibeiInputCapture dropped without being terminated!");
        }
    }
}

impl Stream for LibeiInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        match self.capture_task.poll_unpin(cx) {
            Poll::Ready(r) => match r.expect("failed to join") {
                Ok(()) => Poll::Ready(None),
                Err(e) => Poll::Ready(Some(Err(e))),
            },
            Poll::Pending => self.event_rx.poll_recv(cx).map(|e| e.map(Result::Ok)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ICBarrier, ReleaseSignal, landing_point, pos_to_barrier_for_test};
    use crate::Position;
    use std::collections::HashMap;
    use std::num::NonZeroU32;
    use std::time::Duration;

    /// Two 1920x1080 outputs side by side, barriers on both outer edges.
    fn layout() -> (Vec<ICBarrier>, HashMap<NonZeroU32, Position>) {
        let screens = [(0, 0, 1920, 1080), (1920, 0, 1920, 1080)];
        let mut barriers = vec![];
        let mut ids = HashMap::new();
        let mut next = 1u32;
        for pos in [Position::Left, Position::Right] {
            for screen in screens {
                let id = NonZeroU32::new(next).unwrap();
                next += 1;
                barriers.push(ICBarrier::new(id, pos_to_barrier_for_test(screen, pos)));
                ids.insert(id, pos);
            }
        }
        (barriers, ids)
    }

    /// The owner's pointer left through the left edge and comes home through
    /// the right one: it must land inside the right edge, at the same height.
    #[test]
    fn coming_home_through_another_edge_lands_on_that_edge() {
        let (barriers, ids) = layout();
        let exit_point = (0.0, 400.0);
        let (x, y) = landing_point(&barriers, &ids, Position::Right, exit_point).unwrap();
        assert_eq!(y, 400.0);
        assert!(x > 3800.0 && x < 3840.0, "inside the right edge, got {x}");
        let (x, _) = landing_point(&barriers, &ids, Position::Left, (3839.0, 400.0)).unwrap();
        assert!((0.0..10.0).contains(&x), "inside the left edge, got {x}");
    }

    /// No barrier on that edge (no device there): keep the normal release.
    #[test]
    fn no_barrier_on_the_landing_edge_keeps_the_normal_release() {
        let (barriers, ids) = layout();
        assert!(landing_point(&barriers, &ids, Position::Top, (10.0, 10.0)).is_none());
    }

    #[test]
    fn landing_is_cleared_by_default() {
        let signal = ReleaseSignal::default();
        assert_eq!(signal.landing(), None);
        signal.set_landing(Some(Position::Right));
        assert_eq!(signal.landing(), Some(Position::Right));
    }

    #[tokio::test]
    async fn release_request_survives_begin_delivery_before_session_waits() {
        let signal = ReleaseSignal::default();
        signal.set_capturing(true);
        let release = signal.release(Duration::from_secs(1));
        tokio::pin!(release);
        tokio::select! {
            biased;
            _ = &mut release => panic!("release completed without an acknowledgement"),
            _ = tokio::task::yield_now() => {}
        }
        tokio::time::timeout(Duration::from_millis(100), signal.requested())
            .await
            .expect("release request was lost before the session subscribed");
        signal.complete();
        assert!(release.await);
    }

    #[tokio::test]
    async fn previous_completion_cannot_acknowledge_a_new_capture() {
        let signal = ReleaseSignal::default();
        signal.complete();
        signal.set_capturing(true);
        assert!(!signal.release(Duration::from_millis(10)).await);
    }
}
