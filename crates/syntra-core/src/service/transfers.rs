//! File transfer orchestration across plugins and peers.
//!
//! Bridges three parties: the transfer state machine, out-of-process plugins
//! that expose the files, and the clients that render progress.

use super::*;

/// Maps a supervised process onto the plugin identifier the manifest declares.
///
/// The supervisor keys FUSE adapters by transfer, but the plugin manager
/// shows one entry per plugin, so every FUSE process reports the same id.
fn plugin_id(adapter: &ProcessAdapterId) -> &'static str {
    match adapter {
        ProcessAdapterId::Gtk => crate::plugins::CLIPBOARD_PLUGIN_ID,
        ProcessAdapterId::Fuse { .. } => crate::plugins::FUSE_PLUGIN_ID,
    }
}

impl Service {
    pub(super) fn refresh_manual_routes(&mut self) {
        let routes = self.connected_authenticated_peers();
        let actions = self
            .manual_transfers
            .refresh_routes(&routes, std::time::Instant::now());
        self.execute_manual_actions(actions);
    }

    pub(super) async fn handle_adapter_event(&mut self, event: ManagerEvent) {
        match event {
            // Lifecycle notices drive the health the plugin manager shows;
            // without them every plugin would read as stopped.
            ManagerEvent::Started(adapter, pid) => {
                self.plugins.set_starting(plugin_id(&adapter), pid);
                self.publish_plugins();
            }
            ManagerEvent::Stopped(adapter) => {
                // A deliberate stop clears the process state without
                // recording a failure the user would have to dismiss.
                self.plugins.set_stopped(plugin_id(&adapter));
                self.publish_plugins();
            }
            ManagerEvent::Ready(adapter, declared) => {
                // Adopt the plugin's own description before reporting it, so
                // a stale manifest beside the binary cannot misdescribe it.
                if let Some(metadata) = declared {
                    self.plugins.adopt_declared(plugin_id(&adapter), metadata);
                }
                self.plugins.set_running(plugin_id(&adapter), true);
                self.publish_plugins();
            }
            ManagerEvent::Message { adapter, message } => {
                let adapter_id = match &adapter {
                    ProcessAdapterId::Gtk => "gtk-clipboard".to_owned(),
                    ProcessAdapterId::Fuse { transfer_id } => transfer_id.clone(),
                };
                let actions = match message {
                    AdapterMessage::RangeRequest(request) => {
                        let transfer_id = request.transfer_id.clone();
                        let request_id = request.request_id;
                        let offset = request.offset;
                        match self.transfers.adapter_range_request(&adapter_id, request) {
                            Ok(actions) => Ok(actions),
                            Err(error) => {
                                if let Some(manager) = self.adapter_manager.as_ref() {
                                    let _ = manager.try_send(ManagerCommand::RangeResponse(
                                        syntra_plugin_api::RangeResponse {
                                            transfer_id,
                                            request_id,
                                            offset,
                                            data_base64: String::new(),
                                            eof: true,
                                            error: Some(error.to_string()),
                                        },
                                    ));
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
                    AdapterMessage::Released(released) => self.transfers.adapter_released(released),
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
                            let mut actions = Vec::new();
                            for handle in self.client_manager.clipboard_clients() {
                                self.next_clipboard_transfer =
                                    self.next_clipboard_transfer.wrapping_add(1).max(1);
                                let wire_id = self.next_clipboard_transfer;
                                match crate::file_transfer::FileOffer::from_uri_list(
                                    wire_id, &uri_text,
                                ) {
                                    Ok(offer) => match self.transfers.local_copy_manifest(
                                        Peer::Capture(handle),
                                        wire_id,
                                        adapter_id.clone(),
                                        manifest.clone(),
                                        offer,
                                    ) {
                                        Ok(mut peer_actions) => actions.append(&mut peer_actions),
                                        Err(error) => log::warn!(
                                            "clipboard manifest for {handle} rejected: {error}"
                                        ),
                                    },
                                    Err(error) => {
                                        log::warn!("clipboard manifest rejected: {error}")
                                    }
                                }
                            }
                            Ok(actions)
                        }
                    }
                    AdapterMessage::Hello { .. }
                    | AdapterMessage::Error { .. }
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
                let adapter_id = match &adapter {
                    ProcessAdapterId::Gtk => "gtk-clipboard".to_owned(),
                    ProcessAdapterId::Fuse { transfer_id } => transfer_id.clone(),
                };
                if let Ok(actions) = self
                    .transfers
                    .adapter_cancelled(&adapter_id, syntra_plugin_api::Cancelled { transfer_id })
                {
                    self.execute_transfer_actions(actions);
                }
            }
            ManagerEvent::Rejected { adapter, reason } => {
                log::warn!("adapter {:?} rejected transfer: {}", adapter, reason)
            }
            ManagerEvent::Exited { adapter, status } => {
                self.plugins.set_exited(plugin_id(&adapter), status.clone());
                self.publish_plugins();
                let id = match adapter {
                    ProcessAdapterId::Gtk => "gtk-clipboard".to_owned(),
                    ProcessAdapterId::Fuse { transfer_id } => transfer_id,
                };
                let actions = self.transfers.adapter_lost(&id);
                self.execute_transfer_actions(actions);
                log::warn!("adapter exited: {}", status);
            }
        }
    }

    pub(super) fn handle_source_result(&mut self, result: SourceReadResult) {
        match result.result {
            Ok((file, Some((offset, data)), digest)) => {
                if let Err(error) = self
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
                        self.transfers
                            .source_chunk(
                                &result.owner,
                                result.file_id,
                                result.request_id,
                                offset,
                                data,
                            )
                            .map(|actions| self.execute_transfer_actions(actions))
                    })
                {
                    log::warn!("source read failed: {error}");
                    return;
                }
                if let Some(digest) = digest {
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
            Ok(_) => log::warn!("source read returned no data"),
            Err(error) => log::warn!(
                "source read failed for {:?}/{}: {}",
                result.owner,
                result.file_id,
                error
            ),
        }
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
                        AdapterMessage::RemoteManifest(m) => ManagerCommand::RemoteManifest(m),
                        AdapterMessage::RangeResponse(m) => ManagerCommand::RangeResponse(m),
                        AdapterMessage::PublishFileClipboard(m) => {
                            let operation =
                                if matches!(m.operation, syntra_plugin_api::Operation::Move) {
                                    "cut"
                                } else {
                                    "copy"
                                };
                            let uri_list = format!("{}\r\n", m.uris.join("\r\n")).into_bytes();
                            let gnome_files =
                                format!("{operation}\n{}\n", m.uris.join("\n")).into_bytes();
                            self.emulation.publish_file_clipboard(vec![
                                (syntra_plugin_api::URI_LIST_MIME.to_string(), uri_list),
                                (
                                    syntra_plugin_api::GNOME_COPIED_FILES_MIME.to_string(),
                                    gnome_files,
                                ),
                            ]);
                            continue;
                        }
                        AdapterMessage::Released(m) => ManagerCommand::Released(m),
                        AdapterMessage::Unmounted(m) => ManagerCommand::Unmounted(m),
                        AdapterMessage::Cancel { transfer_id } => {
                            let adapter = if adapter_id == "gtk-clipboard" {
                                ProcessAdapterId::Gtk
                            } else {
                                ProcessAdapterId::Fuse {
                                    transfer_id: adapter_id.clone(),
                                }
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
