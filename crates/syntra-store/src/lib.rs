//! Persistent, local clipboard history storage.
//!
//! Clipboard events are immutable and identified by their originating device and
//! monotonically increasing origin sequence. Pin and dismissal state is local:
//! clearing history never emits or represents a remote deletion.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod worker;

const MAX_TEXT_BYTES: usize = 1024 * 1024;
pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_FILES: usize = 1024;
const MAX_METADATA_BYTES: usize = 16 * 1024;
const MAX_DEVICE_ID_BYTES: usize = 1024;
const MAX_ORIGIN_LABEL_BYTES: usize = 16 * 1024;
const MAX_OPERATION_ID_BYTES: usize = 1024;

/// Inclusive maximum observed sequence for each event origin.
pub type ClearBoundary = BTreeMap<String, u64>;

/// Globally stable identity assigned by the device that first observed a copy.
/// Remote forwarding must retain this identity rather than allocating a new one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HistoryEventId {
    pub origin_device_id: String,
    pub origin_sequence: u64,
}

/// Immutable information about the device that originated an event.
/// `device_id` is an application-provided durable id, never inferred from a hostname.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryOrigin {
    pub device_id: String,
    pub sequence: u64,
    pub label: Option<String>,
}

impl HistoryOrigin {
    pub fn event_id(&self) -> HistoryEventId {
        HistoryEventId {
            origin_device_id: self.device_id.clone(),
            origin_sequence: self.sequence,
        }
    }
}

/// The payload category stored in a history event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryKind {
    Text,
    Image,
    Files,
}

impl HistoryKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Files => "files",
        }
    }

    fn parse(value: &str) -> Result<Self, HistoryError> {
        match value {
            "text" => Ok(Self::Text),
            "image" => Ok(Self::Image),
            "files" => Ok(Self::Files),
            other => Err(HistoryError::CorruptData(format!("unknown kind {other:?}"))),
        }
    }
}

/// Durable metadata for one copied file. Paths and handles are intentionally absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMetadata {
    pub name: String,
    pub size_bytes: u64,
    pub media_type: Option<String>,
}

/// A locally owned copy of clipboard content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryContent {
    Text(String),
    Image {
        bytes: Option<Vec<u8>>,
        media_type: Option<String>,
        width: Option<u32>,
        height: Option<u32>,
    },
    Files(Vec<FileMetadata>),
}

impl HistoryContent {
    fn kind(&self) -> HistoryKind {
        match self {
            Self::Text(_) => HistoryKind::Text,
            Self::Image { .. } => HistoryKind::Image,
            Self::Files(_) => HistoryKind::Files,
        }
    }
}

/// Immutable event plus this installation's mutable pin state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRecord {
    pub event_id: HistoryEventId,
    /// Milliseconds since the Unix epoch, assigned by the originating device.
    pub created_at_ms: i64,
    pub origin_label: Option<String>,
    pub pinned: bool,
    pub content: HistoryContent,
}

/// An immutable event received during device reconciliation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedHistoryEvent {
    pub event_id: HistoryEventId,
    pub created_at_ms: i64,
    pub origin_label: Option<String>,
    pub content: HistoryContent,
}

/// Result of an idempotent [`HistoryStore::merge_imported`] operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeOutcome {
    Inserted,
    AlreadyPresent,
    /// Already known and locally dismissed; it remains absent from list/search.
    Suppressed,
}

/// Failures opening, validating, or querying the history database.
#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("clipboard history database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("invalid clipboard history payload: {0}")]
    InvalidPayload(String),
    #[error("clipboard history database contains invalid data: {0}")]
    CorruptData(String),
    #[error("history event identity collision for {0:?}")]
    EventConflict(HistoryEventId),
    #[error("clear operation {0:?} was reused with a different boundary")]
    ClearOperationConflict(String),
    #[error("system clock is before the Unix epoch")]
    InvalidClock,
}

/// Synchronous SQLite-backed clipboard history, intended for one worker thread.
pub struct HistoryStore {
    connection: Connection,
}

