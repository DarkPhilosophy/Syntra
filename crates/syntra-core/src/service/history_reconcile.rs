//! Clipboard history reconciliation across peers.
//!
//! History is paged rather than streamed, and a global clear is coordinated
//! so it either applies everywhere reachable or reports where it did not.

use super::*;

impl Service {
    pub(super) fn next_history_request_id(&mut self) -> u64 {
        self.next_history_request = self.next_history_request.wrapping_add(1);
        self.next_history_request
    }

    pub(super) fn request_history_sync(&mut self, peer: Peer, offset: u64) {
        let request_id = self.next_history_request_id();
        self.history_sync_progress
            .insert(peer, (request_id, offset, 0));
        self.send_peer(
            peer,
            syntra_proto::ProtoEvent::HistorySyncRequest { request_id, offset },
        );
    }

    pub(super) fn broadcast_history_record(&mut self, record: &ImportedHistoryEvent) {
        let request_id = self.next_history_request_id();
        let Ok(events) = history_sync::record_events(request_id, 0, record) else {
            log::warn!("new history record exceeds synchronization limit");
            return;
        };
        for (_, peer) in self.connected_authenticated_peers() {
            for event in events.iter().cloned() {
                self.send_peer(peer, event);
            }
            self.send_peer(
                peer,
                syntra_proto::ProtoEvent::HistorySyncPageEnd {
                    request_id,
                    next_offset: None,
                },
            );
        }
    }

    pub(super) async fn send_history_page(&self, peer: Peer, request_id: u64, offset: u64) {
        match self.history.export(offset, history_sync::PAGE_SIZE).await {
            Ok((records, next_offset)) => {
                for (record_id, record) in records.iter().enumerate() {
                    if let Ok(events) =
                        history_sync::record_events(request_id, record_id as u64, record)
                    {
                        for event in events {
                            self.send_peer(peer, event);
                        }
                    }
                }
                self.send_peer(
                    peer,
                    syntra_proto::ProtoEvent::HistorySyncPageEnd {
                        request_id,
                        next_offset,
                    },
                );
            }
            Err(error) => log::warn!("cannot export clipboard history page: {error}"),
        }
    }

    pub(super) async fn start_global_history_clear(&mut self) {
        if self.history_clear.is_some() {
            self.notify_frontend(FrontendEvent::HistoryError(
                "A global history clear is already in progress.".into(),
            ));
            return;
        }
        let boundary = match self.history.snapshot_boundary().await {
            Ok(boundary) => boundary,
            Err(error) => {
                self.notify_frontend(FrontendEvent::HistoryError(error));
                return;
            }
        };
        let mut operation_hasher = Sha256::new();
        operation_hasher.update(self.public_key_fingerprint.as_bytes());
        operation_hasher.update(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .to_be_bytes(),
        );
        operation_hasher.update(self.next_history_request_id().to_be_bytes());
        let digest = operation_hasher.finalize();
        let mut operation_id = [0u8; 16];
        operation_id.copy_from_slice(&digest[..16]);
        let peers = self.connected_authenticated_peers();
        let coordinator = history_sync::ClearCoordinator::new(
            operation_id,
            peers.keys().cloned().collect(),
            boundary,
        );
        if peers.is_empty() {
            self.finish_local_only_clear(coordinator).await;
            return;
        }
        for peer in peers.values().copied() {
            self.send_peer(
                peer,
                syntra_proto::ProtoEvent::HistoryClearBoundaryRequest { operation_id },
            );
        }
        self.history_clear = Some(coordinator);
    }

    pub(super) async fn finish_local_only_clear(
        &mut self,
        coordinator: history_sync::ClearCoordinator<String>,
    ) {
        let operation = hex_operation(coordinator.operation_id);
        match self
            .history
            .apply_clear(operation.clone(), coordinator.boundary)
            .await
        {
            Ok(affected) => self.notify_frontend(FrontendEvent::HistoryClearResult {
                operation_id: operation,
                affected: affected as u64,
                peers_acknowledged: 0,
                error: None,
            }),
            Err(error) => self.notify_frontend(FrontendEvent::HistoryClearResult {
                operation_id: operation,
                affected: 0,
                peers_acknowledged: 0,
                error: Some(error),
            }),
        }
    }

    pub(super) fn history_peer_lost(&mut self, peer: Peer) {
        self.history_connected_peers.remove(&peer);
        self.history_reassembler.peer_lost(&peer);
        self.history_sync_progress.remove(&peer);
        let identity = match peer {
            Peer::Capture(handle) => self.authenticated_capture_fingerprints.remove(&handle),
            Peer::Emulation(addr) => self.authenticated_peer_fingerprints.remove(&addr),
        };
        self.refresh_manual_routes();
        if let Some(identity) = identity {
            if let Some(route) = self.connected_authenticated_peers().get(&identity).copied() {
                let retry = self.history_clear.as_ref().and_then(|clear| {
                    if clear.awaiting_boundaries.contains(&identity) {
                        Some(syntra_proto::ProtoEvent::HistoryClearBoundaryRequest {
                            operation_id: clear.operation_id,
                        })
                    } else if clear.awaiting_acks.contains(&identity) {
                        Some(syntra_proto::ProtoEvent::HistoryClearApply {
                            operation_id: clear.operation_id,
                            boundary: history_sync::boundary_to_wire(&clear.boundary),
                        })
                    } else {
                        None
                    }
                });
                if let Some(event) = retry {
                    self.send_peer(route, event);
                }
            } else if let Some(clear) = self.history_clear.as_mut() {
                clear.peer_lost(&identity);
            }
        }
    }
}
