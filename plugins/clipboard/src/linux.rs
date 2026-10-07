//! GTK implementation of the clipboard plugin.

use gtk::{gdk, gio, glib, prelude::*};
use parking_lot::Mutex;
use std::io::{self, BufReader};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use syntra_plugin_api::{
    CopyManifest, EntryKind, Message, Operation, SourceEntry, read_messages, write_message,
};

struct ClipboardBackend(gdk::ContentProvider);

struct ActiveTransfer {
    transfer_id: String,
    backend: ClipboardBackend,
}

fn release_transfer(
    transfer_id: &str,
    active: &Mutex<Option<ActiveTransfer>>,
    clipboard: &gdk::Clipboard,
) {
    let current = {
        let mut active = active.lock();
        if active
            .as_ref()
            .is_none_or(|current| current.transfer_id != transfer_id)
        {
            return;
        }
        active.take().unwrap()
    };

    let ClipboardBackend(_provider) = current.backend;
    if let Err(error) = clipboard.set_content(None::<&gdk::ContentProvider>) {
        eprintln!("GDK clipboard release failed for transfer {transfer_id}: {error}");
    }
}

fn emit(message: &Message) {
    let mut out = io::stdout().lock();
    let _ = write_message(&mut out, message);
}
fn local_entry(uri: &str) -> Option<SourceEntry> {
    let file = gio::File::for_uri(uri);
    let path = file.path()?;
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    let kind = if metadata.is_file() {
        EntryKind::File
    } else if metadata.is_dir() {
        EntryKind::Directory
    } else {
        return None;
    };
    Some(SourceEntry {
        uri: uri.to_string(),
        kind,
        size: metadata.is_file().then_some(metadata.len()),
    })
}
fn process_text(text: &str, generation: &AtomicU64) -> Option<CopyManifest> {
    let mut lines = text.lines();
    let first = lines.next().map(str::trim);
    let operation = if matches!(first, Some("move") | Some("cut")) {
        Operation::Move
    } else {
        Operation::Copy
    };
    let mut uris: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with('#'))
        .map(str::to_owned)
        .collect();
    if matches!(first, Some("copy") | Some("cut") | Some("move")) {
        uris.remove(0);
    }
    if uris.is_empty() || uris.iter().any(|u| !u.starts_with("file://")) {
        return None;
    }
    let entries: Option<Vec<_>> = uris.iter().map(|u| local_entry(u)).collect();
    let entries = entries?;
    (!entries.is_empty()).then(|| CopyManifest {
        transfer_id: format!("gtk-{}", generation.fetch_add(1, Ordering::Relaxed) + 1),
        operation,
        entries,
    })
}

const MAX_FILE_SELECTION_BYTES: usize = 1024 * 1024;

async fn read_file_selection(stream: &gio::InputStream) -> Result<Vec<u8>, glib::Error> {
    let mut payload = Vec::new();
    loop {
        // A successful stream read may contain only part of the URI list.
        // Read through EOF, including one extra byte to detect oversize offers.
        let remaining = MAX_FILE_SELECTION_BYTES + 1 - payload.len();
        let bytes = stream
            .read_bytes_future(remaining.min(8192), glib::Priority::DEFAULT)
            .await?;
        if bytes.is_empty() {
            return Ok(payload);
        }
        payload.extend_from_slice(&bytes);
        if payload.len() > MAX_FILE_SELECTION_BYTES {
            return Err(glib::Error::new(
                gio::IOErrorEnum::InvalidData,
                "file clipboard selection exceeds the size limit",
            ));
        }
    }
}
/// This plugin's own declaration, the single source of truth about it.
///
/// The daemon reads it from the handshake, or from `--describe` before the
/// plugin has ever run, so nothing about it is duplicated in the daemon or in
/// a file beside the binary.
fn hello() -> Message {
    Message::Hello {
        protocol_version: syntra_plugin_api::PROTOCOL_VERSION,
        adapter_id: "gtk-clipboard".into(),
        name: "GNOME Clipboard Source".into(),
        capabilities: syntra_plugin_api::Capabilities {
            clipboard_read: true,
            paste: true,
            cancel: true,
            requires_live_mount: true,
            pointer_events: false,
            mime_types: vec![
                "text/uri-list".into(),
                "x-special/gnome-copied-files".into(),
            ],
        },
        metadata: Some(syntra_plugin_api::PluginMetadata {
            description: "Copy files in your file manager and paste them on another device. Text and images are synchronised by the daemon itself and need no plugin; only file selections require this helper, because reading them needs a desktop toolkit the background service cannot load.".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            author: "Syntra contributors".into(),
            homepage: Some("https://github.com/DarkPhilosophy/syntra".into()),
            source: Some("https://github.com/DarkPhilosophy/syntra/tree/main/plugins/clipboard".into()),
            update_url: Some("https://github.com/DarkPhilosophy/syntra/releases".into()),
            license: Some("GPL-3.0-or-later".into()),
            bundled: true,
            on_demand: false,
            build_fingerprint: syntra_plugin_api::BUILD_FINGERPRINT.into(),
        }),
    }
}

