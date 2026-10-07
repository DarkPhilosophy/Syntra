//! The plugin's side of the conversation with the daemon.
//!
//! The daemon starts the plugin, the plugin says who it is, and from then on
//! the daemon sends settings and pointer events, one JSON object per line. The
//! plugin sends nothing else: the daemon stops a plugin that does.

use std::{
    io::{self, BufRead, Write},
    sync::mpsc::Sender,
};

use syntra_plugin_api::{
    BUILD_FINGERPRINT, Capabilities, Message, PROTOCOL_VERSION, PluginMetadata,
};

/// The identifier the daemon launched this plugin under; it must match the
/// manifest's, or the daemon refuses the handshake.
pub const PLUGIN_ID: &str = "edge-glow";

/// The longest line the plugin will read. A daemon never sends one near this;
/// the limit exists so a broken peer cannot make the plugin buffer without end.
const MAX_LINE: usize = 1024 * 1024;

/// The handshake: who this plugin is and what it listens for.
pub fn hello() -> Message {
    Message::Hello {
        protocol_version: PROTOCOL_VERSION,
        adapter_id: PLUGIN_ID.to_owned(),
        name: "Edge glow".to_owned(),
        capabilities: Capabilities {
            clipboard_read: false,
            paste: false,
            cancel: false,
            requires_live_mount: false,
            pointer_events: true,
            mime_types: Vec::new(),
        },
        metadata: Some(PluginMetadata {
            description: "Lights the edge of the screen a pointer from another device \
                          came in through, and builds up as you push against an edge."
                .to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            author: "Syntra contributors".to_owned(),
            homepage: None,
            source: None,
            update_url: None,
            license: Some(env!("CARGO_PKG_LICENSE").to_owned()).filter(|value| !value.is_empty()),
            bundled: true,
            on_demand: false,
            build_fingerprint: BUILD_FINGERPRINT.to_owned(),
        }),
    }
}

/// Writes the handshake as one line.
pub fn send_hello(out: &mut impl Write) -> io::Result<()> {
    let line = hello().encode_line().map_err(io::Error::other)?;
    out.write_all(line.as_bytes())?;
    out.flush()
}

/// Reads daemon messages from `input` until it closes, passing each on.
///
/// Returns when the daemon closes the pipe, which is how the plugin learns it
/// has been stopped or the daemon has gone, and when the receiving side has
/// been dropped. A line that is not a message is skipped rather than ending
/// the plugin: a newer daemon may send something this build has never heard of.
pub fn read_messages(input: impl BufRead, to_app: &Sender<Message>) {
    let mut input = input;
    let mut line = String::new();
    loop {
        line.clear();
        match read_line_bounded(&mut input, &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let text = line.trim_end();
        if text.is_empty() {
            continue;
        }
        let Ok(message) = Message::decode_line(text) else {
            continue;
        };
        if to_app.send(message).is_err() {
            return;
        }
    }
}

/// `read_line`, but never growing `line` past [`MAX_LINE`].
///
/// Returns the bytes read, `0` at end of input. An over-long line is read to
/// its end and discarded, so the next one starts cleanly.
fn read_line_bounded(input: &mut impl BufRead, line: &mut String) -> io::Result<usize> {
    let mut total = 0;
    let mut bytes = Vec::new();
    let mut discarding = false;
    loop {
        let available = input.fill_buf()?;
        if available.is_empty() {
            break;
        }
        let (used, done) = match available.iter().position(|byte| *byte == b'\n') {
            Some(end) => (end + 1, true),
            None => (available.len(), false),
        };
        if !discarding {
            if bytes.len() + used > MAX_LINE {
                discarding = true;
                bytes.clear();
            } else {
                bytes.extend_from_slice(&available[..used]);
            }
        }
        input.consume(used);
        total += used;
        if done {
            break;
        }
    }
    if !discarding {
        line.push_str(&String::from_utf8_lossy(&bytes));
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Cursor, sync::mpsc};
    use syntra_plugin_api::{Edge, PointerEvent};

    fn collect(input: &str) -> Vec<Message> {
        let (tx, rx) = mpsc::channel();
        read_messages(Cursor::new(input.as_bytes().to_vec()), &tx);
        drop(tx);
        rx.iter().collect()
    }

    /// The handshake must be exactly what the daemon accepts for a plugin
    /// that only listens: its own id, the pointer capability, nothing else.
    #[test]
    fn the_handshake_declares_a_pointer_plugin() {
        let Message::Hello {
            protocol_version,
            adapter_id,
            capabilities,
            ..
        } = hello()
        else {
            panic!("the handshake is a hello");
        };
        assert_eq!(protocol_version, PROTOCOL_VERSION);
        assert_eq!(adapter_id, PLUGIN_ID);
        assert!(capabilities.pointer_events);
        assert!(!capabilities.clipboard_read && !capabilities.paste);
        assert!(capabilities.mime_types.is_empty());
    }

    #[test]
    fn the_handshake_is_one_line_the_daemon_can_decode() {
        let mut out = Vec::new();
        send_hello(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches('\n').count(), 1, "exactly one line");
        assert!(matches!(
            Message::decode_line(text.trim_end()),
            Ok(Message::Hello { .. })
        ));
    }

    #[test]
    fn messages_arrive_in_order() {
        let settings = Message::Settings {
            values: vec![("enabled".into(), "true".into())],
        }
        .encode_line()
        .unwrap();
        let pointer = Message::Pointer(PointerEvent::Entered {
            edge: Edge::Left,
            along: Some(0.5),
            peer: "phone".into(),
        })
        .encode_line()
        .unwrap();
        let got = collect(&format!("{settings}{pointer}"));
        assert_eq!(got.len(), 2);
        assert!(matches!(got[0], Message::Settings { .. }));
        assert!(matches!(got[1], Message::Pointer(_)));
    }

    /// A line this build cannot read is skipped, not fatal: it is how a newer
    /// daemon talks to an older plugin without taking it down.
    #[test]
    fn unreadable_lines_are_skipped_not_fatal() {
        let good = Message::Settings { values: vec![] }.encode_line().unwrap();
        let got = collect(&format!(
            "not json\n\n{{\"type\":\"from_the_future\",\"data\":{{}}}}\n{good}"
        ));
        assert_eq!(got.len(), 1);
        assert!(matches!(got[0], Message::Settings { .. }));
    }

    /// The plugin ends when the daemon closes the pipe, and not before.
    #[test]
    fn it_returns_when_the_daemon_closes_the_pipe() {
        assert!(collect("").is_empty());
    }

    /// A line past the limit is dropped, and the next one is read normally;
    /// the plugin does not buffer without end, nor desynchronise.
    #[test]
    fn an_overlong_line_is_dropped_and_the_next_still_reads() {
        let good = Message::Settings { values: vec![] }.encode_line().unwrap();
        let huge = "x".repeat(MAX_LINE + 10);
        let got = collect(&format!("{huge}\n{good}"));
        assert_eq!(got.len(), 1, "only the well formed line survives");
    }

    /// If the application has stopped listening there is no point reading on.
    #[test]
    fn it_stops_when_the_receiver_is_gone() {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let line = Message::Settings { values: vec![] }.encode_line().unwrap();
        read_messages(Cursor::new(format!("{line}{line}").into_bytes()), &tx);
    }
}
