use crate::file_transfer::{FileOffer, IncomingTransfer, OutgoingFile, TransferError};
use std::{
    collections::{HashMap, VecDeque},
    fmt::Debug,
    hash::Hash,
    path::PathBuf,
    time::{Duration, Instant},
};
use syntra_api::{
    ClipboardTransferDirection, FileReceiveSettings, FrontendEvent, IncomingFileOffer,
    ManualTransferState, ManualTransferStatus,
};
use syntra_proto::{
    ClipboardEntryKind, ClipboardManifestEntry, MAX_MANUAL_FILE_CHUNK_SIZE, ManualDecision,
    ProtoEvent,
};

const FILE_ID: u64 = 1;
const MAX_RECORDS: usize = 32;
const MAX_TERMINAL: usize = 100;
const MAX_RETRIES: u8 = 3;
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);
const RETRY_TIMEOUT: Duration = Duration::from_secs(5);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const REMOTE_PREPARATION_ERROR: &str = "destination unavailable";
const REMOTE_TRANSFER_ERROR: &str = "file transfer failed";
const REMOTE_NOT_ACCEPTED_ERROR: &str = "transfer not accepted";

type RecordKey = (String, u64);

#[derive(Debug)]
pub(crate) enum ManualAction<P> {
    Send { peer: P, event: ProtoEvent },
    Notify(FrontendEvent),
}

struct Record<P> {
    peer: P,
    fingerprint: String,
    id: u64,
    name: String,
    size: u64,
    transferred: u64,
    direction: ClipboardTransferDirection,
    state: ManualTransferState,
    destination: Option<PathBuf>,
    error: Option<String>,
    offer: Option<FileOffer>,
    outgoing: Option<OutgoingFile>,
    incoming: Option<IncomingTransfer>,
    last_activity: Instant,
    last_notification: Instant,
    approval_deadline: Instant,
    retries: u8,
    cached_request: Option<(u64, u32)>,
    cached_response: Option<ProtoEvent>,
    retry_event: Option<ProtoEvent>,
    final_digest: Option<[u8; 32]>,
    early_digest: Option<[u8; 32]>,
    final_result: Option<(bool, Option<String>)>,
}

pub(crate) struct ManualTransfers<P: Clone + Eq + Hash + Debug> {
    records: HashMap<RecordKey, Record<P>>,
    terminal: VecDeque<RecordKey>,
}

/// A file a peer has offered to send, before the user decides.
///
/// Groups what arrives in one `ManualFileOffer`; the fields were previously
/// threaded through as five positional parameters, two of them `String`.
struct OfferedFile<P> {
    /// Peer that made the offer.
    peer: P,
    /// Certificate fingerprint identifying that peer.
    fingerprint: String,
    /// Transfer id, unique per peer.
    id: u64,
    /// File name as proposed by the sender; not yet validated.
    name: String,
    /// Declared size in bytes; not yet checked against the receive settings.
    size: u64,
}

impl<P: Clone + Eq + Hash + Debug> ManualTransfers<P> {
    pub(crate) fn new() -> Self {
        Self {
            records: HashMap::new(),
            terminal: VecDeque::new(),
        }
    }

    fn next_transfer_id(&self) -> Option<u64> {
        for _ in 0..16 {
            let mut bytes = [0u8; 8];
            getrandom::fill(&mut bytes).ok()?;
            let id = u64::from_ne_bytes(bytes);
            if id != 0 && !self.records.keys().any(|(_, existing)| *existing == id) {
                return Some(id);
            }
        }
        None
    }

    pub(crate) async fn send_files(
        &mut self,
        peer: P,
        fingerprint: String,
        paths: Vec<PathBuf>,
        now: Instant,
    ) -> Vec<ManualAction<P>> {
        let mut actions = Vec::new();
        for path in paths {
            if self.active_count() >= MAX_RECORDS {
                actions.push(ManualAction::Notify(FrontendEvent::ManualTransferError(
                    "too many active file transfers".into(),
                )));
                break;
            }
            let Some(id) = self.next_transfer_id() else {
                actions.push(ManualAction::Notify(FrontendEvent::ManualTransferError(
                    "could not create a file transfer identifier".into(),
                )));
                break;
            };
            let preparation =
                tokio::task::spawn_blocking(move || FileOffer::from_regular_file(id, path)).await;
            let offer = match preparation {
                Ok(Ok(offer)) => offer,
                Ok(Err(error)) => {
                    actions.push(ManualAction::Notify(FrontendEvent::ManualTransferError(
                        local_source_error(&error),
                    )));
                    continue;
                }
                Err(error) => {
                    actions.push(ManualAction::Notify(FrontendEvent::ManualTransferError(
                        format!("file source preparation failed: {error}"),
                    )));
                    continue;
                }
            };
            let entry = &offer.entries[0];
            let wire = ProtoEvent::ManualFileOffer {
                transfer_id: id,
                file_name: entry.path.clone(),
                size: entry.size,
            };
            let record = Record {
                peer: peer.clone(),
                fingerprint: fingerprint.clone(),
                id,
                name: entry.path.clone(),
                size: entry.size,
                transferred: 0,
                direction: ClipboardTransferDirection::Sending,
                state: ManualTransferState::Offering,
                destination: None,
                error: None,
                offer: Some(offer),
                outgoing: None,
                incoming: None,
                last_activity: now,
                last_notification: now,
                approval_deadline: now + APPROVAL_TIMEOUT,
                retries: 0,
                cached_request: None,
                cached_response: None,
                retry_event: Some(wire.clone()),
                final_digest: None,
                early_digest: None,
                final_result: None,
            };
            actions.push(ManualAction::Send {
                peer: peer.clone(),
                event: wire,
            });
            actions.push(Self::notify(&record));
            self.store(record);
        }
        actions
    }

    pub(crate) async fn handle_protocol(
        &mut self,
        peer: P,
        fingerprint: String,
        event: ProtoEvent,
        settings: &FileReceiveSettings,
        now: Instant,
    ) -> Vec<ManualAction<P>> {
        if let ProtoEvent::ManualFileOffer {
            transfer_id,
            file_name,
            size,
        } = event
        {
            return self
                .receive_offer(
                    OfferedFile {
                        peer,
                        fingerprint,
                        id: transfer_id,
                        name: file_name,
                        size,
                    },
                    settings,
                    now,
                )
                .await;
        }
        let id = match &event {
            ProtoEvent::ManualFileDecision { transfer_id, .. }
            | ProtoEvent::ManualFileRequest { transfer_id, .. }
            | ProtoEvent::ManualFileChunk { transfer_id, .. }
            | ProtoEvent::ManualFileComplete { transfer_id, .. }
            | ProtoEvent::ManualFileResult { transfer_id, .. }
            | ProtoEvent::ManualFileCancel { transfer_id } => *transfer_id,
            _ => return Vec::new(),
        };
        let key = (fingerprint, id);
        let Some(mut record) = self.records.remove(&key) else {
            return match event {
                ProtoEvent::ManualFileRequest { .. }
                | ProtoEvent::ManualFileChunk { .. }
                | ProtoEvent::ManualFileComplete { .. } => {
                    vec![peer_failure(peer, id, REMOTE_NOT_ACCEPTED_ERROR)]
                }
                ProtoEvent::ManualFileDecision {
                    decision: ManualDecision::Accepted,
                    ..
                } => vec![ManualAction::Send {
                    peer,
                    event: ProtoEvent::ManualFileCancel { transfer_id: id },
                }],
                _ => Vec::new(),
            };
        };
        record.peer = peer;
        let actions = match event {
            ProtoEvent::ManualFileDecision { decision, .. } => {
                Self::decision(&mut record, decision, now)
            }
            ProtoEvent::ManualFileRequest { offset, length, .. } => {
                Self::send_chunk(&mut record, offset, length, now).await
            }
            ProtoEvent::ManualFileChunk { offset, data, .. } => {
                Self::write_chunk(&mut record, offset, data, now).await
            }
            ProtoEvent::ManualFileComplete { sha256, .. } => {
                Self::complete_receiver(&mut record, sha256, now).await
            }
            ProtoEvent::ManualFileResult { success, error, .. } => {
                Self::receive_result(&mut record, success, error, now)
            }
            ProtoEvent::ManualFileCancel { .. } => {
                if is_terminal(&record.state) {
                    Vec::new()
                } else {
                    Self::terminal(&mut record, ManualTransferState::Cancelled, None, now);
                    vec![Self::notify(&record)]
                }
            }
            _ => Vec::new(),
        };
        self.store(record);
        actions
    }

