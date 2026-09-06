use std::path::Path;

use lan_mouse_history::{
    FileMetadata, HistoryContent,
    worker::{HistoryPage as WorkerPage, HistorySummaryContent, ImagePayload},
};
use lan_mouse_ipc::{
    HistoryEventId, HistoryImage, HistoryKind, HistoryPage, HistoryPreview, HistoryRecordSummary,
};

pub(crate) const DATABASE_FILE: &str = "clipboard-history.sqlite3";

pub(crate) fn database_path(app_data_dir: &Path) -> std::path::PathBuf {
    app_data_dir.join(DATABASE_FILE)
}

pub(crate) fn text(value: String) -> HistoryContent {
    HistoryContent::Text(value)
}

pub(crate) fn image(width: u32, height: u32, rgba: Vec<u8>) -> HistoryContent {
    HistoryContent::Image {
        bytes: Some(rgba),
        media_type: Some("image/x-rgba8".into()),
        width: Some(width),
        height: Some(height),
    }
}

pub(crate) fn files(entries: impl IntoIterator<Item = (String, u64)>) -> Option<HistoryContent> {
    let files: Vec<_> = entries
        .into_iter()
        .map(|(name, size_bytes)| FileMetadata {
            name,
            size_bytes,
            media_type: None,
        })
        .collect();
    (!files.is_empty()).then_some(HistoryContent::Files(files))
}

pub(crate) fn ipc_page(query: String, offset: u64, page: WorkerPage) -> HistoryPage {
    HistoryPage {
        query,
        offset,
        next_offset: page.next_offset,
        records: page
            .records
            .into_iter()
            .map(|record| {
                let preview = match record.content {
                    HistorySummaryContent::Text { preview, truncated } => {
                        HistoryPreview::Text { preview, truncated }
                    }
                    HistorySummaryContent::Image {
                        media_type,
                        width,
                        height,
                        size_bytes,
                    } => HistoryPreview::Image {
                        media_type,
                        width,
                        height,
                        size_bytes,
                    },
                    HistorySummaryContent::Files {
                        count,
                        total_size_bytes,
                        names,
                        names_truncated,
                    } => HistoryPreview::Files {
                        count,
                        total_size_bytes,
                        names,
                        names_truncated,
                    },
                };
                HistoryRecordSummary {
                    event_id: ipc_id(record.event_id),
                    created_at_ms: record.created_at_ms,
                    origin_label: record.origin_label,
                    pinned: record.pinned,
                    kind: match record.kind {
                        lan_mouse_history::HistoryKind::Text => HistoryKind::Text,
                        lan_mouse_history::HistoryKind::Image => HistoryKind::Image,
                        lan_mouse_history::HistoryKind::Files => HistoryKind::Files,
                    },
                    preview,
                }
            })
            .collect(),
    }
}

pub(crate) fn ipc_image(event_id: HistoryEventId, payload: ImagePayload) -> HistoryImage {
    HistoryImage {
        event_id,
        bytes: payload.bytes,
        media_type: payload.media_type,
        width: payload.width,
        height: payload.height,
    }
}

pub(crate) fn ipc_id(id: lan_mouse_history::HistoryEventId) -> HistoryEventId {
    HistoryEventId {
        origin_device_id: id.origin_device_id,
        origin_sequence: id.origin_sequence,
    }
}

pub(crate) fn store_id(id: HistoryEventId) -> lan_mouse_history::HistoryEventId {
    lan_mouse_history::HistoryEventId {
        origin_device_id: id.origin_device_id,
        origin_sequence: id.origin_sequence,
    }
}
