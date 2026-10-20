//! Client-facing request handling and event publication.
//!
//! Everything a dashboard, the CLI or a plugin can ask for arrives here and
//! leaves as an authoritative event. The daemon never lets a client predict
//! an outcome: it applies the change, then reports what happened.

use super::*;
use syntra_plugin_api::PluginRole as ManifestRole;

impl Service {
    /// Launches a resident plugin if, and only if, the registry allows it.
    ///
    /// The registry decides: the plugin must be enabled, its executable must
    /// run here and its content must be approved (or it must be bundled). The
    /// content digest is recomputed for every launch, so a file changed after
    /// approval stays stopped. A refusal is recorded against the plugin with a
    /// reason the user can act on, never swallowed.
    ///
    /// `restart` first stops a copy that is already running, so a restart of a
    /// stuck plugin does not leave two.
    pub(super) fn start_resident_plugin(
        &mut self,
        id: &str,
        role: Option<ManifestRole>,
        restart: bool,
    ) {
        let plan = match self.plugins.launch_plan(id, role) {
            Ok(plan) => plan,
            Err(refusal) => {
                log::info!("plugin `{id}` not started: {}", refusal.text());
                if matches!(
                    refusal,
                    crate::plugins::registry::Refusal::Unknown
                        | crate::plugins::registry::Refusal::Disabled
                ) {
                    return;
                }
                // Disabled and unknown plugins are silent; anything else the
                // user can fix is shown on the plugin's entry.
                self.plugins.note_result(id, &Err(refusal.text()));
                return;
            }
        };
        if restart {
            self.send_plugin_command(ManagerCommand::StopPlugin { id: id.to_owned() });
        }
        self.plugins.note_result(id, &Ok(()));
        self.send_plugin_command(ManagerCommand::StartPlugin {
            id: id.to_owned(),
            spec: plan.spec,
            settings: plan.settings,
        });
    }

    /// Brings the running processes in line with what the registry allows.
    ///
    /// Idempotent and cheap: it starts the active provider of each resident
    /// role and stops resident plugins that should no longer run (switched
    /// off, approval withdrawn, or no longer the active provider). Called
    /// after every lifecycle change, so a role never ends with two providers
    /// or with a provider nobody approved.
    pub(super) fn reconcile_plugins(&mut self) {
        let wanted = self.plugins.resident_plugins();
        let running = self.plugins.running_plugins();
        for id in &running {
            if !wanted.iter().any(|(wanted_id, _)| wanted_id == id)
                && !self.plugins.is_on_demand(id)
            {
                self.send_plugin_command(ManagerCommand::StopPlugin { id: id.clone() });
            }
        }
        for (id, role) in wanted {
            if !running.contains(&id) {
                self.start_resident_plugin(&id, Some(role), false);
            }
        }
        self.sync_file_clipboard_provider();
    }

    /// The plugin that provides the file clipboard now, if any.
    ///
    /// Chosen by the registry from capabilities and approval: the user's
    /// explicit preference, else a bundled provider, else the smallest id.
    /// `None` is a normal state; the capability is then unavailable and
    /// nothing else is affected.
    pub(super) fn file_clipboard_provider(&self) -> Option<String> {
        self.plugins.active_provider(ManifestRole::FileClipboard)
    }

    /// Keeps the transfer state machine pointed at the provider in force.
    ///
    /// Called whenever the set of providers can have changed. Transfers
    /// already published stay keyed by the provider they were published to.
    pub(super) fn sync_file_clipboard_provider(&mut self) {
        let provider = self.file_clipboard_provider().unwrap_or_default();
        if self.transfers.file_clipboard_adapter() != provider {
            self.transfers.set_file_clipboard_adapter(provider);
        }
    }

