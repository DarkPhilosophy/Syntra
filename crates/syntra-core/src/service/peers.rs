//! Authenticated peer identity and the peer-to-peer protocol.
//!
//! Certificate fingerprints, device profiles and the dispatch of incoming
//! protocol events belong here. A peer is addressed by fingerprint, never by
//! address, because addresses change and identity must not.

use super::*;

impl Service {
    pub(super) fn connected_authenticated_peers(&self) -> HashMap<String, Peer> {
        history_sync::deduplicate_routes(
            self.authenticated_capture_fingerprints
                .iter()
                .map(|(handle, fingerprint)| (fingerprint.clone(), Peer::Capture(*handle)))
                .chain(
                    self.authenticated_peer_fingerprints
                        .iter()
                        .map(|(addr, fingerprint)| (fingerprint.clone(), Peer::Emulation(*addr))),
                ),
        )
    }

    pub(super) fn send_peer(&self, peer: Peer, event: syntra_proto::ProtoEvent) {
        match peer {
            Peer::Capture(handle) => self.capture.send_proto(handle, event),
            Peer::Emulation(addr) => self.emulation.send_proto(addr, event),
        }
    }

    pub(super) fn request_profile(&mut self, fingerprint: String, peer: Peer) {
        if self.profile_requests.contains_key(&fingerprint) {
            return;
        }
        let request_id = self.next_history_request_id();
        self.profile_requests
            .insert(fingerprint, (request_id, 0, std::time::Instant::now()));
        self.send_peer(
            peer,
            syntra_proto::ProtoEvent::ProfileRequest { request_id },
        );
    }

    pub(super) fn retry_profiles(&mut self) {
        let now = std::time::Instant::now();
        self.profile_reassembler.expire(now);
        let routes = self.connected_authenticated_peers();
        let expired = self
            .profile_requests
            .iter()
            .filter(|(_, (_, _, sent))| {
                now.saturating_duration_since(*sent) >= Duration::from_secs(2)
            })
            .map(|(fingerprint, (_, retries, _))| (fingerprint.clone(), *retries))
            .collect::<Vec<_>>();
        for (fingerprint, retries) in expired {
            self.profile_requests.remove(&fingerprint);
            self.profile_reassembler.remove_peer(&fingerprint);
            if retries >= 3 {
                log::warn!("device profile exchange timed out for {fingerprint}");
                continue;
            }
            if let Some(peer) = routes.get(&fingerprint) {
                let request_id = self.next_history_request_id();
                self.profile_requests
                    .insert(fingerprint, (request_id, retries + 1, now));
                self.send_peer(
                    *peer,
                    syntra_proto::ProtoEvent::ProfileRequest { request_id },
                );
            }
        }
    }

    pub(super) async fn accept_profile(&mut self, fingerprint: String, profile: DeviceProfile) {
        self.profile_requests.remove(&fingerprint);
        if self.peer_profiles.get(&fingerprint) == Some(&profile) {
            return;
        }
        if let Err(error) =
            peer_profile::save_cached(self.config.config_path(), &fingerprint, &profile).await
        {
            self.notify_frontend(FrontendEvent::Error(format!(
                "Cannot save received peer profile: {error}"
            )));
        }
        self.peer_profiles
            .insert(fingerprint.clone(), profile.clone());
        self.notify_frontend(FrontendEvent::PeerDeviceProfile {
            fingerprint,
            profile,
        });
    }