    async fn receive_offer(
        &mut self,
        offer: OfferedFile<P>,
        settings: &FileReceiveSettings,
        now: Instant,
    ) -> Vec<ManualAction<P>> {
        let OfferedFile {
            peer,
            fingerprint,
            id,
            name,
            size,
        } = offer;
        if id == 0 || syntra_proto::validate_manual_file_name(&name).is_err() {
            return vec![peer_failure(peer, id, "invalid file offer")];
        }
        let key = (fingerprint.clone(), id);
        if let Some(record) = self.records.get_mut(&key) {
            if record.direction != ClipboardTransferDirection::Receiving
                || record.name != name
                || record.size != size
            {
                return vec![peer_failure(peer, id, "conflicting file offer")];
            }
            record.peer = peer;
            // Duplicate metadata must never extend human approval or transfer deadlines.
            return vec![Self::send(record, Self::offer_response(record))];
        }
        if self.active_count() >= MAX_RECORDS {
            return vec![peer_failure(peer, id, "too many active file transfers")];
        }
        let mut record = Record {
            peer,
            fingerprint,
            id,
            name,
            size,
            transferred: 0,
            direction: ClipboardTransferDirection::Receiving,
            state: ManualTransferState::AwaitingAcceptance,
            destination: Some(settings.download_directory.clone()),
            error: None,
            offer: None,
            outgoing: None,
            incoming: None,
            last_activity: now,
            last_notification: now,
            approval_deadline: now + APPROVAL_TIMEOUT,
            retries: 0,
            cached_request: None,
            cached_response: None,
            retry_event: None,
            final_digest: None,
            early_digest: None,
            final_result: None,
        };
        let mut actions = if settings.auto_accept {
            Self::prepare_receiver(&mut record, settings.download_directory.clone(), now, true)
                .await
        } else {
            vec![
                Self::send(
                    &record,
                    ProtoEvent::ManualFileDecision {
                        transfer_id: id,
                        decision: ManualDecision::Pending,
                    },
                ),
                Self::notify(&record),
                ManualAction::Notify(FrontendEvent::IncomingFileOffer(Self::offer_snapshot(
                    &record,
                ))),
            ]
        };
        if actions.is_empty() {
            actions.push(Self::notify(&record));
        }
        self.store(record);
        actions
    }

    pub(crate) async fn accept(
        &mut self,
        fingerprint: &str,
        transfer_id: u64,
        directory: PathBuf,
        now: Instant,
    ) -> Vec<ManualAction<P>> {
        let key = (fingerprint.to_owned(), transfer_id);
        let Some(mut record) = self.records.remove(&key) else {
            return Vec::new();
        };
        let actions = Self::prepare_receiver(&mut record, directory, now, false).await;
        self.store(record);
        actions
    }