impl HistoryStore {
    /// Opens or creates a local database and initializes its schema.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, HistoryError> {
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS history_events (
                 origin_device_id TEXT NOT NULL,
                 origin_sequence INTEGER NOT NULL,
                 kind TEXT NOT NULL CHECK(kind IN ('text', 'image', 'files')),
                 created_at_ms INTEGER NOT NULL,
                 origin_label TEXT,
                 text_payload TEXT,
                 image_payload BLOB,
                 media_type TEXT,
                 image_width INTEGER,
                 image_height INTEGER,
                 files_json TEXT,
                 PRIMARY KEY(origin_device_id, origin_sequence)
             );
             CREATE TABLE IF NOT EXISTS history_local_state (
                 origin_device_id TEXT NOT NULL,
                 origin_sequence INTEGER NOT NULL,
                 pinned INTEGER NOT NULL DEFAULT 0 CHECK(pinned IN (0, 1)),
                 dismissed INTEGER NOT NULL DEFAULT 0 CHECK(dismissed IN (0, 1)),
                 PRIMARY KEY(origin_device_id, origin_sequence),
                 FOREIGN KEY(origin_device_id, origin_sequence)
                     REFERENCES history_events(origin_device_id, origin_sequence)
                     ON DELETE CASCADE
             );
             CREATE TABLE IF NOT EXISTS history_clear_operations (
                 operation_id TEXT PRIMARY KEY,
                 boundary_json TEXT NOT NULL,
                 affected_count INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS history_clear_boundaries (
                 operation_id TEXT NOT NULL,
                 origin_device_id TEXT NOT NULL,
                 maximum_sequence INTEGER NOT NULL,
                 PRIMARY KEY(operation_id, origin_device_id),
                 FOREIGN KEY(operation_id) REFERENCES history_clear_operations(operation_id)
                     ON DELETE CASCADE
             );
             CREATE INDEX IF NOT EXISTS history_clear_boundary_lookup
                 ON history_clear_boundaries(origin_device_id, maximum_sequence);
             CREATE INDEX IF NOT EXISTS history_events_created
                 ON history_events(created_at_ms DESC, origin_device_id, origin_sequence);",
        )?;
        ensure_column(&connection, "history_events", "image_width", "INTEGER")?;
        ensure_column(&connection, "history_events", "image_height", "INTEGER")?;
        Ok(Self { connection })
    }
    /// Returns the next persistent origin sequence for `origin_device_id`.
    ///
    /// Existing events and persisted clear boundaries both participate, so
    /// sequences never rewind or repeatedly collide with a suppression range.
    /// Allocate and immediately pass the result to [`Self::record`].
    pub fn next_sequence(&self, origin_device_id: &str) -> Result<u64, HistoryError> {
        validate_string("origin device id", origin_device_id, MAX_DEVICE_ID_BYTES)?;
        let maximum: Option<i64> = self.connection.query_row(
            "SELECT MAX(value) FROM (
                 SELECT MAX(origin_sequence) AS value FROM history_events
                 WHERE origin_device_id = ?1
                 UNION ALL
                 SELECT MAX(maximum_sequence) AS value FROM history_clear_boundaries
                 WHERE origin_device_id = ?1
             )",
            params![origin_device_id],
            |row| row.get(0),
        )?;
        let next = maximum.unwrap_or(0).checked_add(1).ok_or_else(|| {
            HistoryError::InvalidPayload("origin sequence exhausted SQLite INTEGER".into())
        })?;
        u64::try_from(next)
            .map_err(|_| HistoryError::CorruptData("negative persisted origin sequence".into()))
    }

    /// Stores a newly observed event using its application-assigned stable origin id.
    /// New local copies are unpinned. Reusing an id is idempotent only when every
    /// immutable field is identical; otherwise [`HistoryError::EventConflict`] is returned.
    pub fn record(
        &mut self,
        kind: HistoryKind,
        content: HistoryContent,
        origin: HistoryOrigin,
    ) -> Result<MergeOutcome, HistoryError> {
        if kind != content.kind() {
            return Err(HistoryError::InvalidPayload(format!(
                "kind {:?} does not match {:?} content",
                kind,
                content.kind()
            )));
        }
        self.merge_imported(ImportedHistoryEvent {
            event_id: origin.event_id(),
            created_at_ms: now_ms()?,
            origin_label: origin.label,
            content,
        })
    }

    /// Idempotently merges a remote event without changing local pin/dismissal state.
    /// Callers must forward the original `event_id` and timestamp unchanged.
    pub fn merge_imported(
        &mut self,
        event: ImportedHistoryEvent,
    ) -> Result<MergeOutcome, HistoryError> {
        validate_event(&event)?;
        let transaction = self.connection.transaction()?;
        let dismissed = local_flags(&transaction, &event.event_id)?.1;
        if dismissed {
            transaction.commit()?;
            return Ok(MergeOutcome::Suppressed);
        }
        if let Some(existing) = load_event(&transaction, &event.event_id)? {
            if immutable_event(&existing) != event {
                return Err(HistoryError::EventConflict(event.event_id));
            }
            transaction.commit()?;
            return Ok(MergeOutcome::AlreadyPresent);
        }
        if covered_by_clear(&transaction, &event.event_id)? {
            transaction.commit()?;
            return Ok(MergeOutcome::Suppressed);
        }
        insert_event(&transaction, &event)?;
        transaction.commit()?;
        Ok(MergeOutcome::Inserted)
    }

    /// Returns visible records newest first. Locally dismissed events are excluded.
    pub fn list(&self) -> Result<Vec<HistoryRecord>, HistoryError> {
        self.query_records(
            "SELECT e.origin_device_id, e.origin_sequence, e.kind, e.created_at_ms,
                    e.origin_label, COALESCE(s.pinned, 0), e.text_payload,
                    e.image_payload, e.media_type, e.image_width, e.image_height, e.files_json
             FROM history_events e
             LEFT JOIN history_local_state s USING(origin_device_id, origin_sequence)
             WHERE COALESCE(s.dismissed, 0) = 0
             ORDER BY e.created_at_ms DESC, e.origin_device_id, e.origin_sequence DESC",
            None,
        )
    }

    /// Searches visible text, media type, file metadata, and origin labels.
    pub fn search(&self, query: &str) -> Result<Vec<HistoryRecord>, HistoryError> {
        if query.is_empty() {
            return self.list();
        }
        let pattern = format!("%{}%", escape_like(query));
        self.query_records(
            "SELECT e.origin_device_id, e.origin_sequence, e.kind, e.created_at_ms,
                    e.origin_label, COALESCE(s.pinned, 0), e.text_payload,
                    e.image_payload, e.media_type, e.image_width, e.image_height, e.files_json
             FROM history_events e
             LEFT JOIN history_local_state s USING(origin_device_id, origin_sequence)
             WHERE COALESCE(s.dismissed, 0) = 0 AND (
                    e.origin_label LIKE ?1 ESCAPE '\\' COLLATE NOCASE OR
                    e.text_payload LIKE ?1 ESCAPE '\\' COLLATE NOCASE OR
                    e.media_type LIKE ?1 ESCAPE '\\' COLLATE NOCASE OR
                    e.files_json LIKE ?1 ESCAPE '\\' COLLATE NOCASE)
             ORDER BY e.created_at_ms DESC, e.origin_device_id, e.origin_sequence DESC",
            Some(&pattern),
        )
    }

    /// Changes local pin state. Returns `false` when the event is unknown or dismissed.
    pub fn set_pinned(
        &self,
        event_id: &HistoryEventId,
        pinned: bool,
    ) -> Result<bool, HistoryError> {
        let changed = self.connection.execute(
            "INSERT INTO history_local_state
                 (origin_device_id, origin_sequence, pinned, dismissed)
             SELECT ?1, ?2, ?3, 0
             WHERE EXISTS (
                 SELECT 1 FROM history_events
                 WHERE origin_device_id = ?1 AND origin_sequence = ?2)
             ON CONFLICT(origin_device_id, origin_sequence) DO UPDATE SET pinned = excluded.pinned
             WHERE history_local_state.dismissed = 0",
            params![
                event_id.origin_device_id,
                sequence_to_sql(event_id.origin_sequence)?,
                pinned
            ],
        )?;
        Ok(changed != 0)
    }
    /// Captures the inclusive highest retained sequence for every known origin.
    pub fn snapshot_boundary(&self) -> Result<ClearBoundary, HistoryError> {
        let mut statement = self.connection.prepare(
            "SELECT origin_device_id, MAX(origin_sequence)
             FROM history_events GROUP BY origin_device_id",
        )?;
        let mut rows = statement.query([])?;
        let mut boundary = ClearBoundary::new();
        while let Some(row) = rows.next()? {
            let device_id: String = row.get(0)?;
            let sequence: i64 = row.get(1)?;
            boundary.insert(
                device_id,
                u64::try_from(sequence)
                    .map_err(|_| HistoryError::CorruptData("negative origin sequence".into()))?,
            );
        }
        Ok(boundary)
    }

    /// Applies an idempotent clear operation through an inclusive per-origin boundary.
    ///
    /// Origins absent from `boundary`, locally pinned events, and events above a
    /// boundary are untouched. Persisted boundaries suppress covered events that
    /// arrive later through reconciliation. Reusing `operation_id` with an identical
    /// boundary returns the original affected count; differing reuse is rejected.
    pub fn clear_unpinned_through(
        &mut self,
        operation_id: &str,
        boundary: &ClearBoundary,
    ) -> Result<usize, HistoryError> {
        validate_string("clear operation id", operation_id, MAX_OPERATION_ID_BYTES)?;
        for device_id in boundary.keys() {
            validate_string("boundary device id", device_id, MAX_DEVICE_ID_BYTES)?;
        }
        let boundary_json = serde_json::to_string(boundary).map_err(|error| {
            HistoryError::InvalidPayload(format!("clear boundary is not serializable: {error}"))
        })?;
        let transaction = self.connection.transaction()?;
        let existing: Option<(String, i64)> = transaction
            .query_row(
                "SELECT boundary_json, affected_count FROM history_clear_operations
                 WHERE operation_id = ?1",
                params![operation_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((stored_boundary, affected)) = existing {
            if stored_boundary != boundary_json {
                return Err(HistoryError::ClearOperationConflict(operation_id.into()));
            }
            return usize::try_from(affected).map_err(|_| {
                HistoryError::CorruptData("invalid stored clear affected count".into())
            });
        }

        transaction.execute(
            "INSERT INTO history_clear_operations
                 (operation_id, boundary_json, affected_count) VALUES (?1, ?2, 0)",
            params![operation_id, boundary_json],
        )?;
        let mut affected = 0usize;
        for (device_id, maximum_sequence) in boundary {
            let maximum_sequence = sequence_to_sql(*maximum_sequence)?;
            transaction.execute(
                "INSERT INTO history_clear_boundaries
                     (operation_id, origin_device_id, maximum_sequence)
                 VALUES (?1, ?2, ?3)",
                params![operation_id, device_id, maximum_sequence],
            )?;
            affected += transaction.execute(
                "INSERT INTO history_local_state
                     (origin_device_id, origin_sequence, pinned, dismissed)
                 SELECT e.origin_device_id, e.origin_sequence, 0, 1
                 FROM history_events e
                 LEFT JOIN history_local_state s
                   USING(origin_device_id, origin_sequence)
                 WHERE e.origin_device_id = ?1 AND e.origin_sequence <= ?2
                   AND COALESCE(s.pinned, 0) = 0
                   AND COALESCE(s.dismissed, 0) = 0
                 ON CONFLICT(origin_device_id, origin_sequence)
                 DO UPDATE SET dismissed = 1
                 WHERE history_local_state.pinned = 0
                   AND history_local_state.dismissed = 0",
                params![device_id, maximum_sequence],
            )?;
            transaction.execute(
                "UPDATE history_events
                 SET text_payload = NULL, image_payload = NULL,
                     media_type = NULL, files_json = NULL
                 WHERE origin_device_id = ?1 AND origin_sequence <= ?2
                   AND EXISTS (
                       SELECT 1 FROM history_local_state s
                       WHERE s.origin_device_id = history_events.origin_device_id
                         AND s.origin_sequence = history_events.origin_sequence
                         AND s.pinned = 0 AND s.dismissed = 1
                   )",
                params![device_id, maximum_sequence],
            )?;
        }
        transaction.execute(
            "UPDATE history_clear_operations SET affected_count = ?1
             WHERE operation_id = ?2",
            params![
                i64::try_from(affected).map_err(|_| {
                    HistoryError::InvalidPayload(
                        "clear affected count exceeds SQLite INTEGER".into(),
                    )
                })?,
                operation_id
            ],
        )?;
        transaction.commit()?;
        Ok(affected)
    }

    /// Atomically hides every visible, unpinned event on this installation.
    ///
    /// Pinned events and payloads remain intact. Dismissal markers are retained, so
    /// importing the same immutable events after reconnect cannot resurrect them.
    /// No remote deletion information is produced.
    pub fn clear_unpinned(&mut self) -> Result<usize, HistoryError> {
        let transaction = self.connection.transaction()?;
        let removed = transaction.execute(
            "INSERT INTO history_local_state
                 (origin_device_id, origin_sequence, pinned, dismissed)
             SELECT e.origin_device_id, e.origin_sequence, 0, 1
             FROM history_events e
             LEFT JOIN history_local_state s USING(origin_device_id, origin_sequence)
             WHERE COALESCE(s.pinned, 0) = 0 AND COALESCE(s.dismissed, 0) = 0
             ON CONFLICT(origin_device_id, origin_sequence) DO UPDATE SET dismissed = 1
             WHERE history_local_state.pinned = 0 AND history_local_state.dismissed = 0",
            [],
        )?;
        transaction.execute(
            "UPDATE history_events
             SET text_payload = NULL, image_payload = NULL,
                 media_type = NULL, files_json = NULL
             WHERE EXISTS (
                 SELECT 1 FROM history_local_state s
                 WHERE s.origin_device_id = history_events.origin_device_id
                   AND s.origin_sequence = history_events.origin_sequence
                   AND s.pinned = 0 AND s.dismissed = 1
             )",
            [],
        )?;
        transaction.commit()?;
        Ok(removed)
    }

    /// Returns one visible event by stable identity, including its payload.
    pub fn get(&self, event_id: &HistoryEventId) -> Result<Option<HistoryRecord>, HistoryError> {
        let mut statement = self.connection.prepare(
            "SELECT e.origin_device_id, e.origin_sequence, e.kind, e.created_at_ms,
                    e.origin_label, COALESCE(s.pinned, 0), e.text_payload,
                    e.image_payload, e.media_type, e.image_width, e.image_height, e.files_json
             FROM history_events e
             LEFT JOIN history_local_state s USING(origin_device_id, origin_sequence)
             WHERE e.origin_device_id = ?1 AND e.origin_sequence = ?2
               AND COALESCE(s.dismissed, 0) = 0",
        )?;
        statement
            .query_row(
                params![
                    event_id.origin_device_id,
                    sequence_to_sql(event_id.origin_sequence)?
                ],
                decode_record,
            )
            .optional()
            .map_err(HistoryError::from)
    }

    /// Returns a bounded page of visible events. `limit` is always clamped to 50.
    pub fn page(
        &self,
        query: &str,
        offset: u64,
        limit: usize,
    ) -> Result<(Vec<HistoryRecord>, Option<u64>), HistoryError> {
        let limit = limit.clamp(1, 50);
        let all = if query.is_empty() {
            self.list()?
        } else {
            self.search(query)?
        };
        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(all.len());
        let end = start.saturating_add(limit).min(all.len());
        let next = (end < all.len()).then_some(end as u64);
        Ok((all.into_iter().skip(start).take(limit).collect(), next))
    }

    /// Exports an immutable bounded page for authenticated peer reconciliation.
    pub fn export_page(
        &self,
        offset: u64,
        limit: usize,
    ) -> Result<(Vec<ImportedHistoryEvent>, Option<u64>), HistoryError> {
        let (records, next) = self.page("", offset, limit)?;
        Ok((records.iter().map(immutable_event).collect(), next))
    }

    fn query_records(
        &self,
        sql: &str,
        pattern: Option<&str>,
    ) -> Result<Vec<HistoryRecord>, HistoryError> {
        let mut statement = self.connection.prepare(sql)?;
        let mut rows = match pattern {
            Some(pattern) => statement.query(params![pattern])?,
            None => statement.query([])?,
        };
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            records.push(decode_record(row)?);
        }
        Ok(records)
    }
}

