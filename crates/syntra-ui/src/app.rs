use std::cell::Cell;
use std::error::Error;
use std::io;
use std::net::IpAddr;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};

#[cfg(not(target_os = "android"))]
use slint::winit_030::{WinitWindowAccessor, winit};
use slint::{
    Color, ComponentHandle, Model, ModelRc, RenderingState, SharedString, Timer, TimerMode,
    VecModel,
};
use syntra_api::{
    ClipboardTransferDirection, ClipboardTransferState, ClipboardTransferStatus, FrontendEvent,
    FrontendRequest, FrontendRequestWriter, Position,
};

use crate::bridge::{EventSource, RequestSink, TransportError, UiIntent};
use crate::device_identity::client_identity_key;
use crate::localization::Localizer;
use crate::models::{AppViewState, TransportLifecycle, TransportLifecycleEvent};
use crate::platform::{
    CloseBehavior, PlatformAction, PlatformActions, PlatformCallbacks, PlatformTray,
};
use crate::settings::PresentationSettings;
use crate::updates::{ArtifactNaming, Preparation, RepositoryConfig, UpdateStatus, Updater};

// `include_modules!` expands to the Rust bindings the Slint compiler emits
// for `ui/app-window.slint`. Generated code cannot carry our doc comments,
// so the allow is scoped to it and re-exported unchanged.
#[allow(missing_docs)]
mod generated {
    slint::include_modules!();
}
pub use generated::*;

use syntra_api::paths::APPLICATION_ID;

/// Version reported in the about page and used for update comparisons.
const APPLICATION_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Refuse update artifacts larger than this; a runaway download must not fill the disk.
const MAX_UPDATE_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// Creates a window without connecting it to a transport.
pub fn create_app() -> Result<AppWindow, slint::PlatformError> {
    #[cfg(not(target_os = "android"))]
    crate::file_drop::configure_backend();
    let app = AppWindow::new()?;
    let avatar_cache =
        std::cell::RefCell::new(std::collections::VecDeque::<(slint::Image, slint::Image)>::new());
    app.global::<AvatarRenderer>().on_circular(move |source| {
        if let Some((_, rounded)) = avatar_cache
            .borrow()
            .iter()
            .find(|(original, _)| original == &source)
        {
            return rounded.clone();
        }
        let rounded = crate::avatar::circular(&source);
        let size = source.size();
        if size.width <= 256 && size.height <= 256 {
            let mut cache = avatar_cache.borrow_mut();
            if cache.len() >= 64 {
                cache.pop_front();
            }
            cache.push_back((source, rounded.clone()));
        }
        rounded
    });
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    slint::set_xdg_app_id(APPLICATION_ID)?;
    Ok(app)
}

#[cfg(not(target_os = "android"))]
fn acquire_frontend_instance(
    app: &AppWindow,
) -> Result<
    Option<crate::single_instance::PrimaryInstanceGuard>,
    crate::single_instance::SingleInstanceError,
> {
    use crate::single_instance::{InstanceOutcome, acquire};
    let weak = app.as_weak();
    match acquire(move || {
        let _ = weak.upgrade_in_event_loop(|app| {
            app.window().set_minimized(false);
            if let Err(error) = app.show() {
                log::error!("could not restore Syntra window: {error}");
            }
            native_window(&app, |window| window.focus_window());
        });
    })? {
        InstanceOutcome::Primary(guard) => Ok(Some(guard)),
        InstanceOutcome::Existing => Ok(None),
    }
}

fn schedule_launch_ready(app: &AppWindow) -> Option<Timer> {
    let display_time =
        std::time::Duration::from_millis(if app.global::<Theme>().get_motion_enabled() {
            1200
        } else {
            250
        });
    let weak = app.as_weak();
    let fired = Rc::new(Cell::new(false));
    let notifier_fired = Rc::clone(&fired);
    match app.window().set_rendering_notifier(move |state, _| {
        if matches!(state, RenderingState::AfterRendering) && !notifier_fired.replace(true) {
            let weak = weak.clone();
            Timer::single_shot(display_time, move || {
                if let Some(app) = weak.upgrade() {
                    app.set_launch_ready(true);
                }
            });
        }
    }) {
        Ok(()) => None,
        Err(_) => {
            let fallback_weak = app.as_weak();
            let timer = Timer::default();
            timer.start(TimerMode::SingleShot, display_time, move || {
                if let Some(app) = fallback_weak.upgrade() {
                    app.set_launch_ready(true);
                }
            });
            Some(timer)
        }
    }
}

/// Runs the dashboard, attaching to a daemon whenever one is reachable.
///
/// The window opens immediately and unconditionally. If no daemon answers,
/// the dashboard shows the disconnected state and keeps retrying in the
/// background, so the daemon may be started, stopped or upgraded at any time
/// while the dashboard stays open.
pub fn run() -> Result<(), Box<dyn Error>> {
    run_with_startup(StartupMode::Window)
}

/// How the dashboard presents itself when it starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupMode {
    /// Open the window immediately.
    Window,
    /// Start without a window, reachable from the tray.
    ///
    /// Falls back to opening the window when no tray is available, so
    /// `--background` can never produce a process the user cannot reach.
    Background,
}

/// Runs the dashboard, opening the window only in [`StartupMode::Window`].
pub fn run_with_startup(startup: StartupMode) -> Result<(), Box<dyn Error>> {
    let source = IpcEventSource::detached();
    let sink = IpcRequestSink(Arc::clone(&source.writer));
    run_with_transport_startup(source, sink, startup)
}

/// Runs the dashboard against an already-established daemon connection.
pub fn run_with_ipc(
    reader: syntra_api::FrontendEventReader,
    writer: FrontendRequestWriter,
) -> Result<(), Box<dyn Error>> {
    let writer = Arc::new(Mutex::new(Some(writer)));
    run_with_transport(
        IpcEventSource {
            reader: Mutex::new(Some(reader)),
            writer: Arc::clone(&writer),
        },
        IpcRequestSink(writer),
    )
}

/// Runs the complete frontend against any blocking event/request transport.
/// Android uses this entry with its in-process channels; desktop IPC is adapted by
/// [`run_with_ipc`]. Both paths deliberately share the same projection and callbacks.
pub fn run_with_transport<S, W>(source: S, sink: W) -> Result<(), Box<dyn Error>>
where
    S: EventSource + Send + 'static,
    W: RequestSink + Send + 'static,
{
    run_with_transport_startup(source, sink, StartupMode::Window)
}

/// As [`run_with_transport`], but able to start without showing the window.
pub fn run_with_transport_startup<S, W>(
    source: S,
    sink: W,
    startup: StartupMode,
) -> Result<(), Box<dyn Error>>
where
    S: EventSource + Send + 'static,
    W: RequestSink + Send + 'static,
{
    let app = create_app()?;
    #[cfg(not(target_os = "android"))]
    let Some(_instance) = acquire_frontend_instance(&app)? else {
        return Ok(());
    };
    let state = Arc::new(Mutex::new(AppViewState::default()));
    {
        let mut view = state
            .lock()
            .map_err(|_| "application state lock poisoned")?;
        view.reduce_transport(TransportLifecycleEvent::Reconnecting);
        view.capabilities = default_host_capabilities();
    }

    let settings = load_presentation_settings().unwrap_or_default();
    let settings = Arc::new(Mutex::new(settings));
    initialize_presentation(&app, &state, Arc::clone(&settings));
    project_locked_state(&app, &state, &settings);
    let diagnostic_store = Arc::new(Mutex::new(crate::diagnostics::DiagnosticStore::default()));
    // The receiver only raises a dirty flag. A timer does the projection at
    // a fixed rate, so a burst of log lines costs one model rebuild instead
    // of one per line — the previous behaviour blocked the UI thread hard
    // enough to stall resizing and drag-and-drop.
    let diagnostics_dirty = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let diagnostic_notify = {
        let dirty = Arc::clone(&diagnostics_dirty);
        move || dirty.store(true, std::sync::atomic::Ordering::Relaxed)
    };
    let _diagnostic_receiver = match crate::diagnostics::LiveDiagnosticReceiver::start(
        Arc::clone(&diagnostic_store),
        diagnostic_notify,
    ) {
        Ok(receiver) => Some(receiver),
        Err(error) => {
            if let Ok(mut store) = diagnostic_store.lock() {
                *store = crate::diagnostics::DiagnosticStore::unavailable(error.to_string());
            }
            project_live_diagnostics(&app, &diagnostic_store);
            None
        }
    };
    let _diagnostics_timer = {
        let weak = app.as_weak();
        let store = Arc::clone(&diagnostic_store);
        let dirty = Arc::clone(&diagnostics_dirty);
        let timer = Timer::default();
        timer.start(
            TimerMode::Repeated,
            std::time::Duration::from_millis(250),
            move || {
                if !dirty.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                if let Some(app) = weak.upgrade() {
                    project_live_diagnostics(&app, &store);
                }
            },
        );
        timer
    };

    // Latest state awaiting projection. Only the newest matters, so a burst
    // collapses to one rebuild rather than one per event.
    let pending_projection: Arc<Mutex<Option<(AppViewState, PresentationSettings)>>> =
        Arc::new(Mutex::new(None));
    let _projection_timer = {
        let weak = app.as_weak();
        let pending = Arc::clone(&pending_projection);
        let timer = Timer::default();
        timer.start(
            TimerMode::Repeated,
            // Fast enough to feel immediate, slow enough that a chatty peer
            // cannot monopolise the UI thread.
            std::time::Duration::from_millis(60),
            move || {
                let Some((snapshot, presentation)) =
                    pending.lock().ok().and_then(|mut slot| slot.take())
                else {
                    return;
                };
                if let Some(app) = weak.upgrade() {
                    project_app_state(&app, &snapshot, &presentation);
                }
            },
        );
        timer
    };
    let pending_projection = Arc::clone(&pending_projection);
    let (request_tx, request_rx) = mpsc::channel::<FrontendRequest>();
    let request_error_window = app.as_weak();
    let request_error_state = Arc::clone(&state);
    std::thread::spawn(move || {
        while let Ok(request) = request_rx.recv() {
            if sink.send(request).is_err() {
                show_error(
                    &request_error_window,
                    &request_error_state,
                    "The service connection was lost".into(),
                );
            }
        }
    });

    let event_window = app.as_weak();
    let event_state = Arc::clone(&state);
    let event_settings = Arc::clone(&settings);
    let history_requests = request_tx.clone();
    std::thread::spawn(move || {
        loop {
            while let Ok(event) = source.recv() {
                let received_history_page = matches!(&event, FrontendEvent::HistoryPage(_));
                let incoming_offer = matches!(&event, FrontendEvent::IncomingFileOffer(_));
                let settings_reply =
                    matches!(&event, FrontendEvent::FileReceiveSettingsChanged(_, _));
                let manual_error = matches!(&event, FrontendEvent::ManualTransferError(_));
                let manual_status_key = match &event {
                    FrontendEvent::ManualTransferStatus(status) => Some((
                        status.peer_fingerprint.clone(),
                        status.transfer_id.to_string(),
                    )),
                    _ => None,
                };
                let (snapshot, refresh, settings_saved) = {
                    let Ok(mut view) = event_state.lock() else {
                        return;
                    };
                    let settings_were_pending = view.file_receive_pending;
                    view.reduce(event);
                    view.reduce_transport(TransportLifecycleEvent::Resynchronized);
                    let refresh = view.next_history_refresh();
                    let settings_saved = settings_reply
                        && settings_were_pending
                        && view.file_receive_error.is_none();
                    (view.clone(), refresh, settings_saved)
                };
                if let Some(request) = refresh {
                    let _ = history_requests.send(request);
                }
                if received_history_page {
                    if let Some(page) = snapshot.history_page.as_ref() {
                        for record in &page.records {
                            let key = (
                                record.event_id.origin_device_id.clone(),
                                record.event_id.origin_sequence,
                            );
                            if record.kind == syntra_api::HistoryKind::Image
                                && !snapshot.history_images.contains_key(&key)
                            {
                                let _ = history_requests.send(FrontendRequest::GetHistoryImage(
                                    record.event_id.clone(),
                                ));
                            }
                        }
                    }
                }
                let presentation = match event_settings.lock() {
                    Ok(mut settings) => {
                        if let Err(error) = migrate_confirmed_profiles(&mut settings, &snapshot) {
                            show_error(
                                &event_window,
                                &event_state,
                                format!("Cannot migrate saved peer profile: {error}"),
                            );
                        }
                        settings.clone()
                    }
                    Err(_) => PresentationSettings::default(),
                };
                // Only the cheap, order-sensitive work runs per event. The
                // full projection rebuilds every model and is handed to a
                // timer instead: a burst of events used to rebuild the whole
                // interface once per event, which is what made History and
                // the window feel like they were reloading constantly.
                if let Ok(mut slot) = pending_projection.lock() {
                    *slot = Some((snapshot, presentation));
                }
                let _ = event_window.upgrade_in_event_loop(move |app| {
                    let global = app.global::<AppState>();
                    if settings_saved {
                        global.set_file_receive_dirty(false);
                    }
                    if manual_error
                        || manual_status_key.as_ref().is_some_and(|(peer, id)| {
                            global.get_manual_offer_peer().as_str() == peer
                                && global.get_manual_offer_id().as_str() == id
                        })
                    {
                        global.set_manual_offer_busy(false);
                    }
                    if incoming_offer && global.get_manual_offer_visible() {
                        let _ = app.show();
                    }
                });
            }
            let snapshot = {
                let Ok(mut view) = event_state.lock() else {
                    return;
                };
                view.reduce_transport(TransportLifecycleEvent::DaemonUnavailable);
                view.diagnostics.last_error = Some("The service connection was lost".into());
                view.clone()
            };
            let presentation = event_settings
                .lock()
                .ok()
                .map(|value| value.clone())
                .unwrap_or_default();
            let _ = event_window.upgrade_in_event_loop(move |app| {
                project_app_state(&app, &snapshot, &presentation)
            });
            if !source.reconnect() {
                break;
            }
        }
    });

    let platform = desktop_platform();
    bind_app_state_callbacks(
        &app,
        request_tx.clone(),
        Arc::clone(&state),
        Arc::clone(&settings),
        platform.clone(),
        Arc::clone(&diagnostic_store),
    );
    let tray = platform
        .as_ref()
        .and_then(|platform| start_platform_tray(&app, Arc::clone(platform), &state));
    bind_window_callbacks(
        &app,
        Arc::clone(&settings),
        Arc::clone(&state),
        tray.is_some(),
    );
    install_close_behavior(&app, platform, tray.is_some());
    {
        let tx = request_tx.clone();
        let weak = app.as_weak();
        let settings = Arc::clone(&settings);
        let state = Arc::clone(&state);
        let last_sent = std::cell::RefCell::new(None);
        app.on_local_profile_changed(move || {
            let key = settings.lock().ok().map(|saved| {
                (
                    saved.local_device.friendly_name.clone(),
                    saved.local_device.image_path.clone(),
                )
            });
            if key.is_some() && *last_sent.borrow() != key {
                last_sent.replace(key);
                publish_local_profile(&tx, &weak, &settings, &state);
            }
        });
    }

    send_request(&request_tx, &app.as_weak(), &state, FrontendRequest::Sync);
    send_request(
        &request_tx,
        &app.as_weak(),
        &state,
        FrontendRequest::Enumerate(),
    );
    send_request(
        &request_tx,
        &app.as_weak(),
        &state,
        FrontendRequest::QueryPlugins,
    );
    send_request(
        &request_tx,
        &app.as_weak(),
        &state,
        FrontendRequest::QueryDaemonInfo,
    );
    // Uptime is a snapshot, so without a periodic re-query it stays frozen at
    // whatever it was when the client attached.
    let _daemon_info_timer = {
        let requests = request_tx.clone();
        let timer = Timer::default();
        timer.start(
            TimerMode::Repeated,
            std::time::Duration::from_secs(20),
            move || {
                let _ = requests.send(FrontendRequest::QueryDaemonInfo);
            },
        );
        timer
    };
    app.invoke_local_profile_changed();
    let _launch_ready_timer = schedule_launch_ready(&app);
    // Without a tray there is no way back to a hidden window, so a
    // background start would strand the user; open the window instead.
    if startup == StartupMode::Window || tray.is_none() {
        app.show()?;
    } else {
        log::info!("started in the background; use the tray icon to open Syntra");
    }
    #[cfg(not(target_os = "android"))]
    let _file_drop_guard = crate::manual_ui::bind(&app, request_tx.clone(), Arc::clone(&state));
    slint::run_event_loop_until_quit()?;
    app.hide()?;
    drop(tray);
    Ok(())
}

