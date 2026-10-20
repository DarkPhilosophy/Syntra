//! File transfer orchestration across plugins and peers.
//!
//! Bridges three parties: the transfer state machine, out-of-process plugins
//! that expose the files, and the clients that render progress.

/// A clipboard selection scanned off the event loop.
pub(super) struct ManifestOffer {
    generation: u64,
    adapter_id: String,
    manifest: syntra_plugin_api::CopyManifest,
    offer: Result<crate::file_transfer::FileOffer, crate::file_transfer::TransferError>,
}

use super::*;

impl Service {
    /// The key the transfer state machine knows a process under.
    ///
    /// A per-transfer process is keyed by its transfer id, which is what its
    /// frames are correlated with. A resident process is keyed by its plugin
    /// id, whatever that is. Nothing here looks at a plugin's name.
    fn transfer_key(&self, adapter: &ProcessAdapterId) -> String {
        match adapter {
            ProcessAdapterId::Transfer { transfer_id, .. } => transfer_id.clone(),
            ProcessAdapterId::Resident(plugin) | ProcessAdapterId::Settings(plugin) => {
                plugin.clone()
            }
        }
    }

    pub(super) fn refresh_manual_routes(&mut self) {
        let routes = self.connected_authenticated_peers();
        let actions = self
            .manual_transfers
            .refresh_routes(&routes, std::time::Instant::now());
        self.execute_manual_actions(actions);
    }

