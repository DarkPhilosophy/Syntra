use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures::StreamExt;
use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use syntra_plugin_api::{
    GNOME_COPIED_FILES_MIME, Message, MountReady, PROTOCOL_VERSION, RangeResponse, Released,
    RemoteManifest, URI_LIST_MIME, Unmounted,
};
use thiserror::Error;
use tokio::{
    io::AsyncWriteExt,
    process::{Child, Command},
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::sleep,
};
use tokio_util::codec::{FramedRead, LinesCodec};

const CHANNEL_CAPACITY: usize = 64;
const MAX_LINE_LENGTH: usize = 2 * 1024 * 1024;
const MAX_PENDING_RANGES: usize = 256;
const MAX_GTK_RESTARTS: u8 = 3;
const MAX_PLUGIN_RESTARTS: u8 = 3;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum AdapterId {
    Gtk,
    Fuse {
        transfer_id: String,
    },
    /// A plugin discovered at run time, identified by its manifest id.
    ///
    /// Unlike the clipboard and FUSE plugins, it takes part in no transfer: it
    /// is told about settings and pointer events and reports nothing back.
    Plugin(String),
}

#[derive(Clone, Debug)]
pub(crate) struct AdapterPaths {
    gtk_clipboard: Option<PathBuf>,
    fuse: Option<PathBuf>,
}

