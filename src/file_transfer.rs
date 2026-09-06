use lan_mouse_proto::{
    ClipboardEntryKind, ClipboardManifestEntry, MAX_CLIPBOARD_FILE_CHUNK_SIZE,
    MAX_CLIPBOARD_MANIFEST_ENTRIES, MAX_CLIPBOARD_PATH_SIZE, MAX_CLIPBOARD_SIZE,
    MAX_MANUAL_FILE_CHUNK_SIZE,
};
use same_file::Handle as FileIdentity;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    io,
    path::{Component, Path, PathBuf},
    time::Instant,
};
use thiserror::Error;
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
};

pub(crate) type TransferId = u64;
pub(crate) type FileId = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SizeLimitPolicy {
    Clipboard,
    Unbounded,
}

impl SizeLimitPolicy {
    fn allows(self, total: u64) -> bool {
        matches!(self, Self::Unbounded) || total <= MAX_CLIPBOARD_SIZE as u64
    }
}

#[derive(Debug, Error)]
pub(crate) enum TransferError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("invalid file URI: {0}")]
    InvalidUri(String),
    #[error("unsafe transfer path: {0}")]
    UnsafePath(String),
    #[error("unsupported filesystem entry: {0}")]
    UnsupportedEntry(PathBuf),
    #[error("manifest contains too many entries")]
    TooManyEntries,
    #[error("manifest is larger than the clipboard transfer limit")]
    TooLarge,
    #[error("unknown or stale transfer {0}")]
    StaleTransfer(TransferId),
    #[error("unknown file {file_id} in transfer {transfer_id}")]
    UnknownFile {
        transfer_id: TransferId,
        file_id: FileId,
    },
    #[error("unexpected offset for file {file_id}: expected {expected}, got {actual}")]
    UnexpectedOffset {
        file_id: FileId,
        expected: u64,
        actual: u64,
    },
    #[error("size mismatch for file {file_id}: expected {expected}, got {actual}")]
    SizeMismatch {
        file_id: FileId,
        expected: u64,
        actual: u64,
    },
    #[error("digest mismatch for file {file_id}")]
    DigestMismatch { file_id: FileId },
    #[error("transfer was cancelled")]
    Cancelled,
    #[error("source changed during transfer")]
    SourceChanged { file_id: FileId },
}

#[derive(Clone, Debug)]
pub(crate) struct FileOffer {
    pub(crate) transfer_id: TransferId,
    pub(crate) entries: Vec<ClipboardManifestEntry>,
    sources: HashMap<FileId, (PathBuf, SourceIdentity)>,
}

#[derive(Clone, Debug)]
struct SourceIdentity {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

impl SourceIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
        }
    }

    fn matches(&self, metadata: &std::fs::Metadata) -> bool {
        let current_modified = metadata.modified().ok();
        if self.len != metadata.len()
            || self.modified.is_none()
            || current_modified.is_none()
            || self.modified != current_modified
        {
            return false;
        }
        #[cfg(unix)]
        {
            self.dev == metadata.dev() && self.ino == metadata.ino()
        }
        #[cfg(not(unix))]
        {
            true
        }
    }
}

impl FileOffer {
    pub(crate) fn from_uri_list(
        transfer_id: TransferId,
        text: &str,
    ) -> Result<Self, TransferError> {
        let mut roots = Vec::new();
        for line in text.lines() {
            let line = line.trim_end_matches('\r').trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            roots.push(file_uri_to_path(line)?);
        }
        if roots.is_empty() {
            return Err(TransferError::InvalidUri("empty URI list".into()));
        }
        Self::from_paths(transfer_id, &roots)
    }