    async fn prepare_receiver(
        record: &mut Record<P>,
        directory: PathBuf,
        now: Instant,
        automatic: bool,
    ) -> Vec<ManualAction<P>> {
        if record.direction != ClipboardTransferDirection::Receiving
            || record.state != ManualTransferState::AwaitingAcceptance
        {
            return Vec::new();
        }
        if now >= record.approval_deadline {
            return Self::fail(record, "file offer expired", true, now);
        }
        let preparation = async {
            if !directory.is_absolute() {
                return Err(TransferError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "destination must be an absolute directory",
                )));
            }
            tokio::fs::create_dir_all(&directory).await?;
            let entry = ClipboardManifestEntry {
                file_id: FILE_ID,
                path: record.name.clone(),
                kind: ClipboardEntryKind::File,
                size: record.size,
            };
            let mut incoming = IncomingTransfer::new(
                record.id,
                vec![entry],
                directory,
                // The user explicitly accepted this transfer, so no cap applies.
                None,
            )
            .await?;
            incoming.prepare().await?;
            Ok::<_, TransferError>(incoming)
        }
        .await;
        match preparation {
            Ok(incoming) => {
                record.destination = Some(incoming.destination_path().join(&record.name));
                record.incoming = Some(incoming);
                record.state = ManualTransferState::Transferring;
                record.error = None;
                record.last_activity = now;
                record.last_notification = now;
                record.retries = 0;
                // The sender echoes Accepted before the first pull request. This makes
                // acceptance causal even when UDP reorders adjacent datagrams.
                let accepted = ProtoEvent::ManualFileDecision {
                    transfer_id: record.id,
                    decision: ManualDecision::Accepted,
                };
                record.retry_event = Some(accepted.clone());
                vec![Self::send(record, accepted), Self::notify(record)]
            }
            Err(error) => {
                let message = local_receive_error(&error);
                if automatic {
                    Self::terminal(record, ManualTransferState::Failed, Some(message), now);
                    record.final_result = Some((false, Some(REMOTE_PREPARATION_ERROR.into())));
                    record.retry_event = Some(Self::offer_response(record));
                    vec![
                        peer_failure(record.peer.clone(), record.id, REMOTE_PREPARATION_ERROR),
                        Self::notify(record),
                    ]
                } else {
                    record.error = Some(message);
                    vec![Self::notify(record)]
                }
            }
        }
    }

    fn decision(
        record: &mut Record<P>,
        decision: ManualDecision,
        now: Instant,
    ) -> Vec<ManualAction<P>> {
        if is_terminal(&record.state) {
            return Vec::new();
        }
        match (record.direction, decision) {
            (ClipboardTransferDirection::Sending, ManualDecision::Pending) => {
                if record.state != ManualTransferState::Offering {
                    return Vec::new();
                }
                record.state = ManualTransferState::AwaitingAcceptance;
                record.retry_event = None;
                record.retries = 0;
                vec![Self::notify(record)]
            }
            (ClipboardTransferDirection::Sending, ManualDecision::Accepted) => {
                if now >= record.approval_deadline
                    && record.state != ManualTransferState::Transferring
                {
                    return Self::fail(record, "file offer expired", true, now);
                }
                let changed = record.state != ManualTransferState::Transferring;
                if changed {
                    record.state = ManualTransferState::Transferring;
                    record.retry_event = None;
                    record.last_activity = now;
                    record.retries = 0;
                    record.error = None;
                }
                let mut actions = vec![Self::send(
                    record,
                    ProtoEvent::ManualFileDecision {
                        transfer_id: record.id,
                        decision: ManualDecision::Accepted,
                    },
                )];
                if changed {
                    actions.push(Self::notify(record));
                }
                actions
            }
            (ClipboardTransferDirection::Receiving, ManualDecision::Accepted)
                if record.state == ManualTransferState::Transferring =>
            {
                let first_ack = matches!(
                    record.retry_event,
                    Some(ProtoEvent::ManualFileDecision { .. })
                );
                let pull = request(record.id, record.transferred);
                record.retry_event = Some(pull.clone());
                if first_ack {
                    record.last_activity = now;
                    record.retries = 0;
                }
                vec![Self::send(record, pull)]
            }
            (ClipboardTransferDirection::Sending, ManualDecision::Declined) => {
                Self::terminal(record, ManualTransferState::Declined, None, now);
                vec![Self::notify(record)]
            }
            _ => Vec::new(),
        }
    }

    async fn send_chunk(
        record: &mut Record<P>,
        offset: u64,
        length: u32,
        now: Instant,
    ) -> Vec<ManualAction<P>> {
        if is_terminal(&record.state) {
            return Vec::new();
        }
        if record.direction != ClipboardTransferDirection::Sending
            || record.state != ManualTransferState::Transferring
        {
            return vec![peer_failure(
                record.peer.clone(),
                record.id,
                REMOTE_NOT_ACCEPTED_ERROR,
            )];
        }
        if length == 0 || length as usize > MAX_MANUAL_FILE_CHUNK_SIZE || offset > record.size {
            return Self::fail(record, "invalid file request", true, now);
        }
        if record.cached_request == Some((offset, length)) {
            if let Some(response) = record.cached_response.clone() {
                record.last_activity = now;
                record.retries = 0;
                return vec![Self::send(record, response)];
            }
        }
        if offset < record.transferred {
            // An old request cannot rewind the open source or trigger prefix rehashing.
            return Vec::new();
        }
        if offset != record.transferred {
            return Self::fail(record, "unexpected file offset", true, now);
        }
        if record.outgoing.is_none() {
            let Some(offer) = record.offer.as_ref() else {
                return Self::fail(record, "file source unavailable", true, now);
            };
            match offer.open(FILE_ID, 0).await {
                Ok(outgoing) => record.outgoing = Some(outgoing),
                Err(error) => return Self::fail(record, local_source_error(&error), true, now),
            }
        }
        let read = if offset == record.size {
            let source = record.outgoing.as_mut().expect("source opened");
            source.finalize_digest().await.map(|sha256| {
                record.final_digest = Some(sha256);
                ProtoEvent::ManualFileComplete {
                    transfer_id: record.id,
                    sha256,
                }
            })
        } else {
            let source = record.outgoing.as_mut().expect("source opened");
            match source.next_chunk_bounded(length as usize).await {
                Ok(Some((chunk_offset, data))) if chunk_offset == offset && !data.is_empty() => {
                    record.transferred = offset + data.len() as u64;
                    Ok(ProtoEvent::ManualFileChunk {
                        transfer_id: record.id,
                        offset,
                        data,
                    })
                }
                Ok(_) => Err(TransferError::SourceChanged { file_id: FILE_ID }),
                Err(error) => Err(error),
            }
        };
        let response = match read {
            Ok(response) => response,
            Err(error) => return Self::fail(record, local_source_error(&error), true, now),
        };
        record.cached_request = Some((offset, length));
        record.cached_response = Some(response.clone());
        record.retry_event =
            matches!(response, ProtoEvent::ManualFileComplete { .. }).then(|| response.clone());
        record.last_activity = now;
        record.retries = 0;
        let mut actions = vec![Self::send(record, response)];
        Self::progress(record, now, &mut actions);
        actions
    }

    async fn write_chunk(
        record: &mut Record<P>,
        offset: u64,
        data: Vec<u8>,
        now: Instant,
    ) -> Vec<ManualAction<P>> {
        if record.direction != ClipboardTransferDirection::Receiving {
            return Vec::new();
        }
        if is_terminal(&record.state) {
            return record
                .final_result
                .as_ref()
                .map(|_| vec![Self::send(record, Self::offer_response(record))])
                .unwrap_or_default();
        }
        if record.state != ManualTransferState::Transferring || record.incoming.is_none() {
            return vec![peer_failure(
                record.peer.clone(),
                record.id,
                REMOTE_NOT_ACCEPTED_ERROR,
            )];
        }
        let Some(end) = offset.checked_add(data.len() as u64) else {
            return Self::fail(record, "invalid file chunk", true, now);
        };
        if data.is_empty() || data.len() > MAX_MANUAL_FILE_CHUNK_SIZE || end > record.size {
            return Self::fail(record, "invalid file chunk", true, now);
        }
        if offset != record.transferred {
            if offset < record.transferred && end > record.transferred {
                return Self::fail(record, "overlapping file chunk", true, now);
            }
            return vec![Self::send(record, request(record.id, record.transferred))];
        }
        let write = record
            .incoming
            .as_mut()
            .expect("receiver prepared")
            .write_chunk(FILE_ID, offset, &data)
            .await;
        if let Err(error) = write {
            return Self::fail(record, local_receive_error(&error), true, now);
        }
        record.transferred = end;
        record.last_activity = now;
        record.retries = 0;
        if end == record.size {
            if let Some(digest) = record.early_digest.take() {
                return Self::finish_receiver(record, digest, now).await;
            }
        }
        // Pull EOF only after the final data has been committed, never race it.
        let pull = request(record.id, record.transferred);
        record.retry_event = Some(pull.clone());
        let mut actions = vec![Self::send(record, pull)];
        Self::progress(record, now, &mut actions);
        actions
    }

    async fn complete_receiver(
        record: &mut Record<P>,
        digest: [u8; 32],
        now: Instant,
    ) -> Vec<ManualAction<P>> {
        if record.direction != ClipboardTransferDirection::Receiving {
            return Vec::new();
        }
        if is_terminal(&record.state) {
            return vec![Self::send(record, Self::offer_response(record))];
        }
        if record.state != ManualTransferState::Transferring || record.incoming.is_none() {
            return vec![peer_failure(
                record.peer.clone(),
                record.id,
                REMOTE_NOT_ACCEPTED_ERROR,
            )];
        }
        if record.transferred != record.size {
            if record
                .early_digest
                .is_some_and(|previous| previous != digest)
            {
                return Self::fail(record, "conflicting file digest", true, now);
            }
            record.early_digest = Some(digest);
            return vec![Self::send(record, request(record.id, record.transferred))];
        }
        Self::finish_receiver(record, digest, now).await
    }

    async fn finish_receiver(
        record: &mut Record<P>,
        digest: [u8; 32],
        now: Instant,
    ) -> Vec<ManualAction<P>> {
        let incoming = record.incoming.as_mut().expect("receiver prepared");
        let result = incoming
            .finalize_file(FILE_ID, record.size, digest)
            .await
            .and_then(|()| incoming.finish());
        if let Err(error) = result {
            return Self::fail(record, local_receive_error(&error), true, now);
        }
        Self::terminal(record, ManualTransferState::Completed, None, now);
        record.final_result = Some((true, None));
        record.retry_event = Some(Self::offer_response(record));
        vec![
            Self::send(
                record,
                ProtoEvent::ManualFileResult {
                    transfer_id: record.id,
                    success: true,
                    error: None,
                },
            ),
            Self::notify(record),
        ]
    }

    fn receive_result(
        record: &mut Record<P>,
        success: bool,
        error: Option<String>,
        now: Instant,
    ) -> Vec<ManualAction<P>> {
        if is_terminal(&record.state) {
            return Vec::new();
        }
        if !success {
            Self::terminal(
                record,
                ManualTransferState::Failed,
                Some(sanitize_remote_error(error)),
                now,
            );
            record.retry_event = Some(Self::offer_response(record));
            return vec![Self::notify(record)];
        }
        if record.direction == ClipboardTransferDirection::Sending
            && record.state == ManualTransferState::Transferring
            && record.transferred == record.size
            && record.final_digest.is_some()
        {
            Self::terminal(record, ManualTransferState::Completed, None, now);
            record.final_result = Some((true, None));
            record.retry_event = Some(Self::offer_response(record));
            return vec![Self::notify(record)];
        }
        Vec::new()
    }

    pub(crate) fn decline(
        &mut self,
        fingerprint: &str,
        id: u64,
        now: Instant,
    ) -> Vec<ManualAction<P>> {
        let key = (fingerprint.to_owned(), id);
        let Some(mut record) = self.records.remove(&key) else {
            return Vec::new();
        };
        let actions = if record.direction == ClipboardTransferDirection::Receiving
            && record.state == ManualTransferState::AwaitingAcceptance
        {
            Self::terminal(&mut record, ManualTransferState::Declined, None, now);
            vec![
                Self::send(
                    &record,
                    ProtoEvent::ManualFileDecision {
                        transfer_id: id,
                        decision: ManualDecision::Declined,
                    },
                ),
                Self::notify(&record),
            ]
        } else {
            Vec::new()
        };
        self.store(record);
        actions
    }

    pub(crate) fn cancel(
        &mut self,
        fingerprint: &str,
        id: u64,
        now: Instant,
    ) -> Vec<ManualAction<P>> {
        let key = (fingerprint.to_owned(), id);
        let Some(mut record) = self.records.remove(&key) else {
            return Vec::new();
        };
        let actions = if !is_terminal(&record.state) {
            Self::terminal(&mut record, ManualTransferState::Cancelled, None, now);
            vec![
                Self::send(&record, ProtoEvent::ManualFileCancel { transfer_id: id }),
                Self::notify(&record),
            ]
        } else {
            Vec::new()
        };
        self.store(record);
        actions
    }

    pub(crate) fn tick(&mut self, now: Instant) -> Vec<ManualAction<P>> {
        let keys = self.records.keys().cloned().collect::<Vec<_>>();
        let mut actions = Vec::new();
        for key in keys {
            let Some(mut record) = self.records.remove(&key) else {
                continue;
            };
            if is_terminal(&record.state) {
                if record.retries < MAX_RETRIES
                    && now.saturating_duration_since(record.last_activity) >= RETRY_TIMEOUT
                {
                    record.retries += 1;
                    record.last_activity = now;
                    if let Some(event) = record.retry_event.clone() {
                        actions.push(Self::send(&record, event));
                    }
                }
            } else {
                let approval_expired = matches!(
                    record.state,
                    ManualTransferState::Offering | ManualTransferState::AwaitingAcceptance
                ) && now >= record.approval_deadline;
                if approval_expired {
                    actions.push(Self::send(
                        &record,
                        ProtoEvent::ManualFileCancel {
                            transfer_id: record.id,
                        },
                    ));
                    Self::terminal(
                        &mut record,
                        ManualTransferState::Failed,
                        Some("file offer expired".into()),
                        now,
                    );
                    actions.push(Self::notify(&record));
                } else if record.state != ManualTransferState::AwaitingAcceptance
                    && now.saturating_duration_since(record.last_activity) >= RETRY_TIMEOUT
                {
                    if record.retries >= MAX_RETRIES {
                        actions.push(Self::send(
                            &record,
                            ProtoEvent::ManualFileCancel {
                                transfer_id: record.id,
                            },
                        ));
                        Self::terminal(
                            &mut record,
                            ManualTransferState::Failed,
                            Some("file transfer timed out".into()),
                            now,
                        );
                        actions.push(Self::notify(&record));
                    } else {
                        record.retries += 1;
                        record.last_activity = now;
                        if let Some(event) = record.retry_event.clone() {
                            actions.push(Self::send(&record, event));
                        }
                    }
                }
            }
            self.store(record);
        }
        actions
    }

    pub(crate) fn refresh_routes(
        &mut self,
        routes: &HashMap<String, P>,
        now: Instant,
    ) -> Vec<ManualAction<P>> {
        let keys = self.records.keys().cloned().collect::<Vec<_>>();
        let mut actions = Vec::new();
        for key in keys {
            let Some(mut record) = self.records.remove(&key) else {
                continue;
            };
            if let Some(peer) = routes.get(&record.fingerprint) {
                record.peer = peer.clone();
            } else if !is_terminal(&record.state) {
                Self::terminal(
                    &mut record,
                    ManualTransferState::Failed,
                    Some("peer disconnected".into()),
                    now,
                );
                actions.push(Self::notify(&record));
            }
            self.store(record);
        }
        actions
    }

    pub(crate) fn snapshot(&self) -> Vec<ManualTransferStatus> {
        let mut records = self.records.values().collect::<Vec<_>>();
        records.sort_by_key(|record| record.approval_deadline);
        records.into_iter().map(Self::status).collect()
    }

    pub(crate) fn pending_offers(&self) -> Vec<IncomingFileOffer> {
        let mut records = self
            .records
            .values()
            .filter(|record| {
                record.direction == ClipboardTransferDirection::Receiving
                    && record.state == ManualTransferState::AwaitingAcceptance
            })
            .collect::<Vec<_>>();
        records.sort_by_key(|record| record.approval_deadline);
        records.into_iter().map(Self::offer_snapshot).collect()
    }

    fn offer_snapshot(record: &Record<P>) -> IncomingFileOffer {
        IncomingFileOffer {
            peer_fingerprint: record.fingerprint.clone(),
            transfer_id: record.id,
            file_name: record.name.clone(),
            size: record.size,
            suggested_directory: record.destination.clone().unwrap_or_default(),
        }
    }

    fn offer_response(record: &Record<P>) -> ProtoEvent {
        match record.state {
            ManualTransferState::AwaitingAcceptance => ProtoEvent::ManualFileDecision {
                transfer_id: record.id,
                decision: ManualDecision::Pending,
            },
            ManualTransferState::Transferring => ProtoEvent::ManualFileDecision {
                transfer_id: record.id,
                decision: ManualDecision::Accepted,
            },
            ManualTransferState::Declined => ProtoEvent::ManualFileDecision {
                transfer_id: record.id,
                decision: ManualDecision::Declined,
            },
            ManualTransferState::Cancelled => ProtoEvent::ManualFileCancel {
                transfer_id: record.id,
            },
            _ => {
                let (success, error) = record
                    .final_result
                    .clone()
                    .unwrap_or((false, Some(REMOTE_TRANSFER_ERROR.into())));
                ProtoEvent::ManualFileResult {
                    transfer_id: record.id,
                    success,
                    error,
                }
            }
        }
    }

    fn active_count(&self) -> usize {
        self.records
            .values()
            .filter(|record| !is_terminal(&record.state))
            .count()
    }

    fn store(&mut self, record: Record<P>) {
        let key = (record.fingerprint.clone(), record.id);
        if is_terminal(&record.state) && !self.terminal.contains(&key) {
            self.terminal.push_back(key.clone());
        }
        self.records.insert(key, record);
        while self.terminal.len() > MAX_TERMINAL {
            if let Some(oldest) = self.terminal.pop_front() {
                self.records.remove(&oldest);
            }
        }
    }

    fn status(record: &Record<P>) -> ManualTransferStatus {
        ManualTransferStatus {
            peer_fingerprint: record.fingerprint.clone(),
            transfer_id: record.id,
            file_name: record.name.clone(),
            size: record.size,
            transferred: record.transferred,
            direction: record.direction,
            state: record.state.clone(),
            destination: record.destination.clone(),
            error: record.error.clone(),
        }
    }
    fn notify(record: &Record<P>) -> ManualAction<P> {
        ManualAction::Notify(FrontendEvent::ManualTransferStatus(Self::status(record)))
    }
    fn send(record: &Record<P>, event: ProtoEvent) -> ManualAction<P> {
        ManualAction::Send {
            peer: record.peer.clone(),
            event,
        }
    }
    fn progress(record: &mut Record<P>, now: Instant, actions: &mut Vec<ManualAction<P>>) {
        if record.transferred == record.size
            || now.saturating_duration_since(record.last_notification) >= PROGRESS_INTERVAL
        {
            record.last_notification = now;
            actions.push(Self::notify(record));
        }
    }
    /// Moves a record to a terminal state at `now`.
    ///
    /// The time is injected like everywhere else in this state machine.
    /// Reading the real clock here made the retry deadline measured against a
    /// different clock from the one the caller ticks with, so under load a
    /// retry that was due could be skipped.
    fn terminal(
        record: &mut Record<P>,
        state: ManualTransferState,
        error: Option<String>,
        now: Instant,
    ) {
        record.state = state;
        record.error = error;
        record.retries = 0;
        record.last_activity = now;
        record.cached_response = None;
        record.outgoing = None;
        record.offer = None;
        record.incoming = None;
        record.retry_event = match record.state {
            ManualTransferState::Declined => Some(ProtoEvent::ManualFileDecision {
                transfer_id: record.id,
                decision: ManualDecision::Declined,
            }),
            ManualTransferState::Cancelled => Some(ProtoEvent::ManualFileCancel {
                transfer_id: record.id,
            }),
            _ => None,
        };
    }
    fn fail(
        record: &mut Record<P>,
        error: impl Into<String>,
        report_peer: bool,
        now: Instant,
    ) -> Vec<ManualAction<P>> {
        Self::terminal(record, ManualTransferState::Failed, Some(error.into()), now);
        record.retry_event = Some(Self::offer_response(record));
        let mut actions = vec![Self::notify(record)];
        if report_peer {
            actions.push(peer_failure(
                record.peer.clone(),
                record.id,
                REMOTE_TRANSFER_ERROR,
            ));
        }
        actions
    }
}