    pub(super) async fn handle_peer_protocol(
        &mut self,
        peer: Peer,
        event: syntra_proto::ProtoEvent,
    ) {
        use syntra_proto::ProtoEvent;
        let Some(identity) = (match peer {
            Peer::Capture(handle) => self.authenticated_capture_fingerprints.get(&handle),
            Peer::Emulation(addr) => self.authenticated_peer_fingerprints.get(&addr),
        })
        .cloned() else {
            log::warn!(
                "ignoring history protocol from route without certificate identity: {peer:?}"
            );
            return;
        };
        match event {
            ProtoEvent::ManualFileOffer { .. }
            | ProtoEvent::ManualFileDecision { .. }
            | ProtoEvent::ManualFileRequest { .. }
            | ProtoEvent::ManualFileChunk { .. }
            | ProtoEvent::ManualFileComplete { .. }
            | ProtoEvent::ManualFileResult { .. }
            | ProtoEvent::ManualFileCancel { .. } => {
                let actions = self
                    .manual_transfers
                    .handle_protocol(
                        peer,
                        identity,
                        event,
                        &self.file_receive_settings,
                        std::time::Instant::now(),
                    )
                    .await;
                self.execute_manual_actions(actions);
            }
            ProtoEvent::ProfileChanged => {
                self.profile_requests.remove(&identity);
                self.profile_reassembler.remove_peer(&identity);
                self.request_profile(identity, peer);
            }
            ProtoEvent::ProfileRequest { request_id } => {
                if let Ok(events) =
                    peer_profile::encode_profile(request_id, &self.local_device_profile)
                {
                    for event in events {
                        self.send_peer(peer, event);
                    }
                }
            }
            ProtoEvent::ProfileStart {
                request_id,
                width,
                height,
                total_len,
                chunks,
                display_name,
            } => {
                if !self
                    .profile_requests
                    .get(&identity)
                    .is_some_and(|(id, _, _)| *id == request_id)
                {
                    return;
                }
                match self.profile_reassembler.start(
                    &identity,
                    request_id,
                    peer_profile::ProfileHeader {
                        width,
                        height,
                        total_len,
                        chunks,
                        display_name,
                    },
                ) {
                    Ok(Some(profile)) => self.accept_profile(identity.clone(), profile).await,
                    Ok(None) => {}
                    Err(error) => log::warn!("profile rejected: {error}"),
                }
            }
            ProtoEvent::ProfileChunk {
                request_id,
                index,
                data,
            } => {
                if !self
                    .profile_requests
                    .get(&identity)
                    .is_some_and(|(id, _, _)| *id == request_id)
                {
                    return;
                }
                match self
                    .profile_reassembler
                    .chunk(&identity, request_id, index, data)
                {
                    Ok(Some(profile)) => self.accept_profile(identity.clone(), profile).await,
                    Ok(None) => {}
                    Err(error) => log::warn!("profile rejected: {error}"),
                }
            }
            ProtoEvent::HistorySyncRequest { request_id, offset } => {
                self.send_history_page(peer, request_id, offset).await;
            }
            ProtoEvent::HistoryRecordStart {
                request_id,
                record_id,
                total_len,
                chunks,
            } => {
                if let Err(error) = self
                    .history_reassembler
                    .start(peer, request_id, record_id, total_len, chunks)
                {
                    log::warn!("history synchronization rejected: {error}");
                }
            }
            ProtoEvent::HistoryRecordChunk {
                request_id,
                record_id,
                index,
                data,
            } => {
                match self
                    .history_reassembler
                    .chunk(&peer, request_id, record_id, index, data)
                {
                    Ok(Some(record)) => {
                        if let Err(error) = self.history.import(record).await {
                            log::warn!("history synchronization import rejected: {error}");
                        }
                    }
                    Ok(None) => {}
                    Err(error) => log::warn!("history synchronization rejected: {error}"),
                }
            }
            ProtoEvent::HistorySyncPageEnd {
                request_id,
                next_offset,
            } => {
                let discarded = self.history_reassembler.finish_request(&peer, request_id);
                let Some((active_request, page_offset, retries)) =
                    self.history_sync_progress.get(&peer).copied()
                else {
                    return;
                };
                if active_request != request_id {
                    return;
                }
                if discarded != 0 {
                    if retries >= 3 {
                        self.history_sync_progress.remove(&peer);
                        let error = format!(
                            "History synchronization with a device stopped after repeated incomplete transfers ({discarded} incomplete record(s))."
                        );
                        log::warn!("{error}");
                        self.notify_frontend(FrontendEvent::HistoryError(error));
                    } else {
                        let retry_request = self.next_history_request_id();
                        self.history_sync_progress
                            .insert(peer, (retry_request, page_offset, retries + 1));
                        self.send_peer(
                            peer,
                            ProtoEvent::HistorySyncRequest {
                                request_id: retry_request,
                                offset: page_offset,
                            },
                        );
                    }
                } else if let Some(offset) = next_offset {
                    self.history_sync_progress
                        .insert(peer, (request_id, offset, 0));
                    self.send_peer(peer, ProtoEvent::HistorySyncRequest { request_id, offset });
                } else {
                    self.history_sync_progress.remove(&peer);
                }
            }
            ProtoEvent::HistoryClearBoundaryRequest { operation_id } => {
                match self.history.snapshot_boundary().await {
                    Ok(boundary) => self.send_peer(
                        peer,
                        ProtoEvent::HistoryClearBoundary {
                            operation_id,
                            boundary: history_sync::boundary_to_wire(&boundary),
                        },
                    ),
                    Err(error) => self.send_peer(
                        peer,
                        ProtoEvent::HistoryClearAck {
                            operation_id,
                            affected: 0,
                            error: Some(error),
                        },
                    ),
                }
            }
            ProtoEvent::HistoryClearBoundary {
                operation_id,
                boundary,
            } => {
                let ready = self.history_clear.as_mut().is_some_and(|clear| {
                    clear.operation_id == operation_id
                        && clear.add_boundary(&identity, history_sync::boundary_from_wire(boundary))
                });
                if ready {
                    let mut clear = self.history_clear.take().expect("clear exists");
                    let operation = hex_operation(operation_id);
                    match self
                        .history
                        .apply_clear(operation, clear.boundary.clone())
                        .await
                    {
                        Ok(affected) => {
                            let peers = clear.participants.clone();
                            clear.begin_apply(peers.clone(), affected);
                            let boundary = history_sync::boundary_to_wire(&clear.boundary);
                            for identity in peers {
                                if let Some(peer) =
                                    self.connected_authenticated_peers().get(&identity).copied()
                                {
                                    self.send_peer(
                                        peer,
                                        ProtoEvent::HistoryClearApply {
                                            operation_id,
                                            boundary: boundary.clone(),
                                        },
                                    );
                                }
                            }
                            self.history_clear = Some(clear);
                        }
                        Err(error) => self.notify_frontend(FrontendEvent::HistoryClearResult {
                            operation_id: hex_operation(operation_id),
                            affected: 0,
                            peers_acknowledged: 0,
                            error: Some(error),
                        }),
                    }
                }
            }
            ProtoEvent::HistoryClearApply {
                operation_id,
                boundary,
            } => {
                let result = self
                    .history
                    .apply_clear(
                        hex_operation(operation_id),
                        history_sync::boundary_from_wire(boundary),
                    )
                    .await;
                self.send_peer(
                    peer,
                    ProtoEvent::HistoryClearAck {
                        operation_id,
                        affected: result.as_ref().map_or(0, |affected| *affected as u64),
                        error: result.err(),
                    },
                );
            }
            ProtoEvent::HistoryClearAck {
                operation_id,
                affected,
                error,
            } => {
                let done = self.history_clear.as_mut().is_some_and(|clear| {
                    clear.operation_id == operation_id
                        && clear.applying
                        && clear.add_ack(&identity, affected, error)
                });
                if done {
                    let clear = self.history_clear.take().expect("clear exists");
                    self.notify_frontend(FrontendEvent::HistoryClearResult {
                        operation_id: hex_operation(operation_id),
                        affected: clear.affected as u64,
                        peers_acknowledged: clear.peer_count as u32,
                        error: (!clear.errors.is_empty()).then(|| clear.errors.join("; ")),
                    });
                }
            }
            event => self.handle_file_protocol(peer, event),
        }
    }

    pub(super) fn handle_file_protocol(&mut self, peer: Peer, event: syntra_proto::ProtoEvent) {
        use syntra_proto::ProtoEvent;
        match event {
            ProtoEvent::ClipboardManifest {
                transfer_id,
                entries,
            } => {
                let adapter_id = transfer_id.to_string();
                match self.transfers.inbound_manifest(
                    peer,
                    adapter_id,
                    transfer_id.to_string(),
                    transfer_id,
                    syntra_plugin_api::Operation::Copy,
                    entries,
                ) {
                    Ok(actions) => self.execute_transfer_actions(actions),
                    Err(error) => log::warn!("file manifest rejected for {peer:?}: {error}"),
                }
            }
            event => match self.transfers.handle_protocol(&peer, event) {
                Ok(actions) => self.execute_transfer_actions(actions),
                Err(error) => log::warn!("file protocol rejected for {peer:?}: {error}"),
            },
        }
    }
}