    pub(super) async fn handle_adapter_event(&mut self, event: ManagerEvent) {
        // A settings window is a side process of the plugin: its start, exit
        // or crash says nothing about the plugin's own process, so it must not
        // change the state the manager shows, cancel the plugin's transfers or
        // be counted as a restart. Only what it writes to its error stream
        // (a plugin-level diagnostic) is kept.
        let about_settings_window = match &event {
            ManagerEvent::Started(adapter, _)
            | ManagerEvent::Stopped(adapter)
            | ManagerEvent::Ready(adapter, _, _)
            | ManagerEvent::Message { adapter, .. }
            | ManagerEvent::Cancelled { adapter, .. }
            | ManagerEvent::Exited { adapter, .. } => {
                matches!(adapter, ProcessAdapterId::Settings(_))
            }
            ManagerEvent::Rejected { adapter, .. } => {
                matches!(adapter, Some(ProcessAdapterId::Settings(_)))
            }
            ManagerEvent::Diagnostics { .. } => false,
        };
        if about_settings_window {
            if let ManagerEvent::Exited { adapter, status } = &event {
                log::debug!("settings window of `{}` closed: {status}", adapter.plugin());
            }
            return;
        }
        match event {
            // Lifecycle notices drive the health the plugin manager shows;
            // without them every plugin would read as stopped.
            ManagerEvent::Started(adapter, pid) => {
                self.plugins.set_starting(adapter.plugin(), pid);
                self.publish_plugins();
            }
            ManagerEvent::Stopped(adapter) => {
                // A deliberate stop clears the process state without
                // recording a failure the user would have to dismiss.
                self.plugins.set_stopped(adapter.plugin());
                if let Some(transfer_id) = adapter.transfer() {
                    self.transfer_providers.remove(transfer_id);
                }
                self.publish_plugins();
            }
            ManagerEvent::Ready(adapter, declared, grant) => {
                // What the plugin declares about itself is shown, so a stale
                // manifest cannot misdescribe it. It is only displayed: it
                // cannot widen the grant, change who the plugin is or make it
                // bundled.
                if let Some(metadata) = declared {
                    self.plugins.adopt_declared(adapter.plugin(), metadata);
                }
                log::debug!(
                    "plugin {} ready; grant covers {:?}",
                    adapter.plugin(),
                    syntra_plugin_api::PluginRole::ALL
                        .into_iter()
                        .filter(|role| grant.allows(*role))
                        .collect::<Vec<_>>()
                );
                self.plugins.set_running(adapter.plugin(), true);
                self.publish_plugins();
            }
            ManagerEvent::Message { adapter, message } => {
                let adapter_id = self.transfer_key(&adapter);
                let actions = match message {
                    AdapterMessage::RangeRequest(request) => {
                        let transfer_id = request.transfer_id.clone();
                        let request_id = request.request_id;
                        let offset = request.offset;
                        match self.transfers.adapter_range_request(&adapter_id, request) {
                            Ok(actions) => Ok(actions),
                            Err(error) => {
                                if let Some(manager) = self.adapter_manager.as_ref() {
                                    let _ = manager.try_send(ManagerCommand::RangeResponse {
                                        id: adapter.plugin().to_owned(),
                                        response: syntra_plugin_api::RangeResponse {
                                            transfer_id,
                                            request_id,
                                            offset,
                                            data_base64: String::new(),
                                            eof: true,
                                            error: Some(error.to_string()),
                                        },
                                    });
                                }
                                Err(error)
                            }
                        }
                    }
                    AdapterMessage::RemoteManifest(_) => {
                        log::warn!(
                            "adapter {adapter_id} sent an unexpected remote manifest; \
                             cancelling its owned transfers"
                        );
                        Ok(self.transfers.adapter_lost(&adapter_id))
                    }
                    AdapterMessage::MountReady(ready) => {
                        self.transfers.adapter_mount_ready(&adapter_id, ready)
                    }
                    AdapterMessage::Progress(progress) => {
                        self.transfers.adapter_progress(&adapter_id, progress)
                    }
                    AdapterMessage::Completed(completion) => {
                        self.transfers.adapter_completed(&adapter_id, completion)
                    }
                    AdapterMessage::Cancelled(cancelled) => {
                        self.transfers.adapter_cancelled(&adapter_id, cancelled)
                    }
                    AdapterMessage::Released(released) => {
                        self.transfers.adapter_released(&adapter_id, released)
                    }
                    AdapterMessage::Unmounted(unmounted) => {
                        self.transfers.adapter_unmounted(&adapter_id, unmounted)
                    }
                    AdapterMessage::CopyManifest(manifest) => {
                        log::info!(
                            "file clipboard detected: transfer={} entries={}",
                            manifest.transfer_id,
                            manifest.entries.len()
                        );
                        if matches!(manifest.operation, Operation::Move) {
                            log::debug!("rejecting unsupported move clipboard manifest");
                            Ok(Vec::new())
                        } else {
                            if let Some(content) = history::files(
                                manifest
                                    .entries
                                    .iter()
                                    .filter(|entry| {
                                        !matches!(entry.kind, syntra_plugin_api::EntryKind::Other)
                                    })
                                    .map(|entry| {
                                        (entry.uri.clone(), entry.size.unwrap_or_default())
                                    }),
                            ) {
                                self.record_local_history(content).await;
                            }
                            let uri_text = manifest
                                .entries
                                .iter()
                                .filter(|entry| {
                                    !matches!(entry.kind, syntra_plugin_api::EntryKind::Other)
                                })
                                .map(|entry| entry.uri.as_str())
                                .collect::<Vec<_>>()
                                .join("\r\n");
                            // Walking a copied directory touches the disk for
                            // every entry. Doing it here would stall input,
                            // IPC and heartbeats, so build the offer once on
                            // the blocking pool and fan it out on completion.
                            self.manifest_generation = self.manifest_generation.wrapping_add(1);
                            let generation = self.manifest_generation;
                            let tx = self.manifest_offer_tx.clone();
                            tokio::task::spawn_local(async move {
                                let offer = tokio::task::spawn_blocking(move || {
                                    crate::file_transfer::FileOffer::from_uri_list(1, &uri_text)
                                })
                                .await;
                                let offer = match offer {
                                    Ok(result) => result,
                                    Err(error) => {
                                        log::warn!("clipboard manifest scan failed: {error}");
                                        return;
                                    }
                                };
                                let _ = tx
                                    .send(ManifestOffer {
                                        generation,
                                        adapter_id,
                                        manifest,
                                        offer,
                                    })
                                    .await;
                            });
                            Ok(Vec::new())
                        }
                    }
                    // Another application copied text: read it now rather
                    // than when the pointer next crosses to another device.
                    AdapterMessage::ClipboardData { mime_type, .. }
                        if matches!(adapter, ProcessAdapterId::Resident(_))
                            && mime_type.starts_with("text/") =>
                    {
                        log::info!(
                            "local text copy noticed; reading the clipboard (legacy={})",
                            self.legacy_clipboard
                        );
                        if self.legacy_clipboard {
                            self.clipboard.read_once();
                        }
                        Ok(Vec::new())
                    }
                    AdapterMessage::DeclareSettings { settings } => {
                        // Only a resident plugin owns settings; the registry
                        // ignores an id it does not know.
                        self.plugins
                            .adopt_owned_settings(adapter.plugin(), settings);
                        self.publish_plugins();
                        Ok(Vec::new())
                    }
                    AdapterMessage::Hello { .. }
                    | AdapterMessage::Error { .. }
                    | AdapterMessage::Settings { .. }
                    | AdapterMessage::ChangeSetting { .. }
                    | AdapterMessage::OpenSettings { .. }
                    | AdapterMessage::Pointer(_)
                    | AdapterMessage::PasteDestination(_)
                    | AdapterMessage::RangeResponse(_)
                    | AdapterMessage::PublishFileClipboard(_)
                    | AdapterMessage::Cancel { .. }
                    | AdapterMessage::ClipboardData { .. } => {
                        log::debug!("adapter {:?} message not handled by service", adapter);
                        Ok(Vec::new())
                    }
                };
                match actions {
                    Ok(actions) => self.execute_transfer_actions(actions),
                    Err(error) => log::warn!("adapter {:?} event rejected: {error}", adapter),
                }
            }
            ManagerEvent::Cancelled {
                adapter,
                transfer_id,
                ..
            } => {
                let key = self.transfer_key(&adapter);
                if let Ok(actions) = self
                    .transfers
                    .adapter_cancelled(&key, syntra_plugin_api::Cancelled { transfer_id })
                {
                    self.execute_transfer_actions(actions);
                }
            }
            ManagerEvent::Diagnostics { plugin, lines } => {
                self.plugins.set_diagnostics(&plugin, lines);
                self.publish_plugins();
            }
            ManagerEvent::Rejected { adapter, reason } => {
                log::warn!("adapter {:?} rejected transfer: {}", adapter, reason)
            }
            ManagerEvent::Exited { adapter, status } => {
                self.plugins.set_exited(adapter.plugin(), status.clone());
                self.publish_plugins();
                // Whatever the process was serving is lost with it. A plugin
                // that serves no transfer (a pointer plugin) has nothing to
                // cancel: the state machine simply knows nothing under its
                // key. The key is the one the state machine knows the process
                // under.
                let key = self.transfer_key(&adapter);
                let actions = self.transfers.adapter_lost(&key);
                self.execute_transfer_actions(actions);
                if let Some(transfer_id) = adapter.transfer() {
                    self.transfer_providers.remove(transfer_id);
                }
                log::warn!("adapter exited: {}", status);
            }
        }
    }