fn insert_event(
    transaction: &Transaction<'_>,
    event: &ImportedHistoryEvent,
) -> Result<(), HistoryError> {
    let (text, image, media_type, width, height, files_json) = encode_content(&event.content)?;
    transaction.execute(
        "INSERT INTO history_events
             (origin_device_id, origin_sequence, kind, created_at_ms, origin_label,
              text_payload, image_payload, media_type, image_width, image_height, files_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            event.event_id.origin_device_id,
            sequence_to_sql(event.event_id.origin_sequence)?,
            event.content.kind().as_str(),
            event.created_at_ms,
            event.origin_label,
            text,
            image,
            media_type,
            width,
            height,
            files_json
        ],
    )?;
    Ok(())
}

fn load_event(
    transaction: &Transaction<'_>,
    event_id: &HistoryEventId,
) -> Result<Option<HistoryRecord>, HistoryError> {
    transaction
        .query_row(
            "SELECT e.origin_device_id, e.origin_sequence, e.kind, e.created_at_ms,
                    e.origin_label, COALESCE(s.pinned, 0), e.text_payload,
                    e.image_payload, e.media_type, e.image_width, e.image_height, e.files_json
             FROM history_events e
             LEFT JOIN history_local_state s USING(origin_device_id, origin_sequence)
             WHERE e.origin_device_id = ?1 AND e.origin_sequence = ?2",
            params![
                event_id.origin_device_id,
                sequence_to_sql(event_id.origin_sequence)?
            ],
            decode_record,
        )
        .optional()
        .map_err(HistoryError::from)
}

