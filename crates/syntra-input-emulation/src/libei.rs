use futures::{StreamExt, future};
use std::{
    env, fs, io,
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use ashpd::{
    AppID,
    desktop::{
        PersistMode, Session,
        clipboard::{Clipboard, RequestClipboardOptions, SetSelectionOptions},
        remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions},
    },
    documents::{Documents, Permission},
};
use async_trait::async_trait;

use reis::{
    ei::{
        self, Button, Keyboard, Pointer, Scroll, button::ButtonState, handshake::ContextType,
        keyboard::KeyState,
    },
    event::{self, Connection, DeviceCapability, DeviceEvent, EiEvent, SeatEvent},
    tokio::EiConvertEventStream,
};

use syntra_input_event::{Event, KeyboardEvent, PointerEvent};

use crate::error::EmulationError;

use super::{Emulation, EmulationHandle, error::LibeiEmulationCreationError};

#[derive(Clone, Default)]
struct Devices {
    pointer: Arc<RwLock<Option<(ei::Device, ei::Pointer)>>>,
    scroll: Arc<RwLock<Option<(ei::Device, ei::Scroll)>>>,
    button: Arc<RwLock<Option<(ei::Device, ei::Button)>>>,
    keyboard: Arc<RwLock<Option<(ei::Device, ei::Keyboard)>>>,
}

struct ClipboardCommand {
    contents: Vec<(String, Vec<u8>)>,
    published: oneshot::Sender<Result<(), ashpd::Error>>,
}

pub(crate) struct LibeiEmulation {
    context: ei::Context,
    conn: event::Connection,
    devices: Devices,
    ei_task: JoinHandle<()>,
    error: Arc<Mutex<Option<EmulationError>>>,
    libei_error: Arc<AtomicBool>,
    _remote_desktop: RemoteDesktop,
    session: Arc<Session<RemoteDesktop>>,
    clipboard_rx: Option<mpsc::Receiver<(String, Vec<u8>)>>,
    clipboard_task: Option<JoinHandle<()>>,
    clipboard_command_tx: Option<mpsc::Sender<ClipboardCommand>>,
}

/// Get the path to the RemoteDesktop token file
fn get_token_file_path() -> PathBuf {
    let cache_dir = env::var("XDG_CACHE_HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = env::var("HOME").expect("HOME not set");
            PathBuf::from(home).join(".cache")
        });

    cache_dir.join("lan-mouse").join("remote-desktop.token")
}

fn decode_file_uri(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Some(PathBuf::from(String::from_utf8(decoded).ok()?))
}

fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            uri.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    uri
}

async fn export_flatpak_file_uris(mut contents: Vec<(String, Vec<u8>)>) -> Vec<(String, Vec<u8>)> {
    let Some((_, gnome_data)) = contents
        .iter()
        .find(|(mime, _)| mime == "x-special/gnome-copied-files")
    else {
        return contents;
    };
    let Ok(gnome_text) = std::str::from_utf8(gnome_data) else {
        return contents;
    };
    let mut lines = gnome_text.lines();
    let operation = lines.next().unwrap_or("copy");
    let original_uris = lines.filter(|line| !line.is_empty()).collect::<Vec<_>>();
    if original_uris.is_empty() {
        return contents;
    }

    let Ok(documents) = Documents::new().await else {
        return contents;
    };
    let Ok(mount_point) = documents.mount_point().await else {
        return contents;
    };
    let app_ids = [
        "org.gnome.Nautilus",
        "org.gnome.NautilusPreviewer",
        "org.kde.dolphin",
    ]
    .into_iter()
    .filter_map(|app_id| AppID::from_str(app_id).ok())
    .collect::<Vec<_>>();
    if app_ids.is_empty() {
        return contents;
    }
    let mut exported = Vec::with_capacity(original_uris.len());
    for uri in original_uris {
        let Some(path) = decode_file_uri(uri) else {
            return contents;
        };
        let Ok(file) = fs::File::open(&path) else {
            return contents;
        };
        let Ok(document_id) = documents.add(&file, true, false).await else {
            return contents;
        };
        for app_id in &app_ids {
            if documents
                .grant_permissions(document_id.clone(), app_id, &[Permission::Read])
                .await
                .is_err()
            {
                return contents;
            }
        }
        let Some(name) = path.file_name() else {
            return contents;
        };
        exported.push(file_uri(
            &mount_point.as_ref().join(document_id.as_ref()).join(name),
        ));
    }

    let uri_list = exported.join("\r\n") + "\r\n";
    let gnome = format!("{operation}\n{}\n", exported.join("\n"));
    for (mime, data) in &mut contents {
        match mime.as_str() {
            "text/uri-list" => *data = uri_list.as_bytes().to_vec(),
            "x-special/gnome-copied-files" => *data = gnome.as_bytes().to_vec(),
            _ => {}
        }
    }
    log::info!(
        "clipboard-trace event=document-export outcome=success files={}",
        exported.len()
    );
    contents
}

