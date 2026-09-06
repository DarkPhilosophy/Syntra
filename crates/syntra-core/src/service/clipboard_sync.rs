//! Local clipboard observation and history recording.
//!
//! Guards against echo: a value this device just wrote, or a file offer
//! served from our own mount, must not be re-published as a fresh copy.

use super::*;

impl Service {
    pub(super) async fn record_local_history(&mut self, content: syntra_store::HistoryContent) {
        match self.history.record_event(content).await {
            Ok(event) => self.broadcast_history_record(&event),
            Err(error) => log::warn!("failed to persist local clipboard history: {error}"),
        }
    }

    /// A file clipboard published by our own FUSE mount must never be
    /// re-announced as a local copy: that would echo the transfer back to
    /// the peer that sent it.
    pub(super) fn is_own_clipboard_mount(value: &str) -> bool {
        value
            .lines()
            .filter(|line| line.starts_with("file://"))
            .all(|line| {
                line.contains("syntra/clipboard/") || line.contains("lan%2Dmouse/clipboard/")
            })
            && value.lines().any(|line| line.starts_with("file://"))
    }

    pub(super) fn handle_native_file_clipboard(&mut self, mime_type: String, value: String) {
        if Self::is_own_clipboard_mount(&value) {
            log::debug!("ignoring clipboard echo of our own mount");
            return;
        }
        if self.last_file_clipboard.as_deref() == Some(value.as_str()) {
            log::debug!("ignoring duplicate native file clipboard event");
            return;
        }
        self.last_file_clipboard = Some(value.clone());
        log::info!("native file clipboard detected: mime={mime_type}");
        if let Some(manager) = self.adapter_manager.as_ref() {
            let _ = manager.try_send(ManagerCommand::ClipboardData {
                transfer_id: "remote-desktop".to_string(),
                mime_type,
                value,
            });
        }
    }
}
