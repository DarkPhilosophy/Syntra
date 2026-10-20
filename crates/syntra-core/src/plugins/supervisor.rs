//! Running plugin processes.
//!
//! The supervisor owns every plugin process and nothing else does. It launches
//! what it is told to, speaks the plugin protocol with it, enforces what the
//! plugin was approved to do, and ends it, all of its descendants included.
//!
//! # One identity model
//!
//! A process is addressed by [`AdapterId`]: the plugin's id plus, for a
//! per-transfer process, the transfer it serves. There is no identity for "the
//! clipboard plugin" or "the FUSE plugin": a plugin of any id is launched,
//! handshaken, authorised and routed the same way, and the bundled ones differ
//! only in where their approval comes from.
//!
//! # Generations
//!
//! Every launch gets a fresh generation number carried on everything the
//! process produces. An event from a generation that is no longer current, such
//! as a line a stopped plugin wrote on its way out, is dropped, so a revoked
//! process can never act on the daemon's state.
//!
//! # What a plugin may do
//!
//! After the handshake the supervisor holds a [`Grant`]: what the user approved
//! intersected with what the process announced. Every frame the plugin sends is
//! checked against it, and a frame it was not granted ends the process.

use super::{
    process::{self, Diagnostics},
    routing::{Grant, validate_handshake},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures::StreamExt;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};
use syntra_plugin_api::{
    Capabilities, Message, PluginRole, RangeResponse, Released, RemoteManifest, Unmounted,
};
use thiserror::Error;
use tokio::{
    io::AsyncWriteExt,
    process::{Child, Command},
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_util::codec::{FramedRead, LinesCodec};

const CHANNEL_CAPACITY: usize = 64;
const MAX_LINE_LENGTH: usize = 2 * 1024 * 1024;
/// Longest error-stream line held in memory. Diagnostics keep only the first
/// couple of hundred characters of a line anyway, so this is generous while
/// still bounding what an unterminated flood can cost.
const MAX_STDERR_LINE: usize = 8 * 1024;
const MAX_PENDING_RANGES: usize = 256;
const MAX_RESTARTS: u8 = 3;

/// A running plugin process.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum AdapterId {
    /// The long-running process of a plugin, by plugin id.
    Resident(String),
    /// A process started for one transfer and ended with it.
    Transfer {
        /// Plugin identifier.
        plugin: String,
        /// The transfer the process serves, which is also what correlates its
        /// frames: it may only speak about this transfer.
        transfer_id: String,
    },
    /// The settings window of a plugin that is not otherwise running. It is
    /// started with `--settings`, draws its own window and does not speak the
    /// plugin protocol, so it never handshakes and is never granted anything.
    /// It is supervised like every other process of the plugin: contained,
    /// content-checked at spawn, and ended with the plugin.
    Settings(String),
}

impl AdapterId {
    /// The plugin this process belongs to.
    pub(crate) fn plugin(&self) -> &str {
        match self {
            AdapterId::Resident(plugin)
            | AdapterId::Settings(plugin)
            | AdapterId::Transfer { plugin, .. } => plugin,
        }
    }

    /// The transfer a per-transfer process serves.
    pub(crate) fn transfer(&self) -> Option<&str> {
        match self {
            AdapterId::Resident(_) | AdapterId::Settings(_) => None,
            AdapterId::Transfer { transfer_id, .. } => Some(transfer_id),
        }
    }
}

/// Everything needed to launch one plugin, fixed at the time it is asked for.
///
/// The executable is the path the registry reported, checked again by the
/// supervisor just before the launch, and the arguments are the manifest's,
/// delivered unchanged.
#[derive(Clone, Debug)]
pub(crate) struct LaunchSpec {
    /// Exact file to run.
    pub executable: PathBuf,
    /// Arguments passed to it, in order.
    pub args: Vec<String>,
    /// What the user approved (or the bundled descriptor declares).
    pub approved: Capabilities,
    /// Identifiers its handshake may use besides the plugin id.
    pub handshake_ids: Vec<String>,
    /// The role the process is started for, when it is started for one.
    pub role: Option<PluginRole>,
    /// Content digest at approval, rechecked by the caller before every start.
    pub digest: String,
    /// What is needed to recompute the digest at launch. `None` for a bundled
    /// plugin, whose trust is the distribution's and not an approval of
    /// content.
    pub integrity: Option<Integrity>,
}

/// The package a digest was computed from, kept so the supervisor can
/// recompute it at every launch without reaching back into the registry.
#[derive(Clone, Debug)]
pub(crate) struct Integrity {
    /// The manifest as it was approved.
    pub manifest: syntra_plugin_api::PluginManifest,
    /// Where the manifest file is, which anchors its resources.
    pub manifest_path: PathBuf,
}

/// Recomputes the content digest and compares it with the approved one.
///
/// A digest identifies the bytes read at this moment. It does not make the
/// files immutable, so a process able to write the package can still change
/// them between this check and the program starting; that limit is stated, not
/// hidden.
fn verify_content(spec: &LaunchSpec) -> Result<(), String> {
    let Some(integrity) = &spec.integrity else {
        return Ok(());
    };
    let current = syntra_plugin_api::digest_package(
        &integrity.manifest,
        &integrity.manifest_path,
        &spec.executable,
    )
    .map_err(|error| format!("the plugin cannot be checked: {error}"))?;
    if current.digest == spec.digest {
        Ok(())
    } else {
        Err("the plugin's content changed since it was approved".into())
    }
}

/// What the supervisor is asked to do.
#[derive(Debug)]
pub(crate) enum ManagerCommand {
    /// Launch a resident plugin and keep it running.
    ///
    /// `settings` is what the plugin is told once it has handshaken.
    StartPlugin {
        id: String,
        spec: LaunchSpec,
        settings: Vec<(String, String)>,
    },
    /// Launch a per-transfer process for `manifest`'s transfer.
    StartTransfer {
        id: String,
        spec: LaunchSpec,
        manifest: RemoteManifest,
    },
    /// Stop every process of a plugin and do not relaunch them.
    StopPlugin { id: String },
    /// Stop and immediately relaunch a resident plugin.
    RestartPlugin { id: String },
    /// Tell a running plugin its settings changed.
    PluginSettings {
        id: String,
        settings: Vec<(String, String)>,
    },
    /// Pass a user's change to a plugin that owns its settings.
    PluginChangeSetting {
        id: String,
        key: String,
        value: String,
    },
    /// Ask a plugin that owns its settings to open its settings window.
    PluginOpenSettings {
        id: String,
        theme: Vec<(String, String)>,
    },
    /// Open the settings window of a plugin that is not running, as a
    /// supervised process: content-checked at spawn, started with the
    /// manifest's arguments plus `--settings`, contained and ended with the
    /// plugin. Used for a plugin that draws its own settings window but is not
    /// otherwise running; nothing is handshaken or granted.
    OpenSettingsWindow {
        id: String,
        spec: LaunchSpec,
        theme: Vec<(String, String)>,
    },
    /// Forward a pointer event to a running plugin.
    PluginPointer {
        id: String,
        event: syntra_plugin_api::PointerEvent,
    },
    /// Hand a selection to the plugin providing the file clipboard.
    PublishFileClipboard {
        id: String,
        publication: syntra_plugin_api::PublishFileClipboard,
    },
    /// Hand text clipboard data to a plugin that asked for it.
    ClipboardData {
        id: String,
        transfer_id: String,
        mime_type: String,
        value: String,
    },
    /// Answer a plugin's range request.
    RangeResponse { id: String, response: RangeResponse },
    /// Tell the clipboard provider a transfer may be released.
    Released { id: String, released: Released },
    /// Tell a per-transfer process its mount was removed.
    Unmounted { id: String, unmounted: Unmounted },
    /// Cancel a transfer a process serves.
    Cancel {
        adapter: AdapterId,
        transfer_id: String,
    },
    /// Stop every process, then the supervisor.
    Shutdown,
}

/// What the supervisor reports.
#[derive(Debug)]
pub(crate) enum ManagerEvent {
    /// The process was launched, with its pid, and has not handshaken yet.
    Started(AdapterId, u32),
    /// The handshake completed. Carries what the plugin declared about itself
    /// and the grant that applies to it.
    Ready(AdapterId, Option<syntra_plugin_api::PluginMetadata>, Grant),
    /// The process was stopped because a client asked for it. Distinct from
    /// `Exited`: a deliberate stop is not a failure.
    Stopped(AdapterId),
    /// A frame the plugin sent that the grant allows.
    Message {
        adapter: AdapterId,
        message: Message,
    },
    /// A transfer was cancelled because its process ended.
    Cancelled {
        adapter: AdapterId,
        transfer_id: String,
    },
    /// A request could not be carried out, or a process misbehaved.
    Rejected {
        adapter: Option<AdapterId>,
        reason: String,
    },
    /// The process ended on its own.
    Exited { adapter: AdapterId, status: String },
    /// New diagnostic lines the plugin wrote to its error stream.
    Diagnostics { plugin: String, lines: Vec<String> },
}

/// Why supervising a plugin failed.
///
/// Public because it is reachable through `ServiceError::Adapter`.
#[derive(Debug, Error)]
pub enum ManagerError {
    /// The supervisor is not draining commands fast enough.
    ///
    /// Commands are dropped rather than queued without bound, so a wedged
    /// plugin cannot stall the service event loop.
    #[error("adapter manager command queue is full")]
    QueueFull,
    /// The supervisor task has exited: no plugin is reachable.
    #[error("adapter manager has stopped")]
    Stopped,
    /// The supervisor task panicked or was cancelled.
    #[error("adapter manager task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// Handle to the supervisor task.
pub(crate) struct AdapterProcessManager {
    commands: mpsc::Sender<ManagerCommand>,
    task: Option<JoinHandle<()>>,
}

impl AdapterProcessManager {
    /// Starts the supervisor. It launches nothing until told to: an optional
    /// or missing plugin must never stop input sharing from starting.
    pub(crate) fn start() -> (Self, mpsc::Receiver<ManagerEvent>) {
        let (commands, command_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (events, event_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(run_manager(command_rx, events));
        (
            Self {
                commands,
                task: Some(task),
            },
            event_rx,
        )
    }

    pub(crate) fn try_send(&self, command: ManagerCommand) -> Result<(), ManagerError> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ManagerError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => ManagerError::Stopped,
            })
    }

