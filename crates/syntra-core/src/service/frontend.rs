//! Client-facing request handling and event publication.
//!
//! Everything a dashboard, the CLI or a plugin can ask for arrives here and
//! leaves as an authoritative event. The daemon never lets a client predict
//! an outcome: it applies the change, then reports what happened.

use super::*;
/// Infers how this process was started.
///
/// systemd exports `INVOCATION_ID` to the units it starts, and its
/// `NOTIFY_SOCKET` is present for notify-type units. Absent those, a parent
/// named after the dashboard means the dashboard spawned it. Everything else
/// is a hand-started process.
fn daemon_origin() -> syntra_api::DaemonOrigin {
    use syntra_api::DaemonOrigin;
    if std::env::var_os("INVOCATION_ID").is_some() || std::env::var_os("NOTIFY_SOCKET").is_some() {
        return DaemonOrigin::Service;
    }
    #[cfg(target_os = "linux")]
    {
        let parent = std::os::unix::process::parent_id();
        if let Ok(command) = std::fs::read_to_string(format!("/proc/{parent}/comm")) {
            if command.trim() == "syntra" {
                return DaemonOrigin::Dashboard;
            }
        }
    }
    DaemonOrigin::Manual
}

impl Service {
    pub(super) async fn handle_frontend_request(
        &mut self,
        request: Option<Result<FrontendRequest, IpcError>>,
    ) -> bool {
        let request = match request.expect("frontend listener closed") {
            Ok(r) => r,
            Err(e) => {
                log::error!("error receiving request: {e}");
                return false;
            }
        };
        match request {
            FrontendRequest::Sync => self.sync_frontend(),
            FrontendRequest::StopService => return true,
            FrontendRequest::Activate(handle, active) => {
                self.set_client_active(handle, active);
                if active && self.input_sharing {
                    self.reenable_missing_backends();
                }
                self.save_config();
            }
            FrontendRequest::AuthorizeKey(desc, fp) => {
                self.add_authorized_key(desc, fp);
                self.save_config();
            }
            FrontendRequest::ChangePort(port) => self.change_port(port),
            FrontendRequest::DiscoverPeers => {
                if let Some(discovery) = self.discovery.as_ref() {
                    if let Err(error) = discovery.refresh() {
                        self.notify_frontend(FrontendEvent::Error(format!(
                            "mDNS discovery refresh failed: {error}"
                        )));
                    }
                } else {
                    self.notify_frontend(FrontendEvent::Error("mDNS discovery unavailable".into()));
                }
            }
            FrontendRequest::Create => {
                self.add_client();
                self.save_config();
            }
            FrontendRequest::Delete(handle) => {
                self.remove_client(handle);
                self.save_config();
            }
            FrontendRequest::EnableCapture => self.capture.reenable(),
            FrontendRequest::EnableEmulation => self.emulation.reenable(),
            FrontendRequest::SetInputSharing(enabled) => {
                self.input_sharing = enabled;
                self.capture.set_input_sharing(enabled);
                self.emulation.set_input_sharing(enabled);
                if enabled {
                    self.reenable_missing_backends();
                }
                self.notify_frontend(FrontendEvent::InputSharing(enabled));
            }
            FrontendRequest::Enumerate() => self.enumerate(),
            FrontendRequest::UpdateFixIps(handle, fix_ips) => {
                self.update_fix_ips(handle, fix_ips);
                self.save_config();
            }
            FrontendRequest::UpdateHostname(handle, host) => {
                self.update_hostname(handle, host);
                self.save_config();
            }
            FrontendRequest::UpdatePort(handle, port) => {
                self.update_port(handle, port);
                self.save_config();
            }
            FrontendRequest::UpdatePosition(handle, pos) => {
                self.update_pos(handle, pos);
                self.save_config();
            }
            FrontendRequest::ResolveDns(handle) => self.resolve(handle),
            FrontendRequest::SetLocalDeviceProfile(profile) => {
                if let Err(error) = profile.validate() {
                    self.notify_frontend(FrontendEvent::Error(error.into()));
                } else {
                    if self.local_device_profile != profile {
                        match peer_profile::save_cached(
                            self.config.config_path(),
                            &self.public_key_fingerprint,
                            &profile,
                        )
                        .await
                        {
                            Ok(()) => {
                                self.local_device_profile = profile;
                                for (_, peer) in self.connected_authenticated_peers() {
                                    self.send_peer(peer, syntra_proto::ProtoEvent::ProfileChanged);
                                }
                            }
                            Err(error) => self.notify_frontend(FrontendEvent::Error(format!(
                                "Cannot save device profile: {error}"
                            ))),
                        }
                    }
                }
            }
            FrontendRequest::RegenerateIdentity => {
                let path = self.config.cert_path().to_path_buf();
                match tokio::task::spawn_blocking(move || crypto::regenerate_key_and_cert(&path))
                    .await
                {
                    Ok(Ok(fingerprint)) => {
                        self.notify_frontend(FrontendEvent::IdentityRegenerated { fingerprint });
                    }
                    Ok(Err(error)) => self.notify_frontend(FrontendEvent::Error(error.to_string())),
                    Err(error) => self.notify_frontend(FrontendEvent::Error(format!(
                        "identity regeneration failed: {error}"
                    ))),
                }
            }
            FrontendRequest::RemoveAuthorizedKey(key) => {
                self.remove_authorized_key(key);
                self.save_config();
            }
            FrontendRequest::UpdateEnterHook(handle, enter_hook) => {
                self.update_enter_hook(handle, enter_hook)
            }
            FrontendRequest::SetClipboardText(value) => {
                self.clipboard_settings.text = value;
                self.config.set_clipboard_settings(self.clipboard_settings);
                self.save_config();
                self.notify_frontend(FrontendEvent::ClipboardSettings(self.clipboard_settings));
            }
            FrontendRequest::SetClipboardImage(value) => {
                self.clipboard_settings.image = value;
                self.config.set_clipboard_settings(self.clipboard_settings);
                self.save_config();
                self.notify_frontend(FrontendEvent::ClipboardSettings(self.clipboard_settings));
            }
            FrontendRequest::SetClipboardFiles(value) => {
                self.clipboard_settings.files = value;
                self.config.set_clipboard_settings(self.clipboard_settings);
                self.save_config();
                self.notify_frontend(FrontendEvent::ClipboardSettings(self.clipboard_settings));
            }
            FrontendRequest::CancelClipboardTransfer(transfer_id) => {
                log::info!("cancelling clipboard transfer {transfer_id}");
                match self.transfers.cancel_ui(
                    transfer_id,
                    None,
                    syntra_proto::ClipboardCancelReason::User,
                ) {
                    Ok(actions) => self.execute_transfer_actions(actions),
                    Err(error) => log::warn!("cannot cancel transfer {transfer_id}: {error}"),
                }
            }
            FrontendRequest::QueryHistory {
                query,
                offset,
                limit,
            } => {
                match self
                    .history
                    .page(query.clone(), offset, usize::from(limit))
                    .await
                {
                    Ok(page) => self.notify_frontend(FrontendEvent::HistoryPage(
                        history::ipc_page(query, offset, page),
                    )),
                    Err(error) => self.notify_frontend(FrontendEvent::HistoryError(error)),
                }
            }
            FrontendRequest::SetHistoryPinned { event_id, pinned } => {
                let event_id = history::store_id(event_id);
                match self.history.set_pinned(event_id.clone(), pinned).await {
                    Ok(updated) => self.notify_frontend(FrontendEvent::HistoryPinResult {
                        event_id: history::ipc_id(event_id),
                        pinned,
                        updated,
                    }),
                    Err(error) => self.notify_frontend(FrontendEvent::HistoryError(error)),
                }
            }
            FrontendRequest::GetHistoryImage(event_id) => {
                let store_id = history::store_id(event_id.clone());
                match self.history.image(store_id).await {
                    Ok(image) => self.notify_frontend(FrontendEvent::HistoryImageResult {
                        event_id: event_id.clone(),
                        image: image.map(|image| history::ipc_image(event_id, image)),
                        error: None,
                    }),
                    Err(error) => self.notify_frontend(FrontendEvent::HistoryImageResult {
                        event_id,
                        image: None,
                        error: Some(error),
                    }),
                }
            }
            FrontendRequest::ClearGlobalHistory => {
                self.start_global_history_clear().await;
            }
            FrontendRequest::SendFiles {
                peer_fingerprint,
                paths,
            } => {
                let peer = self
                    .connected_authenticated_peers()
                    .get(&peer_fingerprint)
                    .copied();
                if let Some(peer) = peer {
                    let actions = self
                        .manual_transfers
                        .send_files(peer, peer_fingerprint, paths, std::time::Instant::now())
                        .await;
                    self.execute_manual_actions(actions);
                } else {
                    self.notify_frontend(FrontendEvent::ManualTransferError(
                        "The selected device is no longer connected".into(),
                    ));
                }
            }
            FrontendRequest::AcceptFileTransfer {
                peer_fingerprint,
                transfer_id,
                destination_directory,
            } => {
                let actions = self
                    .manual_transfers
                    .accept(
                        &peer_fingerprint,
                        transfer_id,
                        destination_directory,
                        std::time::Instant::now(),
                    )
                    .await;
                self.execute_manual_actions(actions);
            }
            FrontendRequest::DeclineFileTransfer {
                peer_fingerprint,
                transfer_id,
            } => {
                let actions = self
                    .manual_transfers
                    .decline(&peer_fingerprint, transfer_id);
                self.execute_manual_actions(actions);
            }
            FrontendRequest::CancelManualTransfer {
                peer_fingerprint,
                transfer_id,
            } => {
                let actions = self.manual_transfers.cancel(&peer_fingerprint, transfer_id);
                self.execute_manual_actions(actions);
            }
            FrontendRequest::SetFileReceiveSettings(settings) => {
                if !settings.download_directory.is_absolute() {
                    self.notify_frontend(FrontendEvent::FileReceiveSettingsChanged(
                        self.file_receive_settings.clone(),
                        Some("Download directory must be an absolute path".into()),
                    ));
                } else {
                    match self.config.persist_file_receive_settings(&settings) {
                        Ok(()) => {
                            self.file_receive_settings = settings;
                            self.notify_frontend(FrontendEvent::FileReceiveSettingsChanged(
                                self.file_receive_settings.clone(),
                                None,
                            ));
                        }
                        Err(error) => {
                            self.notify_frontend(FrontendEvent::FileReceiveSettingsChanged(
                                self.file_receive_settings.clone(),
                                Some(format!("Cannot save file receive settings: {error}")),
                            ));
                        }
                    }
                }
            }
            FrontendRequest::SaveConfiguration => self.save_config(),
            FrontendRequest::SetLogSpec(spec) => {
                let updated = syntra_log::LogConfig::parse(&spec);
                self.log_config.set_level(updated.level());
                // Clear first: an override the client dropped must stop
                // applying, otherwise levels could only ever be added.
                for subsystem in syntra_log::Subsystem::ALL {
                    self.log_config.set_subsystem_level(subsystem, None);
                }
                for (subsystem, level) in updated.overrides() {
                    self.log_config.set_subsystem_level(subsystem, Some(level));
                }
                log::info!("log configuration set to `{}`", self.log_config.to_spec());
                self.notify_frontend(FrontendEvent::LogSpec(self.log_config.to_spec()));
            }
            FrontendRequest::QueryLogSpec => {
                self.notify_frontend(FrontendEvent::LogSpec(self.log_config.to_spec()));
            }
            FrontendRequest::QueryDaemonInfo => self.publish_daemon_info(),
            FrontendRequest::QueryPlugins => self.publish_plugins(),
            FrontendRequest::SetPluginEnabled { id, enabled } => {
                if self.plugins.set_enabled(&id, enabled) {
                    log::info!(
                        "plugin `{id}` {}",
                        if enabled { "enabled" } else { "disabled" }
                    );
                } else {
                    log::warn!("ignoring request for unknown plugin `{id}`");
                }
                // Reply either way: the client must never be left showing a
                // state the daemon did not accept.
                self.publish_plugins();
            }
            FrontendRequest::RestartPlugin { id } => {
                if self.plugins.executable(&id).is_some() {
                    self.plugins.record_restart(&id);
                    log::info!("restarting plugin `{id}` on request");
                } else {
                    log::warn!("ignoring restart for unknown plugin `{id}`");
                }
                self.publish_plugins();
            }
        }
        false
    }