/// Daemon connection that may be absent.
///
/// A dashboard must outlive the daemon, so the reader is optional and
/// [`EventSource::reconnect`] is the single place that establishes or
/// re-establishes the stream.
struct IpcEventSource {
    reader: Mutex<Option<syntra_api::FrontendEventReader>>,
    writer: Arc<Mutex<Option<FrontendRequestWriter>>>,
}

/// Delay before the first reconnection attempt.
const RECONNECT_MIN: std::time::Duration = std::time::Duration::from_millis(250);
/// Upper bound on the reconnection backoff.
const RECONNECT_MAX: std::time::Duration = std::time::Duration::from_secs(5);

impl IpcEventSource {
    /// Creates a source with no connection yet; the dashboard opens regardless.
    fn detached() -> Self {
        Self {
            reader: Mutex::new(None),
            writer: Arc::new(Mutex::new(None)),
        }
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
            delay = (delay * 2).min(RECONNECT_MAX);
        }
    }
}

struct IpcRequestSink(Arc<Mutex<Option<FrontendRequestWriter>>>);

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

fn default_host_capabilities() -> crate::models::PlatformCapabilities {
    #[cfg(target_os = "android")]
    {
        crate::lifecycle::HostCapabilities::android_default().into()
    }
    #[cfg(not(target_os = "android"))]
    {
        crate::models::PlatformCapabilities {
            capture: true,
            emulation: true,
            clipboard_files: true,
        }
    }
}

fn desktop_platform() -> Option<Arc<dyn PlatformActions>> {
    #[cfg(target_os = "android")]
    {
        None
    }
    #[cfg(not(target_os = "android"))]
    {
        Some(Arc::from(crate::platform::platform()))
    }
}

fn initialize_presentation(
    app: &AppWindow,
    state: &Arc<Mutex<AppViewState>>,
    settings: Arc<Mutex<PresentationSettings>>,
) {
    let initial = settings
        .lock()
        .map(|value| value.clone())
        .unwrap_or_default();
    app.set_sidebar_expanded(initial.sidebar_open);
    project_local_identity(app, &initial);
    apply_appearance(app, &initial);
    install_localizer(app, &initial.locale, settings, Arc::clone(state));
    app.global::<AppState>()
        .set_locale(app.global::<Translations>().get_locale());
}

fn project_local_identity(app: &AppWindow, settings: &PresentationSettings) {
    let name = if settings.local_device.friendly_name.trim().is_empty() {
        hostname::get()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "Syntra".into())
    } else {
        settings.local_device.friendly_name.clone()
    };
    app.set_local_device_name(name.into());
    let image = (!settings.local_device.image_path.is_empty())
        .then(|| {
            slint::Image::load_from_path(std::path::Path::new(&settings.local_device.image_path))
                .ok()
        })
        .flatten();
    app.set_has_local_device_image(image.is_some());
    let image = image.unwrap_or_default();
    app.set_local_device_image(image);
}

/// Migrate legacy presentation keys only after the daemon confirms the peer's
/// authenticated certificate. All subsequent edits use the certificate key.
fn migrate_confirmed_profiles(
    settings: &mut PresentationSettings,
    view: &AppViewState,
) -> io::Result<()> {
    let mut store = crate::device_identity::IdentityStore::new(settings.clone());
    for client in view.clients.values() {
        if let Some(fingerprint) = view.client_fingerprints.get(&client.handle) {
            let legacy = client_identity_key(
                client.hostname.as_deref(),
                &client.fixed_ips,
                client.port,
                client.position,
            );
            store.migrate_key(&legacy, &format!("cert:{fingerprint}"))?;
        }
    }
    *settings = store.settings().clone();
    Ok(())
}

fn local_device_profile(
    settings: &PresentationSettings,
) -> Result<syntra_api::DeviceProfile, String> {
    let display_name = if settings.local_device.friendly_name.trim().is_empty() {
        hostname::get()
            .map_err(|error| error.to_string())?
            .to_string_lossy()
            .into_owned()
    } else {
        settings.local_device.friendly_name.clone()
    };
    let avatar = if settings.local_device.image_path.is_empty() {
        None
    } else {
        let image =
            slint::Image::load_from_path(std::path::Path::new(&settings.local_device.image_path))
                .map_err(|error| format!("Cannot load saved device avatar: {error}"))?;
        let pixels = image.to_rgba8().ok_or("Cannot read device avatar pixels")?;
        let image =
            image::RgbaImage::from_raw(pixels.width(), pixels.height(), pixels.as_bytes().to_vec())
                .ok_or("Invalid device avatar pixels")?;
        let scaled = if image.width() > 128 || image.height() > 128 {
            image::imageops::thumbnail(&image, 128, 128)
        } else {
            image
        };
        Some(syntra_api::PeerAvatar {
            width: scaled.width(),
            height: scaled.height(),
            rgba: scaled.into_raw(),
        })
    };
    let profile = syntra_api::DeviceProfile {
        display_name,
        avatar,
    };
    profile.validate().map_err(str::to_string)?;
    Ok(profile)
}

fn publish_local_profile(
    tx: &mpsc::Sender<FrontendRequest>,
    weak: &slint::Weak<AppWindow>,
    settings: &Arc<Mutex<PresentationSettings>>,
    state: &Arc<Mutex<AppViewState>>,
) {
    let profile = settings
        .lock()
        .map_err(|_| "Device profile settings lock poisoned".to_string())
        .and_then(|settings| local_device_profile(&settings));
    match profile {
        Ok(profile) => send_request(
            tx,
            weak,
            state,
            FrontendRequest::SetLocalDeviceProfile(profile),
        ),
        Err(error) => show_error(weak, state, error),
    }
}
fn hex_color(value: &str) -> Color {
    let raw = value.strip_prefix('#').unwrap_or(value);
    if raw.len() == 6 {
        if let Ok(rgb) = u32::from_str_radix(raw, 16) {
            return Color::from_rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
        }
    }
    Color::from_rgb_u8(113, 131, 85)
}
fn apply_appearance(app: &AppWindow, settings: &PresentationSettings) {
    let theme = app.global::<Theme>();
    let dark = matches!(settings.mode, crate::settings::BaseMode::Black);
    let (
        _background,
        surface,
        elevated,
        header,
        sidebar,
        border,
        text,
        muted,
        selection,
        hover,
        pressed,
    ) = match (settings.mode, settings.palette) {
        (crate::settings::BaseMode::White, crate::settings::Palette::Forest) => (
            "#ffffff", "#f5faf6", "#edf5ef", "#f8fcf9", "#eef7f0", "#c5d8c9", "#17261b", "#52695a",
            "#d8eadc", "#e7f2e9", "#dbe9de",
        ),
        (crate::settings::BaseMode::White, crate::settings::Palette::Ocean) => (
            "#ffffff", "#f4f9fc", "#eaf3f8", "#f8fbfd", "#ecf6fb", "#bfd3df", "#142630", "#4b6674",
            "#d3e8f3", "#e3f0f6", "#d5e7f0",
        ),
        (crate::settings::BaseMode::White, crate::settings::Palette::Slate) => (
            "#ffffff", "#f6f8fa", "#edf1f5", "#fafbfc", "#eef2f6", "#c7d0d9", "#1b232c", "#56636f",
            "#dce3ea", "#e8edf2", "#dce3e9",
        ),
        (crate::settings::BaseMode::White, crate::settings::Palette::Sand) => (
            "#ffffff", "#fbf8f1", "#f4eddf", "#fdfbf7", "#f7f0e3", "#ddcfb7", "#2a2115", "#74644d",
            "#eee0c6", "#f3ead9", "#e9dcc6",
        ),
        (crate::settings::BaseMode::White, crate::settings::Palette::Graphite) => (
            "#ffffff", "#f7f7f7", "#eeeeee", "#fbfbfb", "#f1f1f1", "#cccccc", "#202020", "#626262",
            "#dfdfdf", "#e9e9e9", "#dddddd",
        ),
        (crate::settings::BaseMode::White, crate::settings::Palette::Aubergine) => (
            "#ffffff", "#faf6fa", "#f3ebf3", "#fcf9fc", "#f7eef7", "#dac6da", "#29182a", "#705671",
            "#ead8ea", "#f1e5f1", "#e7d8e7",
        ),
        (crate::settings::BaseMode::White, crate::settings::Palette::Copper) => (
            "#ffffff", "#fbf7f4", "#f4ece6", "#fdfaf8", "#f8efe9", "#decbbf", "#2b1d16", "#755d50",
            "#ecdccf", "#f3e8e1", "#e9dacf",
        ),
        (crate::settings::BaseMode::White, crate::settings::Palette::Rose) => (
            "#ffffff", "#fcf6f8", "#f6eaf0", "#fef9fb", "#faedf2", "#e0c4cf", "#2d1821", "#795663",
            "#f0d7e0", "#f6e5eb", "#ecd6de",
        ),
        (crate::settings::BaseMode::White, crate::settings::Palette::Ice) => (
            "#ffffff", "#f3fafc", "#e8f4f7", "#f8fcfd", "#eaf7f9", "#bdd8de", "#14272c", "#4b6970",
            "#d0e9ee", "#e0f1f4", "#d2e7eb",
        ),
        (crate::settings::BaseMode::White, crate::settings::Palette::Army) => (
            "#ffffff", "#f7f8f2", "#eff1e6", "#fbfbf8", "#f2f4e9", "#cfd3bb", "#222617", "#646a4f",
            "#e1e5cf", "#ebeddf", "#dfe3d2",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Forest) => (
            "#07100a", "#101b13", "#19261d", "#0b160e", "#0d1810", "#304536", "#edf6ef", "#a9bbaa",
            "#274331", "#1e3024", "#294034",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Ocean) => (
            "#061018", "#0e1b24", "#172833", "#091620", "#0b1922", "#2c4655", "#edf6fa", "#a4b9c4",
            "#203f50", "#1b3240", "#254353",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Slate) => (
            "#090d12", "#121920", "#1c252e", "#0d1319", "#10171e", "#34424e", "#f0f4f7", "#a9b4bd",
            "#2b3945", "#222d36", "#2d3a45",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Sand) => (
            "#141009", "#201a11", "#2d2518", "#19140c", "#1d170e", "#4b3d27", "#faf3e7", "#c5b69d",
            "#47391f", "#382e1e", "#493b26",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Graphite) => (
            "#090909", "#151515", "#212121", "#0e0e0e", "#121212", "#3b3b3b", "#f3f3f3", "#b5b5b5",
            "#343434", "#292929", "#373737",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Aubergine) => (
            "#120912", "#201220", "#2e1b2f", "#170d17", "#1c101c", "#49324a", "#f8eef8", "#bea9bf",
            "#432a44", "#352238", "#472e49",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Copper) => (
            "#150c08", "#22150f", "#301f17", "#1a100b", "#1e120d", "#4e382d", "#faefe9", "#c5aea2",
            "#493024", "#39261d", "#4a3227",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Rose) => (
            "#15090e", "#231219", "#321c25", "#1b0d13", "#1f1017", "#50323d", "#fbeef3", "#c8a8b4",
            "#4c2936", "#3c202b", "#4e2c39",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Ice) => (
            "#071217", "#0f1e24", "#182b33", "#0a181d", "#0c1b21", "#2d4852", "#edf8fa", "#a5bdc3",
            "#21434d", "#1b353e", "#264651",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Army) => (
            "#090b07", "#14180f", "#20261a", "#0e110a", "#11150d", "#3a4430", "#f1f4eb", "#b1b9a4",
            "#333e28", "#283121", "#36422b",
        ),
    };
    let (success, warning, danger) = if dark {
        ("#8bd5a5", "#f5c36b", "#f28b82")
    } else {
        ("#247a45", "#8a5a00", "#b83a3a")
    };
    theme.set_background(if dark {
        Color::from_rgb_u8(0, 0, 0)
    } else {
        Color::from_rgb_u8(255, 255, 255)
    });
    theme.set_surface(hex_color(surface));
    theme.set_elevated_surface(hex_color(elevated));
    theme.set_header(hex_color(header));
    theme.set_sidebar(hex_color(sidebar));
    theme.set_border(hex_color(border));
    theme.set_text(hex_color(text));
    theme.set_muted_text(hex_color(muted));
    theme.set_selection(hex_color(selection));
    theme.set_hover_surface(hex_color(hover));
    theme.set_pressed_surface(hex_color(pressed));
    theme.set_success(hex_color(success));
    theme.set_warning(hex_color(warning));
    theme.set_danger(hex_color(danger));
    theme.set_base_mode(if dark { "black".into() } else { "white".into() });
    theme.set_palette(
        format!("{:?}", settings.palette)
            .to_ascii_lowercase()
            .into(),
    );
    theme.set_accent(hex_color(&settings.accent));
    theme.set_titlebar(if settings.titlebar_color.is_empty() {
        hex_color(header)
    } else {
        hex_color(&settings.titlebar_color)
    });
    theme.set_motion_enabled(!settings.reduced_motion);
    let compact = matches!(settings.density, crate::settings::Density::Compact);
    let zoom = settings.zoom_percent as f32 / 100.0;
    theme.set_density_scale(if compact { 0.85 } else { 1.0 });
    theme.set_spacing((if compact { 12.0 } else { 16.0 }) * zoom);
    theme.set_small_spacing((if compact { 6.0 } else { 8.0 }) * zoom);
    theme.set_large_spacing((if compact { 24.0 } else { 32.0 }) * zoom);
    theme.set_content_gap(24.0 * zoom);
    theme.set_card_padding((if compact { 16.0 } else { 20.0 }) * zoom);
    theme.set_control_height((if compact { 40.0 } else { 44.0 }) * zoom);
    theme.set_row_height((if compact { 56.0 } else { 64.0 }) * zoom);
    theme.set_body_size(16.0 * zoom);
    theme.set_caption_size(14.0 * zoom);
    theme.set_title_size(26.0 * zoom);
    theme.set_sidebar_expanded_width(248.0 * zoom);
    theme.set_sidebar_collapsed_width(68.0 * zoom);
    theme.set_nav_item_height(48.0 * zoom);
    let state = app.global::<AppState>();
    state.set_zoom_percent(settings.zoom_percent.into());
    state.set_base_mode(if dark { "black".into() } else { "white".into() });
    state.set_palette(
        format!("{:?}", settings.palette)
            .to_ascii_lowercase()
            .into(),
    );
    state.set_accent(settings.accent.clone().into());
    state.set_titlebar_color(settings.titlebar_color.clone().into());
    state.set_density(if compact {
        "compact".into()
    } else {
        "comfortable".into()
    });
    state.set_reduced_motion(settings.reduced_motion);
}