    pub(crate) async fn shutdown(mut self) -> Result<(), ManagerError> {
        self.commands
            .send(ManagerCommand::Shutdown)
            .await
            .map_err(|_| ManagerError::Stopped)?;
        if let Some(task) = self.task.take() {
            task.await?;
        }
        Ok(())
    }
}

impl Drop for AdapterProcessManager {
    fn drop(&mut self) {
        let _ = self.commands.try_send(ManagerCommand::Shutdown);
        // Aborting the supervisor is deliberately avoided: it owns
        // kill-on-drop children and must be allowed to reap them. The runtime
        // joins it during orderly shutdown.
    }
}

struct AdapterRuntime {
    generation: u64,
    writer: mpsc::Sender<Message>,
    terminate: Option<oneshot::Sender<()>>,
    tasks: Vec<JoinHandle<()>>,
    pid: u32,
    spec: LaunchSpec,
    /// Set once the handshake is accepted.
    grant: Option<Grant>,
}

impl AdapterRuntime {
    fn ready(&self) -> bool {
        self.grant.is_some()
    }
}

#[derive(Debug)]
enum ProcessEvent {
    Line {
        adapter: AdapterId,
        generation: u64,
        line: String,
    },
    Diagnostic {
        adapter: AdapterId,
        generation: u64,
        line: String,
    },
    Failed {
        adapter: AdapterId,
        generation: u64,
        reason: String,
    },
    Exited {
        adapter: AdapterId,
        generation: u64,
        status: String,
    },
}

#[derive(Clone, Copy)]
struct PendingRange {
    offset: u64,
    length: u32,
}

struct State {
    processes: HashMap<AdapterId, AdapterRuntime>,
    pending_ranges: HashMap<(String, u64), PendingRange>,
    pending_messages: HashMap<AdapterId, Message>,
    /// Transfers each plugin announced, so a release or cancel for a transfer
    /// nobody owns is refused.
    owned_transfers: HashMap<String, HashSet<String>>,
    next_generation: u64,
    terminal: HashSet<AdapterId>,
    /// How each plugin was last asked to run, so a crash or restart
    /// relaunches exactly that.
    specs: HashMap<String, LaunchSpec>,
    settings: HashMap<String, Vec<(String, String)>>,
    restarts: HashMap<String, u8>,
    /// Changes for a plugin that owns its settings, made while it was not
    /// running: handed over, in order, as soon as it handshakes.
    pending_changes: HashMap<String, Vec<(String, String)>>,
    diagnostics: HashMap<String, Diagnostics>,
    stopping: bool,
}

impl State {
    fn new() -> Self {
        Self {
            processes: HashMap::new(),
            pending_ranges: HashMap::new(),
            pending_messages: HashMap::new(),
            owned_transfers: HashMap::new(),
            next_generation: 1,
            terminal: HashSet::new(),
            specs: HashMap::new(),
            settings: HashMap::new(),
            restarts: HashMap::new(),
            pending_changes: HashMap::new(),
            diagnostics: HashMap::new(),
            stopping: false,
        }
    }

    fn ready(&self, adapter: &AdapterId) -> bool {
        self.processes
            .get(adapter)
            .is_some_and(AdapterRuntime::ready)
    }

    fn resident_of(&self, id: &str) -> AdapterId {
        AdapterId::Resident(id.to_owned())
    }
}