    /// Offers a scanned clipboard selection to every clipboard peer, unless a
    /// newer selection replaced it while the scan was running.
    pub(super) fn handle_manifest_offer(&mut self, result: ManifestOffer) {
        if result.generation != self.manifest_generation {
            log::debug!("dropping superseded clipboard manifest scan");
            return;
        }
        let offer = match result.offer {
            Ok(offer) => offer,
            Err(error) => {
                log::warn!("clipboard manifest rejected: {error}");
                return;
            }
        };
        let mut actions = Vec::new();
        for handle in self.client_manager.clipboard_clients() {
            self.next_clipboard_transfer = self.next_clipboard_transfer.wrapping_add(1).max(1);
            let wire_id = self.next_clipboard_transfer;
            let mut peer_offer = offer.clone();
            peer_offer.transfer_id = wire_id;
            match self.transfers.local_copy_manifest(
                Peer::Capture(handle),
                wire_id,
                result.adapter_id.clone(),
                result.manifest.clone(),
                peer_offer,
            ) {
                Ok(mut peer_actions) => actions.append(&mut peer_actions),
                Err(error) => log::warn!("clipboard manifest for {handle} rejected: {error}"),
            }
        }
        self.execute_transfer_actions(actions);
    }

    pub(super) fn handle_source_result(&mut self, result: SourceReadResult) {
        match result.result {
            Ok((file, Some((offset, data)), digest)) => {
                let sent = self
                    .transfers
                    .insert_outgoing_file(
                        &result.owner,
                        result.file_id,
                        result.request_id,
                        result.offset,
                        result.length,
                        file,
                    )
                    .and_then(|_| {
                        self.transfers.source_chunk(
                            &result.owner,
                            result.file_id,
                            result.request_id,
                            offset,
                            data,
                        )
                    });
                match (sent, digest) {
                    (Err(error), _) => log::warn!("source read failed: {error}"),
                    (Ok(actions), None) => self.execute_transfer_actions(actions),
                    (Ok(actions), Some(digest)) => {
                        self.execute_transfer_actions(actions);
                        match self.transfers.source_complete(
                            &result.owner,
                            result.file_id,
                            result.request_id,
                            result.offset + result.length,
                            digest,
                        ) {
                            Ok(actions) => self.execute_transfer_actions(actions),
                            Err(error) => log::warn!("source completion failed: {error}"),
                        }
                    }
                }
            }
            Ok(_) => log::warn!("source read returned no data"),
            Err(error) => log::warn!(
                "source read failed for {:?}/{}: {}",
                result.owner,
                result.file_id,
                error
            ),
        }
        // Whatever happened, this range is answered: its file handle and
        // reservation go (a no-op after `source_complete`).
        self.transfers
            .source_range_served(&result.owner, result.file_id, result.request_id);
    }

