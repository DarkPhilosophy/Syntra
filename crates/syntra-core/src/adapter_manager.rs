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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum AdapterId {
    Gtk,
    Fuse { transfer_id: String },
}

#[derive(Clone, Debug)]
pub(crate) struct AdapterPaths {
    gtk_clipboard: PathBuf,
    fuse: PathBuf,
}

impl AdapterPaths {
    pub(crate) fn new(gtk_clipboard: PathBuf, fuse: PathBuf) -> Result<Self, ManagerError> {
        validate_executable("GTK clipboard", &gtk_clipboard)?;
        validate_executable("FUSE", &fuse)?;
        Ok(Self {
            gtk_clipboard,
            fuse,
        })
    }
}

#[derive(Debug)]
pub(crate) enum ManagerCommand {
    RemoteManifest(RemoteManifest),
    RangeResponse(RangeResponse),
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
    Shutdown,
}

#[derive(Debug)]
pub(crate) enum ManagerEvent {
    Started,
    Ready,
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

async fn handle_command(
    command: ManagerCommand,
    state: &mut State,
    process_events: &mpsc::Sender<ProcessEvent>,
    events: &mpsc::Sender<ManagerEvent>,
) {
    match command {
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
            };
            if !owns {
                reject(events, Some(adapter), "cancel for unowned transfer").await;
                return;
            }
            send_ready(state, &adapter, Message::Cancel { transfer_id }, events).await;
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
                emit(events, ManagerEvent::Ready).await;
                if let Some(pending) = state.pending_messages.remove(&adapter) {
                    send_ready(state, &adapter, pending, events).await;
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
    let expected_id = match adapter {
        AdapterId::Gtk => "gtk-clipboard",
        AdapterId::Fuse { .. } => "fuse",
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
        Message::Hello { .. } | Message::Error { .. } => None,
    }
}

async fn spawn_adapter(
    state: &mut State,
    adapter: AdapterId,
    process_events: &mpsc::Sender<ProcessEvent>,
    events: &mpsc::Sender<ManagerEvent>,
) -> Result<(), String> {
    let executable = match &adapter {
        AdapterId::Gtk => state.paths.gtk_clipboard.clone(),
        AdapterId::Fuse { .. } => state.paths.fuse.clone(),
    };
    let generation = state.next_generation;
    state.next_generation = state.next_generation.wrapping_add(1).max(1);
    let runtime = launch(
        executable,
        adapter.clone(),
        generation,
        process_events.clone(),
    )
    .await?;
    state.processes.insert(adapter.clone(), runtime);
    emit(events, ManagerEvent::Started).await;
    Ok(())
}

async fn launch(
    executable: PathBuf,
    adapter: AdapterId,
    generation: u64,
    events: mpsc::Sender<ProcessEvent>,
) -> Result<AdapterRuntime, String> {
    let mut child = Command::new(&executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("failed to spawn {executable:?}: {error}"))?;
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

    Ok(AdapterRuntime {
        generation,
        writer,
        terminate: Some(terminate),
        tasks: vec![writer_task, reader_task, stderr_task, waiter_task],
        ready: false,
    })
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
    for task in runtime.tasks.drain(..) {
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
