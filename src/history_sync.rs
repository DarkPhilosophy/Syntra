use bincode::Options;
use lan_mouse_history::{ClearBoundary, ImportedHistoryEvent};
use lan_mouse_proto::{MAX_HISTORY_CHUNK_SIZE, MAX_HISTORY_RECORD_SIZE, ProtoEvent};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::time::{Duration, Instant};

pub(crate) fn deduplicate_routes<I, R>(routes: impl IntoIterator<Item = (I, R)>) -> HashMap<I, R>
where
    I: Eq + Hash,
{
    routes.into_iter().collect()
}

pub(crate) const PAGE_SIZE: usize = 8;
pub(crate) const CLEAR_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PENDING_RECORDS: usize = 8;
const MAX_PENDING_BYTES: usize = MAX_HISTORY_RECORD_SIZE;

#[derive(Debug)]
struct PendingRecord {
    received: u32,
    total_len: usize,
    chunks: Vec<Option<Vec<u8>>>,
    started: Instant,
}

#[derive(Debug)]
pub(crate) struct Reassembler<P> {
    records: HashMap<(P, u64, u64), PendingRecord>,
    pending_bytes: usize,
}

impl<P> Default for Reassembler<P> {
    fn default() -> Self {
        Self {
            records: HashMap::new(),
            pending_bytes: 0,
        }
    }
}

impl<P: Clone + Eq + Hash> Reassembler<P> {
    pub(crate) fn start(
        &mut self,
        peer: P,
        request_id: u64,
        record_id: u64,
        total_len: u32,
        chunks: u32,
    ) -> Result<(), &'static str> {
        let total_len = total_len as usize;
        if total_len == 0 || total_len > MAX_HISTORY_RECORD_SIZE {
            return Err("history record exceeds pending limits");
        }
        let expected = total_len.div_ceil(MAX_HISTORY_CHUNK_SIZE) as u32;
        if chunks != expected {
            return Err("invalid history record chunk count");
        }
        let key = (peer, request_id, record_id);
        if let Some(previous) = self.records.get(&key) {
            return if previous.total_len == total_len && previous.chunks.len() == chunks as usize {
                Ok(())
            } else {
                Err("conflicting history record start")
            };
        }
        if self.records.len() >= MAX_PENDING_RECORDS
            || self.pending_bytes.saturating_add(total_len) > MAX_PENDING_BYTES
        {
            return Err("history record exceeds pending limits");
        }
        self.pending_bytes += total_len;
        self.records.insert(
            key,
            PendingRecord {
                received: 0,
                total_len,
                chunks: (0..chunks).map(|_| None).collect(),
                started: Instant::now(),
            },
        );
        Ok(())
    }

    pub(crate) fn chunk(
        &mut self,
        peer: &P,
        request_id: u64,
        record_id: u64,
        index: u32,
        data: Vec<u8>,
    ) -> Result<Option<ImportedHistoryEvent>, &'static str> {
        let key = (peer.clone(), request_id, record_id);
        let Some(record) = self.records.get_mut(&key) else {
            return Ok(None);
        };
        let chunks_len = record.chunks.len();
        if index as usize >= chunks_len {
            return Err("invalid history record chunk index");
        }
        let expected_len = if index as usize + 1 == chunks_len {
            record.total_len - MAX_HISTORY_CHUNK_SIZE * (chunks_len - 1)
        } else {
            MAX_HISTORY_CHUNK_SIZE
        };
        if data.len() != expected_len {
            let record = self.records.remove(&key).expect("record exists");
            self.pending_bytes = self.pending_bytes.saturating_sub(record.total_len);
            return Err("invalid history record chunk length");
        }
        let slot = &mut record.chunks[index as usize];
        if let Some(previous) = slot {
            return if previous == &data {
                Ok(None)
            } else {
                Err("conflicting duplicate history record chunk")
            };
        }
        *slot = Some(data);
        record.received += 1;
        if record.received != record.chunks.len() as u32 {
            return Ok(None);
        }
        let record = self.records.remove(&key).expect("record exists");
        self.pending_bytes = self.pending_bytes.saturating_sub(record.total_len);
        let mut bytes = Vec::with_capacity(record.total_len);
        for chunk in record.chunks {
            bytes.extend(chunk.expect("received count covers every chunk"));
        }
        history_codec()
            .deserialize(&bytes)
            .map(Some)
            .map_err(|_| "invalid serialized history record")
    }

    pub(crate) fn peer_lost(&mut self, peer: &P) {
        self.records.retain(|(candidate, _, _), record| {
            let retain = candidate != peer;
            if !retain {
                self.pending_bytes = self.pending_bytes.saturating_sub(record.total_len);
            }
            retain
        });
    }

    /// Discard incomplete records after a request reaches its page boundary.
    pub(crate) fn finish_request(&mut self, peer: &P, request_id: u64) -> usize {
        let mut discarded = 0;
        self.records
            .retain(|(candidate, candidate_request, _), record| {
                let retain = candidate != peer || *candidate_request != request_id;
                if !retain {
                    discarded += 1;
                    self.pending_bytes = self.pending_bytes.saturating_sub(record.total_len);
                }
                retain
            });
        discarded
    }

    pub(crate) fn expire(&mut self, now: Instant) -> usize {
        let mut discarded = 0;
        self.records.retain(|_, record| {
            let retain = now.duration_since(record.started) < CLEAR_TIMEOUT;
            if !retain {
                discarded += 1;
                self.pending_bytes = self.pending_bytes.saturating_sub(record.total_len);
            }
            retain
        });
        discarded
    }
}