    /// Starts a process to serve one incoming transfer through a mounted view.
    ///
    /// The plugin is chosen by role, exactly as the file clipboard is, and
    /// goes through the same launch gate. Returns `false` with no provider, or
    /// one that is refused, so the caller can report the transfer lost and
    /// the peer is told rather than left waiting.
    pub(super) fn start_mounted_receive(
        &mut self,
        manifest: syntra_plugin_api::RemoteManifest,
    ) -> bool {
        let Some(provider) = self
            .plugins
            .active_provider(ManifestRole::MountedFileReceive)
        else {
            log::warn!("no plugin can receive files through a mount; transfer cancelled");
            return false;
        };
        let plan = match self
            .plugins
            .launch_plan(&provider, Some(ManifestRole::MountedFileReceive))
        {
            Ok(plan) => plan,
            Err(refusal) => {
                log::warn!(
                    "mounted receive provider `{provider}` not started: {}",
                    refusal.text()
                );
                self.plugins.note_result(&provider, &Err(refusal.text()));
                self.publish_plugins();
                return false;
            }
        };
        self.transfer_providers
            .insert(manifest.transfer_id.clone(), provider.clone());
        self.send_plugin_command(ManagerCommand::StartTransfer {
            id: provider,
            spec: plan.spec,
            manifest,
        });
        true
    }

    /// Tells every enabled pointer plugin a pointer crossed an edge.
    ///
    /// Sent per crossing, never per motion, so it costs nothing while the
    /// pointer is simply moving. A plugin that is not running, or is busy,
    /// misses the event rather than holding it up: the supervisor drops it.
    pub(super) fn forward_pointer_to_plugins(&mut self, passage: &PointerPassage, entered: bool) {
        use syntra_plugin_api::{Edge, PointerEvent};
        let ids = self.plugins.pointer_plugins();
        log::info!(
            "[plugin] pointer {} through the {:?} edge -> {} plugin(s) {:?}",
            if entered { "entered" } else { "left" },
            passage.edge,
            ids.len(),
            ids
        );
        if ids.is_empty() {
            return;
        }
        let edge = match passage.edge {
            Position::Left => Edge::Left,
            Position::Right => Edge::Right,
            Position::Top => Edge::Top,
            Position::Bottom => Edge::Bottom,
        };
        let event = if entered {
            PointerEvent::Entered {
                edge,
                along: passage.along,
                peer: passage.peer.clone(),
            }
        } else {
            PointerEvent::Left {
                edge,
                along: passage.along,
                peer: passage.peer.clone(),
            }
        };
        for id in ids {
            self.send_plugin_command(ManagerCommand::PluginPointer {
                id,
                event: event.clone(),
            });
        }
    }

    /// Tells every enabled pointer plugin how hard an edge is being pushed.
    ///
    /// `amount` runs from 0.0 to 1.0 and is 0.0 once the push is over. The
    /// capture side already limits how often this is called, and the
    /// supervisor drops it for a plugin that cannot take it, so a burst can
    /// never queue up behind a slow plugin.
    pub(super) fn forward_pressure_to_plugins(
        &mut self,
        edge: Position,
        along: Option<f32>,
        amount: f32,
    ) {
        use syntra_plugin_api::{Edge, PointerEvent};
        let ids = self.plugins.pointer_plugins();
        if ids.is_empty() {
            return;
        }
        let edge = match edge {
            Position::Left => Edge::Left,
            Position::Right => Edge::Right,
            Position::Top => Edge::Top,
            Position::Bottom => Edge::Bottom,
        };
        for id in ids {
            self.send_plugin_command(ManagerCommand::PluginPointer {
                id,
                event: PointerEvent::Pressure {
                    edge,
                    along,
                    amount,
                },
            });
        }
    }
}

