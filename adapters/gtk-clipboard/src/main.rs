use gtk::{gdk, gio, glib, prelude::*};
use lan_mouse_adapter_api::{
    CopyManifest, EntryKind, Message, Operation, SourceEntry, read_messages, write_message,
};
use std::io::{self, BufReader};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const MAX_READ: usize = 1024 * 1024;
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
        protocol_version: lan_mouse_adapter_api::PROTOCOL_VERSION,
        adapter_id: "gtk-clipboard".into(),
        name: "GNOME Clipboard Source".into(),
        capabilities: lan_mouse_adapter_api::Capabilities {
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
            queue.lock().unwrap().push(message);
        }
        cancel.store(true, Ordering::Release);
        loop_for_input.quit();
    });

    let Some(display) = gdk::Display::default() else {
        return;
    };
    let clipboard = display.clipboard();
    let active_transfer: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let suppress_echo = Arc::new(Mutex::new(false));
    let queue_for_main = Arc::clone(&incoming);
    let clipboard_for_main = clipboard.clone();
    let active_for_main = Arc::clone(&active_transfer);
    let suppress_for_main = Arc::clone(&suppress_echo);
    glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        let messages = std::mem::take(&mut *queue_for_main.lock().unwrap());
        for message in messages {
            match message {
                Message::PublishFileClipboard(publication) => {
                    let uri_text = publication.uris.join("\r\n") + "\r\n";
                    let gnome = format!(
                        "{}\r\n{}",
                        if matches!(publication.operation, Operation::Move) {
                            "cut"
                        } else {
                            "copy"
                        },
                        uri_text
                    );
                    let uri_provider = gdk::ContentProvider::for_bytes(
                        "text/uri-list",
                        &glib::Bytes::from(uri_text.as_bytes()),
                    );
                    let gnome_provider = gdk::ContentProvider::for_bytes(
                        "x-special/gnome-copied-files",
                        &glib::Bytes::from(gnome.as_bytes()),
                    );
                    let provider = gdk::ContentProvider::new_union(&[uri_provider, gnome_provider]);
                    let mut active = active_for_main.lock().unwrap();
                    if active.as_deref() != Some(publication.transfer_id.as_str()) {
                        if let Some(old) = active.replace(publication.transfer_id.clone()) {
                            emit(&Message::Released(lan_mouse_adapter_api::Released {
                                transfer_id: old,
                            }));
                        }
                    }
                    *suppress_for_main.lock().unwrap() = true;
                    let _ = clipboard_for_main.set_content(Some(&provider));
                }
                Message::Released(released) => {
                    if active_for_main.lock().unwrap().as_deref()
                        == Some(released.transfer_id.as_str())
                    {
                        let _ = clipboard_for_main.set_content(None::<&gdk::ContentProvider>);
                        *active_for_main.lock().unwrap() = None;
                    }
                }
                Message::Unmounted(event) => {
                    if active_for_main.lock().unwrap().as_deref()
                        == Some(event.transfer_id.as_str())
                    {
                        let _ = clipboard_for_main.set_content(None::<&gdk::ContentProvider>);
                        *active_for_main.lock().unwrap() = None;
                    }
                }
                Message::Cancel { transfer_id } => {
                    if active_for_main.lock().unwrap().as_deref() == Some(transfer_id.as_str()) {
                        let _ = clipboard_for_main.set_content(None::<&gdk::ContentProvider>);
                        *active_for_main.lock().unwrap() = None;
                    }
                }
                _ => {}
            }
        }
        glib::ControlFlow::Continue
    });

    let generation = Arc::new(AtomicU64::new(0));
    let cancel_signal = cancelled.clone();
    let generation_signal = generation.clone();
    let suppress_for_changed = Arc::clone(&suppress_echo);
    let active_for_changed = Arc::clone(&active_transfer);
    clipboard.connect_changed(move |clipboard| {
        if cancel_signal.load(Ordering::Acquire) {
            return;
        }
        let mut suppress = suppress_for_changed.lock().unwrap();
        if *suppress {
            *suppress = false;
            return;
        }
        if let Some(transfer_id) = active_for_changed.lock().unwrap().take() {
            emit(&Message::Released(lan_mouse_adapter_api::Released {
                transfer_id,
            }));
        }
        let available = clipboard.formats().mime_types();
        if clipboard
            .formats()
            .contains_type(gdk::FileList::static_type())
        {
            let generation_typed = generation_signal.clone();
            clipboard.read_value_async(
                gdk::FileList::static_type(),
                glib::Priority::DEFAULT,
                None::<&gio::Cancellable>,
                move |result| {
                    let Ok(value) = result else {
                        return;
                    };
                    let Ok(file_list) = value.get::<gdk::FileList>() else {
                        return;
                    };
                    let uris: Vec<String> = file_list
                        .files()
                        .iter()
                        .map(|file| file.uri().to_string())
                        .collect();
                    let text = uris.join("\r\n") + "\r\n";
                    if let Some(manifest) = process_text(&text, &generation_typed) {
                        emit(&Message::CopyManifest(manifest));
                    }
                },
            );
            return;
        }
        let names: Vec<_> = available.iter().map(|value| value.as_str()).collect();
        let Some(mime) = ["x-special/gnome-copied-files", "text/uri-list"]
            .iter()
            .find(|mime| names.iter().any(|name| name == *mime))
        else {
            return;
        };
        let clipboard = clipboard.clone();
        let generation = generation_signal.clone();
        clipboard.read_async(
            &[mime],
            glib::Priority::DEFAULT,
            None::<&gio::Cancellable>,
            move |result| {
                let Ok((stream, _)) = result else {
                    return;
                };
                stream.read_bytes_async(
                    MAX_READ,
                    glib::Priority::DEFAULT,
                    None::<&gio::Cancellable>,
                    move |result| {
                        let Ok(bytes) = result else {
                            return;
                        };
                        if let Some(manifest) =
                            process_text(&String::from_utf8_lossy(bytes.as_ref()), &generation)
                        {
                            emit(&Message::CopyManifest(manifest));
                        }
                    },
                );
            },
        );
    });

    loop_.run();
}