/// Read the RemoteDesktop token from file
fn read_token() -> Option<String> {
    let token_path = get_token_file_path();
    match fs::read_to_string(&token_path) {
        Ok(token) => Some(token.trim().to_string()),
        Err(_) => None,
    }
}

/// Write the RemoteDesktop token to file
fn write_token(token: &str) -> io::Result<()> {
    let token_path = get_token_file_path();
    if let Some(parent) = token_path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(&token_path, token)?;
    Ok(())
}

async fn get_ei_fd() -> Result<
    (
        RemoteDesktop,
        Session<RemoteDesktop>,
        OwnedFd,
        Option<Clipboard>,
    ),
    ashpd::Error,
> {
    let remote_desktop = RemoteDesktop::new().await?;

    let restore_token = read_token();

    log::debug!("creating session ...");
    let session = remote_desktop.create_session(Default::default()).await?;

    log::debug!("selecting devices ...");
    let options = SelectDevicesOptions::default()
        .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(restore_token.as_deref());
    remote_desktop.select_devices(&session, options).await?;
    let clipboard = Clipboard::new().await.ok();
    let clipboard_requested = match clipboard.as_ref() {
        Some(clipboard) => clipboard
            .request(&session, RequestClipboardOptions::default())
            .await
            .is_ok(),
        None => false,
    };

    log::info!("requesting permission for input emulation");
    let start_response = match remote_desktop
        .start(&session, None, Default::default())
        .await
    {
        Ok(request) => request.response(),
        Err(error) => Err(error),
    };
    let start_response = match start_response {
        Ok(response) => response,
        Err(error) => {
            if restore_token.is_some() {
                if let Err(remove_error) = fs::remove_file(get_token_file_path()) {
                    if remove_error.kind() != io::ErrorKind::NotFound {
                        log::warn!(
                            "failed to discard unusable RemoteDesktop token: {remove_error}"
                        );
                    }
                }
            }
            return Err(error);
        }
    };

    // The restore token is only valid once, we need to re-save it each time
    if let Some(token_str) = start_response.restore_token() {
        if let Err(e) = write_token(token_str) {
            log::warn!("failed to save RemoteDesktop token: {}", e);
        }
    }

    let fd = remote_desktop
        .connect_to_eis(&session, Default::default())
        .await?;
    let clipboard = (clipboard_requested && start_response.is_clipboard_enabled())
        .then_some(clipboard)
        .flatten();
    log::info!(
        "native RemoteDesktop clipboard enabled={}",
        clipboard.is_some()
    );
    Ok((remote_desktop, session, fd, clipboard))
}

