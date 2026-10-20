use std::cell::Cell;
use std::error::Error;
use std::io;
use std::net::IpAddr;
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

use crate::bridge::{EventSource, RequestSink, UiIntent};
use crate::device_identity::client_identity_key;
use crate::localization::Localizer;
use crate::models::{AppViewState, TransportLifecycle, TransportLifecycleEvent};
use crate::platform::{
    CloseBehavior, PlatformAction, PlatformActions, PlatformCallbacks, PlatformTray,
};
use crate::settings::PresentationSettings;
use crate::updates::{ArtifactNaming, Preparation, RepositoryConfig, UpdateStatus, Updater};

mod bindings;
mod plugins;
mod projection;
mod transport;

use bindings::{
    bind_app_state_callbacks, bind_window_callbacks, install_close_behavior, send_request,
    show_error, start_platform_tray,
};
// Defined only where there is a native (winit) window to reach.
#[cfg(not(target_os = "android"))]
use bindings::native_window;
use projection::{
    apply_appearance, project_app_state, project_live_diagnostics, project_local_identity,
    project_locked_state,
};
// Still addressed as `app::device_presentation` / `app::format_bytes` by the
// sibling modules that format devices and sizes.
pub(crate) use projection::{device_presentation, format_bytes};
use transport::IpcEventSource;

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

/// Opens an `https` link in the user's browser. Only credit and project
/// links reach this, but anything else is refused rather than executed.
fn open_link(url: &str) {
    if !url.starts_with("https://") {
        log::warn!("refusing to open non-https link");
        return;
    }
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
    let command = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let command = std::process::Command::new("open").arg(url).spawn();
    #[cfg(windows)]
    let command = std::process::Command::new("explorer").arg(url).spawn();
    #[cfg(target_os = "android")]
    let command: std::io::Result<std::process::Child> = Err(std::io::Error::other(
        "links are not supported on Android yet",
    ));
    match command {
        // Reap the launcher so it does not linger as a zombie.
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(error) => log::warn!("could not open link: {error}"),
    }
}

fn schedule_launch_ready(app: &AppWindow) -> Option<Timer> {
    let display_time =
        std::time::Duration::from_millis(if app.global::<Theme>().get_motion_enabled() {
            2000
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
    let sink = source.sink();
    run_with_transport_startup(source, sink, startup)
}

/// Runs the dashboard against an already-established daemon connection.
pub fn run_with_ipc(
    reader: syntra_api::FrontendEventReader,
    writer: FrontendRequestWriter,
) -> Result<(), Box<dyn Error>> {
    let source = IpcEventSource::attached(reader, writer);
    let sink = source.sink();
    run_with_transport(source, sink)
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
                if let FrontendEvent::HistoryContentResult {
                    event_id,
                    text,
                    image,
                    mut error,
                } = event
                {
                    let pixels = image.and_then(|image| {
                        let decoded = if image.media_type.as_deref() == Some("image/x-rgba8") {
                            image
                                .width
                                .zip(image.height)
                                .and_then(|(w, h)| image::RgbaImage::from_raw(w, h, image.bytes))
                                .ok_or_else(|| "Invalid history image dimensions".to_string())
                        } else {
                            image::load_from_memory(&image.bytes)
                                .map(|image| image.into_rgba8())
                                .map_err(|e| e.to_string())
                        };
                        match decoded {
                            Ok(image) => Some(
                                slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                    image.as_raw(),
                                    image.width(),
                                    image.height(),
                                ),
                            ),
                            Err(failure) => {
                                error = Some(failure);
                                None
                            }
                        }
                    });
                    let _ = event_window.upgrade_in_event_loop(move |app| {
                        let global = app.global::<AppState>();
                        if global.get_history_detail_origin().as_str() != event_id.origin_device_id
                            || global.get_history_detail_sequence().as_str()
                                != event_id.origin_sequence.to_string()
                        {
                            return;
                        }
                        global.set_history_detail_busy(false);
                        global.set_history_detail_error(error.unwrap_or_default().into());
                        if let Some(text) = text {
                            if global.get_history_detail_copy() {
                                app.invoke_history_copy_text(text.into());
                            }
                        }
                        if let Some(pixels) = pixels {
                            global.set_history_detail_image(slint::Image::from_rgba8(pixels));
                            global.set_history_detail_visible(true);
                        }
                    });
                    continue;
                }
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
                    let settings_saved = settings_reply && settings_were_pending;
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
                    if incoming_offer
                        && global.get_manual_offer_visible()
                        && !global.get_suppress_popups()
                    {
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
    #[cfg(target_os = "android")]
    crate::manual_ui::bind(&app, request_tx.clone(), Arc::clone(&state));
    slint::run_event_loop_until_quit()?;
    app.hide()?;
    drop(tray);
    Ok(())
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

// One location for reading and writing: Android keeps it in the app's own
// files directory (SYNTRA_CONFIG_DIR), which the XDG fallback never sees.
fn load_presentation_settings() -> io::Result<PresentationSettings> {
    let Some(path) = crate::settings::presentation_settings_path() else {
        return Ok(PresentationSettings::default());
    };
    PresentationSettings::load(&path)
}

fn save_presentation_settings(settings: &PresentationSettings) -> io::Result<()> {
    let Some(path) = crate::settings::presentation_settings_path() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no user configuration directory is available",
        ));
    };
    settings.save(&path)
}