fn request(transfer_id: u64, offset: u64) -> ProtoEvent {
    ProtoEvent::ManualFileRequest {
        transfer_id,
        offset,
        length: MAX_MANUAL_FILE_CHUNK_SIZE as u32,
    }
}
fn peer_failure<P>(peer: P, transfer_id: u64, error: &str) -> ManualAction<P> {
    ManualAction::Send {
        peer,
        event: ProtoEvent::ManualFileResult {
            transfer_id,
            success: false,
            error: Some(error.into()),
        },
    }
}
fn is_terminal(state: &ManualTransferState) -> bool {
    matches!(
        state,
        ManualTransferState::Completed
            | ManualTransferState::Declined
            | ManualTransferState::Cancelled
            | ManualTransferState::Failed
    )
}
fn local_source_error(error: &TransferError) -> String {
    match error {
        TransferError::UnsupportedEntry(_) => "only regular files can be sent".into(),
        TransferError::UnsafePath(_) => "the file name is not portable".into(),
        _ => error.to_string(),
    }
}
fn local_receive_error(error: &TransferError) -> String {
    match error {
        TransferError::Io(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            "a file with this name already exists".into()
        }
        _ => error.to_string(),
    }
}
fn sanitize_remote_error(error: Option<String>) -> String {
    error
        .filter(|error| !error.is_empty())
        .unwrap_or_else(|| "remote peer rejected the file".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{fs, path::Path};

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "syntra-manual-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    fn settings(directory: &Path) -> FileReceiveSettings {
        FileReceiveSettings {
            auto_accept: false,
            download_directory: directory.to_path_buf(),
        }
    }

    fn sent_event(actions: &[ManualAction<u8>], predicate: impl Fn(&ProtoEvent) -> bool) -> bool {
        actions
            .iter()
            .any(|action| matches!(action, ManualAction::Send { event, .. } if predicate(event)))
    }

    #[tokio::test]
    async fn offer_does_not_create_file_before_acceptance_and_decline_writes_nothing() {
        let directory = temp_dir("consent");
        let mut receiver = ManualTransfers::<u8>::new();
        let now = Instant::now();
        let actions = receiver
            .handle_protocol(
                1,
                "sender".into(),
                ProtoEvent::ManualFileOffer {
                    transfer_id: 7,
                    file_name: "secret.bin".into(),
                    size: 4,
                },
                &settings(&directory),
                now,
            )
            .await;
        assert!(sent_event(&actions, |event| matches!(
            event,
            ProtoEvent::ManualFileDecision {
                decision: ManualDecision::Pending,
                ..
            }
        )));
        assert!(!directory.join("secret.bin").exists());

        receiver.decline("sender", 7, now);
        assert!(!directory.join("secret.bin").exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn unaccepted_request_never_reads_source() {
        let directory = temp_dir("unaccepted");
        let source = directory.join("source.bin");
        fs::write(&source, b"private").unwrap();
        let mut sender = ManualTransfers::<u8>::new();
        let now = Instant::now();
        sender
            .send_files(1, "receiver".into(), vec![source.clone()], now)
            .await;
        let transfer_id = sender.snapshot()[0].transfer_id;
        fs::remove_file(&source).unwrap();

        let actions = sender
            .handle_protocol(
                1,
                "receiver".into(),
                ProtoEvent::ManualFileRequest {
                    transfer_id,
                    offset: 0,
                    length: 16,
                },
                &settings(&directory),
                now,
            )
            .await;
        assert!(sent_event(&actions, |event| matches!(
            event,
            ProtoEvent::ManualFileResult { success: false, .. }
        )));
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn accepted_nonempty_file_writes_exact_bytes_and_duplicate_chunk_is_idempotent() {
        let directory = temp_dir("bytes");
        let mut receiver = ManualTransfers::<u8>::new();
        let now = Instant::now();
        receiver
            .handle_protocol(
                1,
                "sender".into(),
                ProtoEvent::ManualFileOffer {
                    transfer_id: 9,
                    file_name: "data.bin".into(),
                    size: 5,
                },
                &settings(&directory),
                now,
            )
            .await;
        receiver.accept("sender", 9, directory.clone(), now).await;
        let chunk = ProtoEvent::ManualFileChunk {
            transfer_id: 9,
            offset: 0,
            data: b"hello".to_vec(),
        };
        receiver
            .handle_protocol(
                1,
                "sender".into(),
                chunk.clone(),
                &settings(&directory),
                now,
            )
            .await;
        receiver
            .handle_protocol(1, "sender".into(), chunk, &settings(&directory), now)
            .await;
        let digest: [u8; 32] = Sha256::digest(b"hello").into();
        receiver
            .handle_protocol(
                1,
                "sender".into(),
                ProtoEvent::ManualFileComplete {
                    transfer_id: 9,
                    sha256: digest,
                },
                &settings(&directory),
                now,
            )
            .await;
        assert_eq!(fs::read(directory.join("data.bin")).unwrap(), b"hello");
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn accepted_empty_file_completes_and_duplicate_completion_reuses_result() {
        let directory = temp_dir("empty");
        let mut receiver = ManualTransfers::<u8>::new();
        let now = Instant::now();
        receiver
            .handle_protocol(
                1,
                "sender".into(),
                ProtoEvent::ManualFileOffer {
                    transfer_id: 10,
                    file_name: "empty.bin".into(),
                    size: 0,
                },
                &settings(&directory),
                now,
            )
            .await;
        receiver.accept("sender", 10, directory.clone(), now).await;
        let complete = ProtoEvent::ManualFileComplete {
            transfer_id: 10,
            sha256: Sha256::digest([]).into(),
        };
        let first = receiver
            .handle_protocol(
                1,
                "sender".into(),
                complete.clone(),
                &settings(&directory),
                now,
            )
            .await;
        let duplicate = receiver
            .handle_protocol(1, "sender".into(), complete, &settings(&directory), now)
            .await;
        assert!(sent_event(&first, |event| matches!(
            event,
            ProtoEvent::ManualFileResult { success: true, .. }
        )));
        assert!(sent_event(&duplicate, |event| matches!(
            event,
            ProtoEvent::ManualFileResult { success: true, .. }
        )));
        assert_eq!(fs::read(directory.join("empty.bin")).unwrap(), b"");
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn cancellation_removes_partial_file_only() {
        let directory = temp_dir("cancel");
        let existing = directory.join("keep.bin");
        fs::write(&existing, b"keep").unwrap();
        let mut receiver = ManualTransfers::<u8>::new();
        let now = Instant::now();
        receiver
            .handle_protocol(
                1,
                "sender".into(),
                ProtoEvent::ManualFileOffer {
                    transfer_id: 11,
                    file_name: "partial.bin".into(),
                    size: 8,
                },
                &settings(&directory),
                now,
            )
            .await;
        receiver.accept("sender", 11, directory.clone(), now).await;
        receiver
            .handle_protocol(
                1,
                "sender".into(),
                ProtoEvent::ManualFileChunk {
                    transfer_id: 11,
                    offset: 0,
                    data: b"part".to_vec(),
                },
                &settings(&directory),
                now,
            )
            .await;
        receiver.cancel("sender", 11, now);
        assert!(!directory.join("partial.bin").exists());
        assert_eq!(fs::read(existing).unwrap(), b"keep");
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn collision_keeps_offer_pending_and_existing_file_unchanged() {
        let directory = temp_dir("collision");
        let target = directory.join("same.bin");
        fs::write(&target, b"existing").unwrap();
        let mut receiver = ManualTransfers::<u8>::new();
        let now = Instant::now();
        receiver
            .handle_protocol(
                1,
                "sender".into(),
                ProtoEvent::ManualFileOffer {
                    transfer_id: 12,
                    file_name: "same.bin".into(),
                    size: 3,
                },
                &settings(&directory),
                now,
            )
            .await;
        let actions = receiver.accept("sender", 12, directory.clone(), now).await;
        assert!(!sent_event(&actions, |event| matches!(
            event,
            ProtoEvent::ManualFileDecision {
                decision: ManualDecision::Accepted,
                ..
            }
        )));
        assert_eq!(receiver.pending_offers().len(), 1);
        assert!(receiver.snapshot()[0].error.is_some());
        assert_eq!(fs::read(target).unwrap(), b"existing");
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn duplicate_request_resends_identical_bytes() {
        let directory = temp_dir("request-retry");
        let source = directory.join("source.bin");
        fs::write(&source, b"abcdef").unwrap();
        let mut sender = ManualTransfers::<u8>::new();
        let now = Instant::now();
        sender
            .send_files(1, "receiver".into(), vec![source], now)
            .await;
        let transfer_id = sender.snapshot()[0].transfer_id;
        sender
            .handle_protocol(
                1,
                "receiver".into(),
                ProtoEvent::ManualFileDecision {
                    transfer_id,
                    decision: ManualDecision::Accepted,
                },
                &settings(&directory),
                now,
            )
            .await;
        let request = ProtoEvent::ManualFileRequest {
            transfer_id,
            offset: 0,
            length: 3,
        };
        let first = sender
            .handle_protocol(
                1,
                "receiver".into(),
                request.clone(),
                &settings(&directory),
                now,
            )
            .await;
        let second = sender
            .handle_protocol(1, "receiver".into(), request, &settings(&directory), now)
            .await;
        let bytes = |actions: &[ManualAction<u8>]| {
            actions.iter().find_map(|action| match action {
                ManualAction::Send {
                    event: ProtoEvent::ManualFileChunk { offset, data, .. },
                    ..
                } => Some((*offset, data.clone())),
                _ => None,
            })
        };
        assert_eq!(bytes(&first), Some((0, b"abc".to_vec())));
        assert_eq!(bytes(&first), bytes(&second));
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn early_peer_failure_terminates_sender_before_final_digest() {
        let directory = temp_dir("early-sender-failure");
        let source = directory.join("source.bin");
        fs::write(&source, b"private").unwrap();
        let mut sender = ManualTransfers::<u8>::new();
        let now = Instant::now();
        sender
            .send_files(1, "receiver".into(), vec![source], now)
            .await;
        let transfer_id = sender.snapshot()[0].transfer_id;

        sender
            .handle_protocol(
                1,
                "receiver".into(),
                ProtoEvent::ManualFileResult {
                    transfer_id,
                    success: false,
                    error: Some("receiver rejected transfer".into()),
                },
                &settings(&directory),
                now,
            )
            .await;

        let status = &sender.snapshot()[0];
        assert_eq!(status.state, ManualTransferState::Failed);
        assert_eq!(status.error.as_deref(), Some("receiver rejected transfer"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn early_peer_failure_terminates_receiver_before_content() {
        let directory = temp_dir("early-receiver-failure");
        let mut receiver = ManualTransfers::<u8>::new();
        let now = Instant::now();
        receiver
            .handle_protocol(
                1,
                "sender".into(),
                ProtoEvent::ManualFileOffer {
                    transfer_id: 20,
                    file_name: "source.bin".into(),
                    size: 7,
                },
                &settings(&directory),
                now,
            )
            .await;

        receiver
            .handle_protocol(
                1,
                "sender".into(),
                ProtoEvent::ManualFileResult {
                    transfer_id: 20,
                    success: false,
                    error: Some("source unavailable".into()),
                },
                &settings(&directory),
                now,
            )
            .await;

        let status = &receiver.snapshot()[0];
        assert_eq!(status.state, ManualTransferState::Failed);
        assert_eq!(status.error.as_deref(), Some("source unavailable"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn approval_deadline_is_not_extended_by_pending_or_duplicate_offers() {
        let directory = temp_dir("approval-deadline");
        let start = Instant::now();
        let almost_expired = start + APPROVAL_TIMEOUT - Duration::from_secs(1);
        let expired = start + APPROVAL_TIMEOUT + Duration::from_secs(1);

        let source = directory.join("source.bin");
        fs::write(&source, b"data").unwrap();
        let mut sender = ManualTransfers::<u8>::new();
        sender
            .send_files(1, "receiver".into(), vec![source], start)
            .await;
        let transfer_id = sender.snapshot()[0].transfer_id;
        sender
            .handle_protocol(
                1,
                "receiver".into(),
                ProtoEvent::ManualFileDecision {
                    transfer_id,
                    decision: ManualDecision::Pending,
                },
                &settings(&directory),
                almost_expired,
            )
            .await;
        assert_eq!(
            sender.snapshot()[0].state,
            ManualTransferState::AwaitingAcceptance
        );
        sender.tick(expired);
        assert_eq!(sender.snapshot()[0].state, ManualTransferState::Failed);

        let mut receiver = ManualTransfers::<u8>::new();
        let offer = ProtoEvent::ManualFileOffer {
            transfer_id: 21,
            file_name: "incoming.bin".into(),
            size: 4,
        };
        receiver
            .handle_protocol(
                1,
                "sender".into(),
                offer.clone(),
                &settings(&directory),
                start,
            )
            .await;
        receiver
            .handle_protocol(
                1,
                "sender".into(),
                offer,
                &settings(&directory),
                almost_expired,
            )
            .await;
        receiver.tick(expired);
        assert_eq!(receiver.snapshot()[0].state, ManualTransferState::Failed);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn dropped_decline_and_cancel_are_retried_to_terminal_sender() {
        let directory = temp_dir("terminal-retry");
        let settings = settings(&directory);
        let now = Instant::now();

        for cancel in [false, true] {
            let source = directory.join(if cancel {
                "cancelled.bin"
            } else {
                "declined.bin"
            });
            fs::write(&source, b"private").unwrap();
            let mut sender = ManualTransfers::<u8>::new();
            let mut receiver = ManualTransfers::<u8>::new();
            let offer = sender
                .send_files(1, "receiver".into(), vec![source], now)
                .await
                .into_iter()
                .find_map(|action| match action {
                    ManualAction::Send { event, .. } => Some(event),
                    _ => None,
                })
                .unwrap();
            let transfer_id = sender.snapshot()[0].transfer_id;
            let pending = receiver
                .handle_protocol(0, "sender".into(), offer, &settings, now)
                .await
                .into_iter()
                .find_map(|action| match action {
                    ManualAction::Send { event, .. } => Some(event),
                    _ => None,
                })
                .unwrap();
            sender
                .handle_protocol(1, "receiver".into(), pending, &settings, now)
                .await;

            let dropped = if cancel {
                receiver.cancel("sender", transfer_id, now)
            } else {
                receiver.decline("sender", transfer_id, now)
            };
            assert!(sent_event(&dropped, |event| if cancel {
                matches!(event, ProtoEvent::ManualFileCancel { .. })
            } else {
                matches!(
                    event,
                    ProtoEvent::ManualFileDecision {
                        decision: ManualDecision::Declined,
                        ..
                    }
                )
            }));
            assert_eq!(
                sender.snapshot()[0].state,
                ManualTransferState::AwaitingAcceptance
            );

            let retry = receiver.tick(now + RETRY_TIMEOUT + Duration::from_millis(1));
            let terminal = retry
                .into_iter()
                .find_map(|action| match action {
                    ManualAction::Send { event, .. } => Some(event),
                    _ => None,
                })
                .expect("terminal response retried");
            sender
                .handle_protocol(1, "receiver".into(), terminal, &settings, now)
                .await;
            assert_eq!(
                sender.snapshot()[0].state,
                if cancel {
                    ManualTransferState::Cancelled
                } else {
                    ManualTransferState::Declined
                }
            );
        }

        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn autoaccept_preparation_failures_are_retained_bounded_and_replayed() {
        let directory = temp_dir("autoaccept-failure");
        let auto_settings = FileReceiveSettings {
            auto_accept: true,
            download_directory: directory.clone(),
        };
        let mut receiver = ManualTransfers::<u8>::new();
        let now = Instant::now();

        for transfer_id in 1..=(MAX_TERMINAL as u64 + 1) {
            let file_name = format!("existing-{transfer_id}.bin");
            fs::write(directory.join(&file_name), b"keep").unwrap();
            receiver
                .handle_protocol(
                    1,
                    "sender".into(),
                    ProtoEvent::ManualFileOffer {
                        transfer_id,
                        file_name,
                        size: 4,
                    },
                    &auto_settings,
                    now,
                )
                .await;
        }

        assert_eq!(receiver.snapshot().len(), MAX_TERMINAL);
        assert!(
            receiver
                .snapshot()
                .iter()
                .all(|status| status.state == ManualTransferState::Failed)
        );
        let duplicate = receiver
            .handle_protocol(
                1,
                "sender".into(),
                ProtoEvent::ManualFileOffer {
                    transfer_id: MAX_TERMINAL as u64 + 1,
                    file_name: format!("existing-{}.bin", MAX_TERMINAL + 1),
                    size: 4,
                },
                &auto_settings,
                now,
            )
            .await;
        assert!(sent_event(&duplicate, |event| matches!(
            event,
            ProtoEvent::ManualFileResult { success: false, .. }
        )));
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn delayed_fully_received_chunk_is_ignored_without_failing_transfer() {
        let directory = temp_dir("delayed-old-chunk");
        let mut receiver = ManualTransfers::<u8>::new();
        let now = Instant::now();
        receiver
            .handle_protocol(
                1,
                "sender".into(),
                ProtoEvent::ManualFileOffer {
                    transfer_id: 22,
                    file_name: "data.bin".into(),
                    size: 6,
                },
                &settings(&directory),
                now,
            )
            .await;
        receiver.accept("sender", 22, directory.clone(), now).await;
        for (offset, data) in [(0, b"abc".to_vec()), (3, b"def".to_vec())] {
            receiver
                .handle_protocol(
                    1,
                    "sender".into(),
                    ProtoEvent::ManualFileChunk {
                        transfer_id: 22,
                        offset,
                        data,
                    },
                    &settings(&directory),
                    now,
                )
                .await;
        }

        let duplicate = receiver
            .handle_protocol(
                1,
                "sender".into(),
                ProtoEvent::ManualFileChunk {
                    transfer_id: 22,
                    offset: 0,
                    data: b"abc".to_vec(),
                },
                &settings(&directory),
                now,
            )
            .await;
        assert!(sent_event(&duplicate, |event| matches!(
            event,
            ProtoEvent::ManualFileRequest { offset: 6, .. }
        )));
        assert_eq!(
            receiver.snapshot()[0].state,
            ManualTransferState::Transferring
        );

        let completed = receiver
            .handle_protocol(
                1,
                "sender".into(),
                ProtoEvent::ManualFileComplete {
                    transfer_id: 22,
                    sha256: Sha256::digest(b"abcdef").into(),
                },
                &settings(&directory),
                now,
            )
            .await;
        assert!(sent_event(&completed, |event| matches!(
            event,
            ProtoEvent::ManualFileResult { success: true, .. }
        )));
        assert_eq!(fs::read(directory.join("data.bin")).unwrap(), b"abcdef");
        fs::remove_dir_all(directory).unwrap();
    }

    fn enqueue(queue: &mut VecDeque<(u8, ProtoEvent)>, actions: Vec<ManualAction<u8>>) {
        for action in actions {
            if let ManualAction::Send { peer, event } = action {
                let encoded = event.encode().unwrap();
                assert!(encoded.len() <= 1200);
                queue.push_back((peer, ProtoEvent::decode(&encoded).unwrap()));
            }
        }
    }

    #[tokio::test]
    async fn simultaneous_bidirectional_files_complete_without_id_collision() {
        let directory = temp_dir("bidirectional");
        let source_a = directory.join("from-a.bin");
        let source_b = directory.join("from-b.bin");
        let bytes_a = (0..8193)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let bytes_b = b"from the other device";
        fs::write(&source_a, &bytes_a).unwrap();
        fs::write(&source_b, bytes_b).unwrap();
        let settings_a = FileReceiveSettings {
            auto_accept: true,
            download_directory: directory.join("a"),
        };
        let settings_b = FileReceiveSettings {
            auto_accept: true,
            download_directory: directory.join("b"),
        };
        let mut a = ManualTransfers::<u8>::new();
        let mut b = ManualTransfers::<u8>::new();
        let now = Instant::now();
        let mut queue = VecDeque::new();
        enqueue(
            &mut queue,
            a.send_files(1, "b".into(), vec![source_a], now).await,
        );
        enqueue(
            &mut queue,
            b.send_files(0, "a".into(), vec![source_b], now).await,
        );
        for _ in 0..1000 {
            let Some((target, event)) = queue.pop_front() else {
                break;
            };
            let actions = if target == 0 {
                a.handle_protocol(1, "b".into(), event, &settings_a, now)
                    .await
            } else {
                b.handle_protocol(0, "a".into(), event, &settings_b, now)
                    .await
            };
            enqueue(&mut queue, actions);
        }
        assert!(queue.is_empty());
        assert_eq!(
            fs::read(settings_b.download_directory.join("from-a.bin")).unwrap(),
            bytes_a
        );
        assert_eq!(
            fs::read(settings_a.download_directory.join("from-b.bin")).unwrap(),
            bytes_b
        );
        assert!(
            a.snapshot()
                .iter()
                .chain(b.snapshot().iter())
                .all(|record| record.state == ManualTransferState::Completed)
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn source_handle_survives_path_replacement_and_lost_completion_result() {
        let directory = temp_dir("source-ownership");
        let source = directory.join("source.bin");
        let original = (0..5001)
            .map(|index| (index % 239) as u8)
            .collect::<Vec<_>>();
        fs::write(&source, &original).unwrap();
        let receive_settings = FileReceiveSettings {
            auto_accept: true,
            download_directory: directory.join("received"),
        };
        let mut sender = ManualTransfers::<u8>::new();
        let mut receiver = ManualTransfers::<u8>::new();
        let now = Instant::now();
        let mut queue = VecDeque::new();
        enqueue(
            &mut queue,
            sender
                .send_files(1, "receiver".into(), vec![source.clone()], now)
                .await,
        );
        let transfer_id = sender.snapshot()[0].transfer_id;
        let mut replaced = false;
        let mut lost_result = false;
        for _ in 0..1000 {
            let Some((target, event)) = queue.pop_front() else {
                break;
            };
            if target == 1 && !replaced && matches!(&event, ProtoEvent::ManualFileChunk { .. }) {
                fs::rename(&source, directory.join("moved-source.bin")).unwrap();
                fs::write(&source, vec![255; original.len()]).unwrap();
                replaced = true;
            }
            if target == 0
                && !lost_result
                && matches!(&event, ProtoEvent::ManualFileResult { success: true, .. })
            {
                lost_result = true;
                continue;
            }
            let actions = if target == 0 {
                sender
                    .handle_protocol(1, "receiver".into(), event, &receive_settings, now)
                    .await
            } else {
                receiver
                    .handle_protocol(0, "sender".into(), event, &receive_settings, now)
                    .await
            };
            enqueue(&mut queue, actions);
        }
        assert!(replaced && lost_result && queue.is_empty());
        assert_eq!(
            sender.snapshot()[0].state,
            ManualTransferState::Transferring
        );
        let retry = sender.tick(now + RETRY_TIMEOUT + Duration::from_millis(1));
        assert!(sent_event(&retry, |event| matches!(
            event,
            ProtoEvent::ManualFileComplete { .. }
        )));
        assert!(!sent_event(&retry, |event| matches!(
            event,
            ProtoEvent::ManualFileChunk { .. }
        )));
        enqueue(&mut queue, retry);
        for _ in 0..20 {
            let Some((target, event)) = queue.pop_front() else {
                break;
            };
            let actions = if target == 0 {
                sender
                    .handle_protocol(1, "receiver".into(), event, &receive_settings, now)
                    .await
            } else {
                receiver
                    .handle_protocol(0, "sender".into(), event, &receive_settings, now)
                    .await
            };
            enqueue(&mut queue, actions);
        }
        assert_eq!(sender.snapshot()[0].state, ManualTransferState::Completed);
        let delayed = receiver
            .handle_protocol(
                0,
                "sender".into(),
                ProtoEvent::ManualFileChunk {
                    transfer_id,
                    offset: 0,
                    data: original[..8].to_vec(),
                },
                &receive_settings,
                now,
            )
            .await;
        assert!(!sent_event(&delayed, |event| matches!(
            event,
            ProtoEvent::ManualFileResult { success: false, .. }
        )));
        assert_eq!(
            fs::read(receive_settings.download_directory.join("source.bin")).unwrap(),
            original
        );
        assert_eq!(fs::read(&source).unwrap(), vec![255; original.len()]);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn early_completion_waits_for_data_and_bad_digest_removes_partial() {
        let directory = temp_dir("completion-order");
        let mut receiver = ManualTransfers::<u8>::new();
        let now = Instant::now();
        for (id, name, digest) in [
            (101, "good.bin", <[u8; 32]>::from(Sha256::digest(b"abc"))),
            (102, "bad.bin", [0; 32]),
        ] {
            receiver
                .handle_protocol(
                    1,
                    "sender".into(),
                    ProtoEvent::ManualFileOffer {
                        transfer_id: id,
                        file_name: name.into(),
                        size: 3,
                    },
                    &settings(&directory),
                    now,
                )
                .await;
            receiver.accept("sender", id, directory.clone(), now).await;
            receiver
                .handle_protocol(
                    1,
                    "sender".into(),
                    ProtoEvent::ManualFileComplete {
                        transfer_id: id,
                        sha256: digest,
                    },
                    &settings(&directory),
                    now,
                )
                .await;
            assert_eq!(fs::metadata(directory.join(name)).unwrap().len(), 0);
            receiver
                .handle_protocol(
                    1,
                    "sender".into(),
                    ProtoEvent::ManualFileChunk {
                        transfer_id: id,
                        offset: 0,
                        data: b"abc".to_vec(),
                    },
                    &settings(&directory),
                    now,
                )
                .await;
        }
        assert_eq!(fs::read(directory.join("good.bin")).unwrap(), b"abc");
        assert!(!directory.join("bad.bin").exists());
        assert_eq!(
            receiver
                .snapshot()
                .iter()
                .find(|status| status.transfer_id == 102)
                .unwrap()
                .state,
            ManualTransferState::Failed
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