pub fn main() {
    // `--describe` prints the declaration and exits, so the daemon can list
    // this plugin accurately without launching a full session.
    if std::env::args().any(|argument| argument == "--describe") {
        if let Ok(line) = hello().encode_line() {
            print!("{line}");
        }
        return;
    }
    // A windowless Wayland client receives no clipboard offers: wl_data_device
    // delivers them only to the keyboard-focused client. Use the XWayland
    // selection bridge when available, without changing the desktop session.
    // The parent dashboard may itself require Wayland; its backend override
    // must not force this windowless helper onto the same backend.
    // SAFETY: this executable has not initialized GTK or started threads.
    unsafe { std::env::set_var("GDK_BACKEND", "x11,wayland") };
    if !cfg!(target_os = "linux") || gtk::init().is_err() {
        return;
    }
    emit(&hello());
    let loop_ = glib::MainLoop::new(None, false);
    let cancelled = Arc::new(AtomicBool::new(false));
    let incoming: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(Vec::new()));
    let queue = Arc::clone(&incoming);
    let cancel = Arc::clone(&cancelled);
    let loop_for_input = loop_.clone();
    std::thread::spawn(move || {
        for message in read_messages(BufReader::new(io::stdin())).flatten() {
            queue.lock().push(message);
        }
        cancel.store(true, Ordering::Release);
        loop_for_input.quit();
    });

    let Some(display) = gdk::Display::default() else {
        return;
    };
    let clipboard = display.clipboard();
    eprintln!(
        "clipboard-trace event=display-ready backend={}",
        display.type_().name()
    );
    if let Some(path) = std::env::var_os("SYNTRA_DEBUG_COPY_PATH").map(std::path::PathBuf::from) {
        let clipboard = clipboard.clone();
        glib::timeout_add_local_once(std::time::Duration::from_secs(15), move || {
            let file = gio::File::for_path(&path);
            let uri = file.uri();
            let uri_text = format!("{uri}\r\n");
            let gnome = format!("copy\r\n{uri_text}");
            let uri_provider = gdk::ContentProvider::for_bytes(
                "text/uri-list",
                &glib::Bytes::from(uri_text.as_bytes()),
            );
            let gnome_provider = gdk::ContentProvider::for_bytes(
                "x-special/gnome-copied-files",
                &glib::Bytes::from(gnome.as_bytes()),
            );
            let provider = gdk::ContentProvider::new_union(&[uri_provider, gnome_provider]);
            match clipboard.set_content(Some(&provider)) {
                Ok(()) => eprintln!("debug-copy: offered {uri}"),
                Err(error) => eprintln!("debug-copy: failed to offer {uri}: {error}"),
            }
        });
    }
    let active_transfer: Rc<Mutex<Option<ActiveTransfer>>> = Rc::new(Mutex::new(None));
    let remote_file_clipboard: Rc<Mutex<Option<Vec<u8>>>> = Rc::new(Mutex::new(None));
    let generation = Rc::new(AtomicU64::new(0));
    let generation_for_main = Rc::clone(&generation);
    let queue_for_main = Arc::clone(&incoming);
    let clipboard_for_main = clipboard.clone();
    let active_for_main = Rc::clone(&active_transfer);
    let remote_for_main = Rc::clone(&remote_file_clipboard);
    glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        let messages = std::mem::take(&mut *queue_for_main.lock());
        for message in messages {
            match message {
                Message::ClipboardData {
                    mime_type, value, ..
                } => {
                    if matches!(
                        mime_type.as_str(),
                        "x-special/gnome-copied-files" | "text/uri-list"
                    ) {
                        if let Some(manifest) = process_text(&value, &generation_for_main) {
                            emit(&Message::CopyManifest(manifest));
                        }
                    }
                }
                Message::PublishFileClipboard(publication) => {
                    // Nautilus rejects empty URI lines, including a trailing newline.
                    let operation = if matches!(publication.operation, Operation::Move) {
                        "cut"
                    } else {
                        "copy"
                    };
                    let gnome = format!("{operation}\n{}", publication.uris.join("\n"));
                    *remote_for_main.lock() = Some(gnome.as_bytes().to_vec());

                    let files: Vec<gio::File> = publication
                        .uris
                        .iter()
                        .map(|uri| gio::File::for_uri(uri))
                        .collect();
                    let file_list = gdk::FileList::from_array(&files);
                    // Nautilus needs its Copy/Cut format; retain FileList for
                    // Dolphin and GDK's URI-list/portal serialization.
                    let provider = gdk::ContentProvider::new_union(&[
                        gdk::ContentProvider::for_bytes(
                            "x-special/gnome-copied-files",
                            &glib::Bytes::from_owned(gnome.into_bytes()),
                        ),
                        gdk::ContentProvider::for_value(&file_list.to_value()),
                    ]);
                    let backend = match clipboard_for_main.set_content(Some(&provider)) {
                        Ok(()) => Some(ClipboardBackend(provider)),
                        Err(error) => {
                            eprintln!(
                                "GDK clipboard publish failed for transfer {}: {error}",
                                publication.transfer_id
                            );
                            None
                        }
                    };

                    if let Some(backend) = backend {
                        eprintln!(
                            "clipboard-trace event=file-publication source=gtk outcome=ready entries={}",
                            publication.uris.len()
                        );
                        let mut active = active_for_main.lock();
                        if active
                            .as_ref()
                            .is_none_or(|current| current.transfer_id != publication.transfer_id)
                        {
                            if let Some(old) = active.replace(ActiveTransfer {
                                transfer_id: publication.transfer_id.clone(),
                                backend,
                            }) {
                                emit(&Message::Released(syntra_plugin_api::Released {
                                    transfer_id: old.transfer_id,
                                }));
                            }
                        } else {
                            active.as_mut().unwrap().backend = backend;
                        }
                    }
                }
                Message::Released(released) => {
                    release_transfer(&released.transfer_id, &active_for_main, &clipboard_for_main)
                }
                Message::Unmounted(event) => {
                    release_transfer(&event.transfer_id, &active_for_main, &clipboard_for_main)
                }
                Message::Cancel { transfer_id } => {
                    release_transfer(&transfer_id, &active_for_main, &clipboard_for_main)
                }
                _ => {}
            }
        }
        glib::ControlFlow::Continue
    });

    // Event-driven local copy detection: GdkClipboard emits `changed`
    // whenever the selection owner changes. We only ever READ the offer,
    // never take ownership, so no auxiliary window and no polling.
    let cancel_for_changed = Arc::clone(&cancelled);
    let remote_for_changed = Rc::clone(&remote_file_clipboard);
    let active_for_changed = Rc::clone(&active_transfer);
    let generation_for_changed = Rc::clone(&generation);
    let last_seen: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let selection_epoch = Rc::new(AtomicU64::new(0));
    clipboard.connect_changed(move |clipboard| {
        let epoch = selection_epoch.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!(
            "clipboard-trace event=owner-change source=gtk local={} formats=[{}]",
            clipboard.is_local(),
            clipboard.formats().mime_types().join(",")
        );
        if cancel_for_changed.load(Ordering::Acquire) || clipboard.is_local() {
            return;
        }
        // A new owner replaces our offer. Reasserting the previous provider
        // here steals selections from other applications during negotiation.
        *active_for_changed.lock() = None;
        let formats = clipboard.formats();
        let names = formats.mime_types();
        let mimes: Vec<_> = ["x-special/gnome-copied-files", "text/uri-list"]
            .into_iter()
            .filter(|mime| names.iter().any(|name| name.as_str() == *mime))
            .collect();
        if mimes.is_empty() {
            *last_seen.lock() = None;
            if let Some(notice) = text_change_notice(names.iter().map(|name| name.as_str())) {
                emit(&notice);
            }
            return;
        }
        let clipboard = clipboard.clone();
        let remote = Rc::clone(&remote_for_changed);
        let generation = Rc::clone(&generation_for_changed);
        let seen = Arc::clone(&last_seen);
        let selection_epoch = Rc::clone(&selection_epoch);
        glib::MainContext::default().spawn_local(async move {
            for mime in mimes {
                let result = match clipboard
                    .read_future(&[mime], glib::Priority::DEFAULT)
                    .await
                {
                    Ok((stream, _)) => read_file_selection(&stream).await,
                    Err(error) => Err(error),
                };
                if selection_epoch.load(Ordering::Relaxed) != epoch {
                    return;
                }
                let payload = match result {
                    Ok(payload) => payload,
                    Err(error) => {
                        eprintln!("file clipboard read failed: mime={mime}: {error}");
                        continue;
                    }
                };
                if remote.lock().as_deref() == Some(payload.as_slice())
                    || seen.lock().as_deref() == Some(payload.as_slice())
                {
                    return;
                }
                let Ok(text) = std::str::from_utf8(&payload) else {
                    eprintln!("file clipboard rejected: mime={mime}: invalid UTF-8");
                    continue;
                };
                let Some(manifest) = process_text(text, &generation) else {
                    eprintln!("file clipboard rejected: mime={mime}: invalid or unavailable files");
                    continue;
                };
                // Failed or partial reads must remain retryable.
                *seen.lock() = Some(payload);
                eprintln!(
                    "local file copy detected: transfer={} entries={}",
                    manifest.transfer_id,
                    manifest.entries.len()
                );
                emit(&Message::CopyManifest(manifest));
                return;
            }
        });
    });

    loop_.run();
}