impl AdapterPaths {
    /// Probes each built-in helper independently.
    ///
    /// A helper that is missing or not runnable is logged and left out; it
    /// never prevents the supervisor from starting. Both helpers are Linux
    /// packaging, so on Windows and macOS neither exists, and a discovered
    /// plugin such as the edge glow must still be launchable there.
    pub(crate) fn new(gtk_clipboard: PathBuf, fuse: PathBuf) -> Self {
        let usable = |name: &'static str, path: PathBuf| match validate_executable(name, &path) {
            Ok(()) => Some(path),
            Err(error) => {
                log::warn!("{error}");
                None
            }
        };
        Self {
            gtk_clipboard: usable("GTK clipboard", gtk_clipboard),
            fuse: usable("FUSE", fuse),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ManagerCommand {
    RemoteManifest(RemoteManifest),
    RangeResponse(RangeResponse),
    PublishFileClipboard(syntra_plugin_api::PublishFileClipboard),
    /// Launch a discovered plugin and keep it running.
    ///
    /// `settings` is what the plugin is told once it has handshaken.
    StartPlugin {
        id: String,
        executable: PathBuf,
        settings: Vec<(String, String)>,
    },
    /// Tell a running generic plugin its settings changed.
    PluginSettings {
        id: String,
        settings: Vec<(String, String)>,
    },
    /// Forward a pointer event to a running generic plugin.
    PluginPointer {
        id: String,
        event: syntra_plugin_api::PointerEvent,
    },
    ClipboardData {
        transfer_id: String,
        mime_type: String,
        value: String,
    },
    Released(Released),
    Unmounted(Unmounted),
    Cancel {
        adapter: AdapterId,
        transfer_id: String,
    },
    /// Stop a plugin's processes and do not relaunch them.
    ///
    /// Used when a client disables a plugin: the entry must stop being a
    /// running process, not merely be labelled as off.
    StopPlugin {
        plugin: PluginTarget,
    },
    /// Stop and immediately relaunch a plugin's processes.
    ///
    /// The remedy for one that is running but unresponsive.
    RestartPlugin {
        plugin: PluginTarget,
    },
    Shutdown,
}

/// A plugin as a client addresses it, rather than as the supervisor keys it.
///
/// The supervisor keys FUSE adapters per transfer, but a client acts on the
/// plugin as a whole, so a single request may affect several processes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PluginTarget {
    /// The file-clipboard plugin.
    Clipboard,
    /// Every FUSE process, whichever transfers they serve.
    Fuse,
    /// A discovered plugin, by manifest id.
    Generic(String),
}

impl PluginTarget {
    /// Whether `adapter` belongs to this plugin.
    fn matches(&self, adapter: &AdapterId) -> bool {
        match (self, adapter) {
            (PluginTarget::Clipboard, AdapterId::Gtk) => true,
            (PluginTarget::Fuse, AdapterId::Fuse { .. }) => true,
            (PluginTarget::Generic(id), AdapterId::Plugin(owned)) => id == owned,
            _ => false,
        }
    }
}

#[derive(Debug)]
pub(crate) enum ManagerEvent {
    /// The plugin process has been launched but has not handshaken yet.
    ///
    /// Carries the process id so a user can confirm in a task manager that
    /// the plugin they see listed is a real running process, rather than an
    /// entry inferred from a file sitting in a directory.
    Started(AdapterId, u32),
    /// The plugin completed its handshake and is answering.
    ///
    /// Carries what the plugin declared about itself, which supersedes
    /// anything read from a manifest file.
    Ready(AdapterId, Option<syntra_plugin_api::PluginMetadata>),
    /// The plugin was stopped because a client asked for it.
    ///
    /// Distinct from `Exited`: a deliberate stop is not a failure, and
    /// reporting it as one would show a red state for an action the user
    /// just took.
    Stopped(AdapterId),
    Message {
        adapter: AdapterId,
        message: Message,
    },
    Cancelled {
        adapter: AdapterId,
        transfer_id: String,
    },
    Rejected {
        adapter: Option<AdapterId>,
        reason: String,
    },
    Exited {
        adapter: AdapterId,
        status: String,
    },
}

/// Why supervising an out-of-process plugin failed.
///
/// Public because it is reachable through
/// [`ServiceError::Adapter`](crate::service::ServiceError::Adapter); a caller
/// that matches on it has to be able to name it.
#[derive(Debug, Error)]
pub enum ManagerError {
    /// The plugin executable is missing or not runnable.
    ///
    /// Not fatal: the daemon logs it and continues without that capability.
    #[error("{name} adapter executable {path:?} is invalid: {source}")]
    InvalidExecutable {
        /// Human-readable plugin name, used in the message.
        name: &'static str,
        /// Path that was probed.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// The supervisor is not draining commands fast enough.
    ///
    /// Commands are dropped rather than queued without bound, so a wedged
    /// plugin cannot stall the service event loop.
    #[error("adapter manager command queue is full")]
    QueueFull,
    /// The supervisor task has exited; no plugin is reachable.
    #[error("adapter manager has stopped")]
    Stopped,
    /// The supervisor task panicked or was cancelled.
    #[error("adapter manager task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

pub(crate) struct AdapterProcessManager {
    commands: mpsc::Sender<ManagerCommand>,
    task: Option<JoinHandle<()>>,
}

impl AdapterProcessManager {
    pub(crate) fn start(paths: AdapterPaths) -> (Self, mpsc::Receiver<ManagerEvent>) {
        let (commands, command_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (events, event_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(run_manager(paths, command_rx, events));
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
        // Aborting the supervisor is deliberately avoided: it owns kill-on-drop children and
        // must be allowed to reap them. The runtime joins it during orderly shutdown.
    }
}

struct AdapterRuntime {
    generation: u64,
    writer: mpsc::Sender<Message>,
    terminate: Option<oneshot::Sender<()>>,
    tasks: Vec<JoinHandle<()>>,
    ready: bool,
}

#[derive(Debug)]
enum ProcessEvent {
    Line {
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
    paths: AdapterPaths,
    processes: HashMap<AdapterId, AdapterRuntime>,
    pending_ranges: HashMap<(String, u64), PendingRange>,
    pending_messages: HashMap<AdapterId, Message>,
    gtk_transfers: HashSet<String>,
    next_generation: u64,
    terminal: HashSet<AdapterId>,
    gtk_restarts: u8,
    /// Executable of each generic plugin that has been started, by id, kept so
    /// a crash or a restart relaunches the same file.
    plugin_paths: HashMap<String, PathBuf>,
    /// Last settings sent per generic plugin, replayed after every launch.
    plugin_settings: HashMap<String, Vec<(String, String)>>,
    /// Automatic relaunches used so far per generic plugin.
    plugin_restarts: HashMap<String, u8>,
    stopping: bool,
}

async fn run_manager(
    paths: AdapterPaths,
    mut commands: mpsc::Receiver<ManagerCommand>,
    events: mpsc::Sender<ManagerEvent>,
) {
    let (process_events, mut process_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let mut state = State {
        paths,
        processes: HashMap::new(),
        pending_ranges: HashMap::new(),
        pending_messages: HashMap::new(),
        gtk_transfers: HashSet::new(),
        terminal: HashSet::new(),
        next_generation: 1,
        gtk_restarts: 0,
        plugin_paths: HashMap::new(),
        plugin_settings: HashMap::new(),
        plugin_restarts: HashMap::new(),
        stopping: false,
    };

    if let Err(reason) = spawn_adapter(&mut state, AdapterId::Gtk, &process_events, &events).await {
        emit(
            &events,
            ManagerEvent::Rejected {
                adapter: Some(AdapterId::Gtk),
                reason,
            },
        )
        .await;
    }

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

/// Every live adapter belonging to `plugin`.
///
/// Collected before mutating, because stopping an adapter removes it from the
/// map being iterated.
fn matching_adapters(state: &State, plugin: &PluginTarget) -> Vec<AdapterId> {
    state
        .processes
        .keys()
        .filter(|adapter| plugin.matches(adapter))
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
        ManagerCommand::PublishFileClipboard(publication) => {
            send_ready(
                state,
                &AdapterId::Gtk,
                Message::PublishFileClipboard(publication),
                events,
            )
            .await;
        }
        ManagerCommand::ClipboardData {
            transfer_id,
            mime_type,
            value,
        } => {
            send_ready(
                state,
                &AdapterId::Gtk,
                Message::ClipboardData {
                    transfer_id,
                    mime_type,
                    value,
                },
                events,
            )
            .await;
        }
        ManagerCommand::RemoteManifest(manifest) => {
            let transfer_id = manifest.transfer_id.clone();
            if !valid_transfer_id(&transfer_id) {
                reject(events, None, "invalid transfer id").await;
                return;
            }
            let adapter = AdapterId::Fuse {
                transfer_id: transfer_id.clone(),
            };
            if state.processes.contains_key(&adapter) {
                reject(events, Some(adapter), "duplicate FUSE transfer").await;
                return;
            }
            match spawn_adapter(state, adapter.clone(), process_events, events).await {
                Ok(()) => {
                    state
                        .pending_messages
                        .insert(adapter, Message::RemoteManifest(manifest));
                }
                Err(reason) => reject(events, Some(adapter), reason).await,
            }
        }
        ManagerCommand::RangeResponse(response) => {
            let adapter = AdapterId::Fuse {
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
        ManagerCommand::Released(released) => {
            if !state.gtk_transfers.remove(&released.transfer_id) {
                reject(
                    events,
                    Some(AdapterId::Gtk),
                    "release for unknown GTK transfer",
                )
                .await;
                return;
            }
            send_ready(state, &AdapterId::Gtk, Message::Released(released), events).await;
        }
        ManagerCommand::Unmounted(unmounted) => {
            let adapter = AdapterId::Fuse {
                transfer_id: unmounted.transfer_id.clone(),
            };
            if !state.processes.contains_key(&adapter) {
                reject(events, Some(adapter), "unmount for unknown FUSE transfer").await;
                return;
            }
            send_ready(state, &adapter, Message::Unmounted(unmounted), events).await;
        }
        ManagerCommand::Cancel {
            adapter,
            transfer_id,
        } => {
            let owns = match &adapter {
                AdapterId::Gtk => state.gtk_transfers.remove(&transfer_id),
                AdapterId::Fuse { transfer_id: owned } => {
                    owned == &transfer_id && state.processes.contains_key(&adapter)
                }
                // A generic plugin takes part in no transfer.
                AdapterId::Plugin(_) => false,
            };
            if !owns {
                reject(events, Some(adapter), "cancel for unowned transfer").await;
                return;
            }
            send_ready(state, &adapter, Message::Cancel { transfer_id }, events).await;
        }
        ManagerCommand::StopPlugin { plugin } => {
            for adapter in matching_adapters(state, &plugin) {
                // Marked terminal first, so the exit is not treated as a
                // crash and does not trigger the relaunch path.
                state.terminal.insert(adapter.clone());
                stop_adapter(state, &adapter).await;
                emit(events, ManagerEvent::Stopped(adapter)).await;
            }
        }
        ManagerCommand::RestartPlugin { plugin } => {
            for adapter in matching_adapters(state, &plugin) {
                state.terminal.insert(adapter.clone());
                stop_adapter(state, &adapter).await;
            }
            if state.stopping {
                return;
            }
            match plugin {
                // Only the clipboard plugin is long-running among the
                // built-ins. A FUSE process exists solely for a transfer, so
                // relaunching one outside a transfer would create a process
                // with nothing to serve.
                PluginTarget::Clipboard => {
                    if let Err(reason) =
                        spawn_adapter(state, AdapterId::Gtk, process_events, events).await
                    {
                        reject(events, Some(AdapterId::Gtk), reason).await;
                    }
                }
                PluginTarget::Fuse => {}
                PluginTarget::Generic(id) => {
                    let adapter = AdapterId::Plugin(id.clone());
                    // A restart is the user's remedy for a stuck plugin, so it
                    // earns a fresh set of automatic retries.
                    state.terminal.remove(&adapter);
                    state.plugin_restarts.remove(&id);
                    if let Err(reason) =
                        spawn_adapter(state, adapter.clone(), process_events, events).await
                    {
                        reject(events, Some(adapter), reason).await;
                    }
                }
            }
        }
        ManagerCommand::StartPlugin {
            id,
            executable,
            settings,
        } => {
            let adapter = AdapterId::Plugin(id.clone());
            if state.processes.contains_key(&adapter) {
                // Already running; a start request is how a client re-asserts
                // the settings, not a reason to launch a second copy.
                state.plugin_settings.insert(id, settings.clone());
                send_settings(state, &adapter, settings, events).await;
                return;
            }
            state.terminal.remove(&adapter);
            state.plugin_paths.insert(id.clone(), executable);
            state.plugin_settings.insert(id.clone(), settings);
            state.plugin_restarts.remove(&id);
            if let Err(reason) = spawn_adapter(state, adapter.clone(), process_events, events).await
            {
                reject(events, Some(adapter), reason).await;
            }
        }
        ManagerCommand::PluginSettings { id, settings } => {
            let adapter = AdapterId::Plugin(id.clone());
            state.plugin_settings.insert(id, settings.clone());
            // A plugin that is not running has nothing to tell; it will be
            // given the stored values when it next starts.
            if state.processes.get(&adapter).is_some_and(|run| run.ready) {
                send_settings(state, &adapter, settings, events).await;
            }
        }
        ManagerCommand::PluginPointer { id, event } => {
            let adapter = AdapterId::Plugin(id);
            // Pointer events are high-rate and perishable. One for a plugin
            // that is still starting, or one it has no room for, is dropped:
            // never queued, and never a reason to fail a healthy plugin.
            if state.processes.get(&adapter).is_some_and(|run| run.ready) {
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
    match event {
        ProcessEvent::Line {
            adapter,
            generation,
            line,
        } => {
            if state
                .processes
                .get(&adapter)
                .is_none_or(|runtime| runtime.generation != generation)
            {
                return;
            }
            let message = match Message::decode_line(&line) {
                Ok(message) => message,
                Err(error) => {
                    fail_adapter(
                        state,
                        &adapter,
                        &format!("malformed adapter message: {error}"),
                        events,
                    )
                    .await;
                    return;
                }
            };
            let ready = state
                .processes
                .get(&adapter)
                .is_some_and(|runtime| runtime.ready);
            if !ready {
                if validate_hello(&adapter, &message).is_err() {
                    fail_adapter(state, &adapter, "invalid or missing adapter Hello", events).await;
                    return;
                }
                if let Some(runtime) = state.processes.get_mut(&adapter) {
                    runtime.ready = true;
                }
                // What the process says about itself is authoritative; a
                // manifest file is only how it was discovered.
                let declared = match &message {
                    Message::Hello { metadata, .. } => metadata.clone(),
                    _ => None,
                };
                emit(events, ManagerEvent::Ready(adapter.clone(), declared)).await;
                if let Some(pending) = state.pending_messages.remove(&adapter) {
                    send_ready(state, &adapter, pending, events).await;
                }
                // A generic plugin starts from the user's saved values, so it
                // never has to show defaults for a moment and then jump.
                if let AdapterId::Plugin(id) = &adapter {
                    let settings = state.plugin_settings.get(id).cloned().unwrap_or_default();
                    send_settings(state, &adapter, settings, events).await;
                }
                return;
            }
            if let Err(reason) = validate_inbound(state, &adapter, &message) {
                fail_adapter(state, &adapter, &reason, events).await;
                return;
            }
            if matches!(adapter, AdapterId::Fuse { .. })
                && matches!(
                    message,
                    Message::Completed(_) | Message::Cancelled(_) | Message::Unmounted(_)
                )
            {
                state.terminal.insert(adapter.clone());
            }
            if let Message::Released(released) = &message {
                state.gtk_transfers.remove(&released.transfer_id);
            }
            if let Message::CopyManifest(manifest) = &message {
                state.gtk_transfers.insert(manifest.transfer_id.clone());
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
            if state
                .processes
                .get(&adapter)
                .is_none_or(|runtime| runtime.generation != generation)
            {
                return;
            }
            process_exit(state, adapter, reason, process_events, events).await;
        }
    }
}

fn validate_hello(adapter: &AdapterId, message: &Message) -> Result<(), ()> {
    let Message::Hello {
        protocol_version,
        adapter_id,
        capabilities,
        ..
    } = message
    else {
        return Err(());
    };
    if let AdapterId::Plugin(expected) = adapter {
        // A generic plugin is accepted for what it declares, not for the
        // clipboard capabilities the built-ins must have. It must still name
        // itself with the id it was launched under, so one plugin cannot
        // answer for another.
        return if *protocol_version == PROTOCOL_VERSION
            && adapter_id == expected
            && capabilities.pointer_events
        {
            Ok(())
        } else {
            Err(())
        };
    }
    let expected_id = match adapter {
        AdapterId::Gtk => "gtk-clipboard",
        AdapterId::Fuse { .. } => "fuse",
        AdapterId::Plugin(_) => return Err(()),
    };
    if *protocol_version != PROTOCOL_VERSION
        || adapter_id != expected_id
        || !capabilities.clipboard_read
        || !capabilities.paste
        || !capabilities.cancel
        || !capabilities.requires_live_mount
        || !capabilities
            .mime_types
            .iter()
            .any(|mime| mime == URI_LIST_MIME)
        || !capabilities
            .mime_types
            .iter()
            .any(|mime| mime == GNOME_COPIED_FILES_MIME)
    {
        return Err(());
    }
    Ok(())
}

fn validate_inbound(
    state: &mut State,
    adapter: &AdapterId,
    message: &Message,
) -> Result<(), String> {
    // A generic plugin only listens. Checked before anything else because a
    // frame carrying a transfer id would otherwise fall through the built-in
    // adapters' whitelists and be accepted as a real clipboard or transfer
    // event, letting any discovered plugin inject file transfers.
    if matches!(adapter, AdapterId::Plugin(_)) {
        return if matches!(message, Message::Error { .. }) {
            Ok(())
        } else {
            Err("unexpected message from a plugin that only receives".into())
        };
    }
    let transfer = message_transfer_id(message);
    if let Some(transfer_id) = transfer {
        if !valid_transfer_id(transfer_id) {
            return Err("adapter emitted invalid transfer id".into());
        }
        match adapter {
            AdapterId::Gtk
                if !matches!(
                    message,
                    Message::CopyManifest(_)
                        | Message::PasteDestination(_)
                        | Message::Progress(_)
                        | Message::Completed(_)
                        | Message::Cancelled(_)
                        | Message::Released(_)
                        | Message::Error { .. }
                        | Message::ClipboardData { .. }
                ) =>
            {
                return Err("unexpected message from GTK adapter".into());
            }
            AdapterId::Fuse { transfer_id: owned } if transfer_id != owned => {
                return Err("FUSE adapter emitted a message for another transfer".into());
            }
            AdapterId::Fuse { .. }
                if !matches!(
                    message,
                    Message::RangeRequest(_)
                        | Message::MountReady(_)
                        | Message::Progress(_)
                        | Message::Completed(_)
                        | Message::Cancelled(_)
                        | Message::Unmounted(_)
                        | Message::Error { .. }
                ) =>
            {
                return Err("unexpected message from FUSE adapter".into());
            }
            _ => {}
        }
    } else if !matches!(message, Message::Error { .. }) {
        return Err("unexpected uncorrelated adapter message".into());
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
    if let Message::MountReady(MountReady {
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
        | Message::Pointer(_) => None,
    }
}

async fn spawn_adapter(
    state: &mut State,
    adapter: AdapterId,
    process_events: &mpsc::Sender<ProcessEvent>,
    events: &mpsc::Sender<ManagerEvent>,
) -> Result<(), String> {
    let executable = match &adapter {
        AdapterId::Gtk => state
            .paths
            .gtk_clipboard
            .clone()
            .ok_or_else(|| "the clipboard plugin is not installed".to_owned())?,
        AdapterId::Fuse { .. } => state
            .paths
            .fuse
            .clone()
            .ok_or_else(|| "the file transfer plugin is not installed".to_owned())?,
        AdapterId::Plugin(id) => state
            .plugin_paths
            .get(id)
            .cloned()
            .ok_or_else(|| format!("plugin {id} has no registered executable"))?,
    };
    let generation = state.next_generation;
    state.next_generation = state.next_generation.wrapping_add(1).max(1);
    let (runtime, pid) = launch(
        executable,
        adapter.clone(),
        generation,
        process_events.clone(),
    )
    .await?;
    state.processes.insert(adapter.clone(), runtime);
    emit(events, ManagerEvent::Started(adapter.clone(), pid)).await;
    Ok(())
}

async fn launch(
    executable: PathBuf,
    adapter: AdapterId,
    generation: u64,
    events: mpsc::Sender<ProcessEvent>,
) -> Result<(AdapterRuntime, u32), String> {
    let mut child = Command::new(&executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("failed to spawn {executable:?}: {error}"))?;
    // Recorded before the handles are taken: once the child is moved into the
    // runtime the id is no longer reachable, and the interface needs it to
    // prove the plugin is a real process rather than an inferred entry.
    let pid = child.id().unwrap_or_default();
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "adapter stdin was not piped".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "adapter stdout was not piped".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "adapter stderr was not piped".to_string())?;
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
                        reason: format!("adapter write failed: {error}"),
                    })
                    .await;
                return;
            }
        }
        let _ = stdin.shutdown().await;
    });

    let read_adapter = adapter.clone();
    let read_events = events.clone();
    let reader_task = tokio::spawn(async move {
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
                            reason: format!("adapter output failed: {error}"),
                        })
                        .await;
                    return;
                }
            }
        }
    });

    let stderr_adapter = adapter.clone();
    let stderr_task = tokio::spawn(async move {
        let mut lines = FramedRead::new(stderr, LinesCodec::new_with_max_length(MAX_LINE_LENGTH));
        while let Some(line) = lines.next().await {
            match line {
                Ok(line) => log::warn!("adapter {stderr_adapter:?}: {line}"),
                Err(error) => {
                    log::warn!("adapter {stderr_adapter:?} stderr read failed: {error}");
                    return;
                }
            }
        }
    });

    let wait_adapter = adapter.clone();
    let waiter_task = tokio::spawn(async move {
        supervise_child(&mut child, terminate_rx, wait_adapter, generation, events).await;
    });

    Ok((
        AdapterRuntime {
            generation,
            writer,
            terminate: Some(terminate),
            tasks: vec![writer_task, reader_task, stderr_task, waiter_task],
            ready: false,
        },
        pid,
    ))
}

async fn supervise_child(
    child: &mut Child,
    mut terminate: oneshot::Receiver<()>,
    adapter: AdapterId,
    generation: u64,
    events: mpsc::Sender<ProcessEvent>,
) {
    let status = tokio::select! {
        status = child.wait() => status.map(|status| status.to_string()),
        _ = &mut terminate => {
            let _ = child.kill().await;
            child.wait().await.map(|status| status.to_string())
        }
    };
    let status = status.unwrap_or_else(|error| format!("wait failed: {error}"));
    let _ = events
        .send(ProcessEvent::Exited {
            adapter,
            generation,
            status,
        })
        .await;
}

async fn process_exit(
    state: &mut State,
    adapter: AdapterId,
    reason: String,
    process_events: &mpsc::Sender<ProcessEvent>,
    events: &mpsc::Sender<ManagerEvent>,
) {
    let terminal = matches!(adapter, AdapterId::Fuse { .. }) && state.terminal.remove(&adapter);
    // A generic plugin the user stopped must not be relaunched below.
    let deliberate = matches!(adapter, AdapterId::Plugin(_)) && state.terminal.remove(&adapter);
    stop_adapter(state, &adapter).await;
    if !terminal {
        cancel_owned(state, &adapter, events).await;
    }
    emit(
        events,
        ManagerEvent::Exited {
            adapter: adapter.clone(),
            status: reason,
        },
    )
    .await;
    if let AdapterId::Plugin(id) = &adapter {
        let attempts = state.plugin_restarts.entry(id.clone()).or_insert(0);
        if !deliberate && !state.stopping && *attempts < MAX_PLUGIN_RESTARTS {
            *attempts += 1;
            let backoff = 250 * (1_u64 << (*attempts - 1));
            sleep(Duration::from_millis(backoff)).await;
            if let Err(reason) = spawn_adapter(state, adapter.clone(), process_events, events).await
            {
                reject(events, Some(adapter), reason).await;
            }
        }
        return;
    }
    if adapter == AdapterId::Gtk && !state.stopping && state.gtk_restarts < MAX_GTK_RESTARTS {
        state.gtk_restarts += 1;
        sleep(Duration::from_millis(
            100 * (1_u64 << (state.gtk_restarts - 1)),
        ))
        .await;
        if let Err(reason) = spawn_adapter(state, AdapterId::Gtk, process_events, events).await {
            reject(events, Some(AdapterId::Gtk), reason).await;
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
            status: reason.to_string(),
        },
    )
    .await;
}

async fn cancel_owned(state: &mut State, adapter: &AdapterId, events: &mpsc::Sender<ManagerEvent>) {
    let transfers: Vec<String> = match adapter {
        AdapterId::Gtk => state.gtk_transfers.drain().collect(),
        AdapterId::Fuse { transfer_id } => vec![transfer_id.clone()],
        AdapterId::Plugin(_) => Vec::new(),
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

async fn stop_adapter(state: &mut State, adapter: &AdapterId) {
    let Some(mut runtime) = state.processes.remove(adapter) else {
        return;
    };
    drop(runtime.writer);
    if let Some(terminate) = runtime.terminate.take() {
        let _ = terminate.send(());
    }
    // Tasks are [writer, reader, stderr, waiter]. The waiter finishes once the
    // child has been killed and reaped. The readers must not be awaited for an
    // end-of-file after that: a grandchild the plugin forked (a shell wrapper,
    // a graphics driver helper) inherits the pipe and keeps it open, and the
    // supervisor loop would then freeze, taking the clipboard and file
    // transfer plugins down with it.
    let waiter = runtime.tasks.pop();
    if let Some(waiter) = waiter {
        let _ = waiter.await;
    }
    for task in runtime.tasks.drain(..) {
        task.abort();
        let _ = task.await;
    }
}

async fn send_ready(
    state: &mut State,
    adapter: &AdapterId,
    message: Message,
    events: &mpsc::Sender<ManagerEvent>,
) {
    let Some(runtime) = state.processes.get(adapter) else {
        reject(events, Some(adapter.clone()), "adapter is not running").await;
        return;
    };
    if !runtime.ready {
        reject(events, Some(adapter.clone()), "adapter is not ready").await;
        return;
    }
    match runtime.writer.try_send(message) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            fail_adapter(state, adapter, "adapter write queue is full", events).await
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            fail_adapter(state, adapter, "adapter writer has stopped", events).await
        }
    }
}

/// Sends a message that is worthless once stale, dropping it if the plugin is
/// momentarily not reading.
///
/// A pointer event is out of date within a frame, and a plugin may legitimately
/// stop reading stdin for a while (starting its graphics, waiting on vsync).
/// Treating a full queue as a fault would kill a healthy plugin for being busy,
/// so only a closed pipe, which means the process is gone, counts as failure.
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
            log::debug!("dropped a pointer event for a busy plugin {adapter:?}");
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            fail_adapter(state, adapter, "adapter writer has stopped", events).await
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

fn validate_executable(name: &'static str, path: &Path) -> Result<(), ManagerError> {
    let metadata = path
        .metadata()
        .map_err(|source| ManagerError::InvalidExecutable {
            name,
            path: path.to_path_buf(),
            source,
        })?;
    if !metadata.is_file() {
        return Err(ManagerError::InvalidExecutable {
            name,
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(ManagerError::InvalidExecutable {
                name,
                path: path.to_path_buf(),
                source: io::Error::new(io::ErrorKind::PermissionDenied, "not executable"),
            });
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use syntra_plugin_api::{Edge, PointerEvent};

    const PLUGIN_ID: &str = "edge-glow-test";

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("syntra-adapter-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The handshake a real pointer plugin sends, as one JSON line.
    fn hello(id: &str) -> String {
        format!(
            r#"{{"type":"hello","data":{{"protocol_version":{PROTOCOL_VERSION},"adapter_id":"{id}","name":"Stub","capabilities":{{"clipboard_read":false,"paste":false,"cancel":false,"pointer_events":true,"mime_types":[]}}}}}}"#
        )
    }

    /// Writes an executable script: it prints `first`, then runs `rest`.
    ///
    /// A script that was written a moment ago can be refused with `ETXTBSY`
    /// ("text file busy") when another thread forks while the write handle is
    /// still open in the child. That is a property of Linux and of running
    /// tests in parallel, not of the supervisor, so the helper does not return
    /// until the file has actually been executed once.
    fn stub(dir: &Path, name: &str, first: &str, rest: &str) -> PathBuf {
        let path = dir.join(name);
        // `--probe` makes the script leave at once, before it speaks.
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

    /// A supervisor with neither built-in helper installed, as on Windows
    /// and macOS.
    fn bare_manager() -> (AdapterProcessManager, mpsc::Receiver<ManagerEvent>) {
        let missing = PathBuf::from("/nonexistent/syntra-helper");
        AdapterProcessManager::start(AdapterPaths::new(missing.clone(), missing))
    }

    async fn next_event(events: &mut mpsc::Receiver<ManagerEvent>) -> ManagerEvent {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("no supervisor event within 10s")
            .expect("supervisor stopped");
        // A launch that fails produces only `Rejected`, never `Started` or
        // `Exited`, so without this it would read as a silent timeout.
        if let ManagerEvent::Rejected {
            adapter: Some(AdapterId::Plugin(id)),
            reason,
        } = &event
        {
            panic!("launching plugin `{id}` was rejected: {reason}");
        }
        event
    }

    fn start(manager: &AdapterProcessManager, executable: PathBuf) {
        manager
            .try_send(ManagerCommand::StartPlugin {
                id: PLUGIN_ID.into(),
                executable,
                settings: vec![("enabled".into(), "true".into())],
            })
            .unwrap();
    }

    /// Waits for `Ready`, collecting nothing else of interest.
    async fn until_ready(events: &mut mpsc::Receiver<ManagerEvent>) {
        loop {
            match next_event(events).await {
                ManagerEvent::Ready(AdapterId::Plugin(id), _) => {
                    assert_eq!(id, PLUGIN_ID);
                    return;
                }
                ManagerEvent::Exited { status, .. } => panic!("plugin exited early: {status}"),
                _ => {}
            }
        }
    }

    /// Without the Linux helpers there must still be a supervisor, or a
    /// plugin such as the edge glow could never run on Windows or macOS.
    #[tokio::test]
    async fn a_plugin_starts_when_neither_builtin_helper_is_installed() {
        let dir = temp_dir("bare");
        let (manager, mut events) = bare_manager();
        start(
            &manager,
            stub(&dir, "glow", &hello(PLUGIN_ID), "cat >/dev/null"),
        );

        let mut started = false;
        loop {
            match next_event(&mut events).await {
                ManagerEvent::Started(AdapterId::Plugin(_), pid) => {
                    assert!(pid > 0, "a started plugin reports a real process id");
                    started = true;
                }
                ManagerEvent::Ready(AdapterId::Plugin(_), _) => break,
                _ => {}
            }
        }
        assert!(started, "Started must precede Ready");
        manager.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A plugin that stops reading stdin for a while (starting its graphics,
    /// waiting on vsync) is busy, not broken. A burst of perishable pointer
    /// events must be dropped, not turned into a failure that kills it.
    #[tokio::test]
    async fn a_busy_plugin_is_not_failed_by_a_burst_of_pointer_events() {
        let dir = temp_dir("busy");
        let (manager, mut events) = bare_manager();
        // Says hello, then never reads stdin again.
        start(&manager, stub(&dir, "glow", &hello(PLUGIN_ID), "sleep 30"));
        until_ready(&mut events).await;

        // Far more than the 64-slot queue plus the pipe buffer can hold.
        for step in 0..5000 {
            let _ = manager.try_send(ManagerCommand::PluginPointer {
                id: PLUGIN_ID.into(),
                event: PointerEvent::Pressure {
                    edge: Edge::Left,
                    along: Some(0.5),
                    amount: (step % 100) as f32 / 100.0,
                },
            });
            if step % 50 == 0 {
                tokio::task::yield_now().await;
            }
        }
        // Let the supervisor drain everything it was sent.
        tokio::time::sleep(Duration::from_millis(500)).await;

        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event, ManagerEvent::Exited { .. }),
                "a busy plugin was failed: {event:?}"
            );
        }
        manager.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A discovered plugin only listens. One that starts sending clipboard
    /// frames must be stopped, not have them forwarded as real transfers.
    #[tokio::test]
    async fn a_plugin_that_sends_transfer_frames_is_stopped() {
        let dir = temp_dir("forger");
        let (manager, mut events) = bare_manager();
        let forged = r#"{"type":"clipboard_data","data":{"transfer_id":"abc","mime_type":"text/plain","value":"x"}}"#;
        start(
            &manager,
            stub(
                &dir,
                "glow",
                &hello(PLUGIN_ID),
                &format!("printf '%s\\n' '{forged}'\nsleep 30"),
            ),
        );

        let mut seen = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .unwrap_or_else(|_| panic!("no event within 10s; saw so far: {seen:#?}"))
                .expect("supervisor stopped");
            seen.push(format!("{event:?}"));
            match event {
                ManagerEvent::Message { .. } => {
                    panic!("a forged transfer frame was forwarded to the service")
                }
                ManagerEvent::Exited { adapter, .. } => {
                    assert_eq!(adapter, AdapterId::Plugin(PLUGIN_ID.into()));
                    break;
                }
                _ => {}
            }
        }
        // The supervisor must still be alive and answering: a stop that hung on
        // the inherited pipe would leave this second start unserved.
        manager
            .try_send(ManagerCommand::StartPlugin {
                id: "second".into(),
                executable: stub(&dir, "second", &hello("second"), "cat >/dev/null"),
                settings: Vec::new(),
            })
            .unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("the supervisor froze after failing a plugin")
                .expect("supervisor stopped")
            {
                ManagerEvent::Ready(AdapterId::Plugin(id), _) if id == "second" => break,
                _ => {}
            }
        }
        manager.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The whitelist is checked before any transfer-id handling, because a
    /// frame carrying an id would otherwise fall through to acceptance.
    #[test]
    fn a_generic_plugin_may_only_report_errors() {
        let dir = temp_dir("validate");
        let mut state = State {
            paths: AdapterPaths::new(dir.join("a"), dir.join("b")),
            processes: HashMap::new(),
            pending_ranges: HashMap::new(),
            pending_messages: HashMap::new(),
            gtk_transfers: HashSet::new(),
            next_generation: 1,
            terminal: HashSet::new(),
            gtk_restarts: 0,
            plugin_paths: HashMap::new(),
            plugin_settings: HashMap::new(),
            plugin_restarts: HashMap::new(),
            stopping: false,
        };
        let adapter = AdapterId::Plugin("x".into());
        let clipboard = Message::ClipboardData {
            transfer_id: "abc".into(),
            mime_type: "text/plain".into(),
            value: "v".into(),
        };
        let error = Message::Error {
            message: "oops".into(),
        };
        assert!(validate_inbound(&mut state, &adapter, &clipboard).is_err());
        assert!(validate_inbound(&mut state, &adapter, &error).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Stopping a plugin is the user's decision; it must not come back.
    #[tokio::test]
    async fn a_plugin_the_user_stopped_is_not_relaunched() {
        let dir = temp_dir("stopped");
        let (manager, mut events) = bare_manager();
        start(
            &manager,
            stub(&dir, "glow", &hello(PLUGIN_ID), "cat >/dev/null"),
        );
        until_ready(&mut events).await;

        manager
            .try_send(ManagerCommand::StopPlugin {
                plugin: PluginTarget::Generic(PLUGIN_ID.into()),
            })
            .unwrap();
        loop {
            if matches!(next_event(&mut events).await, ManagerEvent::Stopped(_)) {
                break;
            }
        }
        // A relaunch would announce itself well inside this window (the first
        // retry waits 250 ms).
        let quiet = tokio::time::timeout(Duration::from_millis(1500), async {
            loop {
                if let Some(ManagerEvent::Started(..)) = events.recv().await {
                    return;
                }
            }
        })
        .await;
        assert!(quiet.is_err(), "a stopped plugin was started again");
        manager.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