    pub(super) fn notify_transfer_frontend(&mut self, event: TransferFrontendEvent) {
        match event {
            TransferFrontendEvent::Progress {
                ui_id,
                completed,
                total,
                ..
            } => {
                self.announced_transfers.insert(ui_id);
                self.transfer_progress.insert(ui_id, (completed, total));
                self.notify_frontend(FrontendEvent::ClipboardTransferStatus(
                    ClipboardTransferStatus {
                        transfer_id: ui_id,
                        file_id: 0,
                        name: String::new(),
                        direction: ClipboardTransferDirection::Receiving,
                        transferred_bytes: completed,
                        total_bytes: total,
                        bytes_per_second: 0,
                        state: ClipboardTransferState::Transferring,
                    },
                ));
            }
            TransferFrontendEvent::Completed {
                ui_id,
                file_id,
                completed,
                total,
                ..
            } => {
                if self.announced_transfers.remove(&ui_id) {
                    self.transfer_progress.remove(&ui_id);
                    self.notify_frontend(FrontendEvent::ClipboardTransferStatus(
                        ClipboardTransferStatus {
                            transfer_id: ui_id,
                            file_id: file_id.unwrap_or_default(),
                            name: String::new(),
                            direction: ClipboardTransferDirection::Receiving,
                            transferred_bytes: completed,
                            total_bytes: total,
                            bytes_per_second: 0,
                            state: ClipboardTransferState::Completed,
                        },
                    ));
                }
            }
            TransferFrontendEvent::Cancelled {
                ui_id,
                file_id,
                completed,
                total,
                ..
            } => {
                if self.announced_transfers.remove(&ui_id) {
                    self.transfer_progress.remove(&ui_id);
                    self.notify_frontend(FrontendEvent::ClipboardTransferStatus(
                        ClipboardTransferStatus {
                            transfer_id: ui_id,
                            file_id: file_id.unwrap_or_default(),
                            name: String::new(),
                            direction: ClipboardTransferDirection::Receiving,
                            transferred_bytes: completed,
                            total_bytes: total,
                            bytes_per_second: 0,
                            state: ClipboardTransferState::Cancelled,
                        },
                    ));
                }
            }
            TransferFrontendEvent::Failed {
                ui_id,
                file_id,
                completed,
                total,
                error,
                ..
            } => {
                if self.announced_transfers.remove(&ui_id) {
                    self.transfer_progress.remove(&ui_id);
                    self.notify_frontend(FrontendEvent::ClipboardTransferStatus(
                        ClipboardTransferStatus {
                            transfer_id: ui_id,
                            file_id: file_id.unwrap_or_default(),
                            name: String::new(),
                            direction: ClipboardTransferDirection::Receiving,
                            transferred_bytes: completed,
                            total_bytes: total,
                            bytes_per_second: 0,
                            state: ClipboardTransferState::Failed(error),
                        },
                    ));
                }
            }
        }
    }

    pub(super) fn execute_manual_actions(&mut self, actions: Vec<ManualAction<Peer>>) {
        for action in actions {
            match action {
                ManualAction::Send { peer, event } => match peer {
                    Peer::Capture(handle) => self.capture.send_proto(handle, event),
                    Peer::Emulation(addr) => self.emulation.send_proto(addr, event),
                },
                ManualAction::Notify(event) => self.notify_frontend(event),
            }
        }
    }

