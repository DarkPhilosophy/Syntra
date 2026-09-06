use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

use syntra_api::{
    ClipboardTransferDirection, FileReceiveSettings, FrontendRequest, ManualTransferState,
    ManualTransferStatus,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::app::{
    AppState, AppWindow, TransferDirection, TransferItem, TransferPeerItem, TransferState,
    Translations, device_presentation, format_bytes,
};
use crate::models::AppViewState;

fn tr(app: &AppWindow, key: &str) -> SharedString {
    let translations = app.global::<Translations>();
    translations.invoke_translate(key.into(), translations.get_revision())
}

pub(crate) fn project(app: &AppWindow, view: &AppViewState) {
    let global = app.global::<AppState>();
    global.set_manual_transfer_error(
        view.manual_transfer_error
            .clone()
            .unwrap_or_default()
            .into(),
    );
    global.set_file_receive_known(view.status.connected && view.file_receive_settings.is_some());
    global.set_file_receive_error(view.file_receive_error.clone().unwrap_or_default().into());
    #[cfg(target_os = "android")]
    {
        global.set_file_receive_known(false);
        global.set_file_receive_error(tr(app, "manual-transfer-platform-unavailable"));
        global.set_manual_transfer_error(tr(app, "manual-transfer-platform-unavailable"));
    }
    if !global.get_file_receive_dirty() {
        if let Some(settings) = &view.file_receive_settings {
            global.set_file_receive_auto_accept(settings.auto_accept);
            global.set_file_receive_directory(
                settings
                    .download_directory
                    .to_string_lossy()
                    .into_owned()
                    .into(),
            );
        }
    }
    global.set_file_receive_busy(view.file_receive_pending);

    let mut peers = BTreeMap::<String, bool>::new();
    for client in view.clients.values() {
        if let Some(fingerprint) = view.client_fingerprints.get(&client.handle) {
            if !fingerprint.is_empty() {
                peers
                    .entry(fingerprint.clone())
                    .and_modify(|connected| *connected |= client.alive)
                    .or_insert(client.alive);
            }
        }
    }
    for connected in view.connected_devices.values() {
        peers.insert(connected.fingerprint.clone(), true);
    }
    let peers = peers
        .into_iter()
        .map(|(fingerprint, connected)| {
            let (name, image) = device_presentation(app, view, &fingerprint, None);
            TransferPeerItem {
                fingerprint: fingerprint.into(),
                name,
                has_avatar: image.is_some(),
                avatar: image.unwrap_or_default(),
                connected: view.status.connected
                    && view.file_receive_settings.is_some()
                    && connected,
            }
        })
        .collect::<Vec<_>>();
    global.set_transfer_peers(ModelRc::new(VecModel::from(peers)));

    global.set_manual_offer_count(view.pending_file_offers.len().min(i32::MAX as usize) as i32);
    let offer = view
        .pending_file_offers
        .first()
        .filter(|_| view.status.connected);
    global.set_manual_offer_visible(offer.is_some());
    if let Some(offer) = offer {
        let id = offer.transfer_id.to_string();
        let changed = global.get_manual_offer_peer().as_str() != offer.peer_fingerprint
            || global.get_manual_offer_id().as_str() != id;
        if changed {
            global.set_manual_offer_peer(offer.peer_fingerprint.clone().into());
            global.set_manual_offer_id(id.into());
            global.set_manual_offer_directory(
                offer
                    .suggested_directory
                    .to_string_lossy()
                    .into_owned()
                    .into(),
            );
            global.set_manual_offer_busy(false);
        }
        let (name, image) = device_presentation(app, view, &offer.peer_fingerprint, None);
        global.set_manual_offer_peer_name(name);
        global.set_manual_offer_has_peer_image(image.is_some());
        global.set_manual_offer_peer_image(image.unwrap_or_default());
        global.set_manual_offer_file_name(offer.file_name.clone().into());
        global.set_manual_offer_size(format_bytes(offer.size).into());
        let error = view
            .manual_transfers
            .get(&(offer.peer_fingerprint.clone(), offer.transfer_id))
            .and_then(|status| status.error.clone())
            .unwrap_or_default();
        global.set_manual_offer_error(error.into());
    } else {
        global.set_manual_offer_peer("".into());
        global.set_manual_offer_id("".into());
        global.set_manual_offer_busy(false);
    }
}

pub(crate) fn transfer_item(
    app: &AppWindow,
    view: &AppViewState,
    transfer: &ManualTransferStatus,
) -> TransferItem {
    let (state, status_key, cancellable, show_progress) = match transfer.state {
        ManualTransferState::Offering => (
            TransferState::Active,
            "manual-transfer-offering",
            true,
            false,
        ),
        ManualTransferState::AwaitingAcceptance => (
            TransferState::Active,
            "manual-transfer-awaiting",
            true,
            false,
        ),
        ManualTransferState::Transferring => (TransferState::Active, "transfer-active", true, true),
        ManualTransferState::Completed => {
            (TransferState::Completed, "transfer-completed", false, true)
        }
        ManualTransferState::Declined => (
            TransferState::Cancelled,
            "manual-transfer-declined",
            false,
            false,
        ),
        ManualTransferState::Cancelled => {
            (TransferState::Cancelled, "transfer-cancelled", false, false)
        }
        ManualTransferState::Failed => (TransferState::Failed, "transfer-failed", false, false),
    };
    let (peer_label, peer_image) = device_presentation(app, view, &transfer.peer_fingerprint, None);
    let destination_label = transfer
        .destination
        .as_ref()
        .map(|path| {
            let key = if transfer.state == ManualTransferState::Completed {
                "manual-transfer-saved-to"
            } else {
                "manual-transfer-destination"
            };
            format!("{} {}", tr(app, key), path.display())
        })
        .unwrap_or_default();
    TransferItem {
        transfer_id: transfer.transfer_id.to_string().into(),
        file_id: "".into(),
        name: transfer.file_name.clone().into(),
        kind_label: tr(app, "manual-transfer-kind"),
        direction: match transfer.direction {
            ClipboardTransferDirection::Sending => TransferDirection::Outgoing,
            ClipboardTransferDirection::Receiving => TransferDirection::Incoming,
        },
        transferred_label: format_bytes(transfer.transferred).into(),
        total_label: format_bytes(transfer.size).into(),
        rate_label: "".into(),
        progress: if transfer.size == 0 {
            if transfer.state == ManualTransferState::Completed {
                1.0
            } else {
                0.0
            }
        } else {
            (transfer.transferred as f64 / transfer.size as f64).clamp(0.0, 1.0) as f32
        },
        state,
        error_message: transfer.error.clone().unwrap_or_default().into(),
        cancellable,
        manual: true,
        peer_fingerprint: transfer.peer_fingerprint.clone().into(),
        peer_label,
        has_peer_image: peer_image.is_some(),
        peer_image: peer_image.unwrap_or_default(),
        status_label: tr(app, status_key),
        destination_label: destination_label.into(),
        show_progress,
    }
}

#[cfg(not(target_os = "android"))]
mod desktop {
    use super::*;
    use crate::file_drop::{self, FileDropEvent, FileDropGuard};
    use slint::winit_030::WinitWindowAccessor;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone, Copy, Default)]
    struct Bounds {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    }
    impl Bounds {
        fn contains(&self, point: slint::LogicalPosition) -> bool {
            self.width > 0.0
                && self.height > 0.0
                && point.x >= self.x
                && point.y >= self.y
                && point.x < self.x + self.width
                && point.y < self.y + self.height
        }
    }
    #[derive(Default)]
    struct DropTargets {
        viewport: Bounds,
        peers: BTreeMap<String, Bounds>,
    }
    impl DropTargets {
        fn target(&self, app: &AppWindow, point: slint::LogicalPosition) -> Option<String> {
            if app.get_selected_page() != "transfers"
                || app.global::<AppState>().get_manual_offer_visible()
                || !self.viewport.contains(point)
            {
                return None;
            }
            app.global::<AppState>()
                .get_transfer_peers()
                .iter()
                .find(|peer| {
                    peer.connected
                        && self
                            .peers
                            .get(peer.fingerprint.as_str())
                            .is_some_and(|bounds| bounds.contains(point))
                })
                .map(|peer| peer.fingerprint.to_string())
        }
    }

    fn error(app: &AppWindow, state: &Arc<Mutex<AppViewState>>, message: String) {
        if let Ok(mut state) = state.lock() {
            state.manual_transfer_error = Some(message.clone());
        }
        app.global::<AppState>()
            .set_manual_transfer_error(message.into());
    }
    fn send(
        app: &AppWindow,
        state: &Arc<Mutex<AppViewState>>,
        tx: &mpsc::Sender<FrontendRequest>,
        request: FrontendRequest,
    ) {
        if let Ok(mut view) = state.lock() {
            view.manual_transfer_error = None;
        }
        app.global::<AppState>()
            .set_manual_transfer_error("".into());
        if tx.send(request).is_err() {
            error(
                app,
                state,
                tr(app, "manual-transfer-service-offline").to_string(),
            );
        }
    }
    fn offer_key(app: &AppWindow, state: &Arc<Mutex<AppViewState>>) -> Option<(String, u64)> {
        let global = app.global::<AppState>();
        let peer = global.get_manual_offer_peer().to_string();
        let id = global.get_manual_offer_id().parse::<u64>().ok()?;
        state
            .lock()
            .ok()?
            .pending_file_offers
            .iter()
            .any(|offer| offer.peer_fingerprint == peer && offer.transfer_id == id)
            .then_some((peer, id))
    }

    pub(crate) struct ManualUiGuard {
        initialization: Rc<slint::Timer>,
        native: Rc<RefCell<Option<FileDropGuard>>>,
    }
    impl Drop for ManualUiGuard {
        fn drop(&mut self) {
            self.initialization.stop();
            self.native.borrow_mut().take();
        }
    }

    pub(crate) fn bind(
        app: &AppWindow,
        tx: mpsc::Sender<FrontendRequest>,
        state: Arc<Mutex<AppViewState>>,
    ) -> ManualUiGuard {
        let global = app.global::<AppState>();
        let targets = Rc::new(RefCell::new(DropTargets::default()));
        let registry = Rc::clone(&targets);
        global.on_register_manual_drop_target(move |peer, x, y, width, height| {
            registry.borrow_mut().peers.insert(
                peer.to_string(),
                Bounds {
                    x,
                    y,
                    width,
                    height,
                },
            );
        });
        let registry = Rc::clone(&targets);
        global.on_register_manual_drop_viewport(move |x, y, width, height| {
            registry.borrow_mut().viewport = Bounds {
                x,
                y,
                width,
                height,
            };
        });
        global.set_manual_drop_generation(global.get_manual_drop_generation().wrapping_add(1));

        let weak = app.as_weak();
        let sender = tx.clone();
        let model = Arc::clone(&state);
        global.on_choose_manual_files(move |fingerprint| {
            let Some(app) = weak.upgrade() else { return };
            let global = app.global::<AppState>();
            if global.get_manual_picker_busy() {
                return;
            }
            if global.get_manual_offer_visible() {
                error(
                    &app,
                    &model,
                    tr(&app, "manual-transfer-finish-dialog").to_string(),
                );
                return;
            }
            let Some(peer) = global
                .get_transfer_peers()
                .iter()
                .find(|peer| peer.connected && peer.fingerprint == fingerprint)
            else {
                error(
                    &app,
                    &model,
                    tr(&app, "manual-transfer-connect-peer").to_string(),
                );
                return;
            };
            global.set_manual_picker_busy(true);
            let title = format!(
                "{}: {}",
                tr(&app, "manual-transfer-picker-title"),
                peer.name
            );
            let weak = weak.clone();
            let sender = sender.clone();
            let model = Arc::clone(&model);
            if let Err(failure) = slint::spawn_local(async move {
                let files = rfd::AsyncFileDialog::new()
                    .set_title(title)
                    .pick_files()
                    .await;
                let Some(app) = weak.upgrade() else { return };
                app.global::<AppState>().set_manual_picker_busy(false);
                if let Some(files) = files {
                    let paths = files
                        .into_iter()
                        .map(|file| file.path().to_path_buf())
                        .collect::<Vec<_>>();
                    if !paths.is_empty() {
                        send(
                            &app,
                            &model,
                            &sender,
                            FrontendRequest::SendFiles {
                                peer_fingerprint: fingerprint.to_string(),
                                paths,
                            },
                        );
                    }
                }
            }) {
                global.set_manual_picker_busy(false);
                global.set_manual_transfer_error(failure.to_string().into());
            }
        });

        let weak = app.as_weak();
        let sender = tx.clone();
        let model = Arc::clone(&state);
        global.on_cancel_manual_transfer(move |peer, id| {
            if let Some(app) = weak.upgrade() {
                match id.parse::<u64>() {
                    Ok(transfer_id) => send(
                        &app,
                        &model,
                        &sender,
                        FrontendRequest::CancelManualTransfer {
                            peer_fingerprint: peer.to_string(),
                            transfer_id,
                        },
                    ),
                    Err(_) => error(
                        &app,
                        &model,
                        tr(&app, "manual-transfer-invalid-id").to_string(),
                    ),
                }
            }
        });

        let weak = app.as_weak();
        let sender = tx.clone();
        let model = Arc::clone(&state);
        global.on_accept_incoming_file(move || {
            let Some(app) = weak.upgrade() else { return };
            let global = app.global::<AppState>();
            if global.get_manual_offer_busy() {
                return;
            }
            let Some((peer_fingerprint, transfer_id)) = offer_key(&app, &model) else {
                return;
            };
            let directory = PathBuf::from(global.get_manual_offer_directory().as_str());
            if !directory.is_absolute() {
                global.set_manual_offer_error(tr(&app, "manual-transfer-absolute-folder"));
                return;
            }
            global.set_manual_offer_busy(true);
            send(
                &app,
                &model,
                &sender,
                FrontendRequest::AcceptFileTransfer {
                    peer_fingerprint,
                    transfer_id,
                    destination_directory: directory,
                },
            );
        });
        let weak = app.as_weak();
        let sender = tx.clone();
        let model = Arc::clone(&state);
        global.on_decline_incoming_file(move || {
            let Some(app) = weak.upgrade() else { return };
            if let Some((peer_fingerprint, transfer_id)) = offer_key(&app, &model) {
                app.global::<AppState>().set_manual_offer_busy(true);
                send(
                    &app,
                    &model,
                    &sender,
                    FrontendRequest::DeclineFileTransfer {
                        peer_fingerprint,
                        transfer_id,
                    },
                );
            }
        });

        let weak = app.as_weak();
        let model = Arc::clone(&state);
        global.on_choose_incoming_directory(move || {
            let Some(app) = weak.upgrade() else { return };
            let global = app.global::<AppState>();
            if global.get_manual_offer_picker_busy() || global.get_manual_offer_busy() {
                return;
            }
            let Some((peer, id)) = offer_key(&app, &model) else {
                return;
            };
            let directory = global.get_manual_offer_directory().to_string();
            let title = tr(&app, "manual-transfer-choose-folder").to_string();
            global.set_manual_offer_picker_busy(true);
            let weak = weak.clone();
            if let Err(failure) = slint::spawn_local(async move {
                let folder = rfd::AsyncFileDialog::new()
                    .set_title(title)
                    .set_directory(directory)
                    .pick_folder()
                    .await;
                let Some(app) = weak.upgrade() else { return };
                let global = app.global::<AppState>();
                global.set_manual_offer_picker_busy(false);
                if global.get_manual_offer_peer().as_str() == peer
                    && global.get_manual_offer_id().as_str() == id.to_string()
                {
                    if let Some(folder) = folder {
                        global.set_manual_offer_directory(
                            folder.path().to_string_lossy().into_owned().into(),
                        );
                        global.set_manual_offer_error("".into());
                    }
                }
            }) {
                global.set_manual_offer_picker_busy(false);
                global.set_manual_offer_error(failure.to_string().into());
            }
        });

        let weak = app.as_weak();
        global.on_choose_download_directory(move || {
            let Some(app) = weak.upgrade() else { return };
            let global = app.global::<AppState>();
            if global.get_file_receive_picker_busy()
                || global.get_file_receive_busy()
                || global.get_manual_offer_visible()
            {
                return;
            }
            let directory = global.get_file_receive_directory().to_string();
            let title = tr(&app, "manual-transfer-choose-folder").to_string();
            global.set_file_receive_picker_busy(true);
            let weak = weak.clone();
            if let Err(failure) = slint::spawn_local(async move {
                let folder = rfd::AsyncFileDialog::new()
                    .set_title(title)
                    .set_directory(directory)
                    .pick_folder()
                    .await;
                let Some(app) = weak.upgrade() else { return };
                let global = app.global::<AppState>();
                global.set_file_receive_picker_busy(false);
                if let Some(folder) = folder {
                    global.set_file_receive_directory(
                        folder.path().to_string_lossy().into_owned().into(),
                    );
                    global.set_file_receive_dirty(true);
                }
            }) {
                global.set_file_receive_picker_busy(false);
                global.set_file_receive_error(failure.to_string().into());
            }
        });
        let weak = app.as_weak();
        let sender = tx.clone();
        let model = Arc::clone(&state);
        global.on_save_file_receive_settings(move || {
            let Some(app) = weak.upgrade() else { return };
            let global = app.global::<AppState>();
            if global.get_file_receive_busy() || global.get_manual_offer_visible() {
                return;
            }
            let directory = PathBuf::from(global.get_file_receive_directory().as_str());
            if !directory.is_absolute() {
                global.set_file_receive_error(tr(&app, "manual-transfer-absolute-folder"));
                return;
            }
            if let Ok(mut view) = model.lock() {
                view.file_receive_pending = true;
                view.file_receive_error = None;
            }
            global.set_file_receive_busy(true);
            send(
                &app,
                &model,
                &sender,
                FrontendRequest::SetFileReceiveSettings(FileReceiveSettings {
                    auto_accept: global.get_file_receive_auto_accept(),
                    download_directory: directory,
                }),
            );
        });

        // Winit creates its native window on the first event-loop turn, not
        // necessarily during ComponentHandle::show().
        let initialization = Rc::new(slint::Timer::default());
        let native = Rc::new(RefCell::new(None));
        let weak_timer = Rc::downgrade(&initialization);
        let weak_native = Rc::downgrade(&native);
        let weak = app.as_weak();
        let started = std::time::Instant::now();
        initialization.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(25),
            move || {
                let Some(timer) = weak_timer.upgrade() else {
                    return;
                };
                let Some(app) = weak.upgrade() else {
                    timer.stop();
                    return;
                };
                let Some(holder) = weak_native.upgrade() else {
                    timer.stop();
                    return;
                };
                let window_exists = app.window().with_winit_window(|_| true).unwrap_or(false);
                if !window_exists && started.elapsed() < std::time::Duration::from_secs(5) {
                    return;
                }
                timer.stop();
                app.global::<AppState>()
                    .set_manual_drop_supported(file_drop::supported(&app));
                let event_window = app.as_weak();
                let targets = Rc::clone(&targets);
                let state = Arc::clone(&state);
                let tx = tx.clone();
                let guard = file_drop::install(&app, move |event| {
                    let Some(app) = event_window.upgrade() else {
                        return;
                    };
                    match event {
                        FileDropEvent::Hover(position) => {
                            let peer = position
                                .and_then(|point| targets.borrow().target(&app, point))
                                .unwrap_or_default();
                            app.global::<AppState>().set_manual_hover_peer(peer.into());
                        }
                        FileDropEvent::Drop { path, position } => {
                            let peer = targets.borrow().target(&app, position);
                            app.global::<AppState>().set_manual_hover_peer("".into());
                            if let Some(peer_fingerprint) = peer {
                                send(
                                    &app,
                                    &state,
                                    &tx,
                                    FrontendRequest::SendFiles {
                                        peer_fingerprint,
                                        paths: vec![path],
                                    },
                                );
                            } else {
                                error(
                                    &app,
                                    &state,
                                    tr(&app, "manual-transfer-drop-error").to_string(),
                                );
                            }
                        }
                        FileDropEvent::Error(message) => {
                            app.global::<AppState>().set_manual_drop_supported(false);
                            error(&app, &state, message);
                        }
                    }
                });
                holder.replace(Some(guard));
            },
        );
        ManualUiGuard {
            initialization,
            native,
        }
    }
}

#[cfg(not(target_os = "android"))]
pub(crate) use desktop::bind;