fn history_codec() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(MAX_HISTORY_RECORD_SIZE as u64)
}

pub(crate) fn record_events(
    request_id: u64,
    record_id: u64,
    record: &ImportedHistoryEvent,
) -> Result<Vec<ProtoEvent>, String> {
    let bytes = history_codec()
        .serialize(record)
        .map_err(|error| error.to_string())?;
    if bytes.is_empty() || bytes.len() > MAX_HISTORY_RECORD_SIZE {
        return Err("serialized history record exceeds protocol limit".into());
    }
    let total_len = u32::try_from(bytes.len()).map_err(|_| "history record is too large")?;
    let chunks = total_len.div_ceil(MAX_HISTORY_CHUNK_SIZE as u32);
    let mut events = Vec::with_capacity(chunks as usize + 1);
    events.push(ProtoEvent::HistoryRecordStart {
        request_id,
        record_id,
        total_len,
        chunks,
    });
    events.extend(
        bytes
            .chunks(MAX_HISTORY_CHUNK_SIZE)
            .enumerate()
            .map(|(index, data)| ProtoEvent::HistoryRecordChunk {
                request_id,
                record_id,
                index: index as u32,
                data: data.to_vec(),
            }),
    );
    Ok(events)
}

pub(crate) fn boundary_to_wire(boundary: &ClearBoundary) -> Vec<(String, u64)> {
    boundary
        .iter()
        .map(|(origin, sequence)| (origin.clone(), *sequence))
        .collect()
}

pub(crate) fn boundary_from_wire(entries: Vec<(String, u64)>) -> ClearBoundary {
    let mut boundary = ClearBoundary::new();
    for (origin, sequence) in entries {
        boundary
            .entry(origin)
            .and_modify(|known| *known = (*known).max(sequence))
            .or_insert(sequence);
    }
    boundary
}

pub(crate) fn union_boundary(target: &mut ClearBoundary, source: ClearBoundary) {
    for (origin, sequence) in source {
        target
            .entry(origin)
            .and_modify(|known| *known = (*known).max(sequence))
            .or_insert(sequence);
    }
}

pub(crate) struct ClearCoordinator<P> {
    pub(crate) operation_id: [u8; 16],
    pub(crate) boundary: ClearBoundary,
    pub(crate) awaiting_boundaries: HashSet<P>,
    pub(crate) peer_count: usize,
    pub(crate) participants: HashSet<P>,
    pub(crate) awaiting_acks: HashSet<P>,
    pub(crate) affected: usize,
    pub(crate) errors: Vec<String>,
    pub(crate) deadline: Instant,
    pub(crate) applying: bool,
}

impl<P: Clone + Eq + Hash> ClearCoordinator<P> {
    pub(crate) fn new(operation_id: [u8; 16], peers: HashSet<P>, boundary: ClearBoundary) -> Self {
        let peer_count = peers.len();
        let participants = peers.clone();
        Self {
            operation_id,
            boundary,
            awaiting_boundaries: peers,
            peer_count,
            participants,
            awaiting_acks: HashSet::new(),
            affected: 0,
            errors: Vec::new(),
            deadline: Instant::now() + CLEAR_TIMEOUT,
            applying: false,
        }
    }

    pub(crate) fn add_boundary(&mut self, peer: &P, boundary: ClearBoundary) -> bool {
        if !self.awaiting_boundaries.remove(peer) {
            return false;
        }
        union_boundary(&mut self.boundary, boundary);
        self.awaiting_boundaries.is_empty()
    }

    pub(crate) fn begin_apply(&mut self, peers: HashSet<P>, local_affected: usize) {
        self.applying = true;
        self.awaiting_acks = peers;
        self.affected = local_affected;
        self.deadline = Instant::now() + CLEAR_TIMEOUT;
    }