fn install_localizer(
    app: &AppWindow,
    locale: &str,
    settings: Arc<Mutex<PresentationSettings>>,
    state: Arc<Mutex<AppViewState>>,
) {
    let mut localizer = match Localizer::new(locale) {
        Ok(localizer) => localizer,
        Err(error) => {
            show_error(
                &app.as_weak(),
                &state,
                format!("Unable to load translations: {error}"),
            );
            Localizer::new("en-US").expect("embedded fallback translations must be valid")
        }
    };
    let dictionary_directory = crate::settings::presentation_settings_path()
        .and_then(|path| path.parent().map(|parent| parent.join("locales")));
    if let Some(directory) = &dictionary_directory {
        if let Err(error) = localizer.load_directory(directory) {
            show_error(
                &app.as_weak(),
                &state,
                format!("Unable to load custom dictionaries: {error}"),
            );
        }
    }
    app.global::<AppState>()
        .set_locale_options(ModelRc::new(VecModel::from(
            localizer
                .available_locales()
                .into_iter()
                .map(SharedString::from)
                .collect::<Vec<_>>(),
        )));
    app.global::<AppState>()
        .set_language_import_supported(cfg!(not(target_os = "android")));
    let snapshot = localizer.snapshot();
    let localizer = Arc::new(Mutex::new(localizer));
    let translations = app.global::<Translations>();
    translations.set_locale(snapshot.locale.into());
    translations.set_revision(snapshot.revision.min(i32::MAX as u64) as i32);
    translations.on_translate({
        let localizer = Arc::clone(&localizer);
        move |key, _revision| {
            localizer
                .lock()
                .map(|value| value.format(key.as_str(), None).into())
                .unwrap_or_else(|_| key)
        }
    });

    let global = app.global::<AppState>();
    let weak = app.as_weak();
    #[cfg(not(target_os = "android"))]
    {
        let weak = weak.clone();
        let state = Arc::clone(&state);
        let localizer = Arc::clone(&localizer);
        global.on_import_language(move || {
            let weak = weak.clone();
            let state = Arc::clone(&state);
            let localizer = Arc::clone(&localizer);
            let directory = dictionary_directory.clone();
            let _ = slint::spawn_local(async move {
                let Some(file) = rfd::AsyncFileDialog::new()
                    .add_filter("Fluent (.ftl)", &["ftl"])
                    .pick_file()
                    .await
                else {
                    return;
                };
                let path = file.path().to_owned();
                std::thread::spawn(move || {
                    let result = (|| {
                        let directory =
                            directory.ok_or("Custom dictionary directory is unavailable")?;
                        let mut localizer =
                            localizer.lock().map_err(|_| "Localization lock poisoned")?;
                        let locale = localizer.import_file(&path, &directory)?;
                        Ok::<_, String>((locale, localizer.available_locales()))
                    })();
                    match result {
                        Ok((locale, available)) => {
                            let _ = weak.upgrade_in_event_loop(move |app| {
                                let global = app.global::<AppState>();
                                global.set_locale_options(ModelRc::new(VecModel::from(
                                    available
                                        .into_iter()
                                        .map(SharedString::from)
                                        .collect::<Vec<_>>(),
                                )));
                                global.invoke_change_locale(locale.into());
                            });
                        }
                        Err(error) => show_error(&weak, &state, error),
                    }
                });
            });
        });
    }
    global.on_change_locale(move |locale| {
        let result = localizer
            .lock()
            .map_err(|_| "localization state lock poisoned".to_string())
            .and_then(|mut value| {
                value.set_locale(locale.as_str())?;
                Ok(value.snapshot())
            });
        match result {
            Ok(snapshot) => {
                if let Ok(mut view) = state.lock() {
                    view.preferences.locale = snapshot.locale.clone();
                }
                let persist_result = settings
                    .lock()
                    .map_err(|_| "settings lock poisoned".to_string())
                    .and_then(|mut value| {
                        value.locale = snapshot.locale.clone();
                        save_presentation_settings(&value).map_err(|error| error.to_string())
                    });
                if let Some(app) = weak.upgrade() {
                    let translations = app.global::<Translations>();
                    translations.set_locale(snapshot.locale.clone().into());
                    translations.set_revision(snapshot.revision.min(i32::MAX as u64) as i32);
                    app.global::<AppState>().set_locale(snapshot.locale.into());
                }
                if let Err(error) = persist_result {
                    show_error(
                        &weak,
                        &state,
                        format!("Unable to save language preference: {error}"),
                    );
                }
            }
            Err(error) => show_error(&weak, &state, format!("Unable to change language: {error}")),
        }
    });
}
/// Rows handed to the diagnostics view at once.
///
/// The list is a scrolling tail, so projecting more than a screenful plus
/// scrollback buys nothing and costs a full model rebuild per update.
const DIAGNOSTIC_VISIBLE_ROWS: usize = 400;

thread_local! {
    /// The live diagnostics model, kept across projections.
    ///
    /// Replacing the model on every update discards the view's row state and
    /// forces Slint to rebuild the entire list. Holding one model and
    /// appending to it lets a burst of log lines cost only the rows that are
    /// actually new.
    static DIAGNOSTIC_MODEL: Rc<VecModel<DiagnosticEntry>> = Rc::new(VecModel::default());
    /// `(appended, revision)` observed at the last projection.
    static DIAGNOSTIC_CURSOR: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

fn project_live_diagnostics(
    app: &AppWindow,
    store: &Arc<Mutex<crate::diagnostics::DiagnosticStore>>,
) {
    let Ok(store) = store.lock() else { return };
    let global = app.global::<AppState>();

    let (previous_appended, previous_revision) = DIAGNOSTIC_CURSOR.with(|cursor| cursor.get());
    let appended = store.appended();
    let revision = store.revision();
    // A filter change alters which records qualify, so appending would be
    // wrong; only an unchanged filter and a pure growth can be appended.
    let rebuild = revision != previous_revision || appended < previous_appended;
    let new_records = appended.saturating_sub(previous_appended) as usize;
    if !rebuild && new_records == 0 {
        return;
    }

    let rows = store
        .filtered(DIAGNOSTIC_VISIBLE_ROWS)
        .into_iter()
        .map(|entry| DiagnosticEntry {
            timestamp: entry.timestamp.into(),
            level: entry.level.into(),
            stage: entry.stage.into(),
            direction: entry.direction.into(),
            correlation: entry.correlation.into(),
            message: entry.message.into(),
        })
        .collect::<Vec<_>>();

    DIAGNOSTIC_MODEL.with(|model| {
        if rebuild || new_records >= rows.len() {
            // Nothing in common with what is displayed; start again.
            model.set_vec(rows.clone());
        } else {
            // Append the tail, then trim the head back to the window so the
            // model never grows past what the view can show.
            for row in &rows[rows.len() - new_records..] {
                model.push(row.clone());
            }
            while model.row_count() > DIAGNOSTIC_VISIBLE_ROWS {
                model.remove(0);
            }
        }
        global.set_diagnostics(ModelRc::from(model.clone()));
    });
    DIAGNOSTIC_CURSOR.with(|cursor| cursor.set((appended, revision)));

    // Approximate the width from byte length rather than scanning every
    // character of every message; the value only sizes a scroll area.
    let message_columns = rows
        .iter()
        .map(|entry| entry.message.len())
        .max()
        .unwrap_or(80)
        // One pathological line must not widen the table for every row.
        .clamp(80, 400) as i32;
    global.set_diagnostics_message_columns(message_columns);
    global.set_diagnostics_paused(store.filter.paused);
    global.set_diagnostics_error(
        store
            .last_error
            .clone()
            .unwrap_or_else(|| store.stream_status.clone())
            .into(),
    );
}

fn project_locked_state(
    app: &AppWindow,
    state: &Arc<Mutex<AppViewState>>,
    settings: &Arc<Mutex<PresentationSettings>>,
) {
    if let Ok(view) = state.lock() {
        if let Ok(presentation) = settings.lock() {
            project_local_identity(app, &presentation);
            project_app_state(app, &view, &presentation);
        }
    }
}

pub(crate) fn device_presentation(
    app: &AppWindow,
    state: &AppViewState,
    origin: &str,
    recorded_label: Option<&str>,
) -> (SharedString, Option<slint::Image>) {
    let local = state.public_key_fingerprint.as_deref() == Some(origin);
    let client = app
        .global::<AppState>()
        .get_clients()
        .iter()
        .find(|client| client.fingerprint.as_str() == origin);
    let profile = state.peer_profiles.get(origin);
    let label = local
        .then(|| app.get_local_device_name().to_string())
        .or_else(|| {
            client
                .as_ref()
                .map(|client| client.display_name.to_string())
        })
        .filter(|label| !label.trim().is_empty() && label != origin)
        .or_else(|| {
            profile
                .map(|profile| profile.display_name.clone())
                .filter(|label| !label.trim().is_empty() && label != origin)
        })
        .or_else(|| {
            state
                .authorization
                .get(origin)
                .cloned()
                .filter(|label| !label.trim().is_empty() && label != origin)
        })
        .or_else(|| {
            recorded_label
                .filter(|label| !label.trim().is_empty() && *label != origin)
                .map(str::to_owned)
        })
        .map(Into::into)
        .unwrap_or_else(|| {
            let translations = app.global::<Translations>();
            translations
                .invoke_translate("history-unknown-device".into(), translations.get_revision())
        });
    let image = if local {
        app.get_has_local_device_image()
            .then(|| app.get_local_device_image())
    } else {
        client
            .filter(|client| client.has_custom_image)
            .map(|client| client.custom_image)
            .or_else(|| {
                profile
                    .and_then(|profile| profile.avatar.as_ref())
                    .map(|avatar| {
                        slint::Image::from_rgba8(
                            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                &avatar.rgba,
                                avatar.width,
                                avatar.height,
                            ),
                        )
                    })
            })
    };
    (label, image)
}

