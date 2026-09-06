use std::{path::PathBuf, sync::mpsc as std_mpsc, thread};

use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    ClearBoundary, HistoryContent, HistoryError, HistoryEventId, HistoryKind, HistoryOrigin,
    HistoryRecord, HistoryStore, ImportedHistoryEvent, MergeOutcome,
};

pub const MAX_PAGE_SIZE: usize = 50;
pub const MAX_TEXT_PREVIEW_BYTES: usize = 512;
pub const MAX_FILE_NAMES: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistorySummaryContent {
    Text {
        preview: String,
        truncated: bool,
    },
    Image {
        media_type: Option<String>,
        width: Option<u32>,
        height: Option<u32>,
        size_bytes: u64,
    },
    Files {
        count: u32,
        total_size_bytes: u64,
        names: Vec<String>,
        names_truncated: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistorySummary {
    pub event_id: HistoryEventId,
    pub created_at_ms: i64,
    pub origin_label: Option<String>,
    pub pinned: bool,
    pub kind: HistoryKind,
    pub content: HistorySummaryContent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryPage {
    pub records: Vec<HistorySummary>,
    pub next_offset: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImagePayload {
    pub bytes: Vec<u8>,
    pub media_type: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

type Reply<T> = oneshot::Sender<Result<T, String>>;

enum Command {
    Record {
        device_id: Option<String>,
        label: Option<String>,
        content: HistoryContent,
        reply: Reply<HistoryEventId>,
    },
    RecordEvent {
        content: HistoryContent,
        reply: Reply<ImportedHistoryEvent>,
    },
    Import {
        event: ImportedHistoryEvent,
        reply: Reply<MergeOutcome>,
    },
    Export {
        offset: u64,
        limit: usize,
        reply: Reply<(Vec<ImportedHistoryEvent>, Option<u64>)>,
    },
    Page {
        query: String,
        offset: u64,
        limit: usize,
        reply: Reply<HistoryPage>,
    },
    Image {
        event_id: HistoryEventId,
        reply: Reply<Option<ImagePayload>>,
    },
    Pin {
        event_id: HistoryEventId,
        pinned: bool,
        reply: Reply<bool>,
    },
    SnapshotBoundary {
        reply: Reply<ClearBoundary>,
    },
    ApplyClear {
        operation_id: String,
        boundary: ClearBoundary,
        reply: Reply<usize>,
    },
}

#[derive(Clone)]
pub struct HistoryWorker {
    tx: mpsc::UnboundedSender<Command>,
    local_device_id: String,
    changes: watch::Sender<u64>,
}

impl HistoryWorker {
    pub fn start(
        database_path: PathBuf,
        local_device_id: String,
        local_label: Option<String>,
    ) -> Result<Self, HistoryError> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (changes, _) = watch::channel(0_u64);
        let worker_changes = changes.clone();
        let worker_device_id = local_device_id.clone();
        let (opened_tx, opened_rx) = std_mpsc::sync_channel(1);
        thread::Builder::new()
            .name("syntra-history".into())
            .spawn(move || {
                let mut store = match HistoryStore::open(database_path) {
                    Ok(store) => {
                        let _ = opened_tx.send(Ok(()));
                        store
                    }
                    Err(error) => {
                        let _ = opened_tx.send(Err(error));
                        return;
                    }
                };
                while let Some(command) = rx.blocking_recv() {
                    handle_command(
                        &mut store,
                        &worker_device_id,
                        local_label.as_deref(),
                        &worker_changes,
                        command,
                    );
                }
            })
            .map_err(|error| {
                HistoryError::InvalidPayload(format!("cannot start history worker: {error}"))
            })?;
        opened_rx.recv().map_err(|_| {
            HistoryError::InvalidPayload("history worker stopped during startup".into())
        })??;
        Ok(Self {
            tx,
            changes,
            local_device_id,
        })
    }

    pub fn local_device_id(&self) -> &str {
        &self.local_device_id
    }

    /// Subscribes to coalesced invalidations of visible history query results.
    pub fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub async fn record(&self, content: HistoryContent) -> Result<HistoryEventId, String> {
        self.request(|reply| Command::Record {
            device_id: None,
            label: None,
            content,
            reply,
        })
        .await
    }
    /// Records a local copy and returns the immutable event ready for peer export.
    pub async fn record_event(
        &self,
        content: HistoryContent,
    ) -> Result<ImportedHistoryEvent, String> {
        self.request(|reply| Command::RecordEvent { content, reply })
            .await
    }
    /// Records content observed from an authenticated peer whose legacy clipboard
    /// frame does not yet carry a durable history event id.
    pub async fn record_observed(
        &self,
        device_id: String,
        label: Option<String>,
        content: HistoryContent,
    ) -> Result<HistoryEventId, String> {
        self.request(|reply| Command::Record {
            device_id: Some(device_id),
            label,
            content,
            reply,
        })
        .await
    }
    pub async fn import(&self, event: ImportedHistoryEvent) -> Result<MergeOutcome, String> {
        self.request(|reply| Command::Import { event, reply }).await
    }
    pub async fn export(
        &self,
        offset: u64,
        limit: usize,
    ) -> Result<(Vec<ImportedHistoryEvent>, Option<u64>), String> {
        self.request(|reply| Command::Export {
            offset,
            limit,
            reply,
        })
        .await
    }
    pub async fn page(
        &self,
        query: String,
        offset: u64,
        limit: usize,
    ) -> Result<HistoryPage, String> {
        self.request(|reply| Command::Page {
            query,
            offset,
            limit,
            reply,
        })
        .await
    }
    pub async fn image(&self, event_id: HistoryEventId) -> Result<Option<ImagePayload>, String> {
        self.request(|reply| Command::Image { event_id, reply })
            .await
    }
    pub async fn set_pinned(&self, event_id: HistoryEventId, pinned: bool) -> Result<bool, String> {
        self.request(|reply| Command::Pin {
            event_id,
            pinned,
            reply,
        })
        .await
    }
    pub async fn snapshot_boundary(&self) -> Result<ClearBoundary, String> {
        self.request(|reply| Command::SnapshotBoundary { reply })
            .await
    }
    pub async fn apply_clear(
        &self,
        operation_id: String,
        boundary: ClearBoundary,
    ) -> Result<usize, String> {
        self.request(|reply| Command::ApplyClear {
            operation_id,
            boundary,
            reply,
        })
        .await
    }

    async fn request<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> Result<T, String> {
        let (reply, result) = oneshot::channel();
        self.tx
            .send(command(reply))
            .map_err(|_| "history worker stopped".to_string())?;
        result
            .await
            .map_err(|_| "history worker stopped".to_string())?
    }
}

fn handle_command(
    store: &mut HistoryStore,
    local_device_id: &str,
    local_label: Option<&str>,
    changes: &watch::Sender<u64>,
    command: Command,
) {
    match command {
        Command::Record {
            device_id,
            label,
            content,
            reply,
        } => {
            let result = (|| {
                let device_id = device_id.as_deref().unwrap_or(local_device_id);
                let sequence = store.next_sequence(device_id)?;
                let event_id = HistoryEventId {
                    origin_device_id: device_id.to_owned(),
                    origin_sequence: sequence,
                };
                let inserted = store.record(
                    content.kind(),
                    content,
                    HistoryOrigin {
                        device_id: device_id.to_owned(),
                        sequence,
                        label: label.or_else(|| local_label.map(str::to_owned)),
                    },
                )? == MergeOutcome::Inserted;
                Ok((event_id, inserted))
            })();
            respond_changed(reply, result, changes);
        }
        Command::RecordEvent { content, reply } => {
            let result = (|| {
                let sequence = store.next_sequence(local_device_id)?;
                let origin = HistoryOrigin {
                    device_id: local_device_id.to_owned(),
                    sequence,
                    label: local_label.map(str::to_owned),
                };
                let event_id = origin.event_id();
                let inserted =
                    store.record(content.kind(), content, origin)? == MergeOutcome::Inserted;
                let record = store.get(&event_id)?.ok_or_else(|| {
                    HistoryError::InvalidPayload("newly recorded history event disappeared".into())
                })?;
                Ok((
                    ImportedHistoryEvent {
                        event_id: record.event_id,
                        created_at_ms: record.created_at_ms,
                        origin_label: record.origin_label,
                        content: record.content,
                    },
                    inserted,
                ))
            })();
            respond_changed(reply, result, changes);
        }
        Command::Import { event, reply } => {
            let result = store
                .merge_imported(event)
                .map(|outcome| (outcome, outcome == MergeOutcome::Inserted));
            respond_changed(reply, result, changes);
        }
        Command::Export {
            offset,
            limit,
            reply,
        } => respond(reply, store.export_page(offset, limit.min(MAX_PAGE_SIZE))),
        Command::Page {
            query,
            offset,
            limit,
            reply,
        } => {
            respond(
                reply,
                store.page(&query, offset, limit.min(MAX_PAGE_SIZE)).map(
                    |(records, next_offset)| HistoryPage {
                        records: records.into_iter().map(summarize).collect(),
                        next_offset,
                    },
                ),
            );
        }
        Command::Image { event_id, reply } => respond(
            reply,
            store.get(&event_id).and_then(|record| {
                record
                    .and_then(|record| match record.content {
                        HistoryContent::Image {
                            bytes: Some(bytes),
                            media_type,
                            width,
                            height,
                        } => Some(thumbnail(bytes, media_type, width, height)),
                        _ => None,
                    })
                    .transpose()
            }),
        ),
        Command::Pin {
            event_id,
            pinned,
            reply,
        } => {
            let result = (|| {
                let Some(record) = store.get(&event_id)? else {
                    return Ok((false, false));
                };
                if record.pinned == pinned {
                    return Ok((false, false));
                }
                let updated = store.set_pinned(&event_id, pinned)?;
                Ok((updated, updated))
            })();
            respond_changed(reply, result, changes);
        }
        Command::SnapshotBoundary { reply } => respond(reply, store.snapshot_boundary()),
        Command::ApplyClear {
            operation_id,
            boundary,
            reply,
        } => {
            // Replays return the original count without changing visible rows.
            let before = store.connection.total_changes();
            let result = store
                .clear_unpinned_through(&operation_id, &boundary)
                .map(|affected| {
                    (
                        affected,
                        affected != 0 && store.connection.total_changes() != before,
                    )
                });
            respond_changed(reply, result, changes);
        }
    }
}

fn respond<T>(reply: Reply<T>, result: Result<T, HistoryError>) {
    let _ = reply.send(result.map_err(|error| error.to_string()));
}

fn respond_changed<T>(
    reply: Reply<T>,
    result: Result<(T, bool), HistoryError>,
    changes: &watch::Sender<u64>,
) {
    match result {
        Ok((value, true)) => {
            changes.send_modify(|revision| *revision = revision.wrapping_add(1));
            let _ = reply.send(Ok(value));
        }
        Ok((value, false)) => {
            let _ = reply.send(Ok(value));
        }
        Err(error) => {
            let _ = reply.send(Err(error.to_string()));
        }
    }
}

fn summarize(record: HistoryRecord) -> HistorySummary {
    let (kind, content) = match record.content {
        HistoryContent::Text(text) => {
            let preview = truncate_utf8(&text, MAX_TEXT_PREVIEW_BYTES);
            let truncated = preview.len() < text.len();
            (
                HistoryKind::Text,
                HistorySummaryContent::Text { preview, truncated },
            )
        }
        HistoryContent::Image {
            bytes,
            media_type,
            width,
            height,
        } => (
            HistoryKind::Image,
            HistorySummaryContent::Image {
                media_type,
                width,
                height,
                size_bytes: bytes.as_ref().map_or(0, |bytes| bytes.len() as u64),
            },
        ),
        HistoryContent::Files(files) => {
            let count = files.len().min(u32::MAX as usize) as u32;
            let total_size_bytes = files
                .iter()
                .fold(0u64, |total, file| total.saturating_add(file.size_bytes));
            let names = files
                .iter()
                .take(MAX_FILE_NAMES)
                .map(|file| file.name.clone())
                .collect();
            (
                HistoryKind::Files,
                HistorySummaryContent::Files {
                    count,
                    total_size_bytes,
                    names,
                    names_truncated: files.len() > MAX_FILE_NAMES,
                },
            )
        }
    };
    HistorySummary {
        event_id: record.event_id,
        created_at_ms: record.created_at_ms,
        origin_label: record.origin_label,
        pinned: record.pinned,
        kind,
        content,
    }
}

fn thumbnail(
    bytes: Vec<u8>,
    media_type: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<ImagePayload, HistoryError> {
    const MAX_EDGE: u32 = 256;
    let image = if media_type.as_deref() == Some("image/x-rgba8") {
        let (width, height) = width.zip(height).ok_or_else(|| {
            HistoryError::InvalidPayload("RGBA image is missing dimensions".into())
        })?;
        image::RgbaImage::from_raw(width, height, bytes)
            .map(image::DynamicImage::ImageRgba8)
            .ok_or_else(|| HistoryError::InvalidPayload("invalid RGBA image byte length".into()))?
    } else {
        image::load_from_memory(&bytes).map_err(|error| {
            HistoryError::InvalidPayload(format!("cannot decode history image: {error}"))
        })?
    };
    let image = image.thumbnail(MAX_EDGE, MAX_EDGE).into_rgba8();
    let (width, height) = image.dimensions();
    Ok(ImagePayload {
        bytes: image.into_raw(),
        media_type: Some("image/x-rgba8".into()),
        width: Some(width),
        height: Some(height),
    })
}

fn truncate_utf8(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}