    pub(crate) fn add_ack(&mut self, peer: &P, affected: u64, error: Option<String>) -> bool {
        if !self.awaiting_acks.remove(peer) {
            return false;
        }
        self.affected = self
            .affected
            .saturating_add(usize::try_from(affected).unwrap_or(usize::MAX));
        if let Some(error) = error {
            self.errors.push(error);
        }
        self.awaiting_acks.is_empty()
    }
    pub(crate) fn peer_lost(&mut self, peer: &P) {
        if self.awaiting_boundaries.remove(peer) || self.awaiting_acks.remove(peer) {
            self.errors
                .push("peer disconnected during global clear".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lan_mouse_history::{HistoryContent, HistoryEventId};

    fn event(bytes: usize) -> ImportedHistoryEvent {
        ImportedHistoryEvent {
            event_id: HistoryEventId {
                origin_device_id: "origin".into(),
                origin_sequence: 7,
            },
            created_at_ms: 9,
            origin_label: Some("peer".into()),
            content: HistoryContent::Text("x".repeat(bytes)),
        }
    }

    #[test]
    fn record_reassembly_accepts_reordering_and_duplicate_datagrams() {
        let source = event(MAX_HISTORY_CHUNK_SIZE);
        let events = record_events(3, 4, &source).unwrap();
        let ProtoEvent::HistoryRecordStart {
            total_len,
            chunks: chunk_count,
            ..
        } = events[0]
        else {
            panic!()
        };
        let mut chunks = events.into_iter().skip(1).collect::<Vec<_>>();
        let mut reassembler = Reassembler::default();
        reassembler
            .start(1u8, 3, 4, total_len, chunk_count)
            .unwrap();
        chunks.reverse();
        let mut imported = None;
        for event in chunks {
            let ProtoEvent::HistoryRecordChunk { index, data, .. } = event else {
                panic!()
            };
            imported = reassembler
                .chunk(&1, 3, 4, index, data.clone())
                .unwrap()
                .or(imported);
            imported = reassembler
                .chunk(&1, 3, 4, index, data)
                .unwrap()
                .or(imported);
        }
        assert_eq!(imported, Some(source));
    }

    #[test]
    fn incomplete_requests_release_the_entire_reassembly_budget() {
        let source = event(1);
        let events = record_events(3, 4, &source).unwrap();
        let ProtoEvent::HistoryRecordStart {
            total_len, chunks, ..
        } = events[0]
        else {
            panic!()
        };
        let mut reassembler = Reassembler::default();
        for record_id in 0..MAX_PENDING_RECORDS as u64 {
            reassembler
                .start(1u8, 3, record_id, total_len, chunks)
                .unwrap();
        }
        assert_eq!(reassembler.finish_request(&1, 3), MAX_PENDING_RECORDS);
        assert_eq!(reassembler.pending_bytes, 0);
        reassembler.start(1u8, 4, 9, total_len, chunks).unwrap();
        assert_eq!(reassembler.expire(Instant::now() + CLEAR_TIMEOUT), 1);
        assert_eq!(reassembler.pending_bytes, 0);
    }

    #[test]
    fn clear_waits_for_every_boundary_and_ack_and_unions_maxima() {
        let peers = HashSet::from([1u8, 2]);
        let mut clear = ClearCoordinator::new(
            [1; 16],
            peers.clone(),
            ClearBoundary::from([("a".into(), 2)]),
        );
        assert!(!clear.add_boundary(&1, ClearBoundary::from([("a".into(), 4)])));
        assert!(clear.add_boundary(&2, ClearBoundary::from([("b".into(), 3)])));
        assert_eq!(
            clear.boundary,
            ClearBoundary::from([("a".into(), 4), ("b".into(), 3)])
        );
        clear.begin_apply(peers, 5);
        assert!(!clear.add_ack(&1, 2, None));
        assert!(clear.add_ack(&2, 1, Some("remote failure".into())));
        assert_eq!(clear.affected, 8);
        assert_eq!(clear.errors, ["remote failure"]);
    }

    #[test]
    fn clear_counts_duplicate_routes_as_one_device() {
        let routes = deduplicate_routes([
            ("same-device", "outgoing-route"),
            ("same-device", "incoming-route"),
        ]);
        assert_eq!(routes.len(), 1);

        let peers = routes.keys().copied().collect();
        let mut clear =
            ClearCoordinator::new([2; 16], peers, ClearBoundary::from([("origin".into(), 2)]));
        assert!(clear.add_boundary(
            &"same-device",
            ClearBoundary::from([("remote-origin".into(), 2)]),
        ));
        clear.begin_apply(clear.participants.clone(), 2);
        assert!(clear.add_ack(&"same-device", 2, None));
        assert_eq!(clear.peer_count, 1);
        assert_eq!(clear.affected, 4);
    }
}