impl LibeiEmulation {
    pub(crate) async fn new() -> Result<Self, LibeiEmulationCreationError> {
        let (_remote_desktop, session, eifd, clipboard) = get_ei_fd().await?;
        let session = Arc::new(session);
        let (clipboard_tx, clipboard_rx) = mpsc::channel(8);
        let (clipboard_command_tx, mut clipboard_command_rx) = mpsc::channel::<ClipboardCommand>(8);
        let clipboard_task = match clipboard {
            Some(clipboard) => {
                let clipboard_session = Arc::clone(&session);
                Some(tokio::task::spawn_local(async move {
                    let Ok(changed) = clipboard
                        .receive_selection_owner_changed::<RemoteDesktop>()
                        .await
                    else {
                        return;
                    };
                    let Ok(transfers) = clipboard
                        .receive_selection_transfer::<RemoteDesktop>()
                        .await
                    else {
                        return;
                    };
                    futures::pin_mut!(changed);
                    futures::pin_mut!(transfers);
                    // Retain the complete offer until a later SetSelection succeeds.
                    // GNOME may request either file MIME type long after publication.
                    let mut offered = Vec::<(String, Vec<u8>)>::new();
                    let mut trace_seq = 0_u64;
                    'clipboard: loop {
                        tokio::select! {
                            command = clipboard_command_rx.recv() => {
                                let Some(command) = command else { break };
                                trace_seq = trace_seq.wrapping_add(1);
                                let command_seq = trace_seq;
                                let offered_summary = command
                                    .contents
                                    .iter()
                                    .map(|(mime, data)| format!("{mime}:{}B", data.len()))
                                    .collect::<Vec<_>>()
                                    .join(",");
                                log::info!(
                                    "clipboard-trace event=publication-request direction=outbound \
                                     request_id={command_seq} offers=[{offered_summary}]"
                                );
                                let mime_types = command
                                    .contents
                                    .iter()
                                    .map(|(mime, _)| mime.as_str())
                                    .collect::<Vec<_>>();
                                let options =
                                    SetSelectionOptions::default().set_mime_types(&mime_types);
                                let result = clipboard.set_selection(&clipboard_session, options).await;
                                match result {
                                    Ok(()) => {
                                        offered = command.contents;
                                        let _ = command.published.send(Ok(()));
                                        log::info!(
                                            "clipboard-trace event=publication-result direction=outbound \
                                             request_id={command_seq} outcome=success \
                                             retained=[{offered_summary}]"
                                        );
                                    }
                                    Err(error) => {
                                        let message = error.to_string();
                                        let _ = command.published.send(Err(error));
                                        log::warn!(
                                            "clipboard-trace event=publication-result direction=outbound \
                                             request_id={command_seq} outcome=failed \
                                             offers=[{offered_summary}] error_class=portal error={message}"
                                        );
                                    }
                                }
                            }
                            transfer = transfers.next() => {
                                let Some((transfer_session, mime_type, serial)) = transfer else { break };
                                trace_seq = trace_seq.wrapping_add(1);
                                let transfer_seq = trace_seq;
                                log::info!(
                                    "clipboard-trace event=selection-transfer-request direction=inbound \
                                     request_id={transfer_seq} mime={mime_type} serial={serial} \
                                     retained_offers={}",
                                    offered.len()
                                );
                                let data = offered
                                    .iter()
                                    .find_map(|(offered_mime, data)| {
                                        (offered_mime == &mime_type).then_some(data.as_slice())
                                    });
                                let success = if let Some(data) = data {
                                    match clipboard
                                        .selection_write(&transfer_session, serial)
                                        .await
                                    {
                                        Ok(fd) => {
                                            let fd: std::os::fd::OwnedFd = fd.into();
                                            let mut file = tokio::fs::File::from_std(
                                                std::fs::File::from(fd),
                                            );
                                            let result = file.write_all(data).await;
                                            log::info!(
                                                "clipboard-trace event=selection-transfer-write direction=outbound \
                                                 request_id={transfer_seq} mime={mime_type} serial={serial} \
                                                 bytes={} outcome={}",
                                                data.len(),
                                                if result.is_ok() { "success" } else { "failed" }
                                            );
                                            drop(file);
                                            if let Err(error) = &result {
                                                log::warn!(
                                                    "failed to write portal selection transfer \
                                                     for {mime_type}: {error}"
                                                );
                                            }
                                            result.is_ok()
                                        }
                                        Err(error) => {
                                            log::warn!(
                                                "clipboard-trace event=selection-transfer-write direction=outbound \
                                                 request_id={transfer_seq} mime={mime_type} serial={serial} \
                                                 bytes=0 outcome=open-failed error_class=portal"
                                            );
                                            log::warn!(
                                                "failed to open portal selection transfer \
                                                 for {mime_type}: {error}"
                                            );
                                            false
                                        }
                                    }
                                } else {
                                    log::warn!(
                                        "clipboard-trace event=selection-transfer-write direction=outbound \
                                         request_id={transfer_seq} mime={mime_type} serial={serial} \
                                         bytes=0 outcome=rejected reason=unoffered-mime"
                                    );
                                    false
                                };
                                if let Err(error) = clipboard
                                    .selection_write_done(&transfer_session, serial, success)
                                    .await
                                {
                                    log::warn!(
                                        "clipboard-trace event=selection-transfer-done direction=outbound \
                                         request_id={transfer_seq} mime={mime_type} serial={serial} \
                                         success={success} outcome=failed error_class=portal"
                                    );
                                    log::warn!(
                                        "failed to finish portal selection transfer \
                                         for {mime_type}: {error}"
                                    );
                                } else {
                                    log::info!(
                                        "clipboard-trace event=selection-transfer-done direction=outbound \
                                         request_id={transfer_seq} mime={mime_type} serial={serial} \
                                         success={success} outcome=success"
                                    );
                                }
                            }
                            owner = changed.next() => {
                                let Some((changed_session, selection)) = owner else { break };
                                trace_seq = trace_seq.wrapping_add(1);
                                let owner_seq = trace_seq;
                                log::info!(
                                    "clipboard-trace event=owner-changed direction=local \
                                     request_id={owner_seq} session_is_owner={:?} mimes=[{}] \
                                     retained_offers={}",
                                    selection.session_is_owner(),
                                    selection.mime_types().join(","),
                                    offered.len()
                                );
                                if selection.session_is_owner() == Some(true) {
                                    log::info!(
                                        "clipboard-trace event=owner-change-ignored direction=local \
                                         request_id={owner_seq} reason=session-owner"
                                    );
                                    continue;
                                }
                                let file_owner = [
                                    "x-special/gnome-copied-files",
                                    "text/uri-list",
                                ]
                                .iter()
                                .any(|candidate| {
                                    selection.mime_types().iter().any(|mime| mime == *candidate)
                                });
                                if file_owner {
                                    let replaced_offers = offered.len();
                                    offered.clear();
                                    log::info!(
                                        "clipboard-trace event=ownership-replacement direction=local \
                                         request_id={owner_seq} owner_kind=file \
                                         replaced_offers={replaced_offers} outcome=stale-offers-suppressed"
                                    );
                                }
                                let Some(mime_type) = [
                                    "x-special/gnome-copied-files",
                                    "text/uri-list",
                                    "image/png",
                                    "image/jpeg",
                                    "image/jpg",
                                    "text/plain;charset=utf-8",
                                    "text/plain",
                                    "UTF8_STRING",
                                ]
                                .iter()
                                .find(|candidate| {
                                    selection.mime_types().iter().any(|mime| mime == **candidate)
                                })
                                else {
                                    log::info!(
                                        "clipboard-trace event=owner-change-ignored direction=local \
                                         request_id={owner_seq} reason=no-supported-mime"
                                    );
                                    continue;
                                };
                                let read_started = Instant::now();
                                log::info!(
                                    "clipboard-trace event=mime-read-start direction=local \
                                     request_id={owner_seq} mime={mime_type}"
                                );
                                let Ok(fd) = clipboard.selection_read(&changed_session, mime_type).await else {
                                    log::warn!(
                                        "clipboard-trace event=mime-read-result direction=local \
                                         request_id={owner_seq} mime={mime_type} bytes=0 \
                                         outcome=open-failed retry_class=terminal elapsed_ms={}",
                                        read_started.elapsed().as_millis()
                                    );
                                    continue;
                                };
                                let max_bytes = if matches!(
                                    *mime_type,
                                    "image/png" | "image/jpeg" | "image/jpg"
                                ) {
                                    64 * 1024 * 1024
                                } else {
                                    1024 * 1024
                                };
                                let fd: std::os::fd::OwnedFd = fd.into();
                                let mut file = tokio::fs::File::from_std(std::fs::File::from(fd))
                                    .take(max_bytes as u64 + 1);
                                let mut data = Vec::new();
                                let mut read_attempt = 0_u8;
                                loop {
                                    match file.read_to_end(&mut data).await {
                                        Ok(_) => break,
                                        Err(error)
                                            if error.kind() == io::ErrorKind::WouldBlock
                                                && read_attempt < 5 =>
                                        {
                                            read_attempt += 1;
                                            log::info!(
                                                "clipboard-trace event=mime-read-retry direction=local \
                                                 request_id={owner_seq} mime={mime_type} attempt={read_attempt}"
                                            );
                                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                                        }
                                        Err(error) => {
                                            log::warn!(
                                                "clipboard-trace event=mime-read-result direction=local \
                                                 request_id={owner_seq} mime={mime_type} bytes={} \
                                                 outcome=failed retry_class=terminal error_kind={:?} \
                                                 elapsed_ms={}",
                                                data.len(),
                                                error.kind(),
                                                read_started.elapsed().as_millis()
                                            );
                                            continue 'clipboard;
                                        }
                                    }
                                }
                                if data.len() > max_bytes {
                                    log::warn!(
                                        "clipboard-trace event=mime-read-result direction=local \
                                         request_id={owner_seq} mime={mime_type} bytes={} \
                                         outcome=rejected reason=too-large limit={max_bytes} \
                                         retry_class=terminal elapsed_ms={}",
                                        data.len(),
                                        read_started.elapsed().as_millis()
                                    );
                                    continue;
                                }
                                log::info!(
                                    "clipboard-trace event=mime-read-result direction=local \
                                     request_id={owner_seq} mime={mime_type} bytes={} \
                                     outcome=success retry_class=none elapsed_ms={}",
                                    data.len(),
                                    read_started.elapsed().as_millis()
                                );
                                // Some portal implementations omit
                                // session_is_owner for our own SetSelection
                                // notification. Never feed our retained offer
                                // back into Lan Mouse as a new local copy:
                                // that echo cancels and replaces the remote
                                // offer before Files can enable Paste.
                                if offered.iter().any(|(offered_mime, offered_data)| {
                                    offered_mime == *mime_type && offered_data == &data
                                }) {
                                    log::info!(
                                        "clipboard-trace event=owner-change-ignored direction=local \
                                         request_id={owner_seq} mime={mime_type} bytes={} \
                                         reason=retained-offer-echo",
                                        data.len()
                                    );
                                    continue;
                                }
                                log::info!(
                                    "clipboard-trace event=local-selection-forwarded direction=outbound \
                                     request_id={owner_seq} mime={mime_type} bytes={}",
                                    data.len()
                                );
                                if clipboard_tx
                                    .send(((*mime_type).to_string(), data))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                }))
            }
            None => None,
        };
        let stream = UnixStream::from(eifd);
        stream.set_nonblocking(true)?;
        let context = ei::Context::new(stream)?;
        let (conn, events) = context
            .handshake_tokio("de.feschber.LanMouse", ContextType::Sender)
            .await?;
        let devices = Devices::default();
        let libei_error = Arc::new(AtomicBool::default());
        let error = Arc::new(Mutex::new(None));
        let ei_handler = ei_task(
            events,
            conn.clone(),
            context.clone(),
            devices.clone(),
            libei_error.clone(),
            error.clone(),
        );
        let ei_task = tokio::task::spawn_local(ei_handler);

        Ok(Self {
            context,
            conn,
            devices,
            ei_task,
            error,
            libei_error,
            _remote_desktop,
            session,
            clipboard_rx: Some(clipboard_rx),
            clipboard_command_tx: clipboard_task.as_ref().map(|_| clipboard_command_tx),
            clipboard_task,
        })
    }
}

impl Drop for LibeiEmulation {
    fn drop(&mut self) {
        self.ei_task.abort();
        if let Some(task) = self.clipboard_task.take() {
            task.abort();
        }
    }
}

#[async_trait]
impl Emulation for LibeiEmulation {
    fn take_clipboard_receiver(&mut self) -> Option<mpsc::Receiver<(String, Vec<u8>)>> {
        self.clipboard_rx.take()
    }
    async fn set_file_clipboard(
        &mut self,
        contents: Vec<(String, Vec<u8>)>,
    ) -> Result<(), EmulationError> {
        let contents = export_flatpak_file_uris(contents).await;
        if self
            .clipboard_task
            .as_ref()
            .map_or(true, JoinHandle::is_finished)
        {
            return Err(
                io::Error::new(io::ErrorKind::BrokenPipe, "portal clipboard task stopped").into(),
            );
        }
        let (published, result) = oneshot::channel();
        self.clipboard_command_tx
            .as_ref()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::Unsupported, "portal clipboard unavailable")
            })?
            .send(ClipboardCommand {
                contents,
                published,
            })
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "portal clipboard task stopped")
            })?;
        result.await.map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "portal clipboard task stopped")
        })??;
        Ok(())
    }

    fn healthy(&self) -> bool {
        !self.libei_error.load(Ordering::SeqCst)
    }

    async fn consume(
        &mut self,
        event: Event,
        _handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;
        if self.libei_error.load(Ordering::SeqCst) {
            // don't break sending additional events but signal error
            if let Some(e) = self.error.lock().unwrap().take() {
                return Err(e);
            }
        }
        match event {
            Event::Pointer(p) => match p {
                PointerEvent::Motion { time: _, dx, dy } => {
                    let pointer_device = self.devices.pointer.read().unwrap();
                    if let Some((d, p)) = pointer_device.as_ref() {
                        p.motion_relative(dx as f32, dy as f32);
                        d.frame(self.conn.serial(), now);
                    }
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => {
                    let button_device = self.devices.button.read().unwrap();
                    if let Some((d, b)) = button_device.as_ref() {
                        b.button(
                            button,
                            match state {
                                0 => ButtonState::Released,
                                _ => ButtonState::Press,
                            },
                        );
                        d.frame(self.conn.serial(), now);
                    }
                }
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => {
                    let scroll_device = self.devices.scroll.read().unwrap();
                    if let Some((d, s)) = scroll_device.as_ref() {
                        match axis {
                            0 => s.scroll(0., value as f32),
                            _ => s.scroll(value as f32, 0.),
                        }
                        d.frame(self.conn.serial(), now);
                    }
                }
                PointerEvent::AxisDiscrete120 { axis, value } => {
                    let scroll_device = self.devices.scroll.read().unwrap();
                    if let Some((d, s)) = scroll_device.as_ref() {
                        match axis {
                            0 => s.scroll_discrete(0, value),
                            _ => s.scroll_discrete(value, 0),
                        }
                        d.frame(self.conn.serial(), now);
                    }
                }
            },
            Event::Keyboard(k) => match k {
                KeyboardEvent::Key {
                    time: _,
                    key,
                    state,
                } => {
                    let keyboard_device = self.devices.keyboard.read().unwrap();
                    if let Some((d, k)) = keyboard_device.as_ref() {
                        k.key(
                            key,
                            match state {
                                0 => KeyState::Released,
                                _ => KeyState::Press,
                            },
                        );
                        d.frame(self.conn.serial(), now);
                    }
                }
                KeyboardEvent::Modifiers { .. } => {}
            },
        }
        self.context
            .flush()
            .map_err(|e| io::Error::new(e.kind(), e))?;
        Ok(())
    }

    async fn create(&mut self, _: EmulationHandle) {}
    async fn destroy(&mut self, _: EmulationHandle) {}

    async fn terminate(&mut self) {
        let _ = self.session.close().await;
        self.ei_task.abort();
    }
}