    pub(crate) fn from_regular_file(
        transfer_id: TransferId,
        path: PathBuf,
    ) -> Result<Self, TransferError> {
        if transfer_id == 0 {
            return Err(TransferError::StaleTransfer(transfer_id));
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(TransferError::UnsupportedEntry(path));
        }
        let name = path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| TransferError::UnsafePath("invalid file name".into()))?;
        lan_mouse_proto::validate_manual_file_name(name)
            .map_err(|_| TransferError::UnsafePath("invalid file name".into()))?;
        let entry = ClipboardManifestEntry {
            file_id: 1,
            path: name.to_owned(),
            kind: ClipboardEntryKind::File,
            size: metadata.len(),
        };
        let mut sources = HashMap::new();
        sources.insert(1, (path, SourceIdentity::from_metadata(&metadata)));
        Ok(Self {
            transfer_id,
            entries: vec![entry],
            sources,
        })
    }

    fn from_paths(transfer_id: TransferId, roots: &[PathBuf]) -> Result<Self, TransferError> {
        let mut entries = Vec::new();
        let mut sources: HashMap<FileId, (PathBuf, SourceIdentity)> = HashMap::new();
        let mut names = HashSet::new();
        let mut total = 0u64;
        let mut next_id = 1u64;
        for root in roots {
            let metadata = std::fs::symlink_metadata(root)?;
            if metadata.file_type().is_symlink() {
                return Err(TransferError::UnsupportedEntry(root.clone()));
            }
            let base = root
                .file_name()
                .and_then(OsStr::to_str)
                .ok_or_else(|| TransferError::UnsafePath(root.display().to_string()))?;
            validate_relative_path(Path::new(base))?;
            if !names.insert(base.to_owned()) {
                return Err(TransferError::UnsafePath(format!(
                    "duplicate root name {base}"
                )));
            }
            collect_entry(
                root,
                Path::new(base),
                &mut next_id,
                &mut total,
                &mut entries,
                &mut sources,
            )?;
        }
        Ok(Self {
            transfer_id,
            entries,
            sources,
        })
    }

    pub(crate) async fn open(
        &self,
        file_id: FileId,
        offset: u64,
    ) -> Result<OutgoingFile, TransferError> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.file_id == file_id)
            .ok_or(TransferError::UnknownFile {
                transfer_id: self.transfer_id,
                file_id,
            })?;
        if entry.kind != ClipboardEntryKind::File || offset > entry.size {
            return Err(TransferError::UnexpectedOffset {
                file_id,
                expected: entry.size,
                actual: offset,
            });
        }
        let (path, identity) = self
            .sources
            .get(&file_id)
            .ok_or(TransferError::UnknownFile {
                transfer_id: self.transfer_id,
                file_id,
            })?;
        let mut options = tokio::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        let mut file = options.open(path).await?;
        let metadata = file.metadata().await?;
        if !metadata.is_file() || !identity.matches(&metadata) {
            return Err(TransferError::SourceChanged { file_id });
        }
        file.seek(SeekFrom::Start(0)).await?;
        let mut hasher = Sha256::new();
        let mut prefix = offset;
        let mut buffer = vec![0u8; MAX_CLIPBOARD_FILE_CHUNK_SIZE];
        while prefix > 0 {
            if !identity.matches(&file.metadata().await?) {
                return Err(TransferError::SourceChanged { file_id });
            }
            let want = prefix.min(buffer.len() as u64) as usize;
            let read = file.read(&mut buffer[..want]).await?;
            if read != want {
                return Err(TransferError::SizeMismatch {
                    file_id,
                    expected: offset,
                    actual: offset - prefix + read as u64,
                });
            }
            if !identity.matches(&file.metadata().await?) {
                return Err(TransferError::SourceChanged { file_id });
            }
            hasher.update(&buffer[..read]);
            prefix -= read as u64;
        }
        if !identity.matches(&file.metadata().await?) {
            return Err(TransferError::SourceChanged { file_id });
        }
        file.seek(SeekFrom::Start(offset)).await?;
        Ok(OutgoingFile {
            file_id,
            file,
            offset,
            size: entry.size,
            identity: identity.clone(),
            hasher,
            started: Instant::now(),
            cancelled: false,
        })
    }
}
pub(crate) struct OutgoingFile {
    pub(crate) file_id: FileId,
    file: File,
    offset: u64,
    size: u64,
    identity: SourceIdentity,
    started: Instant,
    hasher: Sha256,
    cancelled: bool,
}

impl OutgoingFile {
    pub(crate) async fn next_chunk(&mut self) -> Result<Option<(u64, Vec<u8>)>, TransferError> {
        self.next_chunk_bounded(MAX_CLIPBOARD_FILE_CHUNK_SIZE).await
    }