/// Key identifying one history record.
type HistoryKey = (String, u64);

thread_local! {
    /// Decoded thumbnails, keyed by record.
    ///
    /// Converting an image means copying its whole RGBA buffer. Doing that
    /// for every record on every projection made opening History stutter,
    /// even though the pixels never change: a record is immutable once
    /// received. Confined to the UI thread, which is the only place that
    /// projects.
    static THUMBNAIL_CACHE: std::cell::RefCell<std::collections::HashMap<HistoryKey, slint::Image>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Bound on cached thumbnails, so scrolling a long history cannot grow
/// without limit. Cleared wholesale rather than evicted one by one: the cost
/// is one re-decode of what is still on screen.
const THUMBNAIL_CACHE_LIMIT: usize = 128;

/// Returns the thumbnail for a record, decoding it at most once.
fn history_thumbnail(key: &HistoryKey, state: &AppViewState) -> Option<slint::Image> {
    THUMBNAIL_CACHE.with(|cache| {
        if let Some(image) = cache.borrow().get(key) {
            return Some(image.clone());
        }
        let image = state.history_images.get(key)?;
        let (width, height) = (image.width?, image.height?);
        let length = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        // Reject a payload whose declared size disagrees with its bytes: the
        // buffer constructor would otherwise read out of bounds.
        if length != image.bytes.len() || length > 32 * 1024 * 1024 {
            return None;
        }
        let decoded = slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(
            &image.bytes,
            width,
            height,
        ));
        let mut cache = cache.borrow_mut();
        if cache.len() >= THUMBNAIL_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key.clone(), decoded.clone());
        Some(decoded)
    })
}

fn project_history(app: &AppWindow, state: &AppViewState) {
    let global = app.global::<AppState>();
    global.set_history_error(
        state
            .history_clear_error
            .as_ref()
            .or(state.history_error.as_ref())
            .cloned()
            .unwrap_or_default()
            .into(),
    );
    let status = state
        .history_clear_result
        .map(|(affected, acknowledged)| {
            let translations = app.global::<Translations>();
            let removed = translations
                .invoke_translate("history-cleared-count".into(), translations.get_revision());
            let peers = translations.invoke_translate(
                "history-acknowledged-peers".into(),
                translations.get_revision(),
            );
            format!("{removed}: {affected} · {peers}: {acknowledged}")
        })
        .unwrap_or_default();
    global.set_history_status(status.into());
    global.set_history_busy(state.history_query_pending || state.history_clear_pending);
    global.set_history_has_more(
        state
            .history_page
            .as_ref()
            .is_some_and(|page| page.next_offset.is_some()),
    );
    let records = state
        .history_page
        .as_ref()
        .map(|page| page.records.as_slice())
        .unwrap_or_default();
    let mut sources = std::collections::BTreeMap::new();
    global.set_history_records(ModelRc::new(VecModel::from(
        records
            .iter()
            .enumerate()
            .map(|(index, record)| {
                let key = (
                    record.event_id.origin_device_id.clone(),
                    record.event_id.origin_sequence,
                );
                let thumbnail = history_thumbnail(&key, state);
                let (kind, preview) = match &record.preview {
                    syntra_api::HistoryPreview::Text { preview, .. } => {
                        ("clipboard-text", preview.clone())
                    }
                    syntra_api::HistoryPreview::Image { size_bytes, .. } => {
                        ("clipboard-images", format_bytes(*size_bytes))
                    }
                    syntra_api::HistoryPreview::Files {
                        names,
                        total_size_bytes,
                        ..
                    } => (
                        "clipboard-files",
                        format!("{}\n{}", names.join(", "), format_bytes(*total_size_bytes)),
                    ),
                };
                let (source_label, source_image) =
                    sources.entry(key.0.clone()).or_insert_with(|| {
                        device_presentation(app, state, &key.0, record.origin_label.as_deref())
                    });
                let group_header = if index == 0 {
                    if record.pinned { "Pinned" } else { "Recent" }
                } else if record.pinned && !records[index - 1].pinned {
                    "Pinned"
                } else if !record.pinned && records[index - 1].pinned {
                    "Recent"
                } else {
                    ""
                };
                HistoryItem {
                    origin: key.0.into(),
                    sequence: key.1.to_string().into(),
                    timestamp: chrono::DateTime::from_timestamp_millis(record.created_at_ms)
                        .map(|time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                        .unwrap_or_default()
                        .into(),
                    source_label: source_label.clone(),
                    source_image: source_image.clone().unwrap_or_default(),
                    has_source_image: source_image.is_some(),
                    kind_label: app
                        .global::<Translations>()
                        .invoke_translate(kind.into(), app.global::<Translations>().get_revision()),
                    preview_text: preview.into(),
                    has_thumbnail: thumbnail.is_some(),
                    thumbnail: thumbnail.unwrap_or_default(),
                    pinned: record.pinned,
                    group_header: if group_header == "Pinned" {
                        app.global::<Translations>().invoke_translate(
                            "history-pinned-section".into(),
                            app.global::<Translations>().get_revision(),
                        )
                    } else if group_header == "Recent" {
                        app.global::<Translations>().invoke_translate(
                            "history-recent-section".into(),
                            app.global::<Translations>().get_revision(),
                        )
                    } else {
                        "".into()
                    },
                }
            })
            .collect::<Vec<_>>(),
    )));
}
fn format_uptime(seconds: u64, known: bool) -> String {
    if !known {
        return String::new();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes} min");
    }
    let hours = minutes / 60;
    let remaining_minutes = minutes % 60;
    if hours < 24 {
        return if remaining_minutes == 0 {
            format!("{hours} h")
        } else {
            format!("{hours} h {remaining_minutes} min")
        };
    }
    let days = hours / 24;
    let remaining_hours = hours % 24;
    if remaining_hours == 0 {
        format!("{days} d")
    } else {
        format!("{days} d {remaining_hours} h")
    }
}

fn project_app_state(app: &AppWindow, state: &AppViewState, settings: &PresentationSettings) {
    let global = app.global::<AppState>();
    global.set_connected(state.status.connected);
    global.set_reconnecting(state.status.reconnecting);
    global.set_transport_status(transport_label(state.status.transport.clone()).into());
    global.set_capture_enabled(state.input_health.capture);
    global.set_input_sharing(state.input_sharing.unwrap_or(false));
    global.set_input_sharing_known(state.status.connected && state.input_sharing.is_some());
    let daemon = state.daemon.as_ref();
    global.set_daemon_known(daemon.is_some());
    global.set_daemon_version(daemon.map(|d| d.version.clone()).unwrap_or_default().into());
    global.set_daemon_origin(
        match daemon.map(|d| d.origin) {
            Some(syntra_api::DaemonOrigin::Service) => "service",
            Some(syntra_api::DaemonOrigin::Dashboard) => "dashboard",
            Some(syntra_api::DaemonOrigin::Manual) => "manual",
            None => "",
        }
        .into(),
    );
    global.set_daemon_pid(daemon.map(|d| d.pid.to_string()).unwrap_or_default().into());
    global.set_daemon_uptime(
        format_uptime(
            daemon.map(|d| d.uptime_seconds).unwrap_or(0),
            daemon.is_some(),
        )
        .into(),
    );
    global.set_daemon_executable(
        daemon
            .map(|d| d.executable.clone())
            .unwrap_or_default()
            .into(),
    );
    global.set_daemon_socket(daemon.map(|d| d.socket.clone()).unwrap_or_default().into());
    global.set_daemon_config_dir(
        daemon
            .map(|d| d.config_dir.clone())
            .unwrap_or_default()
            .into(),
    );
    global.set_daemon_port(i32::from(daemon.map(|d| d.port).unwrap_or(0)));
    global.set_daemon_capture_backend(
        daemon
            .and_then(|d| d.capture_backend.clone())
            .unwrap_or_default()
            .into(),
    );
    global.set_daemon_emulation_backend(
        daemon
            .and_then(|d| d.emulation_backend.clone())
            .unwrap_or_default()
            .into(),
    );
    global.set_emulation_enabled(state.input_health.emulation);
    global.set_capture_supported(state.capabilities.capture);
    global.set_emulation_supported(state.capabilities.emulation);
    global.set_clipboard_text_enabled(state.clipboard.text);
    global.set_clipboard_image_enabled(state.clipboard.image);
    global.set_clipboard_files_enabled(state.clipboard.files);
    global.set_clipboard_files_supported(state.capabilities.clipboard_files);
    global.set_listen_port(i32::from(state.diagnostics.port));
    global.set_locale(app.global::<Translations>().get_locale());
    global.set_local_fingerprint(
        state
            .public_key_fingerprint
            .clone()
            .unwrap_or_default()
            .into(),
    );
    global.set_regenerated_fingerprint(
        state
            .pending_identity_fingerprint
            .clone()
            .unwrap_or_default()
            .into(),
    );
    global.set_pending_fingerprint(
        state
            .connection_attempt
            .as_ref()
            .map(|attempt| attempt.fingerprint.clone())
            .unwrap_or_default()
            .into(),
    );
    global.set_diagnostics_error(
        state
            .diagnostics
            .last_error
            .clone()
            .unwrap_or_default()
            .into(),
    );

    let mut zone_counts = [0i32; 4];
    let clients = state
        .clients
        .values()
        .map(|client| {
            let position = position_index(client.position);
            let zone_index = zone_counts[position as usize];
            zone_counts[position as usize] = zone_index.saturating_add(1);
            let fingerprint = state
                .client_fingerprints
                .get(&client.handle)
                .cloned()
                .unwrap_or_default();
            let identity_key = if fingerprint.is_empty() {
                String::new()
            } else {
                format!("cert:{fingerprint}")
            };
            let remote_profile = state.peer_profiles.get(&fingerprint);
            let identity = settings.devices.get(&identity_key);
            let display_name = identity
                .map(|item| item.friendly_name.clone())
                .filter(|name| !name.trim().is_empty())
                .or_else(|| {
                    remote_profile
                        .map(|profile| profile.display_name.clone())
                        .filter(|name| !name.trim().is_empty())
                })
                .unwrap_or_else(|| client.hostname.clone().unwrap_or_default());
            let image = identity
                .filter(|item| !item.image_path.is_empty())
                .and_then(|item| {
                    slint::Image::load_from_path(std::path::Path::new(&item.image_path)).ok()
                })
                .or_else(|| {
                    remote_profile
                        .and_then(|profile| profile.avatar.as_ref())
                        .map(|avatar| {
                            slint::Image::from_rgba8(
                                slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                    &avatar.rgba,
                                    avatar.width,
                                    avatar.height,
                                ),
                            )
                        })
                });
            let has_custom_image = image.is_some();
            let custom_image = image.unwrap_or_default();
            ClientItem {
                handle: client.handle.to_string().into(),
                hostname: client.hostname.clone().unwrap_or_default().into(),
                identity_key: identity_key.clone().into(),
                display_name: display_name.into(),
                custom_image,
                has_custom_image,
                fixed_ips: join_addresses(&client.fixed_ips).into(),
                port: i32::from(client.port),
                position,
                zone_index,
                active: client.active,
                active_address: client
                    .active_addr
                    .map(|address| address.to_string())
                    .unwrap_or_default()
                    .into(),
                alive: state.status.connected && client.alive,
                remote_ready: state.status.connected && client.remote_ready,
                dns_ips: join_addresses(&client.dns_ips).into(),
                ips: join_addresses(&client.ips).into(),
                has_pressed_keys: client.has_pressed_keys,
                resolving: client.resolving,
                peer_commit: client
                    .peer_commit
                    .map(format_peer_commit)
                    .unwrap_or_default()
                    .into(),
                authorized: state.authorization.contains_key(&fingerprint),
                fingerprint: fingerprint.into(),
            }
        })
        .collect::<Vec<_>>();
    global.set_clients(ModelRc::new(VecModel::from(clients)));
    global.set_peer_zone_depth(zone_counts.into_iter().max().unwrap_or(1).max(1));
    global.set_discovered_peers(ModelRc::new(VecModel::from(
        state
            .discovered_peers
            .iter()
            .map(|peer| DiscoveredPeerItem {
                id: peer.id.clone().into(),
                display_name: peer.display_name.clone().into(),
                addresses: peer
                    .addresses
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
                    .into(),
                port: peer.port.into(),
            })
            .collect::<Vec<_>>(),
    )));

    let mut active = Vec::new();
    let mut completed = Vec::new();
    let mut failed = Vec::new();
    let mut cancelled = Vec::new();
    for item in state.transfers.values().map(transfer_item).chain(
        state
            .manual_transfers
            .values()
            .map(|transfer| crate::manual_ui::transfer_item(app, state, transfer)),
    ) {
        match item.state {
            TransferState::Active => active.push(item),
            TransferState::Completed => completed.push(item),
            TransferState::Cancelled => cancelled.push(item),
            TransferState::Failed => failed.push(item),
        }
    }
    global.set_active_transfers(ModelRc::new(VecModel::from(active)));
    global.set_completed_transfers(ModelRc::new(VecModel::from(completed)));
    global.set_failed_transfers(ModelRc::new(VecModel::from(failed)));
    global.set_cancelled_transfers(ModelRc::new(VecModel::from(cancelled)));
    crate::manual_ui::project(app, state);

    /// Stable identifier used for the health badge and its translation key.
    fn plugin_health_id(health: syntra_api::PluginHealth) -> &'static str {
        use syntra_api::PluginHealth;
        match health {
            PluginHealth::Disabled => "disabled",
            PluginHealth::NotInstalled => "not-installed",
            PluginHealth::Stopped => "stopped",
            PluginHealth::Starting => "starting",
            PluginHealth::Healthy => "healthy",
            PluginHealth::Unresponsive => "unresponsive",
            PluginHealth::Failed => "failed",
        }
    }

    global.set_plugins(ModelRc::new(VecModel::from(
        state
            .plugins
            .iter()
            .map(|plugin| PluginEntry {
                id: plugin.id.clone().into(),
                name: plugin.name.clone().into(),
                description: plugin.description.clone().into(),
                version: plugin.version.clone().into(),
                author: plugin.author.clone().into(),
                homepage: plugin.homepage.clone().unwrap_or_default().into(),
                source: plugin.source.clone().unwrap_or_default().into(),
                update_url: plugin.update_url.clone().unwrap_or_default().into(),
                license: plugin.license.clone().unwrap_or_default().into(),
                mime_types: plugin.mime_types.join(", ").into(),
                bundled: plugin.bundled,
                manifest_path: plugin.manifest_path.clone().into(),
                executable: plugin.executable.clone().into(),
                installed: plugin.installed,
                enabled: plugin.enabled,
                running: plugin.running,
                health: plugin_health_id(plugin.health).into(),
                restarts: plugin.restarts as i32,
                pid: plugin
                    .pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_default()
                    .into(),
                protocol_version: plugin.protocol_version as i32,
                supported_protocol_version: plugin.supported_protocol_version as i32,
                error: plugin.error.clone().unwrap_or_default().into(),
            })
            .collect::<Vec<_>>(),
    )));

    let mut keys = state
        .authorization
        .keys()
        .cloned()
        .map(Into::into)
        .collect::<Vec<slint::SharedString>>();
    keys.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    global.set_authorized_keys(ModelRc::new(VecModel::from(keys)));
    if state.navigation.page == "history" {
        project_history(app, state);
    }
}