    /// Sends the authoritative plugin list to every attached client.
    pub(super) fn publish_plugins(&mut self) {
        let snapshot = self.plugins.snapshot();
        self.notify_frontend(FrontendEvent::Plugins(snapshot));
    }

    /// Reports which daemon is answering and how it was started.
    ///
    /// A user seeing only "running" cannot tell whether the daemon they
    /// installed is the one responding, or whether a dashboard quietly
    /// started its own.
    pub(super) fn publish_daemon_info(&mut self) {
        let info = syntra_api::DaemonInfo {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            executable: std::env::current_exe()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|error| format!("unknown ({error})")),
            pid: std::process::id(),
            socket: syntra_api::paths::daemon_socket()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|error| error.to_string()),
            config_dir: syntra_api::paths::config_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|error| error.to_string()),
            uptime_seconds: self.started_at.elapsed().as_secs(),
            origin: daemon_origin(),
            capture_backend: self.capture_backend.clone(),
            emulation_backend: self.emulation_backend.clone(),
            port: self.port,
        };
        self.notify_frontend(FrontendEvent::DaemonInfo(info));
    }

    pub(super) fn save_config(&mut self) {
        let clients = self
            .client_manager
            .get_client_states()
            .into_iter()
            .map(|(handle, c, s)| ConfigClient {
                ips: HashSet::from_iter(c.fix_ips),
                hostname: c.hostname,
                port: c.port,
                pos: c.pos,
                active: s.active,
                enter_hook: c.cmd,
                peer_fingerprint: self.client_manager.peer_fingerprint(handle),
            })
            .collect();
        self.config.set_clients(clients);
        let authorized_keys = self.authorized_keys.read().expect("lock").clone();
        self.config.set_authorized_keys(authorized_keys);
        if let Err(e) = self.config.write_back() {
            log::warn!("failed to write config: {e}");
        }
    }

    pub(super) fn handle_config_change(&mut self) {
        for h in self.client_manager.registered_clients() {
            self.remove_client(h);
        }
        for c in self.config.clients() {
            let handle = self.client_manager.add_with_config(c);
            log::info!("added client {handle}");
            let (c, s) = self.client_manager.get_state(handle).unwrap();
            if s.active {
                self.client_manager.deactivate_client(handle);
                self.activate_client(handle);
            }
            self.notify_frontend(FrontendEvent::Created(handle, c, s));
        }
        let release_bind = self.config.release_bind();
        self.capture.set_release_bind(release_bind);
        let authorized_keys = self.config.authorized_fingerprints();
        self.authorized_keys
            .write()
            .unwrap()
            .clone_from(&authorized_keys);
        self.sync_frontend();
    }

    pub(super) async fn handle_frontend_pending(&mut self) {
        while let Some(event) = self.pending_frontend_events.pop_front() {
            self.frontend_listener.broadcast(event).await;
        }
    }

    pub(super) fn notify_frontend(&mut self, event: FrontendEvent) {
        self.pending_frontend_events.push_back(event);
        self.frontend_event_pending.notify_one();
    }

    pub(super) fn sync_frontend(&mut self) {
        self.enumerate();
        self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
        self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
        self.notify_frontend(FrontendEvent::InputSharing(self.input_sharing));
        self.notify_frontend(FrontendEvent::ClipboardSettings(self.clipboard_settings));
        self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        self.notify_frontend(FrontendEvent::PublicKeyFingerprint(
            self.public_key_fingerprint.clone(),
        ));
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
        for handle in self.client_manager.registered_clients() {
            if let Some(fingerprint) = self.client_manager.peer_fingerprint(handle) {
                self.notify_frontend(FrontendEvent::ClientFingerprint {
                    handle,
                    fingerprint,
                });
            }
        }
        for (fingerprint, profile) in self.peer_profiles.clone() {
            self.notify_frontend(FrontendEvent::PeerDeviceProfile {
                fingerprint,
                profile,
            });
        }
        self.notify_frontend(FrontendEvent::FileReceiveSettingsChanged(
            self.file_receive_settings.clone(),
            None,
        ));
        for status in self.manual_transfers.snapshot() {
            self.notify_frontend(FrontendEvent::ManualTransferStatus(status));
        }
        for offer in self.manual_transfers.pending_offers() {
            self.notify_frontend(FrontendEvent::IncomingFileOffer(offer));
        }
    }

    pub(super) fn enumerate(&mut self) {
        let clients = self.client_manager.get_client_states();
        self.notify_frontend(FrontendEvent::Enumerate(clients));
    }

    pub(super) fn add_authorized_key(&mut self, desc: String, fp: String) {
        self.authorized_keys.write().expect("lock").insert(fp, desc);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    pub(super) fn remove_authorized_key(&mut self, fp: String) {
        self.authorized_keys.write().expect("lock").remove(&fp);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }
}