fn covered_by_clear(
    transaction: &Transaction<'_>,
    event_id: &HistoryEventId,
) -> Result<bool, HistoryError> {
    transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM history_clear_boundaries
                 WHERE origin_device_id = ?1 AND maximum_sequence >= ?2
             )",
            params![
                event_id.origin_device_id,
                sequence_to_sql(event_id.origin_sequence)?
            ],
            |row| row.get(0),
        )
        .map_err(HistoryError::from)
}

fn local_flags(
    transaction: &Transaction<'_>,
    event_id: &HistoryEventId,
) -> Result<(bool, bool), HistoryError> {
    Ok(transaction
        .query_row(
            "SELECT pinned, dismissed FROM history_local_state
             WHERE origin_device_id = ?1 AND origin_sequence = ?2",
            params![
                event_id.origin_device_id,
                sequence_to_sql(event_id.origin_sequence)?
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .unwrap_or((false, false)))
}

fn decode_record(row: &rusqlite::Row<'_>) -> Result<HistoryRecord, rusqlite::Error> {
    let device_id: String = row.get(0)?;
    let sequence: i64 = row.get(1)?;
    let kind_value: String = row.get(2)?;
    let kind = HistoryKind::parse(&kind_value).map_err(conversion_error)?;
    let content = match kind {
        HistoryKind::Text => {
            HistoryContent::Text(row.get::<_, Option<String>>(6)?.ok_or_else(|| {
                conversion_error(HistoryError::CorruptData("text payload missing".into()))
            })?)
        }
        HistoryKind::Image => HistoryContent::Image {
            bytes: row.get(7)?,
            media_type: row.get(8)?,
            width: row.get(9)?,
            height: row.get(10)?,
        },
        HistoryKind::Files => {
            let json: Option<String> = row.get(11)?;
            HistoryContent::Files(
                serde_json::from_str(json.as_deref().ok_or_else(|| {
                    conversion_error(HistoryError::CorruptData("file metadata missing".into()))
                })?)
                .map_err(|error| conversion_error(HistoryError::CorruptData(error.to_string())))?,
            )
        }
    };
    Ok(HistoryRecord {
        event_id: HistoryEventId {
            origin_device_id: device_id,
            origin_sequence: u64::try_from(sequence).map_err(|_| {
                conversion_error(HistoryError::CorruptData("negative origin sequence".into()))
            })?,
        },
        created_at_ms: row.get(3)?,
        origin_label: row.get(4)?,
        pinned: row.get(5)?,
        content,
    })
}

fn immutable_event(record: &HistoryRecord) -> ImportedHistoryEvent {
    ImportedHistoryEvent {
        event_id: record.event_id.clone(),
        created_at_ms: record.created_at_ms,
        origin_label: record.origin_label.clone(),
        content: record.content.clone(),
    }
}

/// The nullable columns one history record occupies.
///
/// In order: text, image bytes, media type, width, height, file name. Each
/// content kind fills a different subset, which is why every column is
/// optional.
type EncodedContent = (
    Option<String>,
    Option<Vec<u8>>,
    Option<String>,
    Option<u32>,
    Option<u32>,
    Option<String>,
);

fn encode_content(content: &HistoryContent) -> Result<EncodedContent, HistoryError> {
    Ok(match content {
        HistoryContent::Text(value) => (Some(value.clone()), None, None, None, None, None),
        HistoryContent::Image {
            bytes,
            media_type,
            width,
            height,
        } => (
            None,
            bytes.clone(),
            media_type.clone(),
            *width,
            *height,
            None,
        ),
        HistoryContent::Files(files) => (
            None,
            None,
            None,
            None,
            None,
            Some(serde_json::to_string(files).map_err(|error| {
                HistoryError::InvalidPayload(format!("file metadata is not serializable: {error}"))
            })?),
        ),
    })
}

fn validate_event(event: &ImportedHistoryEvent) -> Result<(), HistoryError> {
    validate_string(
        "origin device id",
        &event.event_id.origin_device_id,
        MAX_DEVICE_ID_BYTES,
    )?;
    if let Some(label) = event.origin_label.as_deref() {
        validate_string("origin label", label, MAX_ORIGIN_LABEL_BYTES)?;
    }
    sequence_to_sql(event.event_id.origin_sequence)?;
    match &event.content {
        HistoryContent::Text(value) => validate_string("text", value, MAX_TEXT_BYTES)?,
        HistoryContent::Image {
            bytes,
            media_type,
            width,
            height,
        } => {
            if bytes
                .as_ref()
                .is_some_and(|value| value.len() > MAX_IMAGE_BYTES)
            {
                return Err(HistoryError::InvalidPayload(format!(
                    "image exceeds {MAX_IMAGE_BYTES} bytes"
                )));
            }
            if bytes.as_ref().is_some_and(Vec::is_empty) {
                return Err(HistoryError::InvalidPayload(
                    "image bytes must not be empty".into(),
                ));
            }
            if let Some(value) = media_type.as_deref() {
                validate_string("image media type", value, MAX_METADATA_BYTES)?;
            }
            if width.is_some() != height.is_some() {
                return Err(HistoryError::InvalidPayload(
                    "image width and height must be supplied together".into(),
                ));
            }
            if width.is_some_and(|value| value == 0) || height.is_some_and(|value| value == 0) {
                return Err(HistoryError::InvalidPayload(
                    "image dimensions must be positive".into(),
                ));
            }
            if media_type.as_deref() == Some("image/x-rgba8") {
                let expected = width
                    .zip(*height)
                    .and_then(|(width, height)| {
                        usize::try_from(width)
                            .ok()?
                            .checked_mul(usize::try_from(height).ok()?)?
                            .checked_mul(4)
                    })
                    .ok_or_else(|| {
                        HistoryError::InvalidPayload(
                            "RGBA image requires valid width and height".into(),
                        )
                    })?;
                if bytes.as_ref().is_none_or(|bytes| bytes.len() != expected) {
                    return Err(HistoryError::InvalidPayload(
                        "RGBA image byte length does not match width * height * 4".into(),
                    ));
                }
            }
            if bytes.is_none() && media_type.is_none() {
                return Err(HistoryError::InvalidPayload(
                    "image requires bytes or a media type".into(),
                ));
            }
        }
        HistoryContent::Files(files) => {
            if files.is_empty() || files.len() > MAX_FILES {
                return Err(HistoryError::InvalidPayload(format!(
                    "file metadata must contain 1 to {MAX_FILES} entries"
                )));
            }
            for file in files {
                validate_string("file name", &file.name, MAX_METADATA_BYTES)?;
                if let Some(value) = file.media_type.as_deref() {
                    validate_string("file media type", value, MAX_METADATA_BYTES)?;
                }
            }
        }
    }
    Ok(())
}

fn validate_string(label: &str, value: &str, maximum: usize) -> Result<(), HistoryError> {
    if value.is_empty() {
        return Err(HistoryError::InvalidPayload(format!(
            "{label} must not be empty"
        )));
    }
    if value.len() > maximum {
        return Err(HistoryError::InvalidPayload(format!(
            "{label} exceeds {maximum} bytes"
        )));
    }
    Ok(())
}

fn sequence_to_sql(sequence: u64) -> Result<i64, HistoryError> {
    i64::try_from(sequence)
        .map_err(|_| HistoryError::InvalidPayload("origin sequence exceeds SQLite INTEGER".into()))
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    declaration: &str,
) -> Result<(), HistoryError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(());
        }
    }
    connection.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {declaration}"),
        [],
    )?;
    Ok(())
}