fn transfer_item(transfer: &ClipboardTransferStatus) -> TransferItem {
    let (state, error_message, cancellable) = match &transfer.state {
        ClipboardTransferState::Pending | ClipboardTransferState::Transferring => {
            (TransferState::Active, String::new(), true)
        }
        ClipboardTransferState::Completed => (TransferState::Completed, String::new(), false),
        ClipboardTransferState::Cancelled => (TransferState::Cancelled, String::new(), false),
        ClipboardTransferState::Failed(message) => (TransferState::Failed, message.clone(), false),
    };
    let progress = if transfer.total_bytes == 0 {
        0.0
    } else {
        (transfer.transferred_bytes as f64 / transfer.total_bytes as f64).clamp(0.0, 1.0) as f32
    };
    TransferItem {
        transfer_id: transfer.transfer_id.to_string().into(),
        file_id: transfer.file_id.to_string().into(),
        name: transfer.name.clone().into(),
        kind_label: file_kind(&transfer.name).into(),
        direction: match transfer.direction {
            ClipboardTransferDirection::Receiving => TransferDirection::Incoming,
            ClipboardTransferDirection::Sending => TransferDirection::Outgoing,
        },
        transferred_label: format_bytes(transfer.transferred_bytes).into(),
        total_label: format_bytes(transfer.total_bytes).into(),
        rate_label: if transfer.bytes_per_second == 0 {
            "".into()
        } else {
            format!("{}/s", format_bytes(transfer.bytes_per_second)).into()
        },
        progress,
        state,
        error_message: error_message.into(),
        cancellable,
        show_progress: true,
        ..Default::default()
    }
}