    pub(super) fn execute_transfer_actions(&mut self, actions: Vec<TransferAction<Peer>>) {
        for action in actions {
            match action {
                TransferAction::Peer { peer, event } => {
                    log::info!("file transfer protocol event: peer={peer:?} event={event:?}");
                    match peer {
                        Peer::Capture(handle) => self.capture.send_proto(handle, event),
                        Peer::Emulation(addr) => self.emulation.send_proto(addr, event),
                    }
                }
                TransferAction::Adapter {
                    adapter_id,
                    message,
                } => {
                    log::info!(
                        "file transfer adapter event: adapter={adapter_id} message={message:?}"
                    );
                    let command = match message {
                        AdapterMessage::RemoteManifest(m) => {
                            // A transfer received through a mounted view is
                            // served by whichever plugin fills that role. With
                            // none, or one that is refused, the transfer is
                            // reported lost so the peer is told, not left
                            // waiting.
                            let lost_id = m.transfer_id.clone();
                            if !self.start_mounted_receive(m) {
                                let actions = self.transfers.adapter_lost(&lost_id);
                                self.execute_transfer_actions(actions);
                            }
                            continue;
                        }
                        AdapterMessage::RangeResponse(m) => {
                            let Some(plugin) = self.transfer_providers.get(&m.transfer_id) else {
                                continue;
                            };
                            ManagerCommand::RangeResponse {
                                id: plugin.clone(),
                                response: m,
                            }
                        }
                        AdapterMessage::PublishFileClipboard(m) => {
                            let operation =
                                if matches!(m.operation, syntra_plugin_api::Operation::Move) {
                                    "cut"
                                } else {
                                    "copy"
                                };
                            let uri_list = format!("{}\r\n", m.uris.join("\r\n")).into_bytes();
                            let gnome_files =
                                format!("{operation}\n{}", m.uris.join("\n")).into_bytes();
                            self.emulation.publish_file_clipboard(
                                vec![
                                    (syntra_plugin_api::URI_LIST_MIME.to_string(), uri_list),
                                    (
                                        syntra_plugin_api::GNOME_COPIED_FILES_MIME.to_string(),
                                        gnome_files,
                                    ),
                                ],
                                crate::emulation::ClipboardFallback::Adapter(m),
                            );
                            continue;
                        }
                        AdapterMessage::Released(m) => ManagerCommand::Released {
                            id: adapter_id.clone(),
                            released: m,
                        },
                        AdapterMessage::Unmounted(m) => {
                            let Some(plugin) = self.transfer_providers.get(&m.transfer_id) else {
                                continue;
                            };
                            ManagerCommand::Unmounted {
                                id: plugin.clone(),
                                unmounted: m,
                            }
                        }
                        AdapterMessage::Cancel { transfer_id } => {
                            // The state machine names the process by its
                            // adapter id: the plugin id for a resident
                            // provider, the transfer id for a per-transfer
                            // process. Which one this is, and which plugin
                            // serves a transfer, is recorded here, never read
                            // from a name.
                            let adapter = match self.transfer_providers.get(&adapter_id) {
                                Some(plugin) => ProcessAdapterId::Transfer {
                                    plugin: plugin.clone(),
                                    transfer_id: adapter_id.clone(),
                                },
                                None => ProcessAdapterId::Resident(adapter_id.clone()),
                            };
                            ManagerCommand::Cancel {
                                adapter,
                                transfer_id,
                            }
                        }
                        _ => continue,
                    };
                    if let Some(manager) = self.adapter_manager.as_ref() {
                        let _ = manager.try_send(command);
                    }
                }
                TransferAction::Frontend(event) => self.notify_transfer_frontend(event),
                TransferAction::ReadSource {
                    owner,
                    file_id,
                    request_id,
                    offset,
                    length,
                    offer,
                } => {
                    let tx = self.source_result_tx.clone();
                    tokio::task::spawn_local(async move {
                        let result = async {
                            let mut file = offer.open(file_id, offset).await?;
                            let mut hasher = Sha256::new();
                            let chunk = file.next_chunk_bounded(length as usize).await?;
                            if let Some((_, ref data)) = chunk {
                                hasher.update(data);
                            }
                            let digest = file.is_eof().then(|| hasher.finalize().into());
                            Ok((file, chunk, digest))
                        }
                        .await;
                        let _ = tx
                            .send(SourceReadResult {
                                owner,
                                file_id,
                                request_id,
                                offset,
                                length,
                                result,
                            })
                            .await;
                    });
                }
            }
        }
    }
}