fn now_ms() -> Result<i64, HistoryError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| HistoryError::InvalidClock)?;
    i64::try_from(duration.as_millis())
        .map_err(|_| HistoryError::InvalidPayload("timestamp exceeds SQLite INTEGER".into()))
}

fn conversion_error(error: HistoryError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    struct TestDatabase(PathBuf);

    impl TestDatabase {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            Self(std::env::temp_dir().join(format!(
                "syntra-history-{name}-{}-{unique}.sqlite3",
                std::process::id()
            )))
        }
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn imported(sequence: u64, content: HistoryContent) -> ImportedHistoryEvent {
        ImportedHistoryEvent {
            event_id: HistoryEventId {
                origin_device_id: "durable-device-id".into(),
                origin_sequence: sequence,
            },
            created_at_ms: 1_800_000_000_000 + sequence as i64,
            origin_label: Some("Office laptop".into()),
            content,
        }
    }

    #[test]
    fn pin_survives_database_reopen() {
        let database = TestDatabase::new("pin-reopen");
        let event = imported(
            1,
            HistoryContent::Image {
                bytes: Some(vec![1, 2, 3, 4]),
                media_type: Some("image/png".into()),
                width: None,
                height: None,
            },
        );
        {
            let mut store = HistoryStore::open(&database.0).unwrap();
            assert_eq!(
                store.merge_imported(event.clone()).unwrap(),
                MergeOutcome::Inserted
            );
            assert!(store.set_pinned(&event.event_id, true).unwrap());
        }

        let reopened = HistoryStore::open(&database.0).unwrap();
        let records = reopened.list().unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].pinned);
        assert_eq!(records[0].content, event.content);
    }

    #[test]
    fn clear_preserves_pins_and_suppresses_reimported_events() {
        let database = TestDatabase::new("clear-reconcile");
        let pinned = imported(
            1,
            HistoryContent::Files(vec![FileMetadata {
                name: "report.pdf".into(),
                size_bytes: 42,
                media_type: Some("application/pdf".into()),
            }]),
        );
        let cleared = imported(2, HistoryContent::Text("remove me".into()));
        {
            let mut store = HistoryStore::open(&database.0).unwrap();
            store.merge_imported(pinned.clone()).unwrap();
            store.merge_imported(cleared.clone()).unwrap();
            assert!(store.set_pinned(&pinned.event_id, true).unwrap());
            assert_eq!(store.clear_unpinned().unwrap(), 1);
        }

        let mut reopened = HistoryStore::open(&database.0).unwrap();
        assert_eq!(
            reopened.merge_imported(cleared).unwrap(),
            MergeOutcome::Suppressed
        );
        let records = reopened.list().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].event_id, pinned.event_id);
        assert!(records[0].pinned);
        assert_eq!(records[0].content, pinned.content);
    }

    #[test]
    fn conflicting_reuse_of_event_id_is_rejected() {
        let database = TestDatabase::new("identity-conflict");
        let mut store = HistoryStore::open(&database.0).unwrap();
        let first = imported(9, HistoryContent::Text("first".into()));
        let mut conflict = first.clone();
        conflict.content = HistoryContent::Text("different".into());
        store.merge_imported(first).unwrap();
        assert!(matches!(
            store.merge_imported(conflict),
            Err(HistoryError::EventConflict(_))
        ));
    }
    #[test]
    fn next_sequence_survives_clear_and_reopen() {
        let database = TestDatabase::new("persistent-sequence");
        {
            let mut store = HistoryStore::open(&database.0).unwrap();
            for expected in 1..=2 {
                let sequence = store.next_sequence("local-device-id").unwrap();
                assert_eq!(sequence, expected);
                assert_eq!(
                    store
                        .record(
                            HistoryKind::Text,
                            HistoryContent::Text(format!("copy {expected}")),
                            HistoryOrigin {
                                device_id: "local-device-id".into(),
                                sequence,
                                label: None,
                            },
                        )
                        .unwrap(),
                    MergeOutcome::Inserted
                );
            }
            assert_eq!(store.clear_unpinned().unwrap(), 2);
        }

        let mut reopened = HistoryStore::open(&database.0).unwrap();
        let sequence = reopened.next_sequence("local-device-id").unwrap();
        assert_eq!(sequence, 3);
        assert_eq!(
            reopened
                .record(
                    HistoryKind::Text,
                    HistoryContent::Text("copy 3".into()),
                    HistoryOrigin {
                        device_id: "local-device-id".into(),
                        sequence,
                        label: None,
                    },
                )
                .unwrap(),
            MergeOutcome::Inserted
        );
    }

    #[test]
    fn bounded_clear_is_idempotent_and_suppresses_only_covered_events() {
        let database = TestDatabase::new("bounded-clear");
        let pinned = imported(1, HistoryContent::Text("pinned".into()));
        let covered = imported(2, HistoryContent::Text("covered".into()));
        let higher = imported(5, HistoryContent::Text("higher".into()));
        let mut other_origin = imported(1, HistoryContent::Text("other origin".into()));
        other_origin.event_id.origin_device_id = "other-device".into();
        let late = imported(4, HistoryContent::Text("late covered event".into()));

        let mut store = HistoryStore::open(&database.0).unwrap();
        store.merge_imported(pinned.clone()).unwrap();
        store.merge_imported(covered.clone()).unwrap();
        let mut boundary = store.snapshot_boundary().unwrap();
        assert_eq!(boundary.get("durable-device-id"), Some(&2));
        boundary.insert("durable-device-id".into(), 4);
        store.merge_imported(higher.clone()).unwrap();
        store.merge_imported(other_origin.clone()).unwrap();
        assert!(store.set_pinned(&pinned.event_id, true).unwrap());

        assert_eq!(
            store
                .clear_unpinned_through("clear-operation-1", &boundary)
                .unwrap(),
            1
        );
        let covered_payload_was_scrubbed: bool = store
            .connection
            .query_row(
                "SELECT text_payload IS NULL FROM history_events
                 WHERE origin_device_id = ?1 AND origin_sequence = ?2",
                params![
                    covered.event_id.origin_device_id,
                    sequence_to_sql(covered.event_id.origin_sequence).unwrap()
                ],
                |row| row.get(0),
            )
            .unwrap();
        let pinned_payload_remains: bool = store
            .connection
            .query_row(
                "SELECT text_payload IS NOT NULL FROM history_events
                 WHERE origin_device_id = ?1 AND origin_sequence = ?2",
                params![
                    pinned.event_id.origin_device_id,
                    sequence_to_sql(pinned.event_id.origin_sequence).unwrap()
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert!(covered_payload_was_scrubbed);
        assert!(pinned_payload_remains);
        assert_eq!(
            store
                .clear_unpinned_through("clear-operation-1", &boundary)
                .unwrap(),
            1
        );
        assert_eq!(
            store.merge_imported(late.clone()).unwrap(),
            MergeOutcome::Suppressed
        );
        assert_eq!(
            store.merge_imported(late).unwrap(),
            MergeOutcome::Suppressed
        );

        let records = store.list().unwrap();
        assert_eq!(records.len(), 3);
        assert!(
            records
                .iter()
                .any(|record| record.event_id == pinned.event_id && record.pinned)
        );
        assert!(
            records
                .iter()
                .any(|record| record.event_id == higher.event_id)
        );
        assert!(
            records
                .iter()
                .any(|record| record.event_id == other_origin.event_id)
        );
    }

    #[test]
    fn local_sequence_advances_beyond_persisted_clear_boundary() {
        let database = TestDatabase::new("sequence-beyond-boundary");
        let mut store = HistoryStore::open(&database.0).unwrap();
        let mut boundary = ClearBoundary::new();
        boundary.insert("local-device".into(), 10);
        store
            .clear_unpinned_through("clear-own-boundary", &boundary)
            .unwrap();

        let sequence = store.next_sequence("local-device").unwrap();
        assert_eq!(sequence, 11);
        assert_eq!(
            store
                .record(
                    HistoryKind::Text,
                    HistoryContent::Text("new local copy".into()),
                    HistoryOrigin {
                        device_id: "local-device".into(),
                        sequence,
                        label: None,
                    },
                )
                .unwrap(),
            MergeOutcome::Inserted
        );
        assert_eq!(store.list().unwrap().len(), 1);
    }
}