async fn ei_task(
    mut events: EiConvertEventStream,
    _conn: Connection,
    context: ei::Context,
    devices: Devices,
    libei_error: Arc<AtomicBool>,
    error: Arc<Mutex<Option<EmulationError>>>,
) {
    loop {
        match ei_event_handler(&mut events, &context, &devices).await {
            Ok(()) => {}
            Err(e) => {
                libei_error.store(true, Ordering::SeqCst);
                error.lock().unwrap().replace(e);
                // wait for termination -> otherwise we will loop forever
                future::pending::<()>().await;
            }
        }
    }
}

async fn ei_event_handler(
    events: &mut EiConvertEventStream,
    context: &ei::Context,
    devices: &Devices,
) -> Result<(), EmulationError> {
    loop {
        let event = events.next().await.ok_or(EmulationError::EndOfStream)??;
        let capabilities = DeviceCapability::Pointer
            | DeviceCapability::PointerAbsolute
            | DeviceCapability::Keyboard
            | DeviceCapability::Touch
            | DeviceCapability::Scroll
            | DeviceCapability::Button;
        log::debug!("{event:?}");
        match event {
            EiEvent::Disconnected(e) => {
                log::debug!("ei disconnected: {e:?}");
                return Err(EmulationError::EndOfStream);
            }
            EiEvent::SeatAdded(e) => {
                e.seat().bind_capabilities(capabilities);
            }
            EiEvent::SeatRemoved(e) => {
                log::debug!("seat removed: {:?}", e.seat());
            }
            EiEvent::DeviceAdded(e) => {
                let device_type = e.device().device_type();
                log::debug!("device added: {device_type:?}");
                let device = e.device();
                if let Some(pointer) = e.device().interface::<Pointer>() {
                    devices
                        .pointer
                        .write()
                        .unwrap()
                        .replace((device.device().clone(), pointer));
                }
                if let Some(keyboard) = e.device().interface::<Keyboard>() {
                    devices
                        .keyboard
                        .write()
                        .unwrap()
                        .replace((device.device().clone(), keyboard));
                }
                if let Some(scroll) = e.device().interface::<Scroll>() {
                    devices
                        .scroll
                        .write()
                        .unwrap()
                        .replace((device.device().clone(), scroll));
                }
                if let Some(button) = e.device().interface::<Button>() {
                    devices
                        .button
                        .write()
                        .unwrap()
                        .replace((device.device().clone(), button));
                }
            }
            EiEvent::DeviceRemoved(e) => {
                log::debug!("device removed: {:?}", e.device().device_type());
            }
            EiEvent::DevicePaused(e) => {
                log::debug!("device paused: {:?}", e.device().device_type());
            }
            EiEvent::DeviceResumed(e) => {
                log::debug!("device resumed: {:?}", e.device().device_type());
                e.device().device().start_emulating(0, 0);
            }
            EiEvent::KeyboardModifiers(e) => {
                log::debug!("modifiers: {e:?}");
            }
            // only for receiver context
            // EiEvent::Frame(_) => { },
            // EiEvent::DeviceStartEmulating(_) => { },
            // EiEvent::DeviceStopEmulating(_) => { },
            // EiEvent::PointerMotion(_) => { },
            // EiEvent::PointerMotionAbsolute(_) => { },
            // EiEvent::Button(_) => { },
            // EiEvent::ScrollDelta(_) => { },
            // EiEvent::ScrollStop(_) => { },
            // EiEvent::ScrollCancel(_) => { },
            // EiEvent::ScrollDiscrete(_) => { },
            // EiEvent::KeyboardKey(_) => { },
            // EiEvent::TouchDown(_) => { },
            // EiEvent::TouchUp(_) => { },
            // EiEvent::TouchMotion(_) => { },
            _ => unreachable!("unexpected ei event"),
        }
        context.flush().map_err(|e| io::Error::new(e.kind(), e))?;
    }
}
