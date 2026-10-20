//! User intent: the callbacks the window raises, and how each becomes a request
//! to the daemon, a platform action or a local setting change.
//!
//! A callback only forwards intent; what the interface then shows is decided by
//! the daemon's reply, which arrives as an event and is projected by
//! [`super::projection`].

use super::plugins;
use super::projection::{
    DIAGNOSTIC_MODEL, apply_appearance, diagnostic_line, format_bytes, project_history,
    project_live_diagnostics, project_locked_state,
};
use super::*;

#[cfg(not(target_os = "android"))]
fn bind_install_destination(global: &AppState<'_>, weak: &slint::Weak<AppWindow>) {
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
}

pub(super) fn bind_app_state_callbacks(
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
        let tx = tx.clone();
        let state = Arc::clone(&state);
        global.on_history_open(move |origin, sequence, copy| {
            let Some(app) = weak.upgrade() else { return };
            let Ok(origin_sequence) = sequence.parse::<u64>() else {
                return;
            };
            let global = app.global::<AppState>();
            global.set_history_detail_origin(origin.clone());
            global.set_history_detail_sequence(sequence);
            global.set_history_detail_copy(copy);
            global.set_history_detail_busy(true);
            global.set_history_detail_error("".into());
            send_request(
                &tx,
                &weak,
                &state,
                FrontendRequest::GetHistoryContent(syntra_api::HistoryEventId {
                    origin_device_id: origin.to_string(),
                    origin_sequence,
                }),
            );
        });
    }
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
    // Installing binaries and choosing where they go only exists on
    // desktops; Android ships the dashboard inside its package.
    #[cfg(not(target_os = "android"))]
    bind_install_destination(&global, &weak);
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
                        #[cfg(not(target_os = "android"))]
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
        on_change_suppress_popups,
        |saved: &mut PresentationSettings, value: bool| {
            saved.suppress_popups = value;
            Ok::<(), String>(())
        }
    );
    appearance_callback!(
        on_toggle_diagnostic_column,
        |saved: &mut PresentationSettings, index: i32| {
            // The last column is the message and always stays.
            let hideable = crate::settings::DIAGNOSTIC_COLUMNS - 1;
            let index = u32::try_from(index)
                .ok()
                .filter(|index| *index < hideable)
                .ok_or_else(|| "Unknown diagnostic column".to_string())?;
            saved.hidden_diagnostic_columns ^= 1 << index;
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
        global.set_app_version(env!("CARGO_PKG_VERSION").into());
        #[cfg(target_os = "android")]
        {
            use crate::touchpad;
            global.on_touchpad_begin(touchpad::begin);
            global.on_open_phone_control_settings(crate::android::open_accessibility_settings);
            global.set_phone_control_enabled(crate::android::phone_control_enabled());
            {
                // Reflect the setting after the user returns from Android
                // settings; checked cheaply every two seconds.
                let weak = weak.clone();
                let timer = Box::leak(Box::new(slint::Timer::default()));
                timer.start(
                    slint::TimerMode::Repeated,
                    std::time::Duration::from_secs(2),
                    move || {
                        if let Some(app) = weak.upgrade() {
                            app.global::<AppState>()
                                .set_phone_control_enabled(crate::android::phone_control_enabled());
                        }
                    },
                );
            }
            global.on_touchpad_motion(|zone, dx, dy| touchpad::motion(zone, dx, dy, 1.0));
            global.on_touchpad_button(touchpad::button);
            global.on_touchpad_scroll(touchpad::scroll);
            global.on_touchpad_type(|zone, text| touchpad::type_text(zone, &text));
            global.on_touchpad_backspace(touchpad::backspace);
            global.on_touchpad_enter(touchpad::enter);
        }
        {
            let weak = weak.clone();
            global.on_diagnostics_copy_all(move || {
                let text = DIAGNOSTIC_MODEL.with(|model| {
                    model
                        .iter()
                        .map(|entry| diagnostic_line(&entry))
                        .collect::<Vec<_>>()
                        .join("\n")
                });
                if let Some(app) = weak.upgrade() {
                    app.invoke_history_copy_text(text.into());
                }
            });
        }
        global.on_open_link(|url| open_link(&url));
        global.on_add_discovered_peer(move |id| {
            let peer = state.lock().ok().and_then(|view| {
                view.discovered_peers
                    .iter()
                    .find(|peer| peer.id == id.as_str())
                    .cloned()
            });
            if let Some(peer) = peer {
                dispatch(
                    UiIntent::CreateDiscovered(peer.addresses, peer.port),
                    &tx,
                    &weak,
                    &state,
                );
            }
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
    bind_bool(
        &global,
        tx.clone(),
        &weak,
        &state,
        UiIntent::SetIndependentPointers,
        |g, callback| g.on_set_independent_pointers(callback),
    );
    bind_bool(
        &global,
        tx.clone(),
        &weak,
        &state,
        UiIntent::SetHopBypass,
        |g, callback| g.on_set_hop_bypass(callback),
    );
    bind_bool(
        &global,
        tx.clone(),
        &weak,
        &state,
        UiIntent::SetMultiHop,
        |g, callback| g.on_set_multi_hop(callback),
    );
    // "" from the picker means automatic.
    let backend_name = |name: SharedString| (!name.is_empty()).then(|| name.to_string());
    {
        let (tx, weak, state) = (tx.clone(), weak.clone(), Arc::clone(&state));
        global.on_set_capture_backend(move |name| {
            dispatch(
                UiIntent::SetCaptureBackend(backend_name(name)),
                &tx,
                &weak,
                &state,
            )
        });
    }
    {
        let (tx, weak, state) = (tx.clone(), weak.clone(), Arc::clone(&state));
        global.on_set_emulation_backend(move |name| {
            dispatch(
                UiIntent::SetEmulationBackend(backend_name(name)),
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
        global.on_cancel_transfer(move |transfer_id| match transfer_id.parse::<u64>() {
            Ok(transfer_id) => dispatch(UiIntent::CancelTransfer(transfer_id), &tx, &weak, &state),
            Err(_) => show_error(&weak, &state, "Invalid transfer id".into()),
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
    plugins::bind(app, &tx);
    #[cfg(not(target_os = "android"))]
    {
        // Surfaced so the interface can offer a way out when a daemon is
        // alive but unreachable and therefore holding the network port.
        let weak = app.as_weak();
        global.set_unreachable_daemons(crate::platform::daemon::unreachable_daemons().len() as i32);
        global.on_stop_unreachable_daemons(move || {
            let stopped = crate::platform::daemon::stop_unreachable();
            log::info!("stopped {stopped} unreachable service process(es)");
            if let Some(app) = weak.upgrade() {
                let global = app.global::<AppState>();
                global.set_unreachable_daemons(
                    crate::platform::daemon::unreachable_daemons().len() as i32,
                );
                global.set_daemon_start_error(Default::default());
            }
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
                    // Re-count here: a start that fails because the port is
                    // taken is exactly when the recovery control must appear.
                    global.set_unreachable_daemons(
                        crate::platform::daemon::unreachable_daemons().len() as i32,
                    );
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
pub(super) fn native_window<T>(
    app: &AppWindow,
    action: impl FnOnce(&winit::window::Window) -> T,
) -> Option<T> {
    app.window().with_winit_window(action)
}

pub(super) fn bind_window_callbacks(
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
            if page == "permissions" {
                if let Some(app) = weak.upgrade() {
                    app.global::<AppState>().invoke_refresh_flatpak_apps();
                }
            }
            if let Some(app) = weak.upgrade() {
                match page.as_str() {
                    "devices" => app.global::<AppState>().invoke_discover_peers(),
                    "clipboard" => app.global::<AppState>().invoke_history_refresh(),
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
                    let previous = value.sidebar_open;
                    value.sidebar_open = expanded;
                    if let Err(error) = save_presentation_settings(&value) {
                        value.sidebar_open = previous;
                        return Err(error.to_string());
                    }
                    app.set_sidebar_expanded(expanded);
                    Ok(())
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
                    log::debug!(
                        target: "syntra::ui::window",
                        "titlebar minimize clicked: minimized={} maximized={} size={:?}",
                        app.window().is_minimized(),
                        app.window().is_maximized(),
                        app.window().size()
                    );
                    // Go straight to winit. Wayland never reports when a
                    // window is restored from the taskbar, so Slint's cached
                    // "minimized" flag stays true forever after the first
                    // minimize: every later request becomes a no-op and the
                    // maximized state stops syncing as well.
                    #[cfg(not(target_os = "android"))]
                    if native_window(&app, |window| window.set_minimized(true)).is_some() {
                        return;
                    }
                    app.window().set_minimized(true);
                }
            }
        });
        app.on_toggle_maximize_window({
            let weak = app.as_weak();
            move || {
                if let Some(app) = weak.upgrade() {
                    // The compositor may have changed the state (double-click,
                    // tiling, keyboard shortcut); winit reflects that, Slint's
                    // cached flag may not.
                    #[cfg(not(target_os = "android"))]
                    let before = native_window(&app, |window| window.is_maximized())
                        .unwrap_or_else(|| app.window().is_maximized());
                    #[cfg(target_os = "android")]
                    let before = app.window().is_maximized();
                    let maximized = !before;
                    log::debug!(
                        target: "syntra::ui::window",
                        "titlebar maximize clicked: before_maximized={} minimized={} -> {}",
                        before,
                        app.window().is_minimized(),
                        maximized
                    );
                    #[cfg(not(target_os = "android"))]
                    if native_window(&app, |window| window.set_maximized(maximized)).is_none() {
                        app.window().set_maximized(maximized);
                    }
                    #[cfg(target_os = "android")]
                    app.window().set_maximized(maximized);
                    app.set_window_maximized(maximized);
                }
            }
        });
        app.on_begin_window_drag({
            let weak = app.as_weak();
            move || {
                if let Some(app) = weak.upgrade() {
                    if app.global::<Theme>().get_touch_input_active()
                        && crate::file_drop::begin_touch_window_drag(&app)
                    {
                        return;
                    }
                    // A pen has no winit button serial, so the window move is
                    // asked for with the serial of its last tip-down.
                    #[cfg(target_os = "linux")]
                    if crate::tablet::begin_pen_drag(&app) {
                        return;
                    }
                    native_window(&app, |window| {
                        if let Err(error) = window.drag_window() {
                            log::warn!("could not begin window drag: {error}");
                        }
                    });
                }
            }
        });
        app.on_close_window({
            move || {
                log::info!(
                    target: "syntra::ui::window",
                    "titlebar close clicked: exiting application; tray remains available for hide"
                );
                let _ = slint::quit_event_loop();
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

pub(super) fn send_request(
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

pub(super) fn show_error(
    weak: &slint::Weak<AppWindow>,
    state: &Arc<Mutex<AppViewState>>,
    message: String,
) {
    if let Ok(mut view) = state.lock() {
        view.diagnostics.last_error = Some(message.clone());
    }
    let _ = weak.upgrade_in_event_loop(move |app| {
        let global = app.global::<AppState>();
        global.set_diagnostics_error(message.clone().into());
    });
}

pub(super) fn start_platform_tray(
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

pub(super) fn install_close_behavior(
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
