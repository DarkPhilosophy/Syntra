use crate::file_transfer::{FileOffer, OutgoingFile, TransferError};
use base64::Engine;
use lan_mouse_adapter_api::{
    Cancelled as AdapterCancelled, Completion, CopyManifest, EntryKind as AdapterEntryKind,
    Message as AdapterMessage, MountReady, Operation, Progress as AdapterProgress, RangeRequest,
    RangeResponse, Released, RemoteEntry, RemoteManifest, Unmounted,
};
use lan_mouse_proto::{
    ClipboardCancelReason, ClipboardEntryKind, ClipboardFileChunkValidator, ClipboardManifestEntry,
    MAX_CLIPBOARD_FILE_CHUNK_SIZE, MAX_CLIPBOARD_MANIFEST_ENTRIES, MAX_CLIPBOARD_PATH_SIZE,
    MAX_CLIPBOARD_SIZE, ProtoEvent, ProtocolError,
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, hash_map::Entry};
use std::fmt::Debug;
use std::hash::Hash;
use thiserror::Error;

pub(crate) type UiTransferId = u64;
pub(crate) type AdapterId = String;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct TransferOwner<P> {
    pub(crate) peer: P,
    pub(crate) wire_id: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PendingRangeKey<P> {
    pub(crate) owner: TransferOwner<P>,
    pub(crate) file_id: u64,
    pub(crate) request_id: u64,
    pub(crate) offset: u64,
    pub(crate) length: u64,
}

#[derive(Debug)]
pub(crate) enum TransferAction<P> {
    Peer {
        peer: P,
        event: ProtoEvent,
    },
    Adapter {
        adapter_id: AdapterId,
        message: AdapterMessage,
    },
    Frontend(TransferFrontendEvent<P>),
    ReadSource {
        owner: TransferOwner<P>,
        file_id: u64,
        request_id: u64,
        offset: u64,
        length: u64,
        offer: FileOffer,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum TransferFrontendEvent<P> {
    Offered {
        ui_id: UiTransferId,
        owner: TransferOwner<P>,
        adapter_id: AdapterId,
        operation: Operation,
        entries: Vec<ClipboardManifestEntry>,
    },
    Progress {
        ui_id: UiTransferId,
        owner: TransferOwner<P>,
        file_id: u64,
        completed: u64,
        total: u64,
    },
    Completed {
        ui_id: UiTransferId,
        owner: TransferOwner<P>,
        file_id: Option<u64>,
        completed: u64,
        total: u64,
    },
    Cancelled {
        ui_id: UiTransferId,
        owner: TransferOwner<P>,
        file_id: Option<u64>,
        completed: u64,
        total: u64,
        reason: ClipboardCancelReason,
    },
    Failed {
        ui_id: UiTransferId,
        owner: TransferOwner<P>,
        file_id: Option<u64>,
        completed: u64,
        total: u64,
        error: String,
    },
}

#[derive(Debug, Error)]
pub(crate) enum TransferStateError<P: Debug> {
    #[error("unknown transfer owner {0:?}")]
    UnknownOwner(TransferOwner<P>),
    #[error("unknown adapter transfer {adapter_id}:{transfer_id}")]
    UnknownAdapterTransfer {
        adapter_id: AdapterId,
        transfer_id: String,
    },
    #[error("duplicate transfer owner {0:?}")]
    DuplicateOwner(TransferOwner<P>),
    #[error("duplicate adapter transfer {adapter_id}:{transfer_id}")]
    DuplicateAdapterTransfer {
        adapter_id: AdapterId,
        transfer_id: String,
    },
    #[error("duplicate UI transfer id {0}")]
    DuplicateUiId(UiTransferId),
    #[error("UI transfer id space exhausted")]
    UiIdExhausted,
    #[error("unknown file {file_id} for {owner:?}")]
    UnknownFile {
        owner: TransferOwner<P>,
        file_id: u64,
    },
    #[error("unknown pending request {request_id} for {owner:?}, file {file_id}")]
    UnknownRequest {
        owner: TransferOwner<P>,
        file_id: u64,
        request_id: u64,
    },
    #[error("duplicate pending request {request_id} for {owner:?}, file {file_id}")]
    DuplicateRequest {
        owner: TransferOwner<P>,
        file_id: u64,
        request_id: u64,
    },
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),
    #[error("invalid range: {0}")]
    InvalidRange(String),
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("file transfer error: {0}")]
    File(#[from] TransferError),
    #[error("digest mismatch for {owner:?}, file {file_id}")]
    DigestMismatch {
        owner: TransferOwner<P>,
        file_id: u64,
    },
    #[error("completion digest cannot be verified for a range beginning at offset {offset}")]
    UnverifiableDigest { offset: u64 },
    #[error("event is not a file-transfer protocol event")]
    UnsupportedProtocolEvent,
}

/// A clipboard offer is pure metadata: only a real data request marks it as
/// an actual transfer.

struct LocalTransfer<P> {
    ui_id: UiTransferId,
    owner: TransferOwner<P>,
    adapter_id: AdapterId,
    adapter_transfer_id: String,
    operation: Operation,
    offer: FileOffer,
    outgoing: HashMap<(u64, u64), OutgoingFile>,
    /// A local offer is only metadata until the peer actually pastes.
    announced: bool,
    /// Greatest protocol-reported byte position for each file.
    transferred_by_file: HashMap<u64, u64>,
}

struct RemoteTransfer<P> {
    ui_id: UiTransferId,
    owner: TransferOwner<P>,
    adapter_id: AdapterId,
    adapter_transfer_id: String,
    operation: Operation,
    entries: HashMap<u64, ClipboardManifestEntry>,
    /// A remote offer is only metadata until the user actually pastes:
    /// the transfer is announced to the frontend once a paste really
    /// streams data, never when the user merely copies.
    announced: bool,
    /// Greatest validated byte position for each file.
    transferred_by_file: HashMap<u64, u64>,
}

struct PendingRange<P> {
    key: PendingRangeKey<P>,
    validator: ClipboardFileChunkValidator,
    received: u64,
    hasher: Sha256,
    chunk: Vec<u8>,
}

pub(crate) struct TransferState<P> {
    next_ui_id: UiTransferId,
    local: HashMap<TransferOwner<P>, LocalTransfer<P>>,
    remote: HashMap<TransferOwner<P>, RemoteTransfer<P>>,
    pending: HashMap<(TransferOwner<P>, u64, u64), PendingRange<P>>,
    adapter_to_owner: HashMap<(AdapterId, String), HashSet<TransferOwner<P>>>,
    source_pending: HashMap<(TransferOwner<P>, u64, u64), (u64, u64)>,
    ui_to_owner: HashMap<UiTransferId, TransferOwner<P>>,
}

impl<P> Default for TransferState<P> {
    fn default() -> Self {
        Self {
            next_ui_id: 1,
            local: HashMap::new(),
            remote: HashMap::new(),
            pending: HashMap::new(),
            source_pending: HashMap::new(),
            adapter_to_owner: HashMap::new(),
            ui_to_owner: HashMap::new(),
        }
    }
}

impl<P> TransferState<P>
where
    P: Clone + Debug + Eq + Hash,
{
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn owner_for_ui(&self, ui_id: UiTransferId) -> Option<&TransferOwner<P>> {
        self.ui_to_owner.get(&ui_id)
    }

    pub(crate) fn owners_for_adapter(
        &self,
        adapter_id: &str,
        transfer_id: &str,
    ) -> Vec<TransferOwner<P>> {
        self.adapter_to_owner
            .get(&(adapter_id.to_owned(), transfer_id.to_owned()))
            .map(|owners| owners.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) fn local_copy_manifest(
        &mut self,
        peer: P,
        wire_id: u64,
        adapter_id: AdapterId,
        manifest: CopyManifest,
        offer: FileOffer,
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        let owner = TransferOwner {
            peer: peer.clone(),
            wire_id,
        };
        // Validate the replacement before touching the current owner. A malformed
        // clipboard announcement must not make the still-valid offer disappear.
        self.ensure_owner_available(&owner)?;
        if offer.transfer_id != wire_id {
            return Err(TransferStateError::InvalidManifest(
                "file offer transfer id differs from wire id".into(),
            ));
        }
        validate_wire_manifest(&offer.entries)?;
        if matches!(manifest.operation, Operation::Move) {
            return Err(TransferStateError::InvalidManifest(
                "move clipboard transfers are not supported by the wire manifest".into(),
            ));
        }
        /// Every new local copy replaces the previous offer to the same
        /// peer: otherwise each copy leaves another never-pasteable offer
        /// behind and the peer accumulates clones.
        let superseded: Vec<_> = self
            .local
            .iter()
            .filter(|(candidate, transfer)| candidate.peer == peer && !transfer.announced)
            .map(|(candidate, _)| candidate.clone())
            .collect();
        let mut stale_actions = Vec::new();
        for candidate in superseded {
            stale_actions.extend(
                self.cancel_owner(&candidate, None, ClipboardCancelReason::User, true)
                    .unwrap_or_default(),
            );
        }
        let ui_id = self.allocate_ui_id()?;
        let operation = manifest.operation.clone();
        debug_assert!(!matches!(manifest.operation, Operation::Move));
        let entries = offer.entries.clone();
        let adapter_transfer_id = manifest.transfer_id.clone();
        self.insert_mapping(
            ui_id,
            owner.clone(),
            adapter_id.clone(),
            adapter_transfer_id.clone(),
        );
        self.local.insert(
            owner.clone(),
            LocalTransfer {
                ui_id,
                owner: owner.clone(),
                adapter_id: adapter_id.clone(),
                adapter_transfer_id,
                operation: operation.clone(),
                offer,
                outgoing: HashMap::new(),
                announced: false,
                transferred_by_file: HashMap::new(),
            },
        );
        stale_actions.push(TransferAction::Peer {
            peer,
            event: ProtoEvent::ClipboardManifest {
                transfer_id: wire_id,
                entries,
            },
        });
        Ok(stale_actions)
    }

    /// A copy is never a transfer: the frontend only learns about an offer
    /// once a paste actually streams more than a clipboard preview sniff.
    fn announce_local(&mut self, owner: &TransferOwner<P>, length: u64) -> Vec<TransferAction<P>> {
        let Some(transfer) = self.local.get_mut(owner) else {
            return Vec::new();
        };
        if transfer.announced {
            return Vec::new();
        }
        transfer.announced = true;
        vec![TransferAction::Frontend(TransferFrontendEvent::Offered {
            ui_id: transfer.ui_id,
            owner: owner.clone(),
            adapter_id: transfer.adapter_id.clone(),
            operation: transfer.operation.clone(),
            entries: transfer.offer.entries.clone(),
        })]
    }

    pub(crate) fn inbound_manifest(
        &mut self,
        peer: P,
        adapter_id: AdapterId,
        adapter_transfer_id: String,
        wire_id: u64,
        operation: Operation,
        entries: Vec<ClipboardManifestEntry>,
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        let owner = TransferOwner { peer, wire_id };
        if matches!(operation, Operation::Move) {
            return Err(TransferStateError::InvalidManifest(
                "move clipboard transfers are not supported by the wire manifest".into(),
            ));
        }
        // Validate the replacement before superseding the current owner. An
        // invalid or duplicate announcement cannot revoke a usable offer.
        self.ensure_ids_available(&owner, &adapter_id, &adapter_transfer_id)?;
        validate_wire_manifest(&entries)?;
        // A new clipboard offer from the same peer replaces every previous
        // offer from that peer. The portal, not the GTK adapter, owns the
        // published selection, so an old announced offer must not keep a
        // stale FUSE mount alive.
        let superseded: Vec<_> = self
            .remote
            .keys()
            .filter(|candidate| candidate.peer == owner.peer)
            .cloned()
            .collect();
        let mut stale_actions = Vec::new();
        for candidate in superseded {
            stale_actions.extend(
                self.cancel_owner(&candidate, None, ClipboardCancelReason::User, false)
                    .unwrap_or_default(),
            );
        }
        let ui_id = self.allocate_ui_id()?;
        let by_id = entries
            .iter()
            .cloned()
            .map(|entry| (entry.file_id, entry))
            .collect();
        self.insert_mapping(
            ui_id,
            owner.clone(),
            adapter_id.clone(),
            adapter_transfer_id.clone(),
        );
        self.remote.insert(
            owner.clone(),
            RemoteTransfer {
                ui_id,
                owner: owner.clone(),
                adapter_id: adapter_id.clone(),
                adapter_transfer_id: adapter_transfer_id.clone(),
                operation: operation.clone(),
                entries: by_id,
                announced: false,
                transferred_by_file: HashMap::new(),
            },
        );
        let remote_entries = entries
            .iter()
            .map(|entry| RemoteEntry {
                entry_id: entry.file_id,
                path: entry.path.clone(),
                kind: match entry.kind {
                    ClipboardEntryKind::File => AdapterEntryKind::File,
                    ClipboardEntryKind::Directory => AdapterEntryKind::Directory,
                },
                size: (entry.kind == ClipboardEntryKind::File).then_some(entry.size),
            })
            .collect();
        stale_actions.push(TransferAction::Adapter {
            adapter_id: adapter_id.clone(),
            message: AdapterMessage::RemoteManifest(RemoteManifest {
                transfer_id: adapter_transfer_id,
                operation: operation.clone(),
                entries: remote_entries,
            }),
        });
        Ok(stale_actions)
    }

    /// A copy must never look like a transfer: the frontend only learns
    /// about a remote offer once a real paste streams more than a
    /// clipboard preview sniff.
    fn announce_remote(&mut self, owner: &TransferOwner<P>, length: u64) -> Vec<TransferAction<P>> {
        let Some(transfer) = self.remote.get_mut(owner) else {
            return Vec::new();
        };
        if transfer.announced {
            return Vec::new();
        }
        transfer.announced = true;
        let mut entries: Vec<_> = transfer.entries.values().cloned().collect();
        entries.sort_by_key(|entry| entry.file_id);
        vec![TransferAction::Frontend(TransferFrontendEvent::Offered {
            ui_id: transfer.ui_id,
            owner: owner.clone(),
            adapter_id: transfer.adapter_id.clone(),
            operation: transfer.operation.clone(),
            entries,
        })]
    }

    pub(crate) fn adapter_range_request(
        &mut self,
        adapter_id: &str,
        request: RangeRequest,
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        let owner = self
            .owners_for_adapter(adapter_id, &request.transfer_id)
            .into_iter()
            .next()
            .ok_or_else(|| TransferStateError::UnknownAdapterTransfer {
                adapter_id: adapter_id.to_owned(),
                transfer_id: request.transfer_id.clone(),
            })?;
        let transfer = self
            .remote
            .get(&owner)
            .ok_or_else(|| TransferStateError::UnknownOwner(owner.clone()))?;
        if transfer.adapter_id != adapter_id || transfer.adapter_transfer_id != request.transfer_id
        {
            return Err(TransferStateError::UnknownAdapterTransfer {
                adapter_id: adapter_id.to_owned(),
                transfer_id: request.transfer_id,
            });
        }
        let entry = transfer.entries.get(&request.entry_id).ok_or_else(|| {
            TransferStateError::UnknownFile {
                owner: owner.clone(),
                file_id: request.entry_id,
            }
        })?;
        if entry.kind != ClipboardEntryKind::File {
            return Err(TransferStateError::InvalidRange(
                "directory range requested".into(),
            ));
        }
        let length = u64::from(request.length);
        validate_range(entry.size, request.offset, length)?;
        let index = (owner.clone(), request.entry_id, request.request_id);
        if self.pending.contains_key(&index) {
            return Err(TransferStateError::DuplicateRequest {
                owner,
                file_id: request.entry_id,
                request_id: request.request_id,
            });
        }
        let key = PendingRangeKey {
            owner: owner.clone(),
            file_id: request.entry_id,
            request_id: request.request_id,
            offset: request.offset,
            length,
        };
        let validator = ClipboardFileChunkValidator::new(
            owner.wire_id,
            request.entry_id,
            request.request_id,
            request.offset,
            length,
        )?;
        self.pending.insert(
            index,
            PendingRange {
                key,
                validator,
                received: 0,
                hasher: Sha256::new(),
                chunk: Vec::new(),
            },
        );
        let mut actions = self.announce_remote(&owner, length);
        actions.push(TransferAction::Peer {
            peer: owner.peer,
            event: ProtoEvent::ClipboardFileRequest {
                transfer_id: owner.wire_id,
                file_id: request.entry_id,
                request_id: request.request_id,
                offset: request.offset,
                length,
            },
        });
        Ok(actions)
    }

    pub(crate) fn adapter_mount_ready(
        &mut self,
        fuse_adapter_id: &str,
        ready: MountReady,
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        let owner = self
            .owners_for_adapter(fuse_adapter_id, &ready.transfer_id)
            .into_iter()
            .next()
            .ok_or_else(|| TransferStateError::UnknownAdapterTransfer {
                adapter_id: fuse_adapter_id.to_owned(),
                transfer_id: ready.transfer_id.clone(),
            })?;
        let transfer = self
            .remote
            .get(&owner)
            .ok_or_else(|| TransferStateError::UnknownOwner(owner.clone()))?;
        let operation = transfer.operation.clone();
        Ok(vec![TransferAction::Adapter {
            adapter_id: "gtk-clipboard".to_owned(),
            message: AdapterMessage::PublishFileClipboard(
                lan_mouse_adapter_api::PublishFileClipboard {
                    transfer_id: ready.transfer_id,
                    operation,
                    uris: ready.uris,
                },
            ),
        }])
    }

    pub(crate) fn adapter_progress(
        &mut self,
        adapter_id: &str,
        progress: AdapterProgress,
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        let owners = self.owners_for_adapter(adapter_id, &progress.transfer_id);
        if owners.is_empty() {
            return Err(TransferStateError::UnknownAdapterTransfer {
                adapter_id: adapter_id.to_owned(),
                transfer_id: progress.transfer_id,
            });
        }
        let completed = progress.completed_bytes.ok_or_else(|| {
            TransferStateError::InvalidRange("adapter progress lacks byte accounting".into())
        })?;
        let total = owners
            .first()
            .map(|owner| self.transfer_totals(owner).1)
            .unwrap_or(0);
        if progress
            .total_bytes
            .is_some_and(|reported| reported != total)
        {
            return Err(TransferStateError::InvalidRange(
                "adapter progress total differs from manifest".into(),
            ));
        }
        if completed > total {
            return Err(TransferStateError::InvalidRange(
                "adapter progress exceeds total".into(),
            ));
        }
        for owner in &owners {
            if let Some(transfer) = self.local.get_mut(owner) {
                transfer.transferred_by_file.insert(0, completed);
            }
            if let Some(transfer) = self.remote.get_mut(owner) {
                transfer.transferred_by_file.insert(0, completed);
            }
        }
        Ok(owners
            .into_iter()
            .filter_map(|owner| {
                self.transfer_ui_id(&owner).map(|ui_id| {
                    TransferAction::Frontend(TransferFrontendEvent::Progress {
                        ui_id,
                        owner,
                        file_id: 0,
                        completed,
                        total,
                    })
                })
            })
            .collect())
    }

    pub(crate) fn adapter_completed(
        &mut self,
        adapter_id: &str,
        completion: Completion,
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        let owners = self.owners_for_adapter(adapter_id, &completion.transfer_id);
        if owners.is_empty() {
            return Err(TransferStateError::UnknownAdapterTransfer {
                adapter_id: adapter_id.to_owned(),
                transfer_id: completion.transfer_id,
            });
        }
        let mut actions = Vec::with_capacity(owners.len());
        for owner in owners {
            let ui_id = self
                .transfer_ui_id(&owner)
                .ok_or_else(|| TransferStateError::UnknownOwner(owner.clone()))?;
            let (completed, total) = self.transfer_totals(&owner);
            let event = if completion.success {
                TransferFrontendEvent::Completed {
                    ui_id,
                    owner: owner.clone(),
                    file_id: None,
                    completed,
                    total,
                }
            } else {
                TransferFrontendEvent::Failed {
                    ui_id,
                    owner: owner.clone(),
                    file_id: None,
                    completed,
                    total,
                    error: completion
                        .error
                        .clone()
                        .unwrap_or_else(|| "adapter transfer failed".into()),
                }
            };
            actions.push(TransferAction::Frontend(event));
            self.remove_owner(&owner);
        }
        Ok(actions)
    }

    pub(crate) fn adapter_cancelled(
        &mut self,
        adapter_id: &str,
        cancelled: AdapterCancelled,
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        let owners = self.owners_for_adapter(adapter_id, &cancelled.transfer_id);
        if owners.is_empty() {
            return Err(TransferStateError::UnknownAdapterTransfer {
                adapter_id: adapter_id.to_owned(),
                transfer_id: cancelled.transfer_id,
            });
        }
        let mut actions = Vec::with_capacity(owners.len() * 2);
        for owner in owners {
            let ui_id = self
                .transfer_ui_id(&owner)
                .ok_or_else(|| TransferStateError::UnknownOwner(owner.clone()))?;
            let (completed, total) = self.transfer_totals(&owner);
            actions.push(TransferAction::Peer {
                peer: owner.peer.clone(),
                event: ProtoEvent::ClipboardTransferCancel {
                    transfer_id: owner.wire_id,
                    file_id: None,
                    reason: ClipboardCancelReason::User,
                },
            });
            actions.push(TransferAction::Frontend(TransferFrontendEvent::Cancelled {
                ui_id,
                owner: owner.clone(),
                file_id: None,
                reason: ClipboardCancelReason::User,
                completed,
                total,
            }));
            self.remove_owner(&owner);
        }
        Ok(actions)
    }

    pub(crate) fn adapter_released(
        &mut self,
        released: Released,
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        let key = ("gtk-clipboard".to_owned(), released.transfer_id.clone());
        let owners = self.adapter_to_owner.remove(&key).ok_or_else(|| {
            TransferStateError::UnknownAdapterTransfer {
                adapter_id: key.0.clone(),
                transfer_id: released.transfer_id.clone(),
            }
        })?;
        let mut actions = Vec::new();
        for owner in owners {
            if let Some(remote) = self.remote.get(&owner) {
                actions.push(TransferAction::Adapter {
                    adapter_id: remote.adapter_id.clone(),
                    message: AdapterMessage::Unmounted(Unmounted {
                        transfer_id: remote.adapter_transfer_id.clone(),
                        success: true,
                        error: None,
                    }),
                });
                continue;
            }
            let Some(ui_id) = self.local.get(&owner).map(|transfer| transfer.ui_id) else {
                continue;
            };
            let (completed, total) = self.transfer_totals(&owner);
            actions.push(TransferAction::Peer {
                peer: owner.peer.clone(),
                event: ProtoEvent::ClipboardTransferCancel {
                    transfer_id: owner.wire_id,
                    file_id: None,
                    reason: ClipboardCancelReason::User,
                },
            });
            actions.push(TransferAction::Frontend(TransferFrontendEvent::Cancelled {
                ui_id,
                owner: owner.clone(),
                file_id: None,
                reason: ClipboardCancelReason::User,
                completed,
                total,
            }));
            self.remove_owner(&owner);
        }
        Ok(actions)
    }

    pub(crate) fn adapter_unmounted(
        &mut self,
        adapter_id: &str,
        unmounted: Unmounted,
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        let owners = self.owners_for_adapter(adapter_id, &unmounted.transfer_id);
        if owners.is_empty() {
            return Err(TransferStateError::UnknownAdapterTransfer {
                adapter_id: adapter_id.to_owned(),
                transfer_id: unmounted.transfer_id,
            });
        }
        let mut actions = Vec::with_capacity(owners.len());
        for owner in owners {
            let ui_id = self
                .transfer_ui_id(&owner)
                .ok_or_else(|| TransferStateError::UnknownOwner(owner.clone()))?;
            let (completed, total) = self.transfer_totals(&owner);
            let event = if unmounted.success {
                TransferFrontendEvent::Cancelled {
                    ui_id,
                    owner: owner.clone(),
                    file_id: None,
                    completed,
                    total,
                    reason: ClipboardCancelReason::User,
                }
            } else {
                TransferFrontendEvent::Failed {
                    ui_id,
                    owner: owner.clone(),
                    file_id: None,
                    completed,
                    total,
                    error: unmounted
                        .error
                        .clone()
                        .unwrap_or_else(|| "adapter unmount failed".into()),
                }
            };
            actions.push(TransferAction::Frontend(event));
            self.remove_owner(&owner);
        }
        Ok(actions)
    }

    pub(crate) fn source_file_request(
        &mut self,
        peer: &P,
        wire_id: u64,
        file_id: u64,
        request_id: u64,
        offset: u64,
        length: u64,
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        let owner = TransferOwner {
            peer: peer.clone(),
            wire_id,
        };
        let transfer = self
            .local
            .get(&owner)
            .ok_or_else(|| TransferStateError::UnknownOwner(owner.clone()))?;
        let entry = transfer
            .offer
            .entries
            .iter()
            .find(|entry| entry.file_id == file_id)
            .ok_or_else(|| TransferStateError::UnknownFile {
                owner: owner.clone(),
                file_id,
            })?;
        if entry.kind != ClipboardEntryKind::File {
            return Err(TransferStateError::InvalidRange(
                "directory range requested".into(),
            ));
        }
        validate_range(entry.size, offset, length)?;
        let key = (owner.clone(), file_id, request_id);
        match self.source_pending.entry(key) {
            Entry::Vacant(slot) => {
                slot.insert((offset, length));
            }
            Entry::Occupied(_) => {
                return Err(TransferStateError::DuplicateRequest {
                    owner,
                    file_id,
                    request_id,
                });
            }
        }
        let offer = transfer.offer.clone();
        let mut actions = self.announce_local(&owner, length);
        actions.push(TransferAction::ReadSource {
            owner,
            file_id,
            request_id,
            offset,
            length,
            offer,
        });
        Ok(actions)
    }

    pub(crate) fn insert_outgoing_file(
        &mut self,
        owner: &TransferOwner<P>,
        file_id: u64,
        request_id: u64,
        expected_offset: u64,
        expected_length: u64,
        file: OutgoingFile,
    ) -> Result<(), TransferStateError<P>> {
        let transfer = self
            .local
            .get_mut(owner)
            .ok_or_else(|| TransferStateError::UnknownOwner(owner.clone()))?;
        if file.file_id != file_id {
            return Err(TransferStateError::UnknownFile {
                owner: owner.clone(),
                file_id,
            });
        }
        if transfer.outgoing.contains_key(&(file_id, request_id)) {
            return Err(TransferStateError::DuplicateRequest {
                owner: owner.clone(),
                file_id,
                request_id,
            });
        }
        let reservation = self
            .source_pending
            .get(&(owner.clone(), file_id, request_id))
            .copied()
            .ok_or_else(|| TransferStateError::UnknownRequest {
                owner: owner.clone(),
                file_id,
                request_id,
            })?;
        if reservation != (expected_offset, expected_length) {
            return Err(TransferStateError::InvalidRange(
                "source reservation mismatch".into(),
            ));
        }
        transfer.outgoing.insert((file_id, request_id), file);
        Ok(())
    }

    pub(crate) fn take_outgoing_file(
        &mut self,
        owner: &TransferOwner<P>,
        file_id: u64,
        request_id: u64,
    ) -> Result<OutgoingFile, TransferStateError<P>> {
        self.local
            .get_mut(owner)
            .ok_or_else(|| TransferStateError::UnknownOwner(owner.clone()))?
            .outgoing
            .remove(&(file_id, request_id))
            .ok_or_else(|| TransferStateError::UnknownRequest {
                owner: owner.clone(),
                file_id,
                request_id,
            })
    }

    pub(crate) fn source_chunk(
        &self,
        owner: &TransferOwner<P>,
        file_id: u64,
        request_id: u64,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        let transfer = self
            .local
            .get(owner)
            .ok_or_else(|| TransferStateError::UnknownOwner(owner.clone()))?;
        let file = transfer
            .outgoing
            .get(&(file_id, request_id))
            .ok_or_else(|| TransferStateError::UnknownRequest {
                owner: owner.clone(),
                file_id,
                request_id,
            })?;
        let reservation = self
            .source_pending
            .get(&(owner.clone(), file_id, request_id))
            .copied()
            .ok_or_else(|| TransferStateError::UnknownRequest {
                owner: owner.clone(),
                file_id,
                request_id,
            })?;
        let data_len = data.len() as u64;
        if offset != reservation.0
            || data_len > reservation.1
            || data.is_empty()
            || data.len() > MAX_CLIPBOARD_FILE_CHUNK_SIZE
        {
            return Err(TransferStateError::InvalidRange(
                "source chunk outside reservation".into(),
            ));
        }
        Ok(vec![TransferAction::Peer {
            peer: owner.peer.clone(),
            event: ProtoEvent::ClipboardFileChunk {
                transfer_id: owner.wire_id,
                file_id,
                request_id,
                offset,
                data,
            },
        }])
    }

    pub(crate) fn source_complete(
        &mut self,
        owner: &TransferOwner<P>,
        file_id: u64,
        request_id: u64,
        size: u64,
        digest: [u8; 32],
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        let transfer = self
            .local
            .get_mut(owner)
            .ok_or_else(|| TransferStateError::UnknownOwner(owner.clone()))?;
        transfer
            .outgoing
            .remove(&(file_id, request_id))
            .ok_or_else(|| TransferStateError::UnknownRequest {
                owner: owner.clone(),
                file_id,
                request_id,
            })?;
        self.source_pending
            .remove(&(owner.clone(), file_id, request_id));
        Ok(vec![TransferAction::Peer {
            peer: owner.peer.clone(),
            event: ProtoEvent::ClipboardFileComplete {
                transfer_id: owner.wire_id,
                file_id,
                request_id,
                size,
                digest,
            },
        }])
    }

    pub(crate) fn inbound_chunk(
        &mut self,
        peer: &P,
        wire_id: u64,
        file_id: u64,
        request_id: u64,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        let owner = TransferOwner {
            peer: peer.clone(),
            wire_id,
        };
        let index = (owner.clone(), file_id, request_id);
        let pending =
            self.pending
                .get_mut(&index)
                .ok_or_else(|| TransferStateError::UnknownRequest {
                    owner: owner.clone(),
                    file_id,
                    request_id,
                })?;
        pending
            .validator
            .validate_chunk(wire_id, file_id, request_id, offset, data.len())?;
        let received = u64::try_from(data.len()).map_err(|_| ProtocolError::LengthOverflow)?;
        pending.received = received;
        pending.hasher.update(&data);
        pending.chunk = data;
        let transfer = self
            .remote
            .get(&owner)
            .ok_or_else(|| TransferStateError::UnknownOwner(owner.clone()))?;
        let total = transfer
            .entries
            .get(&file_id)
            .ok_or_else(|| TransferStateError::UnknownFile {
                owner: owner.clone(),
                file_id,
            })?
            .size;
        let end = offset
            .checked_add(received)
            .ok_or(ProtocolError::LengthOverflow)?;
        let progress = TransferAction::Frontend(TransferFrontendEvent::Progress {
            ui_id: transfer.ui_id,
            owner: owner.clone(),
            file_id,
            completed: end,
            total,
        });
        if end == total {
            return Ok(vec![progress]);
        }
        if received != pending.key.length {
            return Err(TransferStateError::Protocol(
                ProtocolError::InvalidFileChunk("short non-EOF chunk"),
            ));
        }
        let pending = self
            .pending
            .remove(&index)
            .expect("validated pending request");
        Ok(vec![
            TransferAction::Adapter {
                adapter_id: transfer.adapter_id.clone(),
                message: AdapterMessage::RangeResponse(RangeResponse {
                    transfer_id: transfer.adapter_transfer_id.clone(),
                    request_id,
                    offset,
                    data_base64: base64::engine::general_purpose::STANDARD.encode(&pending.chunk),
                    eof: false,
                    error: None,
                }),
            },
            progress,
        ])
    }

    pub(crate) fn inbound_progress(
        &self,
        peer: &P,
        wire_id: u64,
        file_id: u64,
        completed: u64,
        total: u64,
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        if completed > total {
            return Err(TransferStateError::InvalidRange(
                "progress exceeds total".into(),
            ));
        }
        let owner = TransferOwner {
            peer: peer.clone(),
            wire_id,
        };
        let transfer = self
            .local
            .get(&owner)
            .ok_or_else(|| TransferStateError::UnknownOwner(owner.clone()))?;
        if !transfer
            .offer
            .entries
            .iter()
            .any(|entry| entry.file_id == file_id && entry.size == total)
        {
            return Err(TransferStateError::UnknownFile { owner, file_id });
        }
        Ok(vec![TransferAction::Frontend(
            TransferFrontendEvent::Progress {
                ui_id: transfer.ui_id,
                owner,
                file_id,
                completed,
                total,
            },
        )])
    }

    pub(crate) fn inbound_complete(
        &mut self,
        peer: &P,
        wire_id: u64,
        file_id: u64,
        request_id: u64,
        size: u64,
        digest: [u8; 32],
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        let owner = TransferOwner {
            peer: peer.clone(),
            wire_id,
        };
        let index = (owner.clone(), file_id, request_id);
        let expected_size = self
            .remote
            .get(&owner)
            .and_then(|t| t.entries.get(&file_id))
            .ok_or_else(|| TransferStateError::UnknownFile {
                owner: owner.clone(),
                file_id,
            })?
            .size;
        let pending =
            self.pending
                .get(&index)
                .ok_or_else(|| TransferStateError::UnknownRequest {
                    owner: owner.clone(),
                    file_id,
                    request_id,
                })?;
        let received_end = pending
            .key
            .offset
            .checked_add(pending.received)
            .ok_or(ProtocolError::LengthOverflow)?;
        if pending.received == 0 || received_end != expected_size {
            return Err(TransferStateError::Protocol(
                ProtocolError::InvalidFileChunk(
                    "completion does not cover the requested file range",
                ),
            ));
        }
        if size != expected_size {
            return Err(TransferStateError::Protocol(
                ProtocolError::InvalidFileChunk("completion size differs from manifest"),
            ));
        }
        let actual: [u8; 32] = pending.hasher.clone().finalize().into();
        if actual != digest {
            return Err(TransferStateError::DigestMismatch { owner, file_id });
        }
        let pending = self
            .pending
            .remove(&index)
            .expect("validated pending request");
        let transfer = self
            .remote
            .get(&pending.key.owner)
            .ok_or_else(|| TransferStateError::UnknownOwner(pending.key.owner.clone()))?;
        let actions = vec![
            TransferAction::Adapter {
                adapter_id: transfer.adapter_id.clone(),
                message: AdapterMessage::RangeResponse(RangeResponse {
                    transfer_id: transfer.adapter_transfer_id.clone(),
                    request_id,
                    offset: pending.key.offset,
                    data_base64: base64::engine::general_purpose::STANDARD.encode(&pending.chunk),
                    eof: true,
                    error: None,
                }),
            },
            TransferAction::Frontend(TransferFrontendEvent::Progress {
                ui_id: transfer.ui_id,
                owner: pending.key.owner,
                file_id,
                completed: pending.received,
                total: transfer
                    .entries
                    .get(&file_id)
                    .map(|entry| entry.size)
                    .unwrap_or(pending.received),
            }),
        ];
        Ok(actions)
    }

    pub(crate) fn inbound_cancel(
        &mut self,
        peer: &P,
        wire_id: u64,
        file_id: Option<u64>,
        reason: ClipboardCancelReason,
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        let owner = TransferOwner {
            peer: peer.clone(),
            wire_id,
        };
        self.cancel_owner(&owner, file_id, reason, false)
    }
    pub(crate) fn cancel_ui(
        &mut self,
        ui_id: UiTransferId,
        file_id: Option<u64>,
        reason: ClipboardCancelReason,
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        let owner = self
            .ui_to_owner
            .get(&ui_id)
            .cloned()
            .ok_or(TransferStateError::DuplicateUiId(ui_id))?;
        self.cancel_owner(&owner, file_id, reason, true)
    }
    pub(crate) fn cancel_adapter(
        &mut self,
        adapter_id: &str,
        transfer_id: &str,
        file_id: Option<u64>,
        reason: ClipboardCancelReason,
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        let owner = self
            .owners_for_adapter(adapter_id, transfer_id)
            .into_iter()
            .next()
            .ok_or_else(|| TransferStateError::UnknownAdapterTransfer {
                adapter_id: adapter_id.to_owned(),
                transfer_id: transfer_id.to_owned(),
            })?;
        self.cancel_owner(&owner, file_id, reason, true)
    }

    pub(crate) fn peer_lost(&mut self, peer: &P) -> Vec<TransferAction<P>> {
        let owners: Vec<_> = self
            .local
            .keys()
            .chain(self.remote.keys())
            .filter(|owner| &owner.peer == peer)
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        owners
            .into_iter()
            .flat_map(|owner| {
                self.cancel_owner(&owner, None, ClipboardCancelReason::IoError, false)
                    .unwrap_or_default()
            })
            .collect()
    }

    pub(crate) fn adapter_lost(&mut self, adapter_id: &str) -> Vec<TransferAction<P>> {
        let owners: HashSet<_> = self
            .adapter_to_owner
            .iter()
            .filter(|((id, _), _)| id == adapter_id)
            .flat_map(|(_, owners)| owners.iter().cloned())
            .collect();
        owners
            .into_iter()
            .flat_map(|owner| {
                self.cancel_owner(&owner, None, ClipboardCancelReason::IoError, true)
                    .unwrap_or_default()
            })
            .collect()
    }

    pub(crate) fn handle_protocol(
        &mut self,
        peer: &P,
        event: ProtoEvent,
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        match event {
            ProtoEvent::ClipboardFileChunk {
                transfer_id,
                file_id,
                request_id,
                offset,
                data,
            } => self.inbound_chunk(peer, transfer_id, file_id, request_id, offset, data),
            ProtoEvent::ClipboardFileComplete {
                transfer_id,
                file_id,
                request_id,
                size,
                digest,
            } => self.inbound_complete(peer, transfer_id, file_id, request_id, size, digest),
            ProtoEvent::ClipboardFileRequest {
                transfer_id,
                file_id,
                request_id,
                offset,
                length,
            } => self.source_file_request(peer, transfer_id, file_id, request_id, offset, length),
            ProtoEvent::ClipboardTransferCancel {
                transfer_id,
                file_id,
                reason,
            } => self.inbound_cancel(peer, transfer_id, file_id, reason),
            ProtoEvent::ClipboardTransferProgress {
                transfer_id,
                file_id,
                completed,
                total,
            } => self.inbound_progress(peer, transfer_id, file_id, completed, total),
            ProtoEvent::ClipboardManifest { .. } => Err(TransferStateError::InvalidManifest(
                "manifest requires an explicit adapter and operation".into(),
            )),
            _ => Err(TransferStateError::UnsupportedProtocolEvent),
        }
    }

    fn cancel_owner(
        &mut self,
        owner: &TransferOwner<P>,
        file_id: Option<u64>,
        reason: ClipboardCancelReason,
        notify_peer: bool,
    ) -> Result<Vec<TransferAction<P>>, TransferStateError<P>> {
        let announced = self
            .local
            .get(owner)
            .map(|t| t.announced)
            .or_else(|| self.remote.get(owner).map(|t| t.announced))
            .unwrap_or(false);
        let (cancel_completed, cancel_total) = self.transfer_totals(owner);
        let (ui_id, adapter_id, adapter_transfer_id) = if let Some(transfer) = self.local.get(owner)
        {
            (
                transfer.ui_id,
                transfer.adapter_id.clone(),
                transfer.adapter_transfer_id.clone(),
            )
        } else if let Some(transfer) = self.remote.get(owner) {
            (
                transfer.ui_id,
                transfer.adapter_id.clone(),
                transfer.adapter_transfer_id.clone(),
            )
        } else {
            return Err(TransferStateError::UnknownOwner(owner.clone()));
        };
        if let Some(file_id) = file_id {
            let known = self
                .local
                .get(owner)
                .map(|t| t.offer.entries.iter().any(|e| e.file_id == file_id))
                .or_else(|| {
                    self.remote
                        .get(owner)
                        .map(|t| t.entries.contains_key(&file_id))
                })
                .unwrap_or(false);
            if !known {
                return Err(TransferStateError::UnknownFile {
                    owner: owner.clone(),
                    file_id,
                });
            }
            self.pending.retain(|(candidate, candidate_file, _), _| {
                candidate != owner || *candidate_file != file_id
            });
            self.source_pending
                .retain(|(candidate, candidate_file, _), _| {
                    candidate != owner || *candidate_file != file_id
                });
            if let Some(local) = self.local.get_mut(owner) {
                local
                    .outgoing
                    .retain(|(candidate_file, _), _| *candidate_file != file_id);
            }
        } else {
            self.remove_owner(owner);
        }
        let mut actions = vec![TransferAction::Adapter {
            adapter_id,
            message: AdapterMessage::Cancel {
                transfer_id: adapter_transfer_id,
            },
        }];
        if announced {
            actions.push(TransferAction::Frontend(TransferFrontendEvent::Cancelled {
                ui_id,
                owner: owner.clone(),
                file_id,
                completed: cancel_completed,
                total: cancel_total,
                reason,
            }));
        }
        if notify_peer {
            actions.insert(
                0,
                TransferAction::Peer {
                    peer: owner.peer.clone(),
                    event: ProtoEvent::ClipboardTransferCancel {
                        transfer_id: owner.wire_id,
                        file_id,
                        reason,
                    },
                },
            );
        }
        Ok(actions)
    }

    fn ensure_owner_available(
        &self,
        owner: &TransferOwner<P>,
    ) -> Result<(), TransferStateError<P>> {
        if self.local.contains_key(owner) || self.remote.contains_key(owner) {
            return Err(TransferStateError::DuplicateOwner(owner.clone()));
        }
        Ok(())
    }

    fn ensure_ids_available(
        &self,
        owner: &TransferOwner<P>,
        adapter_id: &str,
        transfer_id: &str,
    ) -> Result<(), TransferStateError<P>> {
        self.ensure_owner_available(owner)?;
        if self
            .adapter_to_owner
            .contains_key(&(adapter_id.to_owned(), transfer_id.to_owned()))
        {
            return Err(TransferStateError::DuplicateAdapterTransfer {
                adapter_id: adapter_id.to_owned(),
                transfer_id: transfer_id.to_owned(),
            });
        }
        Ok(())
    }

    fn allocate_ui_id(&mut self) -> Result<UiTransferId, TransferStateError<P>> {
        let start = self.next_ui_id;
        loop {
            let id = self.next_ui_id;
            self.next_ui_id = self.next_ui_id.checked_add(1).unwrap_or(1);
            if id != 0 && !self.ui_to_owner.contains_key(&id) {
                return Ok(id);
            }
            if self.next_ui_id == start {
                return Err(TransferStateError::UiIdExhausted);
            }
        }
    }

    fn insert_mapping(
        &mut self,
        ui_id: UiTransferId,
        owner: TransferOwner<P>,
        adapter_id: AdapterId,
        transfer_id: String,
    ) {
        self.ui_to_owner.insert(ui_id, owner.clone());
        self.adapter_to_owner
            .entry((adapter_id, transfer_id))
            .or_default()
            .insert(owner);
    }

    fn transfer_totals(&self, owner: &TransferOwner<P>) -> (u64, u64) {
        if let Some(transfer) = self.local.get(owner) {
            let total = transfer.offer.entries.iter().map(|e| e.size).sum();
            let completed = transfer.transferred_by_file.values().copied().sum();
            return (completed, total);
        }
        if let Some(transfer) = self.remote.get(owner) {
            let total = transfer.entries.values().map(|e| e.size).sum();
            let completed = transfer.transferred_by_file.values().copied().sum();
            return (completed, total);
        }
        (0, 0)
    }

    fn transfer_ui_id(&self, owner: &TransferOwner<P>) -> Option<UiTransferId> {
        self.local
            .get(owner)
            .map(|transfer| transfer.ui_id)
            .or_else(|| self.remote.get(owner).map(|transfer| transfer.ui_id))
    }

    fn remove_owner(&mut self, owner: &TransferOwner<P>) {
        let ui_id = self
            .local
            .remove(owner)
            .map(|transfer| transfer.ui_id)
            .or_else(|| self.remote.remove(owner).map(|transfer| transfer.ui_id));
        if let Some(ui_id) = ui_id {
            self.ui_to_owner.remove(&ui_id);
        }
        self.adapter_to_owner.retain(|_, owners| {
            owners.remove(owner);
            !owners.is_empty()
        });
        self.pending
            .retain(|(candidate, _, _), _| candidate != owner);
        self.source_pending
            .retain(|(candidate, _, _), _| candidate != owner);
    }
}

fn validate_range<P: Debug>(
    size: u64,
    offset: u64,
    length: u64,
) -> Result<(), TransferStateError<P>> {
    if length == 0 || length > MAX_CLIPBOARD_FILE_CHUNK_SIZE as u64 {
        return Err(TransferStateError::InvalidRange(
            "length is zero or exceeds one wire chunk".into(),
        ));
    }
    let end = offset
        .checked_add(length)
        .ok_or_else(|| TransferStateError::InvalidRange("range overflows u64".into()))?;
    if offset > size || end > size {
        return Err(TransferStateError::InvalidRange(format!(
            "range {offset}..{end} exceeds file size {size}"
        )));
    }
    Ok(())
}

fn validate_wire_manifest<P: Debug>(
    entries: &[ClipboardManifestEntry],
) -> Result<(), TransferStateError<P>> {
    if entries.is_empty() || entries.len() > MAX_CLIPBOARD_MANIFEST_ENTRIES {
        return Err(TransferStateError::InvalidManifest(
            "invalid entry count".into(),
        ));
    }
    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    let mut total = 0u64;
    for entry in entries {
        if entry.file_id == 0 || !ids.insert(entry.file_id) {
            return Err(TransferStateError::InvalidManifest(
                "zero or duplicate file id".into(),
            ));
        }
        if entry.path.is_empty()
            || entry.path.len() > MAX_CLIPBOARD_PATH_SIZE
            || !paths.insert(&entry.path)
        {
            return Err(TransferStateError::InvalidManifest(
                "empty, oversized, or duplicate path".into(),
            ));
        }
        if entry.path.starts_with('/')
            || entry
                .path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(TransferStateError::InvalidManifest(
                "unsafe relative path".into(),
            ));
        }
        if entry.kind == ClipboardEntryKind::Directory && entry.size != 0 {
            return Err(TransferStateError::InvalidManifest(
                "directory has non-zero size".into(),
            ));
        }
        total = total
            .checked_add(entry.size)
            .ok_or_else(|| TransferStateError::InvalidManifest("manifest size overflow".into()))?;
        if total > MAX_CLIPBOARD_SIZE as u64 {
            return Err(TransferStateError::InvalidManifest(
                "manifest exceeds transfer size limit".into(),
            ));
        }
    }
    Ok(())
}