    pub(crate) async fn next_chunk_bounded(
        &mut self,
        max_len: usize,
    ) -> Result<Option<(u64, Vec<u8>)>, TransferError> {
        if self.cancelled {
            return Err(TransferError::Cancelled);
        }
        if self.offset == self.size {
            return Ok(None);
        }
        if max_len == 0 || max_len > MAX_CLIPBOARD_FILE_CHUNK_SIZE.max(MAX_MANUAL_FILE_CHUNK_SIZE) {
            return Err(TransferError::UnexpectedOffset {
                file_id: self.file_id,
                expected: self.offset,
                actual: self.offset,
            });
        }
        let limit = max_len;
        if !self.identity.matches(&self.file.metadata().await?) {
            return Err(TransferError::SourceChanged {
                file_id: self.file_id,
            });
        }
        let requested = usize::try_from((self.size - self.offset).min(limit as u64))
            .expect("chunk size fits usize");
        let mut data = vec![0; requested];
        let mut filled = 0;
        while filled < requested {
            if !self.identity.matches(&self.file.metadata().await?) {
                return Err(TransferError::SourceChanged {
                    file_id: self.file_id,
                });
            }
            let read = self.file.read(&mut data[filled..]).await?;
            if read == 0 {
                return Err(TransferError::SourceChanged {
                    file_id: self.file_id,
                });
            }
            filled += read;
            if !self.identity.matches(&self.file.metadata().await?) {
                return Err(TransferError::SourceChanged {
                    file_id: self.file_id,
                });
            }
        }
        let offset = self.offset;
        self.offset = self
            .offset
            .checked_add(filled as u64)
            .ok_or(TransferError::TooLarge)?;
        self.hasher.update(&data);
        Ok(Some((offset, data)))
    }
    pub(crate) async fn finalize_digest(&self) -> Result<[u8; 32], TransferError> {
        if self.offset != self.size {
            return Err(TransferError::SizeMismatch {
                file_id: self.file_id,
                expected: self.size,
                actual: self.offset,
            });
        }
        if !self.identity.matches(&self.file.metadata().await?) {
            return Err(TransferError::SourceChanged {
                file_id: self.file_id,
            });
        }
        Ok(self.hasher.clone().finalize().into())
    }
    pub(crate) fn is_eof(&self) -> bool {
        self.offset == self.size
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Progress {
    pub(crate) completed: u64,
    pub(crate) total: u64,
    pub(crate) bytes_per_second: u64,
}

impl Progress {
    fn new(completed: u64, total: u64, started: Instant) -> Self {
        let nanos = started.elapsed().as_nanos();
        let bytes_per_second = if nanos == 0 {
            0
        } else {
            ((completed as u128 * 1_000_000_000) / nanos).min(u64::MAX as u128) as u64
        };
        Self {
            completed,
            total,
            bytes_per_second,
        }
    }
}

pub(crate) struct IncomingTransfer {
    pub(crate) transfer_id: TransferId,
    destination: PathBuf,
    entries: HashMap<FileId, ClipboardManifestEntry>,
    files: HashMap<FileId, IncomingFile>,
    created_files: HashMap<PathBuf, FileIdentity>,
    created_dirs: HashSet<PathBuf>,
    cancelled: bool,
    finished: bool,
}

struct IncomingFile {
    path: PathBuf,
    file: File,
    expected: u64,
    offset: u64,
    hasher: Sha256,
    started: Instant,
}

struct CreatedFileGuard {
    path: PathBuf,
    identity: Option<FileIdentity>,
}

impl CreatedFileGuard {
    fn new(path: PathBuf, identity: FileIdentity) -> Self {
        Self {
            path,
            identity: Some(identity),
        }
    }

    fn disarm(mut self) -> FileIdentity {
        self.identity.take().expect("created file identity present")
    }
}

impl Drop for CreatedFileGuard {
    fn drop(&mut self) {
        if let Some(identity) = &self.identity {
            if owns_created_file(&self.path, identity) {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}
impl IncomingTransfer {
    pub(crate) async fn new(
        transfer_id: TransferId,
        entries: Vec<ClipboardManifestEntry>,
        destination: PathBuf,
    ) -> Result<Self, TransferError> {
        Self::new_with_size_policy(
            transfer_id,
            entries,
            destination,
            SizeLimitPolicy::Clipboard,
        )
        .await
    }

    pub(crate) async fn new_with_size_policy(
        transfer_id: TransferId,
        entries: Vec<ClipboardManifestEntry>,
        destination: PathBuf,
        size_limit: SizeLimitPolicy,
    ) -> Result<Self, TransferError> {
        if entries.is_empty() || entries.len() > MAX_CLIPBOARD_MANIFEST_ENTRIES {
            return Err(TransferError::TooManyEntries);
        }
        let destination = fs::canonicalize(destination).await?;
        let mut by_id = HashMap::new();
        let mut paths = HashSet::new();
        let mut total = 0u64;
        let mut previous = 0;
        for entry in entries {
            if entry.file_id == 0 || entry.file_id <= previous || !paths.insert(entry.path.clone())
            {
                return Err(TransferError::UnsafePath(entry.path));
            }
            previous = entry.file_id;
            validate_relative_path(Path::new(&entry.path))?;
            if entry.kind == ClipboardEntryKind::Directory && entry.size != 0 {
                return Err(TransferError::SizeMismatch {
                    file_id: entry.file_id,
                    expected: 0,
                    actual: entry.size,
                });
            }
            total = total
                .checked_add(entry.size)
                .ok_or(TransferError::TooLarge)?;
            if !size_limit.allows(total) {
                return Err(TransferError::TooLarge);
            }
            by_id.insert(entry.file_id, entry);
        }
        Ok(Self {
            transfer_id,
            destination,
            entries: by_id,
            files: HashMap::new(),
            created_files: HashMap::new(),
            created_dirs: HashSet::new(),
            cancelled: false,
            finished: false,
        })
    }

    pub(crate) fn destination_path(&self) -> PathBuf {
        self.destination.clone()
    }

    pub(crate) async fn prepare(&mut self) -> Result<Vec<(FileId, u64)>, TransferError> {
        self.ensure_active()?;
        let mut entries: Vec<_> = self.entries.values().cloned().collect();
        entries.sort_by_key(|entry| entry.file_id);
        let mut requests = Vec::new();
        for entry in entries {
            let relative = Path::new(&entry.path);
            let target = self.destination.join(relative);
            ensure_contained(&self.destination, &target)?;
            match entry.kind {
                ClipboardEntryKind::Directory => self.create_directory(&target).await?,
                ClipboardEntryKind::File => {
                    let parent = target
                        .parent()
                        .ok_or_else(|| TransferError::UnsafePath(entry.path.clone()))?;
                    self.create_parents(parent).await?;
                    let file = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create_new(true)
                        .open(&target)
                        .await?
                        .into_std()
                        .await;
                    let writer = match file.try_clone() {
                        Ok(writer) => writer,
                        Err(error) => {
                            if let Ok(identity) = FileIdentity::from_file(file) {
                                drop(CreatedFileGuard::new(target.clone(), identity));
                            }
                            return Err(error.into());
                        }
                    };
                    let identity = match FileIdentity::from_file(file) {
                        Ok(identity) => identity,
                        Err(error) => {
                            if let Ok(identity) = FileIdentity::from_file(writer) {
                                drop(CreatedFileGuard::new(target.clone(), identity));
                            }
                            return Err(error.into());
                        }
                    };
                    let file = File::from_std(writer);
                    let created = CreatedFileGuard::new(target.clone(), identity);
                    self.created_files.insert(target.clone(), created.disarm());
                    self.files.insert(
                        entry.file_id,
                        IncomingFile {
                            path: target,
                            file,
                            expected: entry.size,
                            offset: 0,
                            hasher: Sha256::new(),
                            started: Instant::now(),
                        },
                    );
                    requests.push((entry.file_id, 0));
                }
            }
        }
        Ok(requests)
    }

    pub(crate) async fn write_chunk(
        &mut self,
        file_id: FileId,
        offset: u64,
        data: &[u8],
    ) -> Result<Progress, TransferError> {
        self.ensure_active()?;
        if data.is_empty()
            || data.len() > MAX_CLIPBOARD_FILE_CHUNK_SIZE.max(MAX_MANUAL_FILE_CHUNK_SIZE)
        {
            return Err(TransferError::UnexpectedOffset {
                file_id,
                expected: offset,
                actual: offset,
            });
        }
        let incoming = self
            .files
            .get_mut(&file_id)
            .ok_or(TransferError::UnknownFile {
                transfer_id: self.transfer_id,
                file_id,
            })?;
        if offset != incoming.offset {
            return Err(TransferError::UnexpectedOffset {
                file_id,
                expected: incoming.offset,
                actual: offset,
            });
        }
        let next = offset
            .checked_add(data.len() as u64)
            .ok_or(TransferError::TooLarge)?;
        if next > incoming.expected {
            return Err(TransferError::SizeMismatch {
                file_id,
                expected: incoming.expected,
                actual: next,
            });
        }
        incoming.file.write_all(data).await?;
        incoming.hasher.update(data);
        incoming.offset = next;
        Ok(Progress::new(next, incoming.expected, incoming.started))
    }
    pub(crate) async fn resume_file(
        &mut self,
        file_id: FileId,
        offset: u64,
    ) -> Result<u64, TransferError> {
        self.ensure_active()?;
        let incoming = self
            .files
            .get_mut(&file_id)
            .ok_or(TransferError::UnknownFile {
                transfer_id: self.transfer_id,
                file_id,
            })?;
        if offset > incoming.expected {
            return Err(TransferError::UnexpectedOffset {
                file_id,
                expected: incoming.offset,
                actual: offset,
            });
        }
        incoming.file.seek(SeekFrom::Start(0)).await?;
        incoming.hasher = Sha256::new();
        let mut buffer = vec![0u8; MAX_CLIPBOARD_FILE_CHUNK_SIZE];
        let mut remaining = offset;
        while remaining > 0 {
            let want = remaining.min(buffer.len() as u64) as usize;
            let read = incoming.file.read(&mut buffer[..want]).await?;
            if read != want {
                return Err(TransferError::SizeMismatch {
                    file_id,
                    expected: offset,
                    actual: offset - remaining + read as u64,
                });
            }
            incoming.hasher.update(&buffer[..read]);
            remaining -= read as u64;
        }
        incoming.file.seek(SeekFrom::Start(offset)).await?;
        incoming.offset = offset;
        Ok(offset)
    }
    pub(crate) fn finish(&mut self) -> Result<(), TransferError> {
        self.ensure_active()?;
        if !self.files.is_empty() {
            return Err(TransferError::Cancelled);
        }
        self.finished = true;
        Ok(())
    }
    pub(crate) async fn finalize_file(
        &mut self,
        file_id: FileId,
        size: u64,
        digest: [u8; 32],
    ) -> Result<(), TransferError> {
        self.ensure_active()?;
        let incoming = self
            .files
            .get_mut(&file_id)
            .ok_or(TransferError::UnknownFile {
                transfer_id: self.transfer_id,
                file_id,
            })?;
        if size != incoming.expected || incoming.offset != incoming.expected {
            return Err(TransferError::SizeMismatch {
                file_id,
                expected: size,
                actual: incoming.offset,
            });
        }
        incoming.file.flush().await?;
        incoming.file.sync_data().await?;
        let actual: [u8; 32] = incoming.hasher.clone().finalize().into();
        if actual != digest {
            return Err(TransferError::DigestMismatch { file_id });
        }
        self.files.remove(&file_id).expect("file tracked");
        Ok(())
    }

    pub(crate) fn progress(&self, file_id: FileId) -> Option<Progress> {
        self.files
            .get(&file_id)
            .map(|f| Progress::new(f.offset, f.expected, f.started))
    }

    pub(crate) async fn cancel(&mut self) {
        if self.cancelled {
            return;
        }
        self.cancelled = true;
        self.files.clear();
        let files: Vec<_> = self.created_files.drain().collect();
        for (path, identity) in files {
            if owns_created_file(&path, &identity) {
                let _ = fs::remove_file(path).await;
            }
        }
        let mut dirs: Vec<_> = self.created_dirs.drain().collect();
        dirs.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for path in dirs {
            let _ = fs::remove_dir(path).await;
        }
    }
    fn ensure_active(&self) -> Result<(), TransferError> {
        if self.cancelled {
            Err(TransferError::Cancelled)
        } else {
            Ok(())
        }
    }

    async fn create_directory(&mut self, path: &Path) -> Result<(), TransferError> {
        self.create_parents(path).await
    }

    async fn create_parents(&mut self, path: &Path) -> Result<(), TransferError> {
        let relative = path
            .strip_prefix(&self.destination)
            .map_err(|_| TransferError::UnsafePath(path.display().to_string()))?;
        let mut current = self.destination.clone();
        for component in relative.components() {
            current.push(component.as_os_str());
            match fs::symlink_metadata(&current).await {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    return Err(TransferError::UnsafePath(current.display().to_string()));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(&current).await?;
                    self.created_dirs.insert(current.clone());
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}
impl Drop for IncomingTransfer {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.files.clear();
        for (path, identity) in self.created_files.drain() {
            if owns_created_file(&path, &identity) {
                let _ = std::fs::remove_file(path);
            }
        }
        let mut dirs: Vec<_> = self.created_dirs.drain().collect();
        dirs.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for path in dirs {
            let _ = std::fs::remove_dir(path);
        }
    }
}

fn owns_created_file(path: &Path, identity: &FileIdentity) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
        && FileIdentity::from_path(path).is_ok_and(|current| &current == identity)
}

fn collect_entry(
    source: &Path,
    relative: &Path,
    next_id: &mut u64,
    total: &mut u64,
    entries: &mut Vec<ClipboardManifestEntry>,
    sources: &mut HashMap<FileId, (PathBuf, SourceIdentity)>,
) -> Result<(), TransferError> {
    if entries.len() >= MAX_CLIPBOARD_MANIFEST_ENTRIES {
        return Err(TransferError::TooManyEntries);
    }
    validate_relative_path(relative)?;
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        return Err(TransferError::UnsupportedEntry(source.into()));
    }
    let id = *next_id;
    *next_id = next_id
        .checked_add(1)
        .ok_or(TransferError::TooManyEntries)?;
    let path = relative
        .to_str()
        .ok_or_else(|| TransferError::UnsafePath(relative.display().to_string()))?
        .replace(std::path::MAIN_SEPARATOR, "/");
    if metadata.is_file() {
        *total = total
            .checked_add(metadata.len())
            .ok_or(TransferError::TooLarge)?;
        if *total > MAX_CLIPBOARD_SIZE as u64 {
            return Err(TransferError::TooLarge);
        }
        entries.push(ClipboardManifestEntry {
            file_id: id,
            path,
            kind: ClipboardEntryKind::File,
            size: metadata.len(),
        });
        sources.insert(
            id,
            (
                source.to_path_buf(),
                SourceIdentity::from_metadata(&metadata),
            ),
        );
    } else if metadata.is_dir() {
        entries.push(ClipboardManifestEntry {
            file_id: id,
            path,
            kind: ClipboardEntryKind::Directory,
            size: 0,
        });
        let mut children: Vec<_> = std::fs::read_dir(source)?.collect::<Result<_, _>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            collect_entry(
                &child.path(),
                &relative.join(child.file_name()),
                next_id,
                total,
                entries,
                sources,
            )?;
        }
    } else {
        return Err(TransferError::UnsupportedEntry(source.into()));
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<(), TransferError> {
    let encoded = path
        .to_str()
        .ok_or_else(|| TransferError::UnsafePath(path.display().to_string()))?;
    if encoded.is_empty()
        || encoded.len() > MAX_CLIPBOARD_PATH_SIZE
        || encoded.contains('\\')
        || encoded.contains('\0')
    {
        return Err(TransferError::UnsafePath(encoded.into()));
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(TransferError::UnsafePath(encoded.into()));
        }
    }
    Ok(())
}

fn ensure_contained(root: &Path, path: &Path) -> Result<(), TransferError> {
    if !path.starts_with(root) || path == root {
        return Err(TransferError::UnsafePath(path.display().to_string()));
    }
    Ok(())
}

fn file_uri_to_path(uri: &str) -> Result<PathBuf, TransferError> {
    let encoded = uri
        .strip_prefix("file://")
        .ok_or_else(|| TransferError::InvalidUri(uri.into()))?;
    let encoded = if let Some(path) = encoded.strip_prefix("localhost/") {
        format!("/{path}")
    } else if encoded.starts_with('/') {
        encoded.to_owned()
    } else {
        return Err(TransferError::InvalidUri(uri.into()));
    };
    let mut decoded = Vec::with_capacity(encoded.len());
    let bytes = encoded.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(TransferError::InvalidUri(uri.into()));
            }
            let high =
                hex(bytes[index + 1]).ok_or_else(|| TransferError::InvalidUri(uri.into()))?;
            let low = hex(bytes[index + 2]).ok_or_else(|| TransferError::InvalidUri(uri.into()))?;
            decoded.push(high << 4 | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    if decoded.contains(&0) {
        return Err(TransferError::InvalidUri(uri.into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_vec(decoded));
        if !path.is_absolute() {
            return Err(TransferError::InvalidUri(uri.into()));
        }
        Ok(path)
    }
    #[cfg(not(unix))]
    {
        let path = PathBuf::from(
            String::from_utf8(decoded).map_err(|_| TransferError::InvalidUri(uri.into()))?,
        );
        if !path.is_absolute() {
            return Err(TransferError::InvalidUri(uri.into()));
        }
        Ok(path)
    }
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as std_fs;

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "lan-mouse-file-transfer-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std_fs::create_dir(&path).unwrap();
        path
    }

    fn file_entry(name: &str, size: u64) -> ClipboardManifestEntry {
        ClipboardManifestEntry {
            file_id: 1,
            path: name.into(),
            kind: ClipboardEntryKind::File,
            size,
        }
    }

    async fn partial_transfer(directory: &Path, name: &str) -> IncomingTransfer {
        let mut transfer =
            IncomingTransfer::new(1, vec![file_entry(name, 7)], directory.to_path_buf())
                .await
                .unwrap();
        transfer.prepare().await.unwrap();
        transfer.write_chunk(1, 0, b"partial").await.unwrap();
        transfer
    }

    #[tokio::test]
    async fn cancel_preserves_file_that_replaced_created_partial() {
        let directory = temp_dir("cancel-replacement");
        let target = directory.join("received.bin");
        let displaced = directory.join("displaced-partial.bin");
        let mut transfer = partial_transfer(&directory, "received.bin").await;

        transfer.files.clear();
        std_fs::rename(&target, &displaced).unwrap();
        std_fs::write(&target, b"unrelated replacement").unwrap();
        transfer.cancel().await;

        assert_eq!(std_fs::read(&target).unwrap(), b"unrelated replacement");
        std_fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn drop_preserves_file_that_replaced_created_partial() {
        let directory = temp_dir("drop-replacement");
        let target = directory.join("received.bin");
        let displaced = directory.join("displaced-partial.bin");
        let mut transfer = partial_transfer(&directory, "received.bin").await;

        transfer.files.clear();
        std_fs::rename(&target, &displaced).unwrap();
        std_fs::write(&target, b"unrelated replacement").unwrap();
        drop(transfer);

        assert_eq!(std_fs::read(&target).unwrap(), b"unrelated replacement");
        std_fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn cancel_removes_owned_partial_without_touching_neighbor() {
        let directory = temp_dir("cancel-owned");
        let target = directory.join("received.bin");
        let neighbor = directory.join("neighbor.bin");
        std_fs::write(&neighbor, b"keep me").unwrap();
        let mut transfer = partial_transfer(&directory, "received.bin").await;

        transfer.cancel().await;

        assert!(!target.exists());
        assert_eq!(std_fs::read(&neighbor).unwrap(), b"keep me");
        std_fs::remove_dir_all(directory).unwrap();
    }
}
