//! Capture and emulation event handling.
//!
//! The hot path. Capture events are routed to the peer owning the edge that
//! was crossed; emulation events arrive from peers and are replayed locally.

use super::*;

impl Service {
    pub(super) async fn handle_emulation_event(&mut self, event: EmulationEvent) {
        match event {
            EmulationEvent::ConnectionAttempt { fingerprint } => {
                self.notify_frontend(FrontendEvent::ConnectionAttempt { fingerprint });
            }
            EmulationEvent::Entered {
                addr,
                pos,
                fingerprint,
            } => {
                if !self.input_sharing {
                    self.emulation.send_leave_event(addr);
                    return;
                }
                // check if already registered
                if !self.incoming_conns.contains(&addr) {
                    self.add_incoming(addr, pos, fingerprint.clone());
                    self.notify_frontend(FrontendEvent::DeviceEntered {
                        fingerprint,
                        addr,
                        pos,
                    });
                } else {
                    self.update_incoming(addr, pos, fingerprint);
                }
            }
            EmulationEvent::Disconnected { addr } => {
                self.history_peer_lost(Peer::Emulation(addr));
                if let Some(addr) = self.remove_incoming(addr) {
                    self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
                }
            }
            EmulationEvent::PortChanged(port) => match port {
                Ok(port) => {
                    if let Some(discovery) = self.discovery.as_mut() {
                        if let Err(error) = discovery.change_port(port) {
                            self.notify_frontend(FrontendEvent::Error(format!(
                                "mDNS advertisement failed: {error}"
                            )));
                        }
                    }
                    self.port = port;
                    self.config.set_port(port);
                    self.save_config();
                    self.notify_frontend(FrontendEvent::PortChanged(port, None));
                }
                Err(e) => self
                    .notify_frontend(FrontendEvent::PortChanged(self.port, Some(format!("{e}")))),
            },
            EmulationEvent::EmulationDisabled => {
                self.emulation_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::EmulationEnabled(backend) => {
                self.emulation_status = Status::Enabled;
                self.emulation_backend = Some(backend);
                self.publish_daemon_info();
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::ReleaseNotify => self.capture.release(),
            EmulationEvent::Connected { addr, fingerprint } => {
                self.authenticated_peer_fingerprints
                    .insert(addr, fingerprint.clone());
                self.refresh_manual_routes();
                self.history_connected_peers.insert(Peer::Emulation(addr));
                self.notify_frontend(FrontendEvent::DeviceConnected {
                    addr,
                    fingerprint: fingerprint.clone(),
                });
                self.request_profile(fingerprint, Peer::Emulation(addr));
                self.request_history_sync(Peer::Emulation(addr), 0);
            }
            EmulationEvent::PeerHello { addr, commit } => {
                // Map the peer's source addr back to its client handle
                // and stamp the commit. Skip if we don't have an
                // outgoing client configured for this peer (incoming-
                // only setup) — there's nowhere to display the version
                // in that case anyway.
                if let Some(handle) = self.client_manager.get_client(addr) {
                    self.client_manager.set_peer_commit(handle, Some(commit));
                    self.broadcast_client(handle);
                }
            }
            EmulationEvent::Clipboard { content } => {
                // Live clipboard delivery and durable history reconciliation are separate:
                // applying the live value must not allocate a second local-origin record.
                let history_content = match &content {
                    ClipboardContent::Text(text) => Some(history::text(text.clone())),
                    ClipboardContent::Image {
                        width,
                        height,
                        rgba,
                    } => Some(history::image(*width, *height, rgba.clone())),
                };
                if let Some(content) = history_content {
                    self.pending_remote_history_echoes.push_back(content);
                    if self.pending_remote_history_echoes.len() > 8 {
                        self.pending_remote_history_echoes.pop_front();
                    }
                }
                match &content {
                    ClipboardContent::Text(text) if self.clipboard_settings.text => {
                        self.emulation.publish_file_clipboard(vec![
                            (
                                "text/plain;charset=utf-8".to_string(),
                                text.as_bytes().to_vec(),
                            ),
                            ("text/plain".to_string(), text.as_bytes().to_vec()),
                        ]);
                    }
                    _ => self.clipboard.write(content),
                }
            }
            EmulationEvent::NativeClipboard(ClipboardContent::Text(text))
                if self.clipboard_settings.text =>
            {
                self.native_file_selection = false;
                let content = history::text(text.clone());
                if let Some(index) = self
                    .pending_remote_history_echoes
                    .iter()
                    .position(|queued| queued == &content)
                {
                    self.pending_remote_history_echoes.remove(index);
                    return;
                }
                self.record_local_history(content).await;
                for handle in self.client_manager.clipboard_clients() {
                    self.next_clipboard_transfer = self.next_clipboard_transfer.wrapping_add(1);
                    self.capture.send_clipboard(
                        handle,
                        self.next_clipboard_transfer,
                        text.as_bytes().to_vec(),
                        None,
                    );
                }
            }
            EmulationEvent::NativeClipboard(ClipboardContent::Image {
                width,
                height,
                rgba,
            }) if self.clipboard_settings.image => {
                self.native_file_selection = false;
                let content = history::image(width, height, rgba.clone());
                if let Some(index) = self
                    .pending_remote_history_echoes
                    .iter()
                    .position(|queued| queued == &content)
                {
                    self.pending_remote_history_echoes.remove(index);
                    return;
                }
                self.record_local_history(content).await;
                for handle in self.client_manager.clipboard_clients() {
                    self.next_clipboard_transfer = self.next_clipboard_transfer.wrapping_add(1);
                    self.capture.send_clipboard(
                        handle,
                        self.next_clipboard_transfer,
                        rgba.clone(),
                        Some((width, height)),
                    );
                }
            }
            EmulationEvent::NativeClipboard(_) => {
                self.native_file_selection = false;
            }
            EmulationEvent::FileClipboard { mime_type, value } => {
                self.native_file_selection = true;
                self.handle_native_file_clipboard(mime_type, value);
            }
            EmulationEvent::ClipboardProtocol { addr, event } => {
                self.handle_peer_protocol(Peer::Emulation(addr), event)
                    .await;
            }
        }
    }

    pub(super) async fn handle_capture_event(&mut self, event: ICaptureEvent) {
        // Capture events may already be queued when a configured client is deleted.
        // Handles are never reused, so a removed route cannot authenticate a new peer.
        let configured_handle = match &event {
            ICaptureEvent::Clipboard { handle, .. }
            | ICaptureEvent::PeerAuthenticated { handle, .. }
            | ICaptureEvent::FileClipboard { handle, .. }
            | ICaptureEvent::PeerStateChanged(handle)
            | ICaptureEvent::ClientEntered(handle) => Some(*handle),
            _ => None,
        };
        if configured_handle.is_some_and(|handle| !self.client_manager.contains(handle)) {
            return;
        }
        match event {
            ICaptureEvent::Clipboard {
                handle,
                fingerprint,
                event,
            } => {
                self.authenticated_capture_fingerprints
                    .insert(handle, fingerprint);
                self.refresh_manual_routes();
                self.handle_peer_protocol(Peer::Capture(handle), event)
                    .await;
            }
            ICaptureEvent::PeerAuthenticated {
                handle,
                fingerprint,
            } => {
                let changed =
                    self.authenticated_capture_fingerprints.get(&handle) != Some(&fingerprint);
                self.authenticated_capture_fingerprints
                    .insert(handle, fingerprint.clone());
                self.refresh_manual_routes();
                if changed {
                    if self.client_manager.peer_fingerprint(handle).as_ref() != Some(&fingerprint) {
                        self.client_manager
                            .set_peer_fingerprint(handle, fingerprint.clone());
                        self.save_config();
                    }
                    self.notify_frontend(FrontendEvent::ClientFingerprint {
                        handle,
                        fingerprint: fingerprint.clone(),
                    });
                    self.request_profile(fingerprint, Peer::Capture(handle));
                }
            }
            ICaptureEvent::FileClipboard {
                handle,
                mime_type,
                value,
            } => {
                if Self::is_own_clipboard_mount(&value) {
                    log::debug!("ignoring clipboard echo of our own mount");
                    return;
                }
                log::info!(
                    "native clipboard selection routed from capture handle={handle} mime={mime_type}"
                );
                if let Some(manager) = self.adapter_manager.as_ref() {
                    let _ = manager.try_send(ManagerCommand::ClipboardData {
                        transfer_id: format!("portal-{handle}"),
                        mime_type,
                        value,
                    });
                }
            }
            ICaptureEvent::PeerLost(handle) => {
                // The transport died: every transfer bound to it can never
                // complete, so drop it instead of leaving it stalled.
                let actions = self.transfers.peer_lost(&Peer::Capture(handle));
                self.history_peer_lost(Peer::Capture(handle));
                if !actions.is_empty() {
                    log::info!(
                        "peer {handle} lost: dropping {} transfer action(s)",
                        actions.len()
                    );
                    self.execute_transfer_actions(actions);
                }
            }
            ICaptureEvent::PeerStateChanged(handle) => {
                self.broadcast_client(handle);
                if self.client_manager.alive(handle) {
                    if self.history_connected_peers.insert(Peer::Capture(handle)) {
                        self.request_history_sync(Peer::Capture(handle), 0);
                    }
                } else {
                    self.history_peer_lost(Peer::Capture(handle));
                }
            }
            ICaptureEvent::CaptureBegin(handle) => {
                // we entered the capture zone for an incoming connection
                // => notify it that its capture should be released
                if let Some(incoming) = self.incoming_conn_info.get(&handle) {
                    self.emulation.send_leave_event(incoming.addr);
                }
            }
            ICaptureEvent::CaptureDisabled => {
                self.capture_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
                self.emulation.set_capture_ready(false);
            }
            ICaptureEvent::CaptureEnabled(backend) => {
                self.capture_status = Status::Enabled;
                self.capture_backend = Some(backend);
                self.publish_daemon_info();
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
                self.emulation.set_capture_ready(true);
            }
            ICaptureEvent::ClientEntered(handle) => {
                if self.input_sharing {
                    log::info!("entering client {handle} ...");
                    self.spawn_hook_command(handle);
                    if self.legacy_clipboard {
                        self.clipboard.read_once();
                    }
                }
            }
        }
    }
}
