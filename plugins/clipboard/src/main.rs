use gtk::{gdk, gio, glib, prelude::*};
use syntra_plugin_api::{
    CopyManifest, EntryKind, Message, Operation, SourceEntry, read_messages, write_message,
};
use parking_lot::Mutex;
use std::io::{self, BufReader};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

struct ClipboardBackend(gdk::ContentProvider);

struct ActiveTransfer {
    transfer_id: String,
    backend: ClipboardBackend,
    reasserted_after_empty: bool,
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
fn main() {
    if !cfg!(target_os = "linux") || gtk::init().is_err() {
        return;
    }
    emit(&Message::Hello {
        protocol_version: syntra_plugin_api::PROTOCOL_VERSION,
        adapter_id: "gtk-clipboard".into(),
        name: "GNOME Clipboard Source".into(),
        capabilities: syntra_plugin_api::Capabilities {
            clipboard_read: true,
            paste: true,
            cancel: true,
            requires_live_mount: true,
            mime_types: vec![
                "text/uri-list".into(),
                "x-special/gnome-copied-files".into(),
            ],
        },
    });
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
    if let Some(path) = std::env::var_os("LAN_MOUSE_DEBUG_COPY_PATH").map(std::path::PathBuf::from)
    {
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
    let active_transfer: Arc<Mutex<Option<ActiveTransfer>>> = Arc::new(Mutex::new(None));
    let suppress_echo = Arc::new(Mutex::new(false));
    let remote_file_clipboard: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let generation = Arc::new(AtomicU64::new(0));
    let generation_for_main = Arc::clone(&generation);
    let queue_for_main = Arc::clone(&incoming);
    let clipboard_for_main = clipboard.clone();
    let active_for_main = Arc::clone(&active_transfer);
    let suppress_for_main = Arc::clone(&suppress_echo);
    let remote_for_main = Arc::clone(&remote_file_clipboard);
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
                    // `text/uri-list` is CRLF-delimited per RFC 2483, while
                    // Nautilus' private copied-files format is strictly:
                    // `copy\n<URI>\n...`. Feeding CRLF to the latter leaves
                    // the offer visible but disables Paste in Nautilus.
                    let operation = if matches!(publication.operation, Operation::Move) {
                        "cut"
                    } else {
                        "copy"
                    };
                    let gnome = format!("{operation}\n{}\n", publication.uris.join("\n"));
                    *remote_for_main.lock() = Some(gnome.as_bytes().to_vec());
                    *suppress_for_main.lock() = true;

                    let files: Vec<gio::File> = publication
                        .uris
                        .iter()
                        .map(|uri| gio::File::for_uri(uri))
                        .collect();
                    let file_list = gdk::FileList::from_array(&files);
                    // GDK's native FileList provider is the same clipboard
                    // contract used by GTK file managers. It exports the
                    // portal file-transfer formats GNOME Files consumes;
                    // mixing raw MIME providers into the union causes Mutter
                    // to withdraw the selection after advertising it.
                    let provider = gdk::ContentProvider::for_value(&file_list.to_value());
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
                        let mut active = active_for_main.lock();
                        if active
                            .as_ref()
                            .is_none_or(|current| current.transfer_id != publication.transfer_id)
                        {
                            if let Some(old) = active.replace(ActiveTransfer {
                                transfer_id: publication.transfer_id.clone(),
                                backend,
                                reasserted_after_empty: false,
                            }) {
                                emit(&Message::Released(syntra_plugin_api::Released {
                                    transfer_id: old.transfer_id,
                                }));
                            }
                        } else {
                            active.as_mut().unwrap().backend = backend;
                            active.as_mut().unwrap().reasserted_after_empty = false;
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
    let suppress_for_changed = Arc::clone(&suppress_echo);
    let remote_for_changed = Arc::clone(&remote_file_clipboard);
    let active_for_changed = Arc::clone(&active_transfer);
    let generation_for_changed = Arc::clone(&generation);
    let last_seen: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    clipboard.connect_changed(move |clipboard| {
        if cancel_for_changed.load(Ordering::Acquire) {
            return;
        }
        let formats = clipboard.formats();
        let names = formats.mime_types();
        eprintln!("clipboard changed: mimes={names:?}");
        if std::mem::take(&mut *suppress_for_changed.lock()) {
            return;
        }
        let Some(mime) = ["x-special/gnome-copied-files", "text/uri-list"]
            .into_iter()
            .find(|mime| names.iter().any(|name| name.as_str() == *mime))
        else {
            // GNOME can briefly publish an empty selection while changing
            // owners. Reassert our live remote offer once after that
            // transition so Files receives the file MIME metadata.
            let mut active = active_for_changed.lock();
            if let Some(current) = active.as_mut() {
                if !current.reasserted_after_empty {
                    current.reasserted_after_empty = true;
                    *suppress_for_changed.lock() = true;
                    if clipboard.set_content(Some(&current.backend.0)).is_ok() {
                        return;
                    }
                }
            }
            *active = None;
            *last_seen.lock() = None;
            return;
        };
        let remote = Arc::clone(&remote_for_changed);
        let active = Arc::clone(&active_for_changed);
        let generation = Arc::clone(&generation_for_changed);
        let seen = Arc::clone(&last_seen);
        clipboard.read_async(
            &[mime],
            glib::Priority::DEFAULT,
            None::<&gio::Cancellable>,
            move |result| {
                let Ok((stream, _)) = result else {
                    return;
                };
                stream.read_bytes_async(
                    1024 * 1024,
                    glib::Priority::DEFAULT,
                    None::<&gio::Cancellable>,
                    move |bytes| {
                        let Ok(bytes) = bytes else {
                            return;
                        };
                        let payload = bytes.to_vec();
                        if payload.is_empty() {
                            return;
                        }
                        // Never re-announce content this machine received
                        // from the peer, and never announce the same
                        // selection twice.
                        if remote.lock().as_deref() == Some(payload.as_slice()) {
                            return;
                        }
                        // This is a new local file offer, so the adapter no
                        // longer owns the remote offer kept alive above.
                        *active.lock() = None;
                        let mut seen = seen.lock();
                        if seen.as_deref() == Some(payload.as_slice()) {
                            return;
                        }
                        *seen = Some(payload.clone());
                        drop(seen);
                        if let Some(manifest) =
                            process_text(&String::from_utf8_lossy(&payload), &generation)
                        {
                            eprintln!(
                                "local file copy detected: transfer={} entries={}",
                                manifest.transfer_id,
                                manifest.entries.len()
                            );
                            emit(&Message::CopyManifest(manifest));
                        }
                    },
                );
            },
        );
    });

    loop_.run();
}