fn bind_app_state_callbacks(
    app: &AppWindow,
    tx: mpsc::Sender<FrontendRequest>,
    state: Arc<Mutex<AppViewState>>,
    settings: Arc<Mutex<PresentationSettings>>,
    platform: Option<Arc<dyn PlatformActions>>,
    diagnostics: Arc<Mutex<crate::diagnostics::DiagnosticStore>>,
) {
    let global = app.global::<AppState>();
    let weak = app.as_weak();
    {
        let weak = weak.clone();
        let state = Arc::clone(&state);
        let tx = tx.clone();
        global.on_regenerate_identity(move || {
            send_request(&tx, &weak, &state, FrontendRequest::RegenerateIdentity)
        });
    }
    {
        let weak = weak.clone();
        let state = Arc::clone(&state);
        let tx = tx.clone();
        global.on_history_search(move |query| {
            let request = state.lock().ok().and_then(|mut view| {
                view.history_query = query.to_string();
                view.history_error = None;
                view.history_refresh_needed = true;
                view.next_history_refresh()
            });
            if let Some(app) = weak.upgrade() {
                app.global::<AppState>().set_history_query(query);
                if let Ok(view) = state.lock() {
                    project_history(&app, &view);
                }
            }
            if let Some(request) = request {
                send_request(&tx, &weak, &state, request);
            }
        });
    }
    {
        let weak = weak.clone();
        global.on_history_refresh(move || {
            if let Some(app) = weak.upgrade() {
                let global = app.global::<AppState>();
                global.invoke_history_search(global.get_history_query());
            }
        });
    }
    {
        let tx = tx.clone();
        let state = Arc::clone(&state);
        let weak = weak.clone();
        global.on_history_load_more(move || {
            let request = state.lock().ok().and_then(|mut view| {
                if view.history_query_pending
                    || view.history_clear_pending
                    || view.history_refresh_needed
                {
                    return None;
                }
                let offset = view.history_page.as_ref()?.next_offset?;
                view.history_query_pending = true;
                Some(FrontendRequest::QueryHistory {
                    query: view.history_query.clone(),
                    offset,
                    limit: 50,
                })
            });
            if let Some(request) = request {
                if let Some(app) = weak.upgrade() {
                    app.global::<AppState>().set_history_busy(true);
                }
                send_request(&tx, &weak, &state, request);
            }
        });
    }
    {
        let tx = tx.clone();
        let state = Arc::clone(&state);
        let weak = weak.clone();
        global.on_history_pin(
            move |origin, sequence, pinned| match sequence.parse::<u64>() {
                Ok(origin_sequence) => send_request(
                    &tx,
                    &weak,
                    &state,
                    FrontendRequest::SetHistoryPinned {
                        event_id: syntra_api::HistoryEventId {
                            origin_device_id: origin.to_string(),
                            origin_sequence,
                        },
                        pinned,
                    },
                ),
                Err(_) => show_error(&weak, &state, "Invalid history event identifier".into()),
            },
        );
    }
    {
        let tx = tx.clone();
        let state = Arc::clone(&state);
        let weak = weak.clone();
        global.on_history_clear_global(move || {
            if let Ok(mut view) = state.lock() {
                if view.history_clear_pending {
                    return;
                }
                view.history_clear_pending = true;
                view.history_clear_result = None;
                view.history_clear_error = None;
                if let Some(app) = weak.upgrade() {
                    project_history(&app, &view);
                }
            }
            send_request(&tx, &weak, &state, FrontendRequest::ClearGlobalHistory)
        });
    }

    fn repository_config(value: &str) -> Result<RepositoryConfig, String> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(RepositoryConfig::default());
        }
        let Some((owner, repository)) = value.split_once('/') else {
            return Err("Update repository must use the owner/repository format".into());
        };
        if owner.is_empty() || repository.is_empty() || repository.contains('/') {
            return Err("Update repository must contain exactly one owner/repository pair".into());
        }
        Ok(RepositoryConfig {
            owner: Some(owner.to_owned()),
            repository: Some(repository.to_owned()),
        })
    }

    fn describe_update_status(status: UpdateStatus) -> String {
        match status {
            UpdateStatus::UpToDate { current } => {
                format!("Syntra {current} is up to date")
            }
            UpdateStatus::AheadOfLatest { current, latest } => {
                format!("Syntra {current} is newer than the latest release {latest}")
            }
            UpdateStatus::Available(available) => {
                let size = available
                    .asset
                    .declared_bytes
                    .map(|bytes| format!(" ({})", format_bytes(bytes)))
                    .unwrap_or_default();
                format!(
                    "Syntra {} is available: {}{} — {}",
                    available.version, available.asset.name, size, available.release_page
                )
            }
        }
    }

    let initial_updates = settings
        .lock()
        .ok()
        .map(|saved| saved.updates.clone())
        .unwrap_or_default();
    global.set_update_repository(initial_updates.repository.clone().into());
    global.set_update_automatic_check(initial_updates.automatic_check);
    global.set_update_busy(false);
    global.set_update_status("".into());

    let run_update_check = {
        let weak = weak.clone();
        let settings = Arc::clone(&settings);
        Rc::new(move || {
            let Some(app) = weak.upgrade() else {
                return;
            };
            let global = app.global::<AppState>();
            if global.get_update_busy() {
                return;
            }
            let updates = match settings.lock() {
                Ok(saved) => saved.updates.clone(),
                Err(_) => {
                    global.set_update_status("Update settings lock poisoned".into());
                    return;
                }
            };
            let config = match repository_config(&updates.repository) {
                Ok(config) => config,
                Err(error) => {
                    global.set_update_status(error.into());
                    return;
                }
            };
            if config.owner.is_none() {
                global.set_update_status("Update source is not configured".into());
                return;
            }

            global.set_update_busy(true);
            global.set_update_status("Checking for updates…".into());
            let weak = weak.clone();
            std::thread::spawn(move || {
                let result = Updater::prepare(
                    config,
                    APPLICATION_VERSION,
                    ArtifactNaming::for_current_platform(updates.artifact_template),
                    MAX_UPDATE_DOWNLOAD_BYTES,
                )
                .and_then(|prepared| match prepared {
                    Preparation::Configured(updater) => updater.check(),
                    Preparation::Unconfigured => unreachable!("configured repository was checked"),
                })
                .map(describe_update_status)
                .map_err(|error| error.to_string());
                let _ = weak.upgrade_in_event_loop(move |app| {
                    let global = app.global::<AppState>();
                    global.set_update_busy(false);
                    global.set_update_status(
                        result
                            .unwrap_or_else(|error| format!("Update check failed: {error}"))
                            .into(),
                    );
                });
            });
        })
    };

    {
        let weak = weak.clone();
        let settings = Arc::clone(&settings);
        global.on_configure_updates(move |repository| {
            let Some(app) = weak.upgrade() else {
                return;
            };
            let global = app.global::<AppState>();
            let repository = repository.trim().to_owned();
            let result = (|| -> Result<(), String> {
                let config = repository_config(&repository)?;
                let mut saved = settings
                    .lock()
                    .map_err(|_| "Update settings lock poisoned".to_string())?;
                Updater::prepare(
                    config,
                    APPLICATION_VERSION,
                    ArtifactNaming::for_current_platform(saved.updates.artifact_template.clone()),
                    MAX_UPDATE_DOWNLOAD_BYTES,
                )
                .map_err(|error| error.to_string())?;
                let mut next = saved.clone();
                next.updates.repository = repository.clone();
                save_presentation_settings(&next).map_err(|error| error.to_string())?;
                *saved = next;
                Ok(())
            })();
            match result {
                Ok(()) => {
                    global.set_update_repository(repository.clone().into());
                    global.set_update_status(if repository.is_empty() {
                        "Update source is not configured".into()
                    } else {
                        format!("Update source configured for {repository}").into()
                    });
                }
                Err(error) => global
                    .set_update_status(format!("Unable to save update source: {error}").into()),
            }
        });
    }

    {
        let weak = weak.clone();
        let settings = Arc::clone(&settings);
        global.on_set_automatic_update_check(move |automatic_check| {
            let Some(app) = weak.upgrade() else {
                return;
            };
            let global = app.global::<AppState>();
            let result = (|| -> Result<(), String> {
                let mut saved = settings
                    .lock()
                    .map_err(|_| "Update settings lock poisoned".to_string())?;
                let mut next = saved.clone();
                next.updates.automatic_check = automatic_check;
                save_presentation_settings(&next).map_err(|error| error.to_string())?;
                *saved = next;
                Ok(())
            })();
            match result {
                Ok(()) => global.set_update_automatic_check(automatic_check),
                Err(error) => {
                    let persisted = settings
                        .lock()
                        .ok()
                        .map(|saved| saved.updates.automatic_check)
                        .unwrap_or(false);
                    global.set_update_automatic_check(persisted);
                    global.set_update_status(
                        format!("Unable to save automatic update preference: {error}").into(),
                    );
                }
            }
        });
    }

    {
        let run_update_check = Rc::clone(&run_update_check);
        global.on_check_updates(move || run_update_check());
    }
    if initial_updates.automatic_check && !initial_updates.repository.trim().is_empty() {
        let run_update_check = Rc::clone(&run_update_check);
        Timer::single_shot(std::time::Duration::ZERO, move || run_update_check());
    }
    {
        let weak = weak.clone();
        global.on_change_accent_color(move |color| {
            if let Some(app) = weak.upgrade() {
                app.global::<AppState>().invoke_change_accent(
                    format!(
                        "#{:02x}{:02x}{:02x}",
                        color.red(),
                        color.green(),
                        color.blue()
                    )
                    .into(),
                );
            }
        });
    }
    {
        let weak = weak.clone();
        global.on_change_titlebar_color_value(move |color| {
            if let Some(app) = weak.upgrade() {
                app.global::<AppState>().invoke_change_titlebar_color(
                    format!(
                        "#{:02x}{:02x}{:02x}",
                        color.red(),
                        color.green(),
                        color.blue()
                    )
                    .into(),
                );
            }
        });
    }
    global.set_service_supported(cfg!(target_os = "linux"));
    // Show what the unit will actually run: the daemon, not this window.
    global.set_service_binary_path(
        crate::platform::service::daemon_executable()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|error| error.to_string())
            .into(),
    );
    let install_destination = crate::platform::install::status()
        .ok()
        .and_then(|paths| paths.daemon.parent().map(|path| path.display().to_string()))
        .unwrap_or_else(|| "Unavailable".to_string());
    let source_directory = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|path| path.display().to_string()))
        .unwrap_or_else(|| "Unavailable".to_string());
    global.set_service_source_path(source_directory.into());
    global.set_service_install_destination(install_destination.into());
    global.set_service_destination_customised(
        crate::platform::install::configured_directory().is_some(),
    );
    {
        // Choosing a directory must not block the event loop, so the dialog
        // runs on the Slint executor and the result is applied on return.
        let weak = weak.clone();
        global.on_choose_install_destination(move || {
            let weak = weak.clone();
            let _ = slint::spawn_local(async move {
                let Some(directory) = rfd::AsyncFileDialog::new().pick_folder().await else {
                    return;
                };
                let chosen = directory.path().to_path_buf();
                let outcome = crate::platform::install::set_configured_directory(Some(&chosen));
                if let Some(app) = weak.upgrade() {
                    let global = app.global::<AppState>();
                    match outcome {
                        Ok(()) => {
                            global.set_service_install_destination(
                                chosen.display().to_string().into(),
                            );
                            global.set_service_destination_customised(true);
                            global.set_service_error(Default::default());
                        }
                        Err(error) => global.set_service_error(error.to_string().into()),
                    }
                }
            });
        });
    }
    {
        let weak = weak.clone();
        global.on_reset_install_destination(move || {
            let outcome = crate::platform::install::set_configured_directory(None);
            if let Some(app) = weak.upgrade() {
                let global = app.global::<AppState>();
                match outcome {
                    Ok(()) => {
                        global.set_service_install_destination(
                            crate::platform::install::default_directory()
                                .map(|path| path.display().to_string())
                                .unwrap_or_default()
                                .into(),
                        );
                        global.set_service_destination_customised(false);
                        global.set_service_error(Default::default());
                    }
                    Err(error) => global.set_service_error(error.to_string().into()),
                }
            }
        });
    }
    {
        let weak = weak.clone();
        let service_requests = tx.clone();
        global.on_service_action(move |action| {
            let Some(app) = weak.upgrade() else { return };
            let global = app.global::<AppState>();
            if global.get_service_busy() { return; }
            global.set_service_busy(true);
            global.set_service_error("".into());
            let was_connected = global.get_connected();
            let service_requests = service_requests.clone();
            let weak = weak.clone();
            std::thread::spawn(move || {
                use crate::platform::service;
                let result = (|| {
                    match action.as_str() {
                        "refresh" => {}
                        "install" => {
                            // Copy first: a unit pointing at a build tree
                            // silently fails once that tree moves.
                            let installed = crate::platform::install::install_binaries()
                                .map_err(|error| {
                                    service::ServiceError::InvalidBinary(error.to_string())
                                })?;
                            service::install(&installed.daemon)?
                        }
                        "uninstall" => service::uninstall()?,
                        // Launcher entry, distinct from the background
                        // service: one makes the application appear in the
                        // menu, the other runs the daemon at login.
                        #[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
                        "install-desktop-entry" => {
                            crate::platform::install::install_binaries()
                                .map_err(|error| {
                                    service::ServiceError::InvalidBinary(error.to_string())
                                })
                                .and_then(|installed| {
                                    crate::platform::desktop_entry::install(&installed.application)
                                        .map_err(|error| {
                                            service::ServiceError::InvalidBinary(error.to_string())
                                        })
                                })
                                .map(|_| ())
                                .map_err(|error| service::ServiceError::InvalidBinary(error.to_string()))?
                        }
                        #[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
                        "remove-desktop-entry" => crate::platform::desktop_entry::uninstall()
                            .map_err(|error| service::ServiceError::InvalidBinary(error.to_string()))?,
                        "start" => {
                            #[cfg(target_os = "linux")]
                            if was_connected && !service::query()?.running {
                                service_requests.send(FrontendRequest::StopService)
                                    .map_err(|error| service::ServiceError::InvalidResponse(error.to_string()))?;
                                let socket = syntra_api::default_socket_path()
                                    .map_err(|error| service::ServiceError::InvalidResponse(error.to_string()))?;
                                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                                while socket.exists() {
                                    if std::time::Instant::now() >= deadline {
                                        return Err(service::ServiceError::InvalidResponse("The session daemon did not stop; the installed service was not started".into()));
                                    }
                                    std::thread::sleep(std::time::Duration::from_millis(50));
                                }
                            }
                            service::start()?;
                        }
                        "stop" => service::stop()?,
                        "enable" => service::enable_autostart()?,
                        "disable" => service::disable_autostart()?,
                        _ => return Err(service::ServiceError::Unsupported("unknown service action")),
                    }
                    service::query()
                })();
                let _ = weak.upgrade_in_event_loop(move |app| {
                    let global = app.global::<AppState>();
                    global.set_service_busy(false);
                    match result {
                        Ok(status) => {
                            global.set_service_known(true);
                            global.set_service_installed(status.installed);
                            global.set_service_running(status.running);
                            global.set_service_autostart(status.autostart);
                        }
                        Err(error) => {
                            global.set_service_known(false);
                            global.set_service_error(error.to_string().into());
                        }
                    }
                });
            });
        });
    }
    {
        let settings = Arc::clone(&settings);
        let weak = weak.clone();
        let state = Arc::clone(&state);
        global.on_change_palette(move |value| {
            let Some(app) = weak.upgrade() else {
                return;
            };
            let Ok(mut saved) = settings.lock() else {
                return;
            };
            saved.palette = match value.as_str() {
                "forest" => crate::settings::Palette::Forest,
                "ocean" => crate::settings::Palette::Ocean,
                "slate" => crate::settings::Palette::Slate,
                "sand" => crate::settings::Palette::Sand,
                "graphite" => crate::settings::Palette::Graphite,
                "aubergine" => crate::settings::Palette::Aubergine,
                "copper" => crate::settings::Palette::Copper,
                "rose" => crate::settings::Palette::Rose,
                "ice" => crate::settings::Palette::Ice,
                _ => crate::settings::Palette::Army,
            };
            apply_appearance(&app, &saved);
            if let Err(error) = save_presentation_settings(&saved) {
                drop(saved);
                show_error(&weak, &state, error.to_string());
            }
        });
    }
    macro_rules! appearance_callback {
        ($callback:ident, $update:expr) => {{
            let settings = Arc::clone(&settings);
            let weak = weak.clone();
            let state = Arc::clone(&state);
            global.$callback(move |value| {
                let Some(app) = weak.upgrade() else {
                    return;
                };
                let result = (|| -> Result<PresentationSettings, String> {
                    let mut saved = settings
                        .lock()
                        .map_err(|_| "Appearance settings lock poisoned".to_string())?;
                    ($update)(&mut saved, value)?;
                    save_presentation_settings(&saved).map_err(|error| error.to_string())?;
                    Ok(saved.clone())
                })();
                match result {
                    Ok(saved) => apply_appearance(&app, &saved),
                    Err(error) => show_error(&weak, &state, error),
                }
            });
        }};
    }
    appearance_callback!(
        on_change_zoom,
        |saved: &mut PresentationSettings, value: i32| {
            saved.zoom_percent = value.clamp(75, 200) as u16;
            Ok::<(), String>(())
        }
    );
    appearance_callback!(
        on_change_base_mode,
        |saved: &mut PresentationSettings, value: SharedString| {
            saved.mode = match value.as_str() {
                "white" => crate::settings::BaseMode::White,
                "black" => crate::settings::BaseMode::Black,
                _ => return Err("Unknown appearance mode".to_string()),
            };
            Ok::<(), String>(())
        }
    );
    appearance_callback!(
        on_change_density,
        |saved: &mut PresentationSettings, value: SharedString| {
            saved.density = match value.as_str() {
                "compact" => crate::settings::Density::Compact,
                "comfortable" => crate::settings::Density::Comfortable,
                _ => return Err("Unknown layout density".to_string()),
            };
            Ok::<(), String>(())
        }
    );
    appearance_callback!(
        on_change_reduced_motion,
        |saved: &mut PresentationSettings, value: bool| {
            saved.reduced_motion = value;
            Ok::<(), String>(())
        }
    );
    appearance_callback!(
        on_change_accent,
        |saved: &mut PresentationSettings, value: SharedString| {
            let value = value.trim();
            if value.len() != 7
                || !value.starts_with('#')
                || !value[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err("Accent must be a #RRGGBB color".to_string());
            }
            saved.accent = value.to_string();
            Ok::<(), String>(())
        }
    );
    appearance_callback!(
        on_change_titlebar_color,
        |saved: &mut PresentationSettings, value: SharedString| {
            let value = value.trim();
            if !value.is_empty()
                && (value.len() != 7
                    || !value.starts_with('#')
                    || !value[1..].bytes().all(|byte| byte.is_ascii_hexdigit()))
            {
                return Err("Title bar must be a #RRGGBB color or empty for automatic".to_string());
            }
            saved.titlebar_color = value.to_string();
            Ok::<(), String>(())
        }
    );

    {
        let tx = tx.clone();
        let weak = weak.clone();
        let state = Arc::clone(&state);
        global.on_set_input_sharing(move |enabled| {
            send_request(
                &tx,
                &weak,
                &state,
                FrontendRequest::SetInputSharing(enabled),
            );
        });
    }
    bind_no_arg(
        &global,
        tx.clone(),
        &weak,
        &state,
        UiIntent::Create,
        |g, callback| g.on_create_client(callback),
    );
    bind_no_arg(
        &global,
        tx.clone(),
        &weak,
        &state,
        UiIntent::DiscoverPeers,
        |global, callback| global.on_discover_peers(callback),
    );
    bind_no_arg(
        &global,
        tx.clone(),
        &weak,
        &state,
        UiIntent::EnableCapture,
        |g, callback| g.on_enable_capture(callback),
    );
    bind_no_arg(
        &global,
        tx.clone(),
        &weak,
        &state,
        UiIntent::EnableEmulation,
        |g, callback| g.on_enable_emulation(callback),
    );
    bind_no_arg(
        &global,
        tx.clone(),
        &weak,
        &state,
        UiIntent::Sync,
        |g, callback| g.on_synchronize(callback),
    );

    {
        let tx = tx.clone();
        let weak = weak.clone();
        let state = Arc::clone(&state);
        global.on_activate_client(move |handle, active| {
            parse_client_handle(handle.as_str())
                .map(|handle| UiIntent::Activate(handle, active))
                .map_or_else(
                    |error| show_error(&weak, &state, error),
                    |intent| dispatch(intent, &tx, &weak, &state),
                );
        });
    }
    {
        let tx = tx.clone();
        let weak = weak.clone();
        let state = Arc::clone(&state);
        global.on_delete_client(move |handle| {
            dispatch_client_intent(handle.as_str(), &tx, &weak, &state, UiIntent::Delete)
        });
    }
    {
        let tx = tx.clone();
        let weak = weak.clone();
        let state = Arc::clone(&state);
        global.on_resolve_dns(move |handle| {
            dispatch_client_intent(handle.as_str(), &tx, &weak, &state, UiIntent::ResolveDns)
        });
    }
    {
        let tx = tx.clone();
        let weak = weak.clone();
        let state = Arc::clone(&state);
        global.on_update_hostname(move |handle, hostname| {
            match parse_client_handle(handle.as_str()) {
                Ok(handle) => dispatch(
                    UiIntent::UpdateHostname(handle, Some(hostname.to_string())),
                    &tx,
                    &weak,
                    &state,
                ),
                Err(error) => show_error(&weak, &state, error),
            }
        });
    }
    {
        let tx = tx.clone();
        let weak = weak.clone();
        let state = Arc::clone(&state);
        global.on_update_port(move |handle, port| {
            match (parse_client_handle(handle.as_str()), parse_port(port)) {
                (Ok(handle), Ok(port)) => {
                    dispatch(UiIntent::UpdatePort(handle, port), &tx, &weak, &state)
                }
                (Err(error), _) | (_, Err(error)) => show_error(&weak, &state, error),
            }
        });
    }
    {
        let tx = tx.clone();
        let weak = weak.clone();
        let state = Arc::clone(&state);
        global.on_update_position(move |handle, position| {
            match (
                parse_client_handle(handle.as_str()),
                parse_position(position),
            ) {
                (Ok(handle), Ok(position)) => dispatch(
                    UiIntent::UpdatePosition(handle, position),
                    &tx,
                    &weak,
                    &state,
                ),
                (Err(error), _) | (_, Err(error)) => show_error(&weak, &state, error),
            }
        });
    }
    {
        let tx = tx.clone();
        let weak = weak.clone();
        let state = Arc::clone(&state);
        global.on_update_fixed_ips(move |handle, addresses| {
            match (
                parse_client_handle(handle.as_str()),
                parse_addresses(addresses.as_str()),
            ) {
                (Ok(handle), Ok(addresses)) => dispatch(
                    UiIntent::UpdateFixIps(handle, addresses),
                    &tx,
                    &weak,
                    &state,
                ),
                (Err(error), _) | (_, Err(error)) => show_error(&weak, &state, error),
            }
        });
    }
    {
        let tx = tx.clone();
        let weak = weak.clone();
        let state = Arc::clone(&state);
        global.on_change_port(move |port| match parse_port(port) {
            Ok(port) => dispatch(UiIntent::ChangePort(port), &tx, &weak, &state),
            Err(error) => show_error(&weak, &state, error),
        });
    }
    {
        let tx = tx.clone();
        let weak = weak.clone();
        let state = Arc::clone(&state);
        global.on_authorize_key(move |description, fingerprint| {
            dispatch(
                UiIntent::Authorize(description.to_string(), fingerprint.to_string()),
                &tx,
                &weak,
                &state,
            )
        });
    }
    {
        let tx = tx.clone();
        let weak = weak.clone();
        let state = Arc::clone(&state);
        global.on_remove_authorized_key(move |fingerprint| {
            dispatch(
                UiIntent::RemoveAuthorized(fingerprint.to_string()),
                &tx,
                &weak,
                &state,
            )
        });
    }
    {
        let tx = tx.clone();
        let weak = weak.clone();
        let state = Arc::clone(&state);
        let settings = Arc::clone(&settings);
        global.on_save_configuration(move || {
            let result = settings
                .lock()
                .map_err(|_| "settings lock poisoned".to_string())
                .and_then(|value| {
                    save_presentation_settings(&value).map_err(|error| error.to_string())
                });
            if let Err(error) = result {
                show_error(
                    &weak,
                    &state,
                    format!("Unable to save preferences: {error}"),
                );
                return;
            }
            dispatch(UiIntent::SaveConfiguration, &tx, &weak, &state);
        });
    }
    bind_bool(
        &global,
        tx.clone(),
        &weak,
        &state,
        UiIntent::SetClipboardText,
        |g, callback| g.on_set_clipboard_text(callback),
    );
    bind_bool(
        &global,
        tx.clone(),
        &weak,
        &state,
        UiIntent::SetClipboardImage,
        |g, callback| g.on_set_clipboard_image(callback),
    );
    bind_bool(
        &global,
        tx.clone(),
        &weak,
        &state,
        UiIntent::SetClipboardFiles,
        |g, callback| g.on_set_clipboard_files(callback),
    );
    {
        let tx = tx.clone();
        let weak = weak.clone();
        let state = Arc::clone(&state);
        global.on_cancel_transfer(move |transfer_id| match transfer_id.parse::<u64>() {
            Ok(transfer_id) => dispatch(UiIntent::CancelTransfer(transfer_id), &tx, &weak, &state),
            Err(_) => show_error(&weak, &state, "Invalid transfer id".into()),
        });
    }

    {
        let store = Arc::clone(&diagnostics);
        let weak = weak.clone();
        global.on_diagnostics_level_changed(move |value| {
            if let Ok(mut store) = store.lock() {
                store.filter.level = value.to_string();
                // A different filter selects different records, so the view
                // must rebuild rather than append.
                store.invalidate();
            }
            if let Some(app) = weak.upgrade() {
                project_live_diagnostics(&app, &store);
            }
        });
    }
    {
        let store = Arc::clone(&diagnostics);
        let weak = weak.clone();
        global.on_diagnostics_stage_changed(move |value| {
            if let Ok(mut store) = store.lock() {
                store.filter.stage = value.to_string();
                // A different filter selects different records, so the view
                // must rebuild rather than append.
                store.invalidate();
            }
            if let Some(app) = weak.upgrade() {
                project_live_diagnostics(&app, &store);
            }
        });
    }
    {
        let store = Arc::clone(&diagnostics);
        let weak = weak.clone();
        global.on_diagnostics_direction_changed(move |value| {
            if let Ok(mut store) = store.lock() {
                store.filter.direction = value.to_string();
                // A different filter selects different records, so the view
                // must rebuild rather than append.
                store.invalidate();
            }
            if let Some(app) = weak.upgrade() {
                project_live_diagnostics(&app, &store);
            }
        });
    }
    {
        let store = Arc::clone(&diagnostics);
        let weak = weak.clone();
        global.on_diagnostics_text_changed(move |value| {
            if let Ok(mut store) = store.lock() {
                store.filter.query = value.to_string();
                // A different filter selects different records, so the view
                // must rebuild rather than append.
                store.invalidate();
            }
            if let Some(app) = weak.upgrade() {
                project_live_diagnostics(&app, &store);
            }
        });
    }
    {
        let store = Arc::clone(&diagnostics);
        let weak = weak.clone();
        global.on_diagnostics_pause(move |paused| {
            if let Ok(mut store) = store.lock() {
                store.set_paused(paused);
            }
            if let Some(app) = weak.upgrade() {
                project_live_diagnostics(&app, &store);
            }
        });
    }
    {
        let store = Arc::clone(&diagnostics);
        let weak = weak.clone();
        global.on_diagnostics_clear(move || {
            if let Ok(mut store) = store.lock() {
                store.clear();
            }
            if let Some(app) = weak.upgrade() {
                project_live_diagnostics(&app, &store);
            }
        });
    }
    {
        // Plugin management is entirely the daemon's decision; the interface
        // only forwards the intent and waits for the authoritative reply.
        let requests = tx.clone();
        global.on_set_plugin_enabled(move |id, enabled| {
            let _ = requests.send(FrontendRequest::SetPluginEnabled {
                id: id.to_string(),
                enabled,
            });
        });
    }
    {
        let requests = tx.clone();
        global.on_restart_plugin(move |id| {
            let _ = requests.send(FrontendRequest::RestartPlugin { id: id.to_string() });
        });
    }
    {
        let requests = tx.clone();
        global.on_refresh_plugins(move || {
            let _ = requests.send(FrontendRequest::QueryPlugins);
        });
    }
    #[cfg(not(target_os = "android"))]
    {
        // Starting blocks until the daemon answers, so it runs off the UI
        // thread; the window must stay responsive while a service starts.
        let weak = app.as_weak();
        global.on_start_daemon(move || {
            let weak = weak.clone();
            if let Some(app) = weak.upgrade() {
                let global = app.global::<AppState>();
                global.set_daemon_starting(true);
                global.set_daemon_start_error(Default::default());
            }
            std::thread::spawn(move || {
                let outcome = crate::platform::daemon::start();
                let _ = weak.upgrade_in_event_loop(move |app| {
                    let global = app.global::<AppState>();
                    global.set_daemon_starting(false);
                    if let Err(error) = outcome {
                        global.set_daemon_start_error(error.to_string().into());
                    }
                });
            });
        });
    }
    bind_platform_action(
        &global,
        &weak,
        &state,
        platform.clone(),
        PlatformAction::RequestPermissions,
        |g, callback| g.on_request_permission(callback),
    );
    bind_platform_action(
        &global,
        &weak,
        &state,
        platform.clone(),
        PlatformAction::OpenPermissionSettings,
        |g, callback| g.on_open_permission_settings(callback),
    );
    {
        let weak = weak.clone();
        let state = Arc::clone(&state);
        let platform = platform.clone();
        global.on_grant_flatpak_access(move |application_id| {
            let Some(platform) = platform.clone() else {
                show_error(
                    &weak,
                    &state,
                    "This action is unavailable on this platform".into(),
                );
                return;
            };
            let application_id = application_id.to_string();
            if application_id.is_empty() {
                show_error(&weak, &state, "Select a Flatpak application first".into());
                return;
            }
            if let Some(app) = weak.upgrade() {
                app.global::<AppState>().set_flatpak_grant_status("".into());
                app.global::<AppState>()
                    .set_flatpak_applications_error("".into());
            }
            let weak = weak.clone();
            std::thread::spawn(move || {
                let result = platform.perform(PlatformAction::GrantFlatpakFilesystemAccess {
                    application_id: application_id.clone(),
                });
                let _ = weak.upgrade_in_event_loop(move |app| {
                    let state = app.global::<AppState>();
                    match result {
                        Ok(()) => state.set_flatpak_grant_status(application_id.into()),
                        Err(error) => {
                            state.set_flatpak_applications_error(error.to_string().into())
                        }
                    }
                });
            });
        });
    }
    {
        let weak = weak.clone();
        global.on_search_flatpak_apps(move |query| {
            if let Some(app) = weak.upgrade() {
                let global = app.global::<AppState>();
                global.set_flatpak_search(query);
                filter_flatpak_applications(&global);
            }
        });
    }
    {
        let weak = weak.clone();
        let platform = platform.clone();
        global.on_refresh_flatpak_apps(move || {
            let Some(platform) = platform.clone() else {
                let _ = weak.upgrade_in_event_loop(|app| {
                    app.global::<AppState>().set_flatpak_applications_error(
                        "Flatpak application discovery is unavailable on this platform".into(),
                    );
                });
                return;
            };
            let _ = weak.upgrade_in_event_loop(|app| {
                let global = app.global::<AppState>();
                global.set_flatpak_applications_loading(true);
                global.set_flatpak_applications_error("".into());
            });
            let weak = weak.clone();
            std::thread::spawn(move || {
                let result = platform.installed_flatpak_applications();
                let _ = weak.upgrade_in_event_loop(move |app| {
                    let global = app.global::<AppState>();
                    global.set_flatpak_applications_loading(false);
                    match result {
                        Ok(applications) => {
                            let entries = applications
                                .into_iter()
                                .map(|application| FlatpakApplication {
                                    application_id: application.application_id.into(),
                                    name: application.name.into(),
                                })
                                .collect::<Vec<_>>();
                            global.set_flatpak_all_applications(ModelRc::new(VecModel::from(
                                entries,
                            )));
                            filter_flatpak_applications(&global);
                            global.set_flatpak_applications_error("".into());
                        }
                        Err(error) => {
                            global.set_flatpak_applications(ModelRc::new(VecModel::default()));
                            global.set_flatpak_all_applications(ModelRc::new(VecModel::default()));
                            global.set_flatpak_applications_error(error.to_string().into());
                        }
                    }
                });
            });
        });
    }
}