async fn run_manager(
    mut commands: mpsc::Receiver<ManagerCommand>,
    events: mpsc::Sender<ManagerEvent>,
) {
    let (process_events, mut process_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let mut state = State::new();

    while !state.stopping {
        tokio::select! {
            command = commands.recv() => match command {
                Some(ManagerCommand::Shutdown) | None => state.stopping = true,
                Some(command) => handle_command(command, &mut state, &process_events, &events).await,
            },
            event = process_rx.recv() => {
                if let Some(event) = event {
                    handle_process_event(event, &mut state, &process_events, &events).await;
                }
            }
        }
    }

    let adapters: Vec<_> = state.processes.keys().cloned().collect();
    for adapter in adapters {
        stop_adapter(&mut state, &adapter).await;
    }
}

/// Every live process belonging to `plugin`.
///
/// Collected before mutating, because stopping a process removes it from the
/// map being iterated.
fn processes_of(state: &State, plugin: &str) -> Vec<AdapterId> {
    state
        .processes
        .keys()
        .filter(|adapter| adapter.plugin() == plugin)
        .cloned()
        .collect()
}

async fn handle_command(
    command: ManagerCommand,
    state: &mut State,
    process_events: &mpsc::Sender<ProcessEvent>,
    events: &mpsc::Sender<ManagerEvent>,
) {
    match command {
        ManagerCommand::PublishFileClipboard { id, publication } => {
            send_ready(
                state,
                &state.resident_of(&id),
                Message::PublishFileClipboard(publication),
                events,
            )
            .await;
        }
        ManagerCommand::ClipboardData {
            id,
            transfer_id,
            mime_type,
            value,
        } => {
            send_ready(
                state,
                &state.resident_of(&id),
                Message::ClipboardData {
                    transfer_id,
                    mime_type,
                    value,
                },
                events,
            )
            .await;
        }
        ManagerCommand::StartTransfer { id, spec, manifest } => {
            let transfer_id = manifest.transfer_id.clone();
            if !valid_transfer_id(&transfer_id) {
                reject(events, None, "invalid transfer id").await;
                return;
            }
            let adapter = AdapterId::Transfer {
                plugin: id,
                transfer_id: transfer_id.clone(),
            };
            if state.processes.contains_key(&adapter) {
                reject(events, Some(adapter), "duplicate transfer").await;
                return;
            }
            state.specs.insert(adapter.plugin().to_owned(), spec);
            match spawn_adapter(state, adapter.clone(), process_events, events).await {
                Ok(()) => {
                    state
                        .pending_messages
                        .insert(adapter, Message::RemoteManifest(manifest));
                }
                Err(reason) => reject(events, Some(adapter), reason).await,
            }
        }
        ManagerCommand::RangeResponse { id, response } => {
            let adapter = AdapterId::Transfer {
                plugin: id,
                transfer_id: response.transfer_id.clone(),
            };
            let key = (response.transfer_id.clone(), response.request_id);
            let Some(pending) = state.pending_ranges.remove(&key) else {
                reject(events, Some(adapter), "uncorrelated range response").await;
                return;
            };
            if response.offset != pending.offset {
                reject(
                    events,
                    Some(adapter.clone()),
                    "range response offset mismatch",
                )
                .await;
                fail_adapter(state, &adapter, "range response offset mismatch", events).await;
                return;
            }
            if response.error.is_none() {
                match BASE64.decode(&response.data_base64) {
                    Ok(data) if data.len() <= pending.length as usize => {}
                    Ok(_) => {
                        reject(
                            events,
                            Some(adapter.clone()),
                            "range response exceeds requested length",
                        )
                        .await;
                        fail_adapter(state, &adapter, "oversized range response", events).await;
                        return;
                    }
                    Err(_) => {
                        reject(
                            events,
                            Some(adapter.clone()),
                            "range response contains invalid base64",
                        )
                        .await;
                        fail_adapter(state, &adapter, "invalid range response", events).await;
                        return;
                    }
                }
            }
            send_ready(state, &adapter, Message::RangeResponse(response), events).await;
        }
        ManagerCommand::Released { id, released } => {
            let owned = state
                .owned_transfers
                .get_mut(&id)
                .is_some_and(|transfers| transfers.remove(&released.transfer_id));
            let adapter = state.resident_of(&id);
            if !owned {
                reject(events, Some(adapter), "release for an unknown transfer").await;
                return;
            }
            send_ready(state, &adapter, Message::Released(released), events).await;
        }
        ManagerCommand::Unmounted { id, unmounted } => {
            let adapter = AdapterId::Transfer {
                plugin: id,
                transfer_id: unmounted.transfer_id.clone(),
            };
            if !state.processes.contains_key(&adapter) {
                reject(events, Some(adapter), "unmount for an unknown transfer").await;
                return;
            }
            send_ready(state, &adapter, Message::Unmounted(unmounted), events).await;
        }
        ManagerCommand::Cancel {
            adapter,
            transfer_id,
        } => {
            let owns = match &adapter {
                AdapterId::Resident(plugin) => state
                    .owned_transfers
                    .get_mut(plugin)
                    .is_some_and(|transfers| transfers.remove(&transfer_id)),
                AdapterId::Transfer {
                    transfer_id: owned, ..
                } => owned == &transfer_id && state.processes.contains_key(&adapter),
                AdapterId::Settings(_) => false,
            };
            if !owns {
                reject(events, Some(adapter), "cancel for an unowned transfer").await;
                return;
            }
            send_ready(state, &adapter, Message::Cancel { transfer_id }, events).await;
        }
        ManagerCommand::StopPlugin { id } => {
            for adapter in processes_of(state, &id) {
                // Marked terminal first, so the exit is not treated as a
                // crash and does not trigger the relaunch path.
                state.terminal.insert(adapter.clone());
                stop_adapter(state, &adapter).await;
                emit(events, ManagerEvent::Stopped(adapter)).await;
            }
            // A stopped plugin forgets how it was asked to run: starting it
            // again goes through approval and the registry afresh, so a
            // revoked approval can never be revived from this cache.
            state.specs.remove(&id);
            state.restarts.remove(&id);
        }
        ManagerCommand::RestartPlugin { id } => {
            let resident = state.resident_of(&id);
            for adapter in processes_of(state, &id) {
                state.terminal.insert(adapter.clone());
                stop_adapter(state, &adapter).await;
            }
            if state.stopping {
                return;
            }
            // A transfer process exists solely for its transfer, so only the
            // resident one is relaunched. A restart is the user's remedy for
            // a stuck plugin, so it earns a fresh set of automatic retries.
            if state.specs.contains_key(&id) {
                state.terminal.remove(&resident);
                state.restarts.remove(&id);
                if let Err(reason) =
                    spawn_adapter(state, resident.clone(), process_events, events).await
                {
                    reject(events, Some(resident), reason).await;
                }
            }
        }
        ManagerCommand::StartPlugin { id, spec, settings } => {
            let adapter = state.resident_of(&id);
            if state.processes.contains_key(&adapter) {
                // Already running; a start request is how a client re-asserts
                // the settings, not a reason to launch a second copy.
                state.settings.insert(id, settings.clone());
                send_settings(state, &adapter, settings, events).await;
                return;
            }
            state.terminal.remove(&adapter);
            state.specs.insert(id.clone(), spec);
            state.settings.insert(id.clone(), settings);
            state.restarts.remove(&id);
            if let Err(reason) = spawn_adapter(state, adapter.clone(), process_events, events).await
            {
                reject(events, Some(adapter), reason).await;
            }
        }
        ManagerCommand::PluginSettings { id, settings } => {
            let adapter = state.resident_of(&id);
            state.settings.insert(id, settings.clone());
            // A plugin that is not running has nothing to tell; it will be
            // given the stored values when it next starts.
            if state.ready(&adapter) {
                send_settings(state, &adapter, settings, events).await;
            }
        }
        ManagerCommand::PluginChangeSetting { id, key, value } => {
            let adapter = state.resident_of(&id);
            if state.ready(&adapter) {
                send_ready(
                    state,
                    &adapter,
                    Message::ChangeSetting { key, value },
                    events,
                )
                .await;
            } else {
                // Kept, not dropped: the plugin decides when it next runs.
                // Only the latest value per key matters.
                let queued = state.pending_changes.entry(id).or_default();
                queued.retain(|(queued_key, _)| *queued_key != key);
                if queued.len() < 64 {
                    queued.push((key, value));
                }
            }
        }
        ManagerCommand::PluginOpenSettings { id, theme } => {
            let adapter = state.resident_of(&id);
            if state.ready(&adapter) {
                send_ready(state, &adapter, Message::OpenSettings { theme }, events).await;
            } else {
                log::warn!("cannot open the settings of `{id}`: it is not running");
            }
        }
        ManagerCommand::OpenSettingsWindow { id, spec, theme } => {
            if state.stopping {
                return;
            }
            let adapter = AdapterId::Settings(id.clone());
            if let Err(reason) =
                spawn_settings_window(state, &id, spec, theme, process_events, events).await
            {
                reject(events, Some(adapter), reason).await;
            }
        }
        ManagerCommand::PluginPointer { id, event } => {
            let adapter = state.resident_of(&id);
            // Pointer events are high-rate and perishable. One for a plugin
            // that is still starting, or one it has no room for, is dropped:
            // never queued, and never a reason to fail a healthy plugin.
            let granted = state
                .processes
                .get(&adapter)
                .and_then(|run| run.grant.as_ref())
                .is_some_and(|grant| grant.allows(PluginRole::Pointer));
            if granted {
                send_droppable(state, &adapter, Message::Pointer(event), events).await;
            }
        }
        ManagerCommand::Shutdown => state.stopping = true,
    }
}

async fn handle_process_event(
    event: ProcessEvent,
    state: &mut State,
    process_events: &mpsc::Sender<ProcessEvent>,
    events: &mpsc::Sender<ManagerEvent>,
) {
    // Whatever the event, it counts only if it comes from the process that is
    // current for its identity. A late line from a stopped generation is
    // dropped, so a revoked process cannot act.
    let current = |state: &State, adapter: &AdapterId, generation: u64| {
        state
            .processes
            .get(adapter)
            .is_some_and(|runtime| runtime.generation == generation)
    };
    match event {
        ProcessEvent::Diagnostic {
            adapter,
            generation,
            line,
        } => {
            if !current(state, &adapter, generation) {
                return;
            }
            let plugin = adapter.plugin().to_owned();
            let ring = state.diagnostics.entry(plugin.clone()).or_default();
            ring.push(&line);
            let lines = ring.lines();
            emit(events, ManagerEvent::Diagnostics { plugin, lines }).await;
        }
        ProcessEvent::Line {
            adapter,
            generation,
            line,
        } => {
            if !current(state, &adapter, generation) {
                return;
            }
            let message = match Message::decode_line(&line) {
                Ok(message) => message,
                Err(error) => {
                    fail_adapter(
                        state,
                        &adapter,
                        &format!("malformed message: {error}"),
                        events,
                    )
                    .await;
                    return;
                }
            };
            if !state.ready(&adapter) {
                accept_handshake(state, &adapter, &message, events).await;
                return;
            }
            if let Err(reason) = validate_inbound(state, &adapter, &message) {
                fail_adapter(state, &adapter, &reason, events).await;
                return;
            }
            if adapter.transfer().is_some()
                && matches!(
                    message,
                    Message::Completed(_) | Message::Cancelled(_) | Message::Unmounted(_)
                )
            {
                state.terminal.insert(adapter.clone());
            }
            if let Message::Released(released) = &message {
                if let Some(transfers) = state.owned_transfers.get_mut(adapter.plugin()) {
                    transfers.remove(&released.transfer_id);
                }
            }
            if let Message::CopyManifest(manifest) = &message {
                state
                    .owned_transfers
                    .entry(adapter.plugin().to_owned())
                    .or_default()
                    .insert(manifest.transfer_id.clone());
            }
            emit(events, ManagerEvent::Message { adapter, message }).await;
        }
        ProcessEvent::Failed {
            adapter,
            generation,
            reason,
        }
        | ProcessEvent::Exited {
            adapter,
            generation,
            status: reason,
        } => {
            if !current(state, &adapter, generation) {
                return;
            }
            process_exit(state, adapter, reason, process_events, events).await;
        }
    }
}

/// Completes (or refuses) the handshake of a process that is not ready yet.
async fn accept_handshake(
    state: &mut State,
    adapter: &AdapterId,
    message: &Message,
    events: &mpsc::Sender<ManagerEvent>,
) {
    let Some(runtime) = state.processes.get(adapter) else {
        return;
    };
    let spec = runtime.spec.clone();
    let plugin = adapter.plugin().to_owned();
    let expected_role = spec.role;
    let accepts = |announced: &str| {
        announced == plugin || spec.handshake_ids.iter().any(|id| id == announced)
    };
    let grant = match validate_handshake(&plugin, accepts, &spec.approved, message, expected_role) {
        Ok(grant) => grant,
        Err(reason) => {
            fail_adapter(
                state,
                adapter,
                &format!("handshake refused: {reason}"),
                events,
            )
            .await;
            return;
        }
    };
    if let Some(runtime) = state.processes.get_mut(adapter) {
        runtime.grant = Some(grant.clone());
    }
    // What the process says about itself is displayed; it grants nothing and
    // cannot change who the plugin is.
    let declared = match message {
        Message::Hello { metadata, .. } => metadata.clone(),
        _ => None,
    };
    emit(
        events,
        ManagerEvent::Ready(adapter.clone(), declared, grant),
    )
    .await;
    if let Some(pending) = state.pending_messages.remove(adapter) {
        send_ready(state, adapter, pending, events).await;
    }
    // A resident plugin starts from the user's saved values, so it never has
    // to show defaults for a moment and then jump.
    if let AdapterId::Resident(id) = adapter {
        let settings = state.settings.get(id).cloned().unwrap_or_default();
        send_settings(state, adapter, settings, events).await;
        for (key, value) in state.pending_changes.remove(id).unwrap_or_default() {
            send_ready(
                state,
                adapter,
                Message::ChangeSetting { key, value },
                events,
            )
            .await;
        }
    }
}

/// Checks a frame from a ready process against its grant and, for a
/// per-transfer process, against the transfer it serves.
fn validate_inbound(
    state: &mut State,
    adapter: &AdapterId,
    message: &Message,
) -> Result<(), String> {
    let grant = state
        .processes
        .get(adapter)
        .and_then(|runtime| runtime.grant.as_ref())
        .ok_or("the process has no grant")?;
    if !grant.permits(message) {
        return Err("a frame the plugin was not approved to send".into());
    }
    if let Message::DeclareSettings { settings } = message {
        return validate_declared_settings(settings);
    }
    let transfer = message_transfer_id(message);
    if let Some(transfer_id) = transfer {
        if !valid_transfer_id(transfer_id) {
            return Err("the plugin emitted an invalid transfer id".into());
        }
        // A per-transfer process may speak only about its own transfer.
        if let AdapterId::Transfer {
            transfer_id: owned, ..
        } = adapter
        {
            if transfer_id != owned {
                return Err("the plugin emitted a message for another transfer".into());
            }
        }
    } else if !matches!(message, Message::Error { .. }) {
        return Err("an uncorrelated message".into());
    }
    if let Message::RangeRequest(request) = message {
        if request.length == 0 {
            return Err("zero-length range request".into());
        }
        if state.pending_ranges.len() >= MAX_PENDING_RANGES {
            return Err("too many pending range requests".into());
        }
        let key = (request.transfer_id.clone(), request.request_id);
        if state
            .pending_ranges
            .insert(
                key,
                PendingRange {
                    offset: request.offset,
                    length: request.length,
                },
            )
            .is_some()
        {
            return Err("duplicate range request id".into());
        }
    }
    if let Message::MountReady(syntra_plugin_api::MountReady {
        mount_uri, uris, ..
    }) = message
    {
        if mount_uri.is_empty() || uris.is_empty() {
            return Err("invalid mount-ready message".into());
        }
    }
    Ok(())
}

fn message_transfer_id(message: &Message) -> Option<&str> {
    match message {
        Message::CopyManifest(value) => Some(&value.transfer_id),
        Message::PasteDestination(value) => Some(&value.transfer_id),
        Message::Progress(value) => Some(&value.transfer_id),
        Message::RemoteManifest(value) => Some(&value.transfer_id),
        Message::RangeRequest(value) => Some(&value.transfer_id),
        Message::RangeResponse(value) => Some(&value.transfer_id),
        Message::MountReady(value) => Some(&value.transfer_id),
        Message::PublishFileClipboard(value) => Some(&value.transfer_id),
        Message::Released(value) => Some(&value.transfer_id),
        Message::Unmounted(value) => Some(&value.transfer_id),
        Message::Completed(value) => Some(&value.transfer_id),
        Message::Cancelled(value) => Some(&value.transfer_id),
        Message::Cancel { transfer_id } => Some(transfer_id),
        Message::ClipboardData { transfer_id, .. } => Some(transfer_id),
        Message::Hello { .. }
        | Message::Error { .. }
        | Message::Settings { .. }
        | Message::DeclareSettings { .. }
        | Message::ChangeSetting { .. }
        | Message::OpenSettings { .. }
        | Message::Pointer(_) => None,
    }
}

/// Bounds a plugin's own settings, which reach every attached interface.
fn validate_declared_settings(
    settings: &[syntra_plugin_api::SettingControl],
) -> Result<(), String> {
    const MAX_SETTINGS: usize = 64;
    const MAX_TEXT: usize = 256;
    const MAX_OPTIONS: usize = 64;
    if settings.len() > MAX_SETTINGS {
        return Err("plugin declared too many settings".into());
    }
    let mut keys = HashSet::new();
    for setting in settings {
        let too_long = [
            &setting.key,
            &setting.label,
            &setting.value,
            &setting.default,
        ]
        .iter()
        .any(|text| text.len() > MAX_TEXT);
        if setting.key.is_empty() || too_long || !keys.insert(setting.key.as_str()) {
            return Err("plugin declared an invalid setting".into());
        }
        match &setting.kind {
            syntra_plugin_api::SettingControlKind::Choice { options } => {
                if options.is_empty()
                    || options.len() > MAX_OPTIONS
                    || options.iter().any(|option| {
                        option.value.len() > MAX_TEXT || option.label.len() > MAX_TEXT
                    })
                {
                    return Err("plugin declared an invalid choice".into());
                }
            }
            syntra_plugin_api::SettingControlKind::Number { min, max } if min > max => {
                return Err("plugin declared an empty number range".into());
            }
            _ => {}
        }
    }
    Ok(())
}

/// Checks the launch inputs and starts the process.
async fn spawn_adapter(
    state: &mut State,
    adapter: AdapterId,
    process_events: &mpsc::Sender<ProcessEvent>,
    events: &mpsc::Sender<ManagerEvent>,
) -> Result<(), String> {
    let spec = state
        .specs
        .get(adapter.plugin())
        .cloned()
        .ok_or_else(|| format!("plugin {} has no registered launch", adapter.plugin()))?;
    // Every launch passes here: a start, a restart and a relaunch after a
    // crash. The content is checked again each time, against the digest that
    // was approved, so a file replaced after approval does not run just because
    // an earlier launch was allowed. A mismatch also drops the cached launch,
    // so a crash loop cannot keep retrying changed code.
    if let Err(reason) = verify_content(&spec) {
        state.specs.remove(adapter.plugin());
        return Err(reason);
    }
    check_executable(&spec.executable)?;
    let generation = state.next_generation;
    state.next_generation = state.next_generation.wrapping_add(1).max(1);
    state
        .diagnostics
        .entry(adapter.plugin().to_owned())
        .or_default()
        .clear();
    let (runtime, pid) = launch(
        spec,
        adapter.clone(),
        generation,
        process_events.clone(),
        None,
    )?;
    state.processes.insert(adapter.clone(), runtime);
    emit(events, ManagerEvent::Started(adapter, pid)).await;
    Ok(())
}

/// Starts a plugin's settings window as a supervised process.
///
/// It goes through the same gate as any launch: the approved content is
/// recomputed and compared first, the executable is checked, and the process
/// is started with the filtered environment, the manifest's arguments plus
/// `--settings`, in its own process group, with its error stream drained into
/// the bounded diagnostics. It never handshakes and is never granted anything.
///
/// Only one settings window per plugin exists: asking again while it is open
/// leaves it alone. The cached launch of the *plugin* is not touched, so a
/// settings request cannot revive or alter how the plugin itself is run, and a
/// content mismatch refuses the window without discarding anything.
async fn spawn_settings_window(
    state: &mut State,
    id: &str,
    spec: LaunchSpec,
    theme: Vec<(String, String)>,
    process_events: &mpsc::Sender<ProcessEvent>,
    events: &mpsc::Sender<ManagerEvent>,
) -> Result<(), String> {
    let adapter = AdapterId::Settings(id.to_owned());
    if state.processes.contains_key(&adapter) {
        return Ok(());
    }
    verify_content(&spec)?;
    check_executable(&spec.executable)?;
    let generation = state.next_generation;
    state.next_generation = state.next_generation.wrapping_add(1).max(1);
    // Lines without a newline in a name or value cannot split the encoding.
    let theme: String = theme
        .iter()
        .filter(|(name, value)| !name.contains('\n') && !value.contains('\n'))
        .map(|(name, value)| format!("{name}={value}\n"))
        .collect();
    let (runtime, pid) = launch(
        spec,
        adapter.clone(),
        generation,
        process_events.clone(),
        Some(theme),
    )?;
    state.processes.insert(adapter.clone(), runtime);
    emit(events, ManagerEvent::Started(adapter, pid)).await;
    Ok(())
}

/// Spawns the process, contained, with a filtered environment and the
/// manifest's arguments, and starts the tasks that read and write it.
fn launch(
    spec: LaunchSpec,
    adapter: AdapterId,
    generation: u64,
    events: mpsc::Sender<ProcessEvent>,
    theme: Option<String>,
) -> Result<(AdapterRuntime, u32), String> {
    let mut command = Command::new(&spec.executable);
    // The manifest's arguments, unchanged, and for a settings window `--settings`
    // after them: the process is the plugin's own program in its settings mode,
    // not a differently-argumented one.
    command.args(&spec.args);
    if matches!(adapter, AdapterId::Settings(_)) {
        command.arg("--settings");
    }
    command
        .env_clear()
        .envs(process::filtered_environment(std::env::vars_os()));
    if let Some(theme) = &theme {
        command.env("SYNTRA_PLUGIN_THEME", theme);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    process::contain(&mut command);
    if !(cfg!(unix) || process::WINDOWS_JOB_CONTAINMENT) {
        log::debug!(
            "plugin `{}` is started without descendant containment on this platform",
            adapter.plugin()
        );
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start the plugin: {}", error.kind()))?;
    // Recorded before the handles are taken: once the child is moved into the
    // runtime the id is no longer reachable, and the interface needs it to
    // prove the plugin is a real process rather than an inferred entry.
    let pid = child.id().unwrap_or_default();
    let stdin = child.stdin.take().ok_or("stdin was not piped")?;
    let stdout = child.stdout.take().ok_or("stdout was not piped")?;
    let stderr = child.stderr.take().ok_or("stderr was not piped")?;

    let (writer, mut writes) = mpsc::channel::<Message>(CHANNEL_CAPACITY);
    let (terminate, terminate_rx) = oneshot::channel();

    let write_adapter = adapter.clone();
    let write_events = events.clone();
    let writer_task = tokio::spawn(async move {
        let mut stdin = stdin;
        while let Some(message) = writes.recv().await {
            let line = match message.encode_line() {
                Ok(line) => line,
                Err(error) => {
                    let _ = write_events
                        .send(ProcessEvent::Failed {
                            adapter: write_adapter.clone(),
                            generation,
                            reason: error.to_string(),
                        })
                        .await;
                    return;
                }
            };
            if let Err(error) = stdin.write_all(line.as_bytes()).await {
                let _ = write_events
                    .send(ProcessEvent::Failed {
                        adapter: write_adapter.clone(),
                        generation,
                        reason: format!("write failed: {}", error.kind()),
                    })
                    .await;
                return;
            }
        }
        let _ = stdin.shutdown().await;
    });

    let read_adapter = adapter.clone();
    let read_events = events.clone();
    let is_settings_window = matches!(adapter, AdapterId::Settings(_));
    let reader_task = tokio::spawn(async move {
        if is_settings_window {
            // A settings window does not speak the plugin protocol. Whatever
            // it prints is read and discarded in fixed-size chunks, so a full
            // pipe can never block it and nothing it writes is parsed.
            use tokio::io::AsyncReadExt;
            let mut stdout = stdout;
            let mut sink = [0_u8; 4096];
            while matches!(stdout.read(&mut sink).await, Ok(read) if read > 0) {}
            return;
        }
        let mut lines = FramedRead::new(stdout, LinesCodec::new_with_max_length(MAX_LINE_LENGTH));
        while let Some(line) = lines.next().await {
            match line {
                Ok(line) => {
                    if read_events
                        .send(ProcessEvent::Line {
                            adapter: read_adapter.clone(),
                            generation,
                            line,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    let _ = read_events
                        .send(ProcessEvent::Failed {
                            adapter: read_adapter.clone(),
                            generation,
                            reason: format!("output failed: {error}"),
                        })
                        .await;
                    return;
                }
            }
        }
    });

    // The error stream is read in bounded lines and only ever reaches the
    // interface through the sanitising ring. It is never written verbatim to
    // the daemon's log.
    let stderr_adapter = adapter.clone();
    let stderr_events = events.clone();
    let stderr_task = tokio::spawn(async move {
        let mut lines = BoundedLines::new(stderr, MAX_STDERR_LINE);
        while let Some(line) = lines.next_line().await {
            if stderr_events
                .send(ProcessEvent::Diagnostic {
                    adapter: stderr_adapter.clone(),
                    generation,
                    line,
                })
                .await
                .is_err()
            {
                return;
            }
        }
    });

    let wait_adapter = adapter;
    let waiter_task = tokio::spawn(supervise_child(
        child,
        terminate_rx,
        wait_adapter,
        generation,
        pid,
        events,
    ));

    Ok((
        AdapterRuntime {
            generation,
            writer,
            terminate: Some(terminate),
            tasks: vec![writer_task, reader_task, stderr_task, waiter_task],
            pid,
            spec,
            grant: None,
        },
        pid,
    ))
}

fn check_executable(path: &std::path::Path) -> Result<(), String> {
    let metadata = path
        .metadata()
        .map_err(|error| format!("the plugin executable is unavailable: {}", error.kind()))?;
    if !metadata.is_file() {
        return Err("the plugin executable is not a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("the plugin executable is not executable".into());
        }
    }
    Ok(())
}

/// Waits for the child, or ends it (and its whole group) when asked.
async fn supervise_child(
    mut child: Child,
    mut terminate: oneshot::Receiver<()>,
    adapter: AdapterId,
    generation: u64,
    pid: u32,
    events: mpsc::Sender<ProcessEvent>,
) {
    let status = tokio::select! {
        status = child.wait() => status.map(|status| status.to_string()),
        _ = &mut terminate => {
            end_process(&mut child, pid).await;
            Ok("stopped".to_owned())
        }
    };
    // Whatever the child left running in its group goes with it, even when it
    // exited by itself.
    #[cfg(unix)]
    process::kill_group(pid);
    let status = status.unwrap_or_else(|error| format!("wait failed: {}", error.kind()));
    let _ = events
        .send(ProcessEvent::Exited {
            adapter,
            generation,
            status,
        })
        .await;
}

/// Ends a process in bounded time: ask the group, wait a grace period, then
/// kill the group and reap the child.
async fn end_process(child: &mut Child, pid: u32) {
    #[cfg(unix)]
    process::terminate_group(pid);
    #[cfg(not(unix))]
    let _ = pid;
    if timeout(process::GRACE, child.wait()).await.is_ok() {
        return;
    }
    #[cfg(unix)]
    process::kill_group(pid);
    let _ = child.start_kill();
    // Bounded: shutdown can never hang on a process that will not die.
    let _ = timeout(process::REAP, child.wait()).await;
}

async fn process_exit(
    state: &mut State,
    adapter: AdapterId,
    reason: String,
    process_events: &mpsc::Sender<ProcessEvent>,
    events: &mpsc::Sender<ManagerEvent>,
) {
    let terminal = state.terminal.remove(&adapter);
    stop_adapter(state, &adapter).await;
    if !terminal {
        cancel_owned(state, &adapter, events).await;
    }
    emit(
        events,
        ManagerEvent::Exited {
            adapter: adapter.clone(),
            status: reason.clone(),
        },
    )
    .await;
    // Only a resident plugin is relaunched, and only when nobody stopped it.
    if let AdapterId::Resident(id) = &adapter {
        let attempts = state.restarts.entry(id.clone()).or_insert(0);
        if !terminal && !state.stopping && *attempts < MAX_RESTARTS && state.specs.contains_key(id)
        {
            *attempts += 1;
            let backoff = 250 * (1_u64 << (*attempts - 1));
            sleep(Duration::from_millis(backoff)).await;
            if let Err(reason) = spawn_adapter(state, adapter.clone(), process_events, events).await
            {
                reject(events, Some(adapter), reason).await;
            }
        }
    }
}

async fn fail_adapter(
    state: &mut State,
    adapter: &AdapterId,
    reason: &str,
    events: &mpsc::Sender<ManagerEvent>,
) {
    stop_adapter(state, adapter).await;
    cancel_owned(state, adapter, events).await;
    emit(
        events,
        ManagerEvent::Exited {
            adapter: adapter.clone(),
            status: reason.to_owned(),
        },
    )
    .await;
}

async fn cancel_owned(state: &mut State, adapter: &AdapterId, events: &mpsc::Sender<ManagerEvent>) {
    let transfers: Vec<String> = match adapter {
        AdapterId::Resident(plugin) => state
            .owned_transfers
            .remove(plugin)
            .map(|set| set.into_iter().collect())
            .unwrap_or_default(),
        AdapterId::Transfer { transfer_id, .. } => vec![transfer_id.clone()],
        // A settings window owns no transfer, so its exit cancels none.
        AdapterId::Settings(_) => Vec::new(),
    };
    for transfer_id in transfers {
        state
            .pending_ranges
            .retain(|(owned, _), _| owned != &transfer_id);
        emit(
            events,
            ManagerEvent::Cancelled {
                adapter: adapter.clone(),
                transfer_id,
            },
        )
        .await;
    }
}

/// Stops one process and waits for its tasks, in bounded time.
async fn stop_adapter(state: &mut State, adapter: &AdapterId) {
    let Some(mut runtime) = state.processes.remove(adapter) else {
        return;
    };
    drop(runtime.writer);
    if let Some(terminate) = runtime.terminate.take() {
        let _ = terminate.send(());
    }
    // Tasks are [writer, reader, stderr, waiter]. The waiter finishes once the
    // child has been killed and reaped. The readers must not be awaited for
    // end-of-file after that: a grandchild the plugin forked (a shell wrapper,
    // a graphics driver helper) inherits the pipe and keeps it open, and the
    // supervisor loop would then freeze, taking the clipboard and file
    // transfer plugins down with it.
    let waiter = runtime.tasks.pop();
    if let Some(waiter) = waiter {
        let _ = timeout(process::GRACE + process::REAP, waiter).await;
    }
    for task in runtime.tasks.drain(..) {
        task.abort();
        let _ = task.await;
    }
    // The group is ended even if the waiter timed out.
    #[cfg(unix)]
    process::kill_group(runtime.pid);
}

async fn send_ready(
    state: &mut State,
    adapter: &AdapterId,
    message: Message,
    events: &mpsc::Sender<ManagerEvent>,
) {
    let Some(runtime) = state.processes.get(adapter) else {
        reject(events, Some(adapter.clone()), "the plugin is not running").await;
        return;
    };
    if !runtime.ready() {
        reject(events, Some(adapter.clone()), "the plugin is not ready").await;
        return;
    }
    match runtime.writer.try_send(message) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            fail_adapter(state, adapter, "the plugin's input queue is full", events).await;
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            fail_adapter(state, adapter, "the plugin's writer has stopped", events).await;
        }
    }
}

/// Sends a message that is worthless once stale, dropping it if the plugin is
/// momentarily not reading.
///
/// A pointer event is out of date within a frame, and a plugin may
/// legitimately stop reading stdin for a while (starting its graphics, waiting
/// on vsync). Treating a full queue as a fault would kill a healthy plugin for
/// being busy, so only a closed pipe, which means the process is gone, counts
/// as failure.
async fn send_droppable(
    state: &mut State,
    adapter: &AdapterId,
    message: Message,
    events: &mpsc::Sender<ManagerEvent>,
) {
    let Some(runtime) = state.processes.get(adapter) else {
        return;
    };
    match runtime.writer.try_send(message) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            log::debug!("dropped a pointer event for a busy plugin");
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            fail_adapter(state, adapter, "the plugin's writer has stopped", events).await;
        }
    }
}

async fn send_settings(
    state: &mut State,
    adapter: &AdapterId,
    settings: Vec<(String, String)>,
    events: &mpsc::Sender<ManagerEvent>,
) {
    send_ready(
        state,
        adapter,
        Message::Settings { values: settings },
        events,
    )
    .await;
}

async fn emit(events: &mpsc::Sender<ManagerEvent>, event: ManagerEvent) {
    let _ = events.send(event).await;
}

async fn reject(
    events: &mpsc::Sender<ManagerEvent>,
    adapter: Option<AdapterId>,
    reason: impl Into<String>,
) {
    emit(
        events,
        ManagerEvent::Rejected {
            adapter,
            reason: reason.into(),
        },
    )
    .await;
}

fn valid_transfer_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Reads newline-terminated lines holding at most `limit` bytes of any one
/// line in memory.
///
/// Bytes past the limit are dropped up to the next newline and the line is
/// reported once as `[line too long]`, so a plugin that writes megabytes
/// without a newline costs a fixed buffer and the stream keeps being drained.
/// (A framed codec is not used because it reports end-of-stream after an
/// over-long line, which would stop the drain and block the plugin.)
struct BoundedLines<R> {
    reader: R,
    limit: usize,
    chunk: Box<[u8; 4096]>,
    start: usize,
    end: usize,
    line: Vec<u8>,
    overflowed: bool,
    eof: bool,
}

impl<R: tokio::io::AsyncRead + Unpin> BoundedLines<R> {
    fn new(reader: R, limit: usize) -> Self {
        Self {
            reader,
            limit,
            chunk: Box::new([0; 4096]),
            start: 0,
            end: 0,
            line: Vec::new(),
            overflowed: false,
            eof: false,
        }
    }

    /// The next line, or `None` at end of stream or on a read error. A final
    /// line without a newline is returned as well.
    async fn next_line(&mut self) -> Option<String> {
        use tokio::io::AsyncReadExt;
        loop {
            if self.start == self.end {
                if self.eof {
                    return self.finish();
                }
                match self.reader.read(&mut self.chunk[..]).await {
                    Ok(0) => {
                        self.eof = true;
                        continue;
                    }
                    Ok(read) => {
                        self.start = 0;
                        self.end = read;
                    }
                    Err(_) => {
                        self.eof = true;
                        self.start = 0;
                        self.end = 0;
                        continue;
                    }
                }
            }
            let available = &self.chunk[self.start..self.end];
            let newline = available.iter().position(|byte| *byte == b'\n');
            let taken = newline.unwrap_or(available.len());
            // Disjoint fields: the chunk is only read, the line is written.
            append_bounded(
                &mut self.line,
                &mut self.overflowed,
                self.limit,
                &available[..taken],
            );
            match newline {
                Some(_) => {
                    self.start += taken + 1;
                    return self.finish();
                }
                None => self.start += taken,
            }
        }
    }

    /// Ends the current line. `None` only when the stream ended with nothing
    /// pending.
    fn finish(&mut self) -> Option<String> {
        if self.overflowed {
            self.overflowed = false;
            self.line.clear();
            return Some("[line too long]".to_owned());
        }
        if self.line.is_empty() && self.eof {
            return None;
        }
        let text = String::from_utf8_lossy(&self.line).into_owned();
        self.line.clear();
        Some(text)
    }
}

/// Appends `bytes` to `line` without letting it exceed `limit`; anything
/// that did not fit marks the line as overflowed.
fn append_bounded(line: &mut Vec<u8>, overflowed: &mut bool, limit: usize, bytes: &[u8]) {
    let room = limit.saturating_sub(line.len());
    if bytes.len() > room {
        *overflowed = true;
    }
    line.extend_from_slice(&bytes[..bytes.len().min(room)]);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, path::Path};
    use syntra_plugin_api::{
        Edge, GNOME_COPIED_FILES_MIME, PROTOCOL_VERSION, PointerEvent, URI_LIST_MIME,
    };

    /// An over-long line is reported once and skipped, memory stays bounded,
    /// and reading resumes with the next line instead of ending.
    #[tokio::test]
    async fn bounded_lines_skip_an_overlong_line_and_keep_reading() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let producer = tokio::spawn(async move {
            writer.write_all(b"first\n").await.unwrap();
            let flood = vec![b'x'; 1_000_000];
            writer.write_all(&flood).await.unwrap();
            writer.write_all(b"\nsecond\n\nlast").await.unwrap();
        });
        let mut lines = BoundedLines::new(reader, 16);
        assert_eq!(lines.next_line().await.as_deref(), Some("first"));
        assert_eq!(lines.next_line().await.as_deref(), Some("[line too long]"));
        assert_eq!(lines.next_line().await.as_deref(), Some("second"));
        assert_eq!(lines.next_line().await.as_deref(), Some(""));
        assert_eq!(lines.next_line().await.as_deref(), Some("last"));
        assert_eq!(lines.next_line().await, None);
        assert!(
            lines.line.capacity() <= 64,
            "buffered {}",
            lines.line.capacity()
        );
        producer.await.unwrap();
    }

    fn file_clipboard() -> Capabilities {
        Capabilities {
            clipboard_read: true,
            paste: true,
            cancel: true,
            requires_live_mount: false,
            pointer_events: false,
            mime_types: vec![URI_LIST_MIME.into(), GNOME_COPIED_FILES_MIME.into()],
        }
    }

    fn pointer() -> Capabilities {
        Capabilities {
            pointer_events: true,
            ..Capabilities::default()
        }
    }

    fn hello_line(id: &str, capabilities: &Capabilities) -> String {
        let capabilities = serde_json::to_string(capabilities).unwrap();
        format!(
            r#"{{"type":"hello","data":{{"protocol_version":{PROTOCOL_VERSION},"adapter_id":"{id}","name":"Stub","capabilities":{capabilities}}}}}"#
        )
    }

    /// Writes an executable that prints `first`, then runs `rest`.
    ///
    /// A script written a moment ago can be refused with `ETXTBSY` while another
    /// thread forks with the write handle still open, so it is run once under
    /// `--probe` until the kernel lets it start.
    fn stub(dir: &Path, name: &str, first: &str, rest: &str) -> PathBuf {
        let path = dir.join(name);
        let body = format!(
            "#!/bin/sh\n[ \"$1\" = --probe ] && exit 0\nprintf '%s\\n' '{first}'\n{rest}\n"
        );
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        for _ in 0..200 {
            match std::process::Command::new(&path).arg("--probe").status() {
                Ok(_) => return path,
                Err(error) if error.raw_os_error() == Some(26) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("cannot run the stub plugin: {error}"),
            }
        }
        panic!("the stub plugin stayed busy for two seconds");
    }

    fn spec(executable: PathBuf, approved: Capabilities) -> LaunchSpec {
        LaunchSpec {
            executable,
            args: Vec::new(),
            approved,
            handshake_ids: Vec::new(),
            role: None,
            digest: String::new(),
            integrity: None,
        }
    }

    async fn next_event(events: &mut mpsc::Receiver<ManagerEvent>) -> ManagerEvent {
        tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("no supervisor event within 10s")
            .expect("supervisor stopped")
    }

    /// Waits for `Ready` of `plugin`, skipping lifecycle noise.
    async fn until_ready(events: &mut mpsc::Receiver<ManagerEvent>, plugin: &str) -> Grant {
        loop {
            match next_event(events).await {
                ManagerEvent::Ready(adapter, _, grant) if adapter.plugin() == plugin => {
                    return grant;
                }
                ManagerEvent::Exited { adapter, status } => {
                    panic!("{:?} exited early: {status}", adapter)
                }
                ManagerEvent::Rejected { reason, .. } => panic!("rejected: {reason}"),
                _ => {}
            }
        }
    }

    fn start(
        manager: &AdapterProcessManager,
        id: &str,
        spec: LaunchSpec,
        settings: Vec<(String, String)>,
    ) {
        manager
            .try_send(ManagerCommand::StartPlugin {
                id: id.into(),
                spec,
                settings,
            })
            .unwrap();
    }

    fn dir(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("syntra-supervisor-{name}-"))
            .tempdir()
            .unwrap()
    }

    /// Without any plugin the supervisor still starts, and shuts down: an
    /// absent provider never prevents input sharing.
    #[tokio::test]
    async fn a_supervisor_with_no_plugin_starts_and_stops() {
        let (manager, mut events) = AdapterProcessManager::start();
        manager.shutdown().await.unwrap();
        assert!(events.recv().await.is_none(), "nothing was ever launched");
    }

    /// A plugin with an id nobody has heard of, approved for the file
    /// clipboard, completes a handshake and is granted that role: no id is
    /// special.
    #[tokio::test]
    async fn a_third_party_plugin_with_a_new_id_is_granted_the_file_clipboard_role() {
        let directory = dir("third-party");
        let id = "totally-new-vendor";
        let executable = stub(
            directory.path(),
            "provider",
            &hello_line(id, &file_clipboard()),
            "cat >/dev/null",
        );
        let (manager, mut events) = AdapterProcessManager::start();
        start(&manager, id, spec(executable, file_clipboard()), Vec::new());
        let grant = until_ready(&mut events, id).await;
        assert!(grant.allows(PluginRole::FileClipboard));
        assert!(!grant.allows(PluginRole::Pointer));
        manager.shutdown().await.unwrap();
    }

    /// The arguments in the manifest are the arguments the process receives,
    /// in order, and its environment holds none of the daemon's secrets.
    #[tokio::test]
    async fn the_manifests_arguments_reach_the_process_and_secrets_do_not() {
        let directory = dir("argv");
        let report = directory.path().join("report");
        let id = "argv-check";
        let executable = stub(
            directory.path(),
            "provider",
            &hello_line(id, &pointer()),
            &format!(
                "printf '%s|' \"$@\" > '{report}'\nenv > '{report}.env'\ncat >/dev/null",
                report = report.display()
            ),
        );
        // Cargo supplies this variable to the test process, but the closed
        // plugin environment must not pass it through. No global mutation.
        assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());

        let mut launch = spec(executable, pointer());
        launch.args = vec!["--serve".into(), "--mode=a b".into(), "last".into()];
        let (manager, mut events) = AdapterProcessManager::start();
        start(&manager, id, launch, Vec::new());
        until_ready(&mut events, id).await;
        // The report is written before the first read of stdin.
        for _ in 0..100 {
            if report.exists() && Path::new(&format!("{}.env", report.display())).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            std::fs::read_to_string(&report).unwrap(),
            "--serve|--mode=a b|last|"
        );
        let environment = std::fs::read_to_string(format!("{}.env", report.display())).unwrap();
        assert!(
            !environment.contains("CARGO_MANIFEST_DIR"),
            "the plugin inherited a variable it is not allowed"
        );
        manager.shutdown().await.unwrap();
    }

    /// The handshake can narrow, never widen, what was approved: a pointer
    /// plugin that announces file clipboard too does not get it, and a frame it
    /// sends as that role stops it.
    #[tokio::test]
    async fn a_plugin_cannot_forge_frames_for_a_role_it_was_not_approved_for() {
        let directory = dir("forge");
        let id = "forger";
        let forged = r#"{"type":"clipboard_data","data":{"transfer_id":"abc","mime_type":"text/plain","value":"x"}}"#;
        // Announces everything, then sends a clipboard frame.
        let mut announced = file_clipboard();
        announced.pointer_events = true;
        let executable = stub(
            directory.path(),
            "provider",
            &hello_line(id, &announced),
            &format!("printf '%s\\n' '{forged}'\nsleep 30"),
        );
        let (manager, mut events) = AdapterProcessManager::start();
        start(&manager, id, spec(executable, pointer()), Vec::new());
        let grant = until_ready(&mut events, id).await;
        assert!(
            !grant.allows(PluginRole::FileClipboard),
            "the grant was narrowed"
        );

        loop {
            match next_event(&mut events).await {
                ManagerEvent::Message { .. } => panic!("a forged frame was forwarded"),
                ManagerEvent::Exited { adapter, .. } => {
                    assert_eq!(adapter, AdapterId::Resident(id.into()));
                    break;
                }
                _ => {}
            }
        }
        manager.shutdown().await.unwrap();
    }

    /// A frame the grant allows reaches the service; the same plugin's frame
    /// for another transfer role would not.
    #[tokio::test]
    async fn an_approved_role_frames_are_forwarded() {
        let directory = dir("forward");
        let id = "provider-with-copy";
        let copy = r#"{"type":"copy_manifest","data":{"transfer_id":"abc","operation":"copy","entries":[]}}"#;
        let executable = stub(
            directory.path(),
            "provider",
            &hello_line(id, &file_clipboard()),
            &format!("printf '%s\\n' '{copy}'\ncat >/dev/null"),
        );
        let (manager, mut events) = AdapterProcessManager::start();
        start(&manager, id, spec(executable, file_clipboard()), Vec::new());
        until_ready(&mut events, id).await;
        loop {
            if let ManagerEvent::Message { adapter, message } = next_event(&mut events).await {
                assert_eq!(adapter.plugin(), id);
                assert!(matches!(message, Message::CopyManifest(_)));
                break;
            }
        }
        manager.shutdown().await.unwrap();
    }

    /// A handshake naming another plugin, or a version this daemon does not
    /// speak, never becomes ready.
    #[tokio::test]
    async fn a_handshake_for_another_plugin_is_refused() {
        let directory = dir("wrong-id");
        let executable = stub(
            directory.path(),
            "provider",
            &hello_line("someone-else", &pointer()),
            "cat >/dev/null",
        );
        let (manager, mut events) = AdapterProcessManager::start();
        start(&manager, "mine", spec(executable, pointer()), Vec::new());
        loop {
            match next_event(&mut events).await {
                ManagerEvent::Ready(..) => panic!("an impostor became ready"),
                ManagerEvent::Exited { status, .. } => {
                    assert!(status.contains("handshake refused"), "{status}");
                    break;
                }
                _ => {}
            }
        }
        manager.shutdown().await.unwrap();
    }

    /// A manifest alias lets a plugin released before ids were enforced keep
    /// working.
    #[tokio::test]
    async fn a_declared_handshake_alias_is_accepted() {
        let directory = dir("alias");
        let executable = stub(
            directory.path(),
            "provider",
            &hello_line("gtk-clipboard", &file_clipboard()),
            "cat >/dev/null",
        );
        let mut launch = spec(executable, file_clipboard());
        launch.handshake_ids = vec!["gtk-clipboard".into()];
        let (manager, mut events) = AdapterProcessManager::start();
        start(&manager, "clipboard", launch, Vec::new());
        until_ready(&mut events, "clipboard").await;
        manager.shutdown().await.unwrap();
    }

    /// A plugin that never handshakes is not ready, and the failure is
    /// reported rather than leaving the daemon waiting.
    #[tokio::test]
    async fn a_missing_or_unrunnable_executable_is_rejected_not_fatal() {
        let (manager, mut events) = AdapterProcessManager::start();
        start(
            &manager,
            "ghost",
            spec(PathBuf::from("/nonexistent/syntra-plugin"), pointer()),
            Vec::new(),
        );
        match next_event(&mut events).await {
            ManagerEvent::Rejected { reason, .. } => {
                assert!(
                    !reason.contains("/nonexistent"),
                    "no path in the reason: {reason}"
                );
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
        manager.shutdown().await.unwrap();
    }

    /// Content that changed after it was approved is not launched, even by a
    /// relaunch that only has the cached launch.
    #[tokio::test]
    async fn changed_content_is_refused_at_launch() {
        use syntra_plugin_api::{ManifestLoad, PluginManifest, digest_package};
        let directory = dir("integrity");
        let id = "integrity";
        let executable = stub(
            directory.path(),
            "provider",
            &hello_line(id, &pointer()),
            "cat >/dev/null",
        );
        let manifest_text = format!(
            r#"{{"manifest_version":1,"protocol_version":1,"id":"{id}","name":"I","executable":"provider",
            "capabilities":{{"pointer_events":true,"mime_types":[]}}}}"#
        );
        let manifest = match PluginManifest::parse(&manifest_text).unwrap() {
            ManifestLoad::Valid(manifest) => *manifest,
            ManifestLoad::Unsupported { .. } => unreachable!(),
        };
        let manifest_path = directory.path().join("syntra-plugin.json");
        std::fs::write(&manifest_path, &manifest_text).unwrap();
        let approved = digest_package(&manifest, &manifest_path, &executable)
            .unwrap()
            .digest;

        let mut launch = spec(executable.clone(), pointer());
        launch.digest = approved;
        launch.integrity = Some(Integrity {
            manifest,
            manifest_path,
        });
        let (manager, mut events) = AdapterProcessManager::start();
        start(&manager, id, launch.clone(), Vec::new());
        until_ready(&mut events, id).await;

        // The program is replaced while it runs; the next launch must refuse.
        manager
            .try_send(ManagerCommand::StopPlugin { id: id.into() })
            .unwrap();
        loop {
            if matches!(next_event(&mut events).await, ManagerEvent::Stopped(_)) {
                break;
            }
        }
        std::fs::write(&executable, "#!/bin/sh\necho replaced\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        start(&manager, id, launch, Vec::new());
        loop {
            match next_event(&mut events).await {
                ManagerEvent::Rejected { reason, .. } => {
                    assert!(reason.contains("changed since it was approved"), "{reason}");
                    break;
                }
                ManagerEvent::Ready(..) => panic!("changed content was launched"),
                _ => {}
            }
        }
        manager.shutdown().await.unwrap();
    }

    /// Disabling stops the process and it is not relaunched; starting again
    /// runs a new generation, and anything the old one still says is dropped.
    #[tokio::test]
    async fn stopping_is_final_until_started_again_and_a_new_run_is_a_new_generation() {
        let directory = dir("generations");
        let id = "generation-check";
        let executable = stub(
            directory.path(),
            "provider",
            &hello_line(id, &pointer()),
            "cat >/dev/null",
        );
        let (manager, mut events) = AdapterProcessManager::start();
        start(
            &manager,
            id,
            spec(executable.clone(), pointer()),
            Vec::new(),
        );
        until_ready(&mut events, id).await;

        manager
            .try_send(ManagerCommand::StopPlugin { id: id.into() })
            .unwrap();
        loop {
            if matches!(next_event(&mut events).await, ManagerEvent::Stopped(_)) {
                break;
            }
        }
        // A deliberate stop is not relaunched: nothing starts within the first
        // retry window.
        let quiet = tokio::time::timeout(Duration::from_millis(1500), async {
            loop {
                if let Some(ManagerEvent::Started(..)) = events.recv().await {
                    return;
                }
            }
        })
        .await;
        assert!(quiet.is_err(), "a stopped plugin was started again");

        start(&manager, id, spec(executable, pointer()), Vec::new());
        until_ready(&mut events, id).await;
        manager.shutdown().await.unwrap();
    }

    /// Pointer events reach only a plugin granted the pointer role.
    #[tokio::test]
    async fn pointer_events_go_only_to_a_pointer_grant() {
        let directory = dir("pointer");
        let received = directory.path().join("received");
        let id = "listener";
        let executable = stub(
            directory.path(),
            "provider",
            &hello_line(id, &pointer()),
            &format!("cat > '{}'", received.display()),
        );
        let (manager, mut events) = AdapterProcessManager::start();
        start(&manager, id, spec(executable, pointer()), Vec::new());
        until_ready(&mut events, id).await;
        manager
            .try_send(ManagerCommand::PluginPointer {
                id: id.into(),
                event: PointerEvent::Entered {
                    edge: Edge::Left,
                    along: Some(0.5),
                    peer: "phone".into(),
                },
            })
            .unwrap();
        // Wait until the frame has been written, so shutdown cannot race it.
        let mut text = String::new();
        for _ in 0..200 {
            text = std::fs::read_to_string(&received).unwrap_or_default();
            if text.contains(r#""type":"pointer""#) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(text.contains(r#""type":"pointer""#), "{text}");

        // A plugin approved only for the file clipboard is sent nothing.
        let other = "files-only";
        let other_received = directory.path().join("other-received");
        let other_exe = stub(
            directory.path(),
            "other",
            &hello_line(other, &file_clipboard()),
            &format!("cat > '{}'", other_received.display()),
        );
        start(
            &manager,
            other,
            spec(other_exe, file_clipboard()),
            Vec::new(),
        );
        until_ready(&mut events, other).await;
        manager
            .try_send(ManagerCommand::PluginPointer {
                id: other.into(),
                event: PointerEvent::Left {
                    edge: Edge::Right,
                    along: None,
                    peer: "phone".into(),
                },
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        manager.shutdown().await.unwrap();
        let other_text = std::fs::read_to_string(&other_received).unwrap_or_default();
        assert!(
            !other_text.contains("pointer"),
            "a file clipboard plugin was sent pointer events: {other_text}"
        );
    }

    /// Stopping a plugin ends the whole process group, so a descendant it
    /// started does not outlive it, and shutdown does not hang on it.
    #[tokio::test]
    async fn shutdown_ends_a_grandchild_the_plugin_started() {
        let directory = dir("descendants");
        let pid_file = directory.path().join("grandchild.pid");
        let id = "forker";
        let executable = stub(
            directory.path(),
            "provider",
            &hello_line(id, &pointer()),
            // The grandchild inherits the pipes and sleeps for ever.
            &format!("sleep 300 &\necho $! > '{}'\nwait", pid_file.display()),
        );
        let (manager, mut events) = AdapterProcessManager::start();
        start(&manager, id, spec(executable, pointer()), Vec::new());
        until_ready(&mut events, id).await;
        for _ in 0..100 {
            if pid_file.exists() && !std::fs::read_to_string(&pid_file).unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let grandchild: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        let started = std::time::Instant::now();
        manager.shutdown().await.unwrap();
        assert!(
            started.elapsed() < process::GRACE + process::REAP + Duration::from_secs(2),
            "shutdown hung on a descendant"
        );
        // The process is gone: signal 0 only checks existence.
        let mut gone = false;
        for _ in 0..100 {
            // SAFETY: `kill` with signal 0 performs only an existence check.
            if unsafe { libc::kill(grandchild, 0) } != 0 {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(gone, "the plugin's descendant survived shutdown");
    }

    /// What the plugin writes to its error stream is kept sanitised and
    /// bounded, and reaches the service as diagnostics, not as raw log.
    #[tokio::test]
    async fn error_stream_lines_become_sanitised_bounded_diagnostics() {
        let directory = dir("stderr");
        let id = "chatty";
        let executable = stub(
            directory.path(),
            "provider",
            &hello_line(id, &pointer()),
            "printf '\\033[31mred\\033[0m text\\n' >&2\ncat >/dev/null",
        );
        let (manager, mut events) = AdapterProcessManager::start();
        start(&manager, id, spec(executable, pointer()), Vec::new());
        until_ready(&mut events, id).await;
        loop {
            if let ManagerEvent::Diagnostics { plugin, lines } = next_event(&mut events).await {
                assert_eq!(plugin, id);
                assert!(
                    lines.iter().all(|line| !line.contains('\u{1b}')),
                    "{lines:?}"
                );
                assert!(lines.iter().any(|line| line.contains("red")), "{lines:?}");
                break;
            }
        }
        manager.shutdown().await.unwrap();
    }

    /// A plugin that floods its error stream with megabytes and no newline
    /// cannot make the daemon buffer them, and the stream keeps being drained:
    /// a later ordinary line still arrives and shutdown still completes.
    #[tokio::test]
    async fn unterminated_error_stream_flood_is_bounded_and_does_not_wedge_the_plugin() {
        let directory = dir("stderr-flood");
        let id = "flooder";
        let executable = stub(
            directory.path(),
            "provider",
            &hello_line(id, &pointer()),
            // ~5 MB without a newline, then one normal line, then idle.
            "head -c 5000000 /dev/zero | tr '\\0' x >&2\nprintf '\\nafter the flood\\n' >&2\ncat >/dev/null",
        );
        let (manager, mut events) = AdapterProcessManager::start();
        start(&manager, id, spec(executable, pointer()), Vec::new());
        until_ready(&mut events, id).await;
        let mut longest = 0;
        loop {
            if let ManagerEvent::Diagnostics { plugin, lines } = next_event(&mut events).await {
                assert_eq!(plugin, id);
                longest = longest.max(
                    lines
                        .iter()
                        .map(|line| line.chars().count())
                        .max()
                        .unwrap_or(0),
                );
                if lines.iter().any(|line| line.contains("after the flood")) {
                    break;
                }
            }
        }
        assert!(
            longest <= process::DIAGNOSTIC_WIDTH + 1,
            "a flooded line was kept at {longest} characters"
        );
        tokio::time::timeout(Duration::from_secs(10), manager.shutdown())
            .await
            .expect("shutdown must not hang after a flood")
            .unwrap();
    }

    fn settings_window(
        manager: &AdapterProcessManager,
        id: &str,
        spec: LaunchSpec,
        theme: Vec<(String, String)>,
    ) {
        manager
            .try_send(ManagerCommand::OpenSettingsWindow {
                id: id.into(),
                spec,
                theme,
            })
            .unwrap();
    }

    /// The standalone settings window is the plugin's own program, run as the
    /// manager runs any plugin: the manifest's arguments come first and
    /// `--settings` last, the theme arrives in `SYNTRA_PLUGIN_THEME`, and the
    /// daemon's secrets are not inherited. It does not handshake, so what it
    /// prints is never parsed (a line that is not a protocol frame must not
    /// fail it), and its error stream still becomes bounded diagnostics.
    #[tokio::test]
    async fn a_standalone_settings_window_runs_with_manifest_args_theme_and_a_clean_environment() {
        let directory = dir("settings-window");
        let record = directory.path().join("record");
        let id = "windowed";
        // A known Cargo variable is present in the parent, never in the
        // plugin. Do not mutate a shared environment while other tests spawn.
        assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
        let executable = stub(
            directory.path(),
            "provider",
            // Not a protocol frame: a protocol process would be failed for it.
            "this is not json",
            &format!(
                "{{ printf 'ARGS:'; for a in \"$@\"; do printf ' [%s]' \"$a\"; done; printf '\\n';\n\
                 printf 'THEME:%s\\n' \"$SYNTRA_PLUGIN_THEME\";\n\
                 printf 'CARGO:%s\\n' \"${{CARGO_MANIFEST_DIR-unset}}\"; }} > '{}'\n\
                 echo window-diagnostic >&2\n\
                 sleep 300",
                record.display()
            ),
        );
        let mut launch = spec(executable, pointer());
        launch.args = vec!["--serve".into(), "--quiet".into()];
        let (manager, mut events) = AdapterProcessManager::start();
        settings_window(
            &manager,
            id,
            launch,
            vec![("accent".into(), "#ff0000".into())],
        );

        let mut started = false;
        let mut diagnostic = false;
        for _ in 0..200 {
            match next_event(&mut events).await {
                ManagerEvent::Started(adapter, pid) => {
                    assert_eq!(adapter, AdapterId::Settings(id.into()));
                    assert!(pid > 0);
                    started = true;
                }
                ManagerEvent::Ready(..) => panic!("a settings window must never handshake"),
                ManagerEvent::Exited { status, .. } => {
                    panic!("the window was failed instead of left open: {status}")
                }
                ManagerEvent::Rejected { reason, .. } => panic!("rejected: {reason}"),
                ManagerEvent::Diagnostics { plugin, lines } => {
                    assert_eq!(plugin, id);
                    diagnostic = lines.iter().any(|line| line.contains("window-diagnostic"));
                }
                _ => {}
            }
            if started && diagnostic && record.exists() {
                break;
            }
        }
        assert!(
            started && diagnostic,
            "started={started} diagnostic={diagnostic}"
        );
        let recorded = std::fs::read_to_string(&record).unwrap();
        assert!(
            recorded.contains("ARGS: [--serve] [--quiet] [--settings]"),
            "{recorded}"
        );
        assert!(recorded.contains("THEME:accent=#ff0000"), "{recorded}");
        assert!(
            recorded.contains("CARGO:unset"),
            "the daemon's environment leaked: {recorded}"
        );
        manager.shutdown().await.unwrap();
    }

    /// A settings window is gated by the approved content at the moment it is
    /// spawned: a package that no longer matches its approval is refused and
    /// nothing is run, and the refusal does not disturb the plugin.
    #[tokio::test]
    async fn a_standalone_settings_window_is_refused_when_the_approved_content_changed() {
        let directory = dir("settings-integrity");
        let ran = directory.path().join("ran");
        let id = "tampered";
        let executable = stub(
            directory.path(),
            "provider",
            "",
            &format!("touch '{}'\nsleep 300", ran.display()),
        );
        let manifest_path = directory.path().join("syntra-plugin.json");
        let manifest = match syntra_plugin_api::PluginManifest::parse(&format!(
            r#"{{"manifest_version":1,"protocol_version":{PROTOCOL_VERSION},"id":"{id}","name":"T","executable":"provider","capabilities":{{"pointer_events":true,"mime_types":[]}}}}"#
        ))
        .unwrap()
        {
            syntra_plugin_api::ManifestLoad::Valid(manifest) => *manifest,
            other => panic!("the fixture manifest must load: {other:?}"),
        };
        std::fs::write(&manifest_path, "{}").unwrap();
        let mut launch = spec(executable, pointer());
        // A digest nothing can match: the files on disk are "not what was approved".
        launch.digest = "00".repeat(32);
        launch.integrity = Some(Integrity {
            manifest,
            manifest_path,
        });
        let (manager, mut events) = AdapterProcessManager::start();
        settings_window(&manager, id, launch, Vec::new());
        loop {
            match next_event(&mut events).await {
                ManagerEvent::Rejected { adapter, reason } => {
                    assert_eq!(adapter, Some(AdapterId::Settings(id.into())));
                    assert!(!reason.is_empty());
                    break;
                }
                ManagerEvent::Started(..) => panic!("changed content was started"),
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!ran.exists(), "the refused window's code ran");
        manager.shutdown().await.unwrap();
    }

    /// Disabling, revoking and uninstalling all stop the plugin by id, which
    /// must also end its settings window and everything that window started;
    /// the window's exit must not be reported as a plugin failure or restart.
    #[tokio::test]
    async fn stopping_a_plugin_ends_its_settings_window_and_that_windows_descendants() {
        let directory = dir("settings-stop");
        let pid_file = directory.path().join("grandchild.pid");
        let id = "stoppable";
        let executable = stub(
            directory.path(),
            "provider",
            "",
            &format!("sleep 300 &\necho $! > '{}'\nwait", pid_file.display()),
        );
        let (manager, mut events) = AdapterProcessManager::start();
        settings_window(&manager, id, spec(executable, pointer()), Vec::new());
        for _ in 0..200 {
            if pid_file.exists() && !std::fs::read_to_string(&pid_file).unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let grandchild: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        manager
            .try_send(ManagerCommand::StopPlugin { id: id.into() })
            .unwrap();
        let mut stopped = false;
        let mut relaunched = false;
        for _ in 0..200 {
            match tokio::time::timeout(Duration::from_millis(100), events.recv()).await {
                Ok(Some(ManagerEvent::Stopped(adapter))) => {
                    assert_eq!(adapter, AdapterId::Settings(id.into()));
                    stopped = true;
                }
                Ok(Some(ManagerEvent::Started(..))) if stopped => relaunched = true,
                Ok(Some(_)) => {}
                Ok(None) | Err(_) if stopped => break,
                Ok(None) | Err(_) => {}
            }
        }
        assert!(
            stopped,
            "the settings window was not stopped with its plugin"
        );
        assert!(!relaunched, "a stopped settings window came back");
        let mut gone = false;
        for _ in 0..100 {
            // SAFETY: `kill` with signal 0 performs only an existence check.
            if unsafe { libc::kill(grandchild, 0) } != 0 {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(gone, "the settings window's descendant survived the stop");
        manager.shutdown().await.unwrap();
    }
}