use std::time::Instant;
/// Infers how this process was started.
///
/// Android always starts the service inside its dashboard process. Elsewhere,
/// systemd exports `INVOCATION_ID` to the units it starts, and its
/// `NOTIFY_SOCKET` is present for notify-type units. Absent those, a parent
/// named after the dashboard means the dashboard spawned it. Everything else
/// is a hand-started process.
fn daemon_origin() -> syntra_api::DaemonOrigin {
    use syntra_api::DaemonOrigin;
    #[cfg(target_os = "android")]
    {
        DaemonOrigin::Dashboard
    }
    #[cfg(not(target_os = "android"))]
    {
        if std::env::var_os("INVOCATION_ID").is_some()
            || std::env::var_os("NOTIFY_SOCKET").is_some()
        {
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
}

impl Service {
    pub(super) async fn handle_frontend_request(
        &mut self,
        request: Option<Result<FrontendRequest, IpcError>>,
    ) -> bool {
        let Some(request) = request else {
            log::error!("frontend listener closed; stopping the service");
            return true;
        };
        let request = match request {
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
            FrontendRequest::CreateDiscovered { addresses, port } => {
                self.add_discovered_client(addresses, port);
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
            FrontendRequest::SetIndependentPointers(enabled) => {
                #[cfg(target_os = "linux")]
                if enabled {
                    if let Some(problem) = crate::gnome_guard::independent_pointer_blocker() {
                        log::warn!("independent pointers refused: {problem}");
                        // Keep the request: it takes effect by itself when the
                        // service next starts under a GNOME Shell that has the
                        // setting, instead of asking the user to enable it
                        // again after the login.
                        self.config.set_independent_pointers(true);
                        self.save_config();
                        self.notify_frontend(FrontendEvent::Error(problem));
                        self.notify_frontend(FrontendEvent::IndependentPointers(false));
                        return false;
                    }
                }
                self.config.set_independent_pointers(enabled);
                self.emulation.set_independent_pointers(enabled);
                #[cfg(target_os = "linux")]
                self.capture.set_separate_peer_pointers(
                    enabled && crate::gnome_guard::peers_have_own_cursor(),
                );
                self.save_config();
                self.notify_frontend(FrontendEvent::IndependentPointers(enabled));
            }
            FrontendRequest::SetMultiHop(enabled) => {
                self.config.set_multi_hop(enabled);
                self.capture.set_multi_hop(enabled);
                self.emulation.set_multi_hop(enabled);
                self.save_config();
                self.notify_frontend(FrontendEvent::MultiHop(enabled));
            }
            FrontendRequest::SetCaptureBackend(name) => {
                let backend = match name.as_deref().map(CaptureBackend::from_name) {
                    Some(None) => {
                        self.notify_frontend(FrontendEvent::Error(format!(
                            "unknown capture backend {}",
                            name.unwrap_or_default()
                        )));
                        return false;
                    }
                    Some(backend) => backend,
                    None => None,
                };
                log::info!(
                    "capture backend chosen: {}",
                    name.as_deref().unwrap_or("automatic")
                );
                self.config.set_capture_backend(backend);
                self.save_config();
                self.capture.set_backend(backend.map(Into::into));
                self.publish_daemon_info();
            }
            FrontendRequest::SetEmulationBackend(name) => {
                let backend = match name.as_deref().map(EmulationBackend::from_name) {
                    Some(None) => {
                        self.notify_frontend(FrontendEvent::Error(format!(
                            "unknown emulation backend {}",
                            name.unwrap_or_default()
                        )));
                        return false;
                    }
                    Some(backend) => backend,
                    None => None,
                };
                log::info!(
                    "emulation backend chosen: {}",
                    name.as_deref().unwrap_or("automatic")
                );
                self.config.set_emulation_backend(backend);
                self.save_config();
                self.legacy_clipboard = uses_desktop_clipboard(backend);
                self.emulation.set_backend(backend.map(Into::into));
                self.publish_daemon_info();
            }
            FrontendRequest::SetHopBypass(enabled) => {
                self.config.set_hop_bypass(enabled);
                self.emulation.set_hop_bypass(enabled);
                self.save_config();
                self.notify_frontend(FrontendEvent::HopBypass(enabled));
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
            FrontendRequest::GetHistoryContent(event_id) => {
                let result = self
                    .history
                    .content(history::store_id(event_id.clone()))
                    .await;
                let (text, image, error) = match result {
                    Ok(Some(syntra_store::HistoryContent::Text(text))) => (Some(text), None, None),
                    Ok(Some(syntra_store::HistoryContent::Image {
                        bytes: Some(bytes),
                        media_type,
                        width,
                        height,
                    })) => (
                        None,
                        Some(syntra_api::HistoryImage {
                            event_id: event_id.clone(),
                            bytes,
                            media_type,
                            width,
                            height,
                        }),
                        None,
                    ),
                    Ok(_) => (
                        None,
                        None,
                        Some("History content is no longer available".into()),
                    ),
                    Err(error) => (None, None, Some(error)),
                };
                self.notify_frontend(FrontendEvent::HistoryContentResult {
                    event_id,
                    text,
                    image,
                    error,
                });
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
                overwrite,
            } => {
                let now = std::time::Instant::now();
                let actions = if overwrite {
                    self.manual_transfers
                        .overwrite(&peer_fingerprint, transfer_id, destination_directory, now)
                        .await
                } else {
                    self.manual_transfers
                        .accept(&peer_fingerprint, transfer_id, destination_directory, now)
                        .await
                };
                self.execute_manual_actions(actions);
            }
            FrontendRequest::DeclineFileTransfer {
                peer_fingerprint,
                transfer_id,
            } => {
                let actions =
                    self.manual_transfers
                        .decline(&peer_fingerprint, transfer_id, Instant::now());
                self.execute_manual_actions(actions);
            }
            FrontendRequest::CancelManualTransfer {
                peer_fingerprint,
                transfer_id,
            } => {
                let actions =
                    self.manual_transfers
                        .cancel(&peer_fingerprint, transfer_id, Instant::now());
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
                    // Recording the preference is not enough: disabling must
                    // actually stop the process, and enabling must start one,
                    // or the control is a label with no effect. What runs is
                    // decided by the registry from capabilities and approval,
                    // never from the plugin's name.
                    if enabled {
                        self.reconcile_plugins();
                    } else {
                        self.send_plugin_command(ManagerCommand::StopPlugin { id: id.clone() });
                        // Another provider may now be the active one.
                        self.reconcile_plugins();
                    }
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
                if self.plugins.is_enabled(&id) {
                    self.plugins.record_restart(&id);
                    let role = self.plugins.resident_role_of(&id);
                    // Goes through `launch_plan` again: a restart is a launch,
                    // so approval is re-verified against the files now.
                    self.start_resident_plugin(&id, role, true);
                    log::info!("restarting plugin `{id}` on request");
                } else {
                    log::info!("ignoring restart for a disabled or unknown plugin `{id}`");
                }
                self.publish_plugins();
            }
            FrontendRequest::SetPluginSetting { id, key, value } => {
                if self.plugins.owns_settings(&id) {
                    // The plugin keeps and checks its own values; it answers
                    // with what is in force, which reaches the interface. A
                    // change made while it is not running is queued for it.
                    // Until it answers, the requested value is what is shown:
                    // in its last declaration, or before it first ran, in the
                    // manifest's controls.
                    if !self.plugins.request_owned_setting(&id, &key, &value) {
                        self.plugins.set_setting(&id, &key, &value);
                    }
                    log::info!("plugin `{id}`: asking it to set `{key}`");
                    self.send_plugin_command(ManagerCommand::PluginChangeSetting {
                        id: id.clone(),
                        key,
                        value,
                    });
                } else if self.plugins.set_setting(&id, &key, &value) {
                    log::info!("plugin `{id}` setting `{key}` set to `{value}`");
                    // Stored and saved; now handed to the process too, so the
                    // change shows without a restart. A plugin that is not
                    // running is given the stored values when it starts.
                    if self.plugins.is_running(&id) {
                        self.send_plugin_command(ManagerCommand::PluginSettings {
                            settings: self.plugins.settings_of(&id),
                            id: id.clone(),
                        });
                    }
                } else {
                    // Not an error to retry: the value was not one the plugin
                    // declared it understands, or the plugin or key is unknown.
                    log::warn!("ignoring setting `{key}` = `{value}` for plugin `{id}`");
                }
                // Reply either way, so the control reverts if it was refused.
                self.publish_plugins();
            }
            FrontendRequest::OpenPluginSettings { id, theme } => {
                if !self.plugins.owns_settings(&id) {
                    log::warn!("plugin `{id}` has no settings window of its own");
                } else if self.plugins.is_running(&id) {
                    log::info!("plugin `{id}`: opening its settings window");
                    self.send_plugin_command(ManagerCommand::PluginOpenSettings { id, theme });
                } else {
                    // A stopped plugin's window opens on its own, which runs
                    // its code: so it goes through the same gate as any other
                    // launch. An unapproved or changed plugin does not run
                    // just to show a settings window.
                    match self.plugins.launch_plan(&id, None) {
                        Ok(plan) => {
                            log::info!(
                                "plugin `{id}` is not running: opening its settings window alone"
                            );
                            self.send_plugin_command(ManagerCommand::OpenSettingsWindow {
                                id,
                                spec: plan.spec,
                                theme,
                            });
                        }
                        Err(refusal) => {
                            log::info!("plugin `{id}` settings not opened: {}", refusal.text());
                            self.plugins.note_result(&id, &Err(refusal.text()));
                            self.publish_plugins();
                        }
                    }
                }
            }
            FrontendRequest::ReviewPlugin { path } => {
                // Reads and hashes files, so it runs off the event loop. It
                // executes nothing.
                self.review_plugin(std::path::PathBuf::from(path));
            }
            FrontendRequest::InstallPlugin { path } => {
                self.install_plugin(std::path::PathBuf::from(path), None);
            }
            FrontendRequest::UpdatePlugin { id, path } => {
                self.install_plugin(std::path::PathBuf::from(path), Some(id));
            }
            FrontendRequest::ApprovePlugin { id, digest } => {
                self.plugins
                    .mark_pending(&id, syntra_api::PluginPending::Approving);
                let result = self.plugins.approve(&id, &digest);
                if let Err(reason) = &result {
                    log::warn!("approval of `{id}` refused: {reason}");
                }
                self.plugins.note_result(&id, &result);
                // Approving does not switch the plugin on: that stays the
                // user's separate choice. Reconcile only so a role whose
                // provider just became eligible is served.
                self.reconcile_plugins();
                self.publish_plugins();
            }
            FrontendRequest::RevokePluginApproval { id } => {
                self.plugins
                    .mark_pending(&id, syntra_api::PluginPending::Revoking);
                // Stop first: once the approval is gone nothing may keep
                // running on it.
                self.send_plugin_command(ManagerCommand::StopPlugin { id: id.clone() });
                let result = self.plugins.revoke(&id);
                self.plugins.note_result(&id, &result);
                self.reconcile_plugins();
                self.publish_plugins();
            }
            FrontendRequest::UninstallPlugin { id } => {
                self.plugins
                    .mark_pending(&id, syntra_api::PluginPending::Removing);
                self.uninstall_plugin(&id);
                self.publish_plugins();
            }
            FrontendRequest::SetPluginRolePreference { role, id } => {
                let result = self.plugins.set_role_preference(role, id.as_deref());
                if let Err(reason) = &result {
                    log::warn!("role preference refused: {reason}");
                }
                if let Some(id) = &id {
                    self.plugins.note_result(id, &result);
                }
                // The active provider of the role may have changed: the old
                // one stops and the new one starts.
                self.reconcile_plugins();
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

    /// Starts reviewing a package on the blocking pool; the result comes back
    /// as a [`PluginJob`], so nothing in the loop waits on file input.
    pub(super) fn review_plugin(&mut self, source: std::path::PathBuf) {
        let jobs = self.plugin_jobs_tx.clone();
        tokio::task::spawn_blocking(move || {
            let review = crate::plugins::registry::inspect_package(&source);
            let _ = jobs.blocking_send(PluginJob::Reviewed(review));
        });
    }

    /// Installs (or, with `replacing`, updates) a package into Syntra's own
    /// store.
    ///
    /// Installing never runs the plugin and never switches it on or approves
    /// it: the copy lands disabled and unapproved, and the user decides
    /// afterwards, on the digest shown. An update keeps the plugin's settings
    /// (they live apart from the package) but its content is new, so any
    /// earlier approval no longer matches and a fresh one is needed.
    ///
    /// The checks that need the registry happen here; the inspection and the
    /// copy run on the blocking pool and report through [`PluginJob`].
    pub(super) fn install_plugin(&mut self, source: std::path::PathBuf, replacing: Option<String>) {
        use crate::plugins::package_store;
        let Some(store) = self.plugins.store().map(std::path::Path::to_path_buf) else {
            self.plugin_action_failed(
                replacing.as_deref(),
                package_store::StoreError::NoStore.to_string(),
            );
            return;
        };
        if let Some(id) = &replacing {
            if !self.plugins.is_installed_package(id) {
                self.plugin_action_failed(
                    Some(id),
                    "only a plugin installed by Syntra can be updated by Syntra".into(),
                );
                return;
            }
            self.plugins
                .mark_pending(id, syntra_api::PluginPending::Installing);
            // The old process must not outlive its replaced files.
            self.send_plugin_command(ManagerCommand::StopPlugin { id: id.clone() });
            self.publish_plugins();
        }
        let jobs = self.plugin_jobs_tx.clone();
        tokio::task::spawn_blocking(move || {
            let review = crate::plugins::registry::inspect_package(&source);
            let outcome = if !review.valid {
                Err(review
                    .error
                    .clone()
                    .unwrap_or_else(|| "the package is not valid".into()))
            } else if replacing.as_ref().is_some_and(|id| *id != review.id) {
                Err("the package is a different plugin than the one being updated".to_owned())
            } else {
                package_store::install(&source, &store)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            };
            let _ = jobs.blocking_send(PluginJob::Installed {
                id: if review.id.is_empty() {
                    replacing.unwrap_or_default()
                } else {
                    review.id.clone()
                },
                review,
                result: outcome,
            });
        });
    }

    /// Applies the result of plugin work that ran on the blocking pool.
    pub(super) fn finish_plugin_job(&mut self, job: PluginJob) {
        match job {
            PluginJob::Reviewed(review) => {
                self.plugins.record_review(review);
            }
            PluginJob::Installed { id, review, result } => {
                match &result {
                    Ok(()) => {
                        self.plugins.rediscover(
                            &std::env::current_exe().unwrap_or_default(),
                            syntra_api::paths::config_dir()
                                .ok()
                                .map(|directory| directory.join("plugins"))
                                .as_deref(),
                        );
                        self.plugins.forget_review(&id);
                    }
                    Err(reason) => {
                        log::warn!("plugin install refused: {reason}");
                        // A package that is not valid has no plugin to carry
                        // the reason, so it is shown as a candidate with it.
                        if !review.valid && self.plugins.is_unknown(&id) {
                            self.plugins.record_review(review);
                        }
                    }
                }
                if !id.is_empty() {
                    self.plugins.note_result(&id, &result);
                }
                self.reconcile_plugins();
            }
        }
        self.publish_plugins();
    }

    /// Removes a package Syntra installed, and nothing else.
    ///
    /// A drop-in plugin and a bundled one are refused: the first belongs to the
    /// user, the second to the distribution. The process stops first so no
    /// running file is deleted, and the plugin's approval and role
    /// preferences go with it. Its saved settings are kept, so installing it
    /// again finds them.
    pub(super) fn uninstall_plugin(&mut self, id: &str) {
        use crate::plugins::package_store;
        let result: Result<(), String> = (|| {
            if !self.plugins.is_installed_package(id) {
                return Err(package_store::StoreError::NotOwned.to_string());
            }
            let store = self
                .plugins
                .store()
                .map(std::path::Path::to_path_buf)
                .ok_or_else(|| package_store::StoreError::NoStore.to_string())?;
            self.send_plugin_command(ManagerCommand::StopPlugin { id: id.to_owned() });
            package_store::uninstall(id, &store).map_err(|error| error.to_string())?;
            self.plugins.forget(id)?;
            self.plugins.rediscover(
                &std::env::current_exe().unwrap_or_default(),
                syntra_api::paths::config_dir()
                    .ok()
                    .map(|directory| directory.join("plugins"))
                    .as_deref(),
            );
            Ok(())
        })();
        self.plugins.note_result(id, &result);
        self.reconcile_plugins();
    }

    /// Records that a lifecycle request could not be carried out, against the
    /// plugin it named when there is one, so the interface can show why.
    fn plugin_action_failed(&mut self, id: Option<&str>, reason: String) {
        log::warn!("plugin request refused: {reason}");
        if let Some(id) = id {
            self.plugins.note_result(id, &Err(reason));
        }
        self.publish_plugins();
    }

    /// Forwards a command to the plugin supervisor, if one is running.
    ///
    /// A failure is recorded against the plugin rather than swallowed: a
    /// control that silently does nothing is exactly what this change exists
    /// to remove.
    fn send_plugin_command(&mut self, command: ManagerCommand) {
        let Some(manager) = self.adapter_manager.as_ref() else {
            log::warn!("plugin supervisor is not running; command ignored");
            return;
        };
        if let Err(error) = manager.try_send(command) {
            log::warn!("could not reach the plugin supervisor: {error}");
        }
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
            capture_backends: CaptureBackend::choices(),
            emulation_backends: EmulationBackend::choices(),
            capture_backend_choice: self.config.capture_backend().map(BackendName::name),
            emulation_backend_choice: self.config.emulation_backend().map(BackendName::name),
            port: self.port,
            build_fingerprint: syntra_api::BUILD_FINGERPRINT.to_owned(),
        };
        self.notify_frontend(FrontendEvent::DaemonInfo(info));
    }

    /// Tells emulation which side of this screen each configured peer sits
    /// on, so entering pointers follow this machine's layout.
    pub(super) fn publish_peer_sides(&self) {
        let mut sides = std::collections::HashMap::new();
        let mut beyond = std::collections::HashMap::new();
        for (handle, config, state) in self.client_manager.get_client_states() {
            let side = match config.pos {
                syntra_api::Position::Left => syntra_proto::Position::Left,
                syntra_api::Position::Right => syntra_proto::Position::Right,
                syntra_api::Position::Top => syntra_proto::Position::Top,
                syntra_api::Position::Bottom => syntra_proto::Position::Bottom,
            };
            for ip in config.fix_ips.iter().chain(state.ips.iter()) {
                sides.insert(*ip, side);
            }
            if let Some(fingerprint) = self.client_manager.peer_fingerprint(handle) {
                beyond.insert(side, fingerprint);
            }
        }
        self.emulation.set_peer_sides(sides, beyond);
        self.publish_peer_availability();
    }

    /// Tells emulation which configured devices cannot take input right now
    /// (not connected, or their emulation is off), so the edge towards one
    /// is a wall rather than a hand-off that will be refused.
    pub(super) fn publish_peer_availability(&self) {
        let unavailable = self
            .client_manager
            .get_client_states()
            .into_iter()
            .filter(|(_, _, state)| !(state.alive && state.remote_ready))
            .filter_map(|(handle, ..)| self.client_manager.peer_fingerprint(handle))
            .collect();
        self.emulation.set_unavailable_peers(unavailable);
    }

    pub(super) fn save_config(&mut self) {
        self.publish_peer_sides();
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
        // What is applied, not just what is saved: a saved choice that waits
        // for a login must not show as on while emulation runs without it.
        #[cfg(target_os = "linux")]
        let independent =
            self.config.independent_pointers() && crate::gnome_guard::independent_pointer_ready();
        #[cfg(not(target_os = "linux"))]
        let independent = self.config.independent_pointers();
        self.notify_frontend(FrontendEvent::IndependentPointers(independent));
        self.notify_frontend(FrontendEvent::HopBypass(self.config.hop_bypass()));
        self.notify_frontend(FrontendEvent::MultiHop(self.config.multi_hop()));
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