fn filter_flatpak_applications(global: &AppState) {
    let query = global.get_flatpak_search().trim().to_lowercase();
    let applications = global
        .get_flatpak_all_applications()
        .iter()
        .filter(|application| {
            application.name.to_lowercase().contains(&query)
                || application.application_id.to_lowercase().contains(&query)
        })
        .collect::<Vec<_>>();
    global.set_flatpak_applications(ModelRc::new(VecModel::from(applications)));
}
#[cfg(not(target_os = "android"))]
fn native_window<T>(
    app: &AppWindow,
    action: impl FnOnce(&winit::window::Window) -> T,
) -> Option<T> {
    app.window().with_winit_window(action)
}

fn bind_window_callbacks(
    app: &AppWindow,
    settings: Arc<Mutex<PresentationSettings>>,
    state: Arc<Mutex<AppViewState>>,
    tray_started: bool,
) {
    let weak = app.as_weak();
    app.on_navigate({
        let state = Arc::clone(&state);
        let weak = weak.clone();
        move |page| {
            if let Ok(mut view) = state.lock() {
                view.navigation.page = page.to_string();
            }
            if page == "clipboard" {
                if let Some(app) = weak.upgrade() {
                    app.global::<AppState>().invoke_refresh_flatpak_apps();
                }
            }
            if let Some(app) = weak.upgrade() {
                match page.as_str() {
                    "devices" => app.global::<AppState>().invoke_discover_peers(),
                    "history" => app.global::<AppState>().invoke_history_refresh(),
                    "settings" | "overview" => app
                        .global::<AppState>()
                        .invoke_service_action("refresh".into()),
                    _ => {}
                }
            }
        }
    });
    {
        let weak = weak.clone();
        let state = Arc::clone(&state);
        let settings = Arc::clone(&settings);
        app.on_toggle_sidebar(move || {
            let Some(app) = weak.upgrade() else { return };
            let result = settings
                .lock()
                .map_err(|_| "settings lock poisoned".to_string())
                .and_then(|mut value| {
                    let expanded = !app.get_sidebar_expanded();
                    app.set_sidebar_expanded(expanded);
                    value.sidebar_open = expanded;
                    save_presentation_settings(&value).map_err(|error| error.to_string())
                });
            if let Err(error) = result {
                show_error(
                    &weak,
                    &state,
                    format!("Unable to save sidebar preference: {error}"),
                );
            }
        });
    }

    app.set_hide_to_tray_enabled(tray_started);
    macro_rules! identity_callback {
        ($callback:ident, || $operation:expr) => {
            identity_callback!($callback, | | $operation);
        };
        ($callback:ident, |$($argument:ident),*| $operation:expr) => {{
            let settings = Arc::clone(&settings);
            let state = Arc::clone(&state);
            let weak = app.as_weak();
            app.$callback(move |$($argument),*| {
                let result = settings.lock()
                    .map_err(|_| io::Error::other("settings lock poisoned"))
                    .and_then(|mut shared| {
                        let mut store = crate::device_identity::IdentityStore::new(shared.clone());
                        ($operation)(&mut store)?;
                        *shared = store.settings().clone();
                        Ok(())
                    });
                match result {
                    Ok(()) => {
                        if let Some(app) = weak.upgrade() {
                            project_locked_state(&app, &state, &settings);
                            app.invoke_local_profile_changed();
                        }
                    }
                    Err(error) => show_error(&weak, &state, error.to_string()),
                }
            });
        }};
    }
    identity_callback!(on_update_display_name, |key, name| {
        |store: &mut crate::device_identity::IdentityStore| {
            store.set_peer_name(key.to_string(), name.to_string())
        }
    });
    identity_callback!(on_clear_display_image, |key| {
        |store: &mut crate::device_identity::IdentityStore| store.clear_peer_image(key.as_str())
    });
    #[cfg(not(target_os = "android"))]
    app.on_choose_display_image({
        let weak = app.as_weak();
        let settings = Arc::clone(&settings);
        let state = Arc::clone(&state);
        move |key| {
            let key = key.to_string();
            let weak = weak.clone();
            let settings = Arc::clone(&settings);
            let state = Arc::clone(&state);
            let _ = slint::spawn_local(async move {
                match crate::device_identity::pick_image().await {
                    Ok(Some(path)) => {
                        let result = settings
                            .lock()
                            .map(|value| crate::device_identity::IdentityStore::new(value.clone()))
                            .map_err(|_| io::Error::other("settings lock poisoned"))
                            .and_then(|mut store| {
                                store.set_peer_image(&key, path)?;
                                Ok(store)
                            });
                        match result {
                            Ok(store) => {
                                if let Ok(mut shared) = settings.lock() {
                                    *shared = store.settings().clone();
                                }
                                if let Some(app) = weak.upgrade() {
                                    project_locked_state(&app, &state, &settings);
                                }
                            }
                            Err(error) => show_error(&weak, &state, error.to_string()),
                        }
                    }
                    Ok(None) => {}
                    Err(error) => show_error(&weak, &state, error.to_string()),
                }
            });
        }
    });
    identity_callback!(on_update_local_name, |name| {
        |store: &mut crate::device_identity::IdentityStore| store.set_local_name(name.to_string())
    });
    identity_callback!(on_clear_local_image, || {
        |store: &mut crate::device_identity::IdentityStore| store.clear_local_image()
    });

    #[cfg(not(target_os = "android"))]
    app.on_choose_local_image({
        let weak = app.as_weak();
        let settings = Arc::clone(&settings);
        let state = Arc::clone(&state);
        move || {
            let weak = weak.clone();
            let settings = Arc::clone(&settings);
            let state = Arc::clone(&state);
            let _ = slint::spawn_local(async move {
                match crate::device_identity::pick_image().await {
                    Ok(Some(path)) => {
                        let result = settings
                            .lock()
                            .map(|value| crate::device_identity::IdentityStore::new(value.clone()))
                            .map_err(|_| io::Error::other("settings lock poisoned"))
                            .and_then(|mut store| {
                                store.set_local_image(path)?;
                                Ok(store)
                            });
                        match result {
                            Ok(store) => {
                                if let Ok(mut shared) = settings.lock() {
                                    *shared = store.settings().clone();
                                }
                                if let Some(app) = weak.upgrade() {
                                    project_locked_state(&app, &state, &settings);
                                    app.invoke_local_profile_changed();
                                }
                            }
                            Err(error) => show_error(&weak, &state, error.to_string()),
                        }
                    }
                    Ok(None) => {}
                    Err(error) => show_error(&weak, &state, error.to_string()),
                }
            });
        }
    });
    #[cfg(not(target_os = "android"))]
    {
        app.on_hide_to_tray({
            let weak = app.as_weak();
            move || {
                if tray_started {
                    if let Some(app) = weak.upgrade() {
                        let _ = app.window().hide();
                    }
                }
            }
        });
        app.on_minimize_window({
            let weak = app.as_weak();
            move || {
                if let Some(app) = weak.upgrade() {
                    app.window().set_minimized(true);
                }
            }
        });
        app.on_toggle_maximize_window({
            let weak = app.as_weak();
            move || {
                if let Some(app) = weak.upgrade() {
                    let maximized = !app.window().is_maximized();
                    app.window().set_maximized(maximized);
                    app.set_window_maximized(maximized);
                }
            }
        });
        app.on_begin_window_drag({
            let weak = app.as_weak();
            move || {
                if let Some(app) = weak.upgrade() {
                    native_window(&app, |window| {
                        if let Err(error) = window.drag_window() {
                            log::warn!("could not begin window drag: {error}");
                        }
                    });
                }
            }
        });
        app.on_close_window({
            let weak = app.as_weak();
            move || {
                if tray_started {
                    if let Some(app) = weak.upgrade() {
                        let _ = app.window().hide();
                    }
                } else {
                    let _ = slint::quit_event_loop();
                }
            }
        });
    }
}