/// Tells the daemon that another application copied text. The daemon reads
/// the selection itself; without this it only noticed when the pointer next
/// crossed to another device, so a copy could wait minutes before syncing.
fn text_change_notice<'a>(formats: impl IntoIterator<Item = &'a str>) -> Option<Message> {
    let mime = formats
        .into_iter()
        .find(|mime| mime.starts_with("text/plain") || *mime == "UTF8_STRING")?;
    Some(Message::ClipboardData {
        transfer_id: "selection-changed".into(),
        mime_type: mime.into(),
        value: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::text_change_notice;
    use syntra_plugin_api::Message;

    #[test]
    fn copied_text_is_announced_and_other_formats_are_not() {
        let notice = text_change_notice(["TARGETS", "text/plain;charset=utf-8"]);
        assert!(matches!(
            notice,
            Some(Message::ClipboardData { ref mime_type, .. }) if mime_type == "text/plain;charset=utf-8"
        ));
        assert!(text_change_notice(["image/png"]).is_none());
        assert!(text_change_notice([]).is_none());
    }

    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    #[test]
    fn file_manager_formats_preserve_empty_files_and_escaped_names() {
        let (file, stream) = gio::File::new_tmp(Some("syntra-clipboard #XXXXXX")).unwrap();
        let uri = file.uri();
        let generation = AtomicU64::new(0);
        for selection in [format!("copy\n{uri}\n"), format!("{uri}\r\n")] {
            let manifest = process_text(&selection, &generation).unwrap();
            assert_eq!(manifest.entries.len(), 1);
            assert_eq!(manifest.entries[0].uri, uri.as_str());
            assert!(matches!(manifest.entries[0].kind, EntryKind::File));
            assert_eq!(manifest.entries[0].size, Some(0));
        }
        stream.close(None::<&gio::Cancellable>).unwrap();
        file.delete(None::<&gio::Cancellable>).unwrap();
    }

    #[test]
    fn fragmented_file_selection_is_read_through_eof() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let payload = b"copy\nfile:///tmp/first%20file\nfile:///tmp/second\n";
        writer.write_all(&payload[..12]).unwrap();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            continue_rx.recv().unwrap();
            writer.write_all(&payload[12..]).unwrap();
        });
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                context.block_on(async {
                    // SAFETY: the stream takes exclusive ownership of this socket.
                    let stream: gio::InputStream =
                        unsafe { gio::UnixInputStream::take_fd(reader) }.upcast();
                    let wake = glib::timeout_source_new(
                        std::time::Duration::from_millis(20),
                        None,
                        glib::Priority::DEFAULT,
                        move || {
                            continue_tx.send(()).unwrap();
                            glib::ControlFlow::Break
                        },
                    );
                    wake.attach(Some(&context));
                    assert_eq!(read_file_selection(&stream).await.unwrap(), payload);
                });
            })
            .unwrap();
        producer.join().unwrap();
    }

    #[test]
    fn file_selection_limit_rejects_truncation() {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                context.block_on(async {
                    let bytes = glib::Bytes::from_owned(vec![b'a'; MAX_FILE_SELECTION_BYTES]);
                    let stream = gio::MemoryInputStream::from_bytes(&bytes).upcast();
                    assert_eq!(read_file_selection(&stream).await.unwrap(), bytes.as_ref());
                    let bytes = glib::Bytes::from_owned(vec![b'a'; MAX_FILE_SELECTION_BYTES + 1]);
                    let stream = gio::MemoryInputStream::from_bytes(&bytes).upcast();
                    assert!(
                        read_file_selection(&stream)
                            .await
                            .unwrap_err()
                            .matches(gio::IOErrorEnum::InvalidData)
                    );
                });
            })
            .unwrap();
    }
}