fn bind_no_arg(
    global: &AppState,
    tx: mpsc::Sender<FrontendRequest>,
    weak: &slint::Weak<AppWindow>,
    state: &Arc<Mutex<AppViewState>>,
    intent: UiIntent,
    install: impl FnOnce(&AppState, Box<dyn Fn()>),
) {
    let weak = weak.clone();
    let state = Arc::clone(state);
    install(
        global,
        Box::new(move || dispatch(intent.clone(), &tx, &weak, &state)),
    );
}

fn bind_bool(
    global: &AppState,
    tx: mpsc::Sender<FrontendRequest>,
    weak: &slint::Weak<AppWindow>,
    state: &Arc<Mutex<AppViewState>>,
    intent: impl Fn(bool) -> UiIntent + 'static,
    install: impl FnOnce(&AppState, Box<dyn Fn(bool)>),
) {
    let weak = weak.clone();
    let state = Arc::clone(state);
    install(
        global,
        Box::new(move |value| dispatch(intent(value), &tx, &weak, &state)),
    );
}

fn bind_platform_action(
    global: &AppState,
    weak: &slint::Weak<AppWindow>,
    state: &Arc<Mutex<AppViewState>>,
    platform: Option<Arc<dyn PlatformActions>>,
    action: PlatformAction,
    install: impl FnOnce(&AppState, Box<dyn Fn()>),
) {
    let weak = weak.clone();
    let state = Arc::clone(state);
    install(
        global,
        Box::new(move || match platform.as_ref() {
            Some(platform) => {
                if let Err(error) = platform.perform(action.clone()) {
                    show_error(&weak, &state, error.to_string());
                }
            }
            None => show_error(
                &weak,
                &state,
                "This action is unavailable on this platform".into(),
            ),
        }),
    );
}

fn dispatch(
    intent: UiIntent,
    tx: &mpsc::Sender<FrontendRequest>,
    weak: &slint::Weak<AppWindow>,
    state: &Arc<Mutex<AppViewState>>,
) {
    match intent.into_request() {
        Ok(request) => send_request(tx, weak, state, request),
        Err(error) => show_error(weak, state, intent_error_message(error)),
    }
}

fn send_request(
    tx: &mpsc::Sender<FrontendRequest>,
    weak: &slint::Weak<AppWindow>,
    state: &Arc<Mutex<AppViewState>>,
    request: FrontendRequest,
) {
    if tx.send(request).is_err() {
        show_error(weak, state, "The service connection is unavailable".into());
    }
}

fn dispatch_client_intent(
    handle: &str,
    tx: &mpsc::Sender<FrontendRequest>,
    weak: &slint::Weak<AppWindow>,
    state: &Arc<Mutex<AppViewState>>,
    intent: impl FnOnce(u64) -> UiIntent,
) {
    match parse_client_handle(handle) {
        Ok(handle) => dispatch(intent(handle), tx, weak, state),
        Err(error) => show_error(weak, state, error),
    }
}

fn show_error(weak: &slint::Weak<AppWindow>, state: &Arc<Mutex<AppViewState>>, message: String) {
    if let Ok(mut view) = state.lock() {
        view.diagnostics.last_error = Some(message.clone());
    }
    let _ = weak.upgrade_in_event_loop(move |app| {
        let global = app.global::<AppState>();
        global.set_diagnostics_error(message.clone().into());
    });
}

fn start_platform_tray(
    app: &AppWindow,
    platform: Arc<dyn PlatformActions>,
    state: &Arc<Mutex<AppViewState>>,
) -> Option<Box<dyn PlatformTray>> {
    if !platform.capabilities().tray {
        return None;
    }
    let show_window = app.as_weak();
    let quit_window = app.as_weak();
    let callbacks = PlatformCallbacks::new(
        move || {
            let _ = show_window.upgrade_in_event_loop(|app| {
                let _ = app.window().show();
            });
        },
        move || {
            let _ = quit_window.upgrade_in_event_loop(|_| {
                let _ = slint::quit_event_loop();
            });
        },
    );
    match platform.start_tray(callbacks) {
        Ok(tray) => Some(tray),
        Err(error) => {
            show_error(
                &app.as_weak(),
                state,
                format!("System tray unavailable: {error}"),
            );
            None
        }
    }
}

fn install_close_behavior(
    app: &AppWindow,
    platform: Option<Arc<dyn PlatformActions>>,
    tray_started: bool,
) {
    let behavior = platform
        .as_ref()
        .map(|platform| platform.capabilities().close_behavior)
        .unwrap_or(CloseBehavior::Exit);
    app.window().on_close_requested(move || {
        if behavior == CloseBehavior::Hide && tray_started {
            if let Some(platform) = platform.as_ref() {
                if let Err(error) = platform.perform(PlatformAction::NotifyRunningInBackground) {
                    log::warn!("could not show background notification: {error}");
                }
            }
            slint::CloseRequestResponse::HideWindow
        } else {
            let _ = slint::quit_event_loop();
            slint::CloseRequestResponse::HideWindow
        }
    });
}

fn parse_client_handle(value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("Invalid client id: {value}"))
}

fn parse_port(value: i32) -> Result<u16, String> {
    u16::try_from(value)
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| format!("Invalid port: {value}"))
}

fn parse_position(value: i32) -> Result<Position, String> {
    match value {
        0 => Ok(Position::Left),
        1 => Ok(Position::Right),
        2 => Ok(Position::Top),
        3 => Ok(Position::Bottom),
        _ => Err(format!("Invalid client position: {value}")),
    }
}

fn parse_addresses(value: &str) -> Result<Vec<IpAddr>, String> {
    value
        .split(|character: char| character == ',' || character.is_whitespace())
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<IpAddr>()
                .map_err(|_| format!("Invalid IP address: {part}"))
        })
        .collect()
}

fn intent_error_message(error: crate::bridge::IntentError) -> String {
    match error {
        crate::bridge::IntentError::EmptyField(field) => {
            format!("Required field is empty: {field}")
        }
        crate::bridge::IntentError::InvalidPort => "Invalid port".into(),
        crate::bridge::IntentError::InvalidTransferId => "Invalid transfer id".into(),
    }
}

fn transport_label(transport: TransportLifecycle) -> &'static str {
    match transport {
        TransportLifecycle::Unavailable => "Unavailable",
        TransportLifecycle::Reconnecting => "Reconnecting",
        TransportLifecycle::Ready => "Ready",
    }
}

fn position_index(position: Position) -> i32 {
    match position {
        Position::Left => 0,
        Position::Right => 1,
        Position::Top => 2,
        Position::Bottom => 3,
    }
}

fn join_addresses(addresses: &[IpAddr]) -> String {
    addresses
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_peer_commit(commit: [u8; 8]) -> String {
    commit.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut amount = bytes as f64;
    let mut unit = 0;
    while amount >= 1024.0 && unit + 1 < UNITS.len() {
        amount /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{amount:.1} {}", UNITS[unit])
    }
}

fn file_kind(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_uppercase())
        .filter(|extension| !extension.is_empty())
        .unwrap_or_else(|| "File".into())
}

fn presentation_settings_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from)?;
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library").join("Application Support"))?;
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    #[cfg(target_os = "windows")]
    return Some(base.join("Syntra").join("presentation.json"));
    #[cfg(target_os = "macos")]
    return Some(base.join("Syntra").join("presentation.json"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    Some(base.join("syntra").join("presentation.json"))
}

fn load_presentation_settings() -> io::Result<PresentationSettings> {
    let Some(path) = presentation_settings_path() else {
        return Ok(PresentationSettings::default());
    };
    PresentationSettings::load(&path)
}

fn save_presentation_settings(settings: &PresentationSettings) -> io::Result<()> {
    let Some(path) = presentation_settings_path() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no user configuration directory is available",
        ));
    };
    settings.save(&path)
}
