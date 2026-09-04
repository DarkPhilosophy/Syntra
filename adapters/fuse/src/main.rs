#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

#[cfg(target_os = "linux")]
mod linux {
    use fuser::{
        FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory,
        ReplyEntry, ReplyOpen, Request,
    };
    use lan_mouse_adapter_api::{
        EntryKind, Message, MountReady, Progress, RangeRequest, RangeResponse, RemoteManifest,
        Unmounted, read_messages, write_message,
    };
    use lan_mouse_proto::MAX_CLIPBOARD_FILE_CHUNK_SIZE;
    use libc::{EACCES, EINVAL, EIO, ENOENT, ENOTDIR, EROFS};
    use parking_lot::Mutex;
    use std::{
        collections::{HashMap, HashSet},
        ffi::OsStr,
        fs,
        io::{self, BufRead, BufReader, BufWriter},
        os::unix::ffi::OsStrExt,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant, SystemTime},
    };

    const TTL: Duration = Duration::from_secs(1);
    const ROOT: u64 = 1;
    const MAX_CHUNK: u32 = MAX_CLIPBOARD_FILE_CHUNK_SIZE as u32;
    const MAX_RANGE_ATTEMPTS: u64 = 4;
    const RANGE_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);

    #[derive(Clone)]
    struct Node {
        ino: u64,
        parent: u64,
        name: String,
        kind: EntryKind,
        size: u64,
        children: Vec<u64>,
        entry_id: u64,
    }

    struct TransferProgress {
        covered: HashMap<u64, Vec<(u64, u64)>>,
        completed: HashSet<u64>,
        completed_bytes: u64,
        total_bytes: u64,
        completed_entries: u64,
        total_entries: u64,
    }

    impl TransferProgress {
        fn record(&mut self, entry_id: u64, offset: u64, length: u64, file_size: u64) -> bool {
            let end = offset.saturating_add(length).min(file_size);
            let ranges = self.covered.entry(entry_id).or_default();
            if end > offset {
                ranges.push((offset, end));
                ranges.sort_unstable_by_key(|range| range.0);
                let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
                for &(start, end) in ranges.iter() {
                    if let Some(last) = merged.last_mut() {
                        if start <= last.1 {
                            last.1 = last.1.max(end);
                            continue;
                        }
                    }
                    merged.push((start, end));
                }
                *ranges = merged;
            }
            let entry_complete =
                ranges.iter().map(|(start, end)| end - start).sum::<u64>() == file_size;
            let previous = self.completed_bytes;
            self.completed_bytes = self
                .covered
                .values()
                .flat_map(|ranges| ranges.iter())
                .map(|(start, end)| end - start)
                .sum();
            if entry_complete {
                self.completed_entries += self.completed.insert(entry_id) as u64;
            }
            self.completed_bytes != previous
        }

        fn snapshot(&self) -> (u64, u64, u64, u64) {
            (self.completed_entries, self.total_entries, self.completed_bytes, self.total_bytes)
        }
    }

    struct Fs {
        nodes: HashMap<u64, Node>,
        children: HashMap<(u64, Vec<u8>), u64>,
        out: Arc<Mutex<BufWriter<io::Stdout>>>,
        pending: Arc<Mutex<HashMap<u64, mpsc::Sender<RangeResponse>>>>,
        next_request: AtomicU64,
        transfer_id: String,
        cancelled: Arc<AtomicBool>,
        progress: Arc<Mutex<TransferProgress>>,
    }

    fn safe_component(value: &str) -> bool {
        !value.is_empty()
            && value != "."
            && value != ".."
            && !value.contains('/')
            && !value.contains('\\')
            && !value.as_bytes().contains(&0)
    }

    fn attr(node: &Node) -> FileAttr {
        let directory = matches!(node.kind, EntryKind::Directory);
        FileAttr {
            ino: node.ino,
            size: node.size,
            blocks: node.size.div_ceil(512),
            atime: SystemTime::UNIX_EPOCH,
            mtime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
            crtime: SystemTime::UNIX_EPOCH,
            kind: if directory {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if directory { 0o500 } else { 0o400 },
            nlink: if directory { 2 } else { 1 },
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            flags: 0,
            blksize: 4096,
        }
    }

    impl Filesystem for Fs {
        fn lookup(&mut self, _: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
            let started = Instant::now();
            let key = (parent, name.as_bytes().to_vec());
            match self.children.get(&key).and_then(|ino| self.nodes.get(ino)) {
                Some(node) => {
                    eprintln!(
                        "clipboard-trace event=lookup direction=local transfer_id={} \
                         file_id={} parent_ino={} ino={} name_bytes={} outcome=found \
                         elapsed_ms={}",
                        self.transfer_id,
                        node.entry_id,
                        parent,
                        node.ino,
                        name.as_bytes().len(),
                        started.elapsed().as_millis()
                    );
                    reply.entry(&TTL, &attr(node), 0);
                }
                None => {
                    eprintln!(
                        "clipboard-trace event=lookup direction=local transfer_id={} \
                         parent_ino={} name_bytes={} outcome=error errno={} elapsed_ms={}",
                        self.transfer_id,
                        parent,
                        name.as_bytes().len(),
                        ENOENT,
                        started.elapsed().as_millis()
                    );
                    reply.error(ENOENT);
                }
            }
        }

        fn getattr(&mut self, _: &Request<'_>, ino: u64, reply: ReplyAttr) {
            match self.nodes.get(&ino) {
                Some(node) => reply.attr(&TTL, &attr(node)),
                None => reply.error(ENOENT),
            }
        }

        fn readdir(
            &mut self,
            _: &Request<'_>,
            ino: u64,
            _: u64,
            offset: i64,
            mut reply: ReplyDirectory,
        ) {
            let Some(node) = self.nodes.get(&ino).cloned() else {
                reply.error(ENOENT);
                return;
            };
            if !matches!(node.kind, EntryKind::Directory) {
                reply.error(ENOTDIR);
                return;
            }
            let mut entries = vec![
                (node.ino, FileType::Directory, ".".to_owned()),
                (node.parent, FileType::Directory, "..".to_owned()),
            ];
            entries.extend(
                node.children
                    .iter()
                    .filter_map(|ino| self.nodes.get(ino))
                    .map(|child| {
                        (
                            child.ino,
                            if matches!(child.kind, EntryKind::Directory) {
                                FileType::Directory
                            } else {
                                FileType::RegularFile
                            },
                            child.name.clone(),
                        )
                    }),
            );
            for (index, (entry_ino, kind, name)) in
                entries.into_iter().enumerate().skip(offset.max(0) as usize)
            {
                if reply.add(entry_ino, (index + 1) as i64, kind, name) {
                    break;
                }
            }
            reply.ok();
        }

        fn open(&mut self, _: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
            let started = Instant::now();
            let (file_id, outcome, errno) =
                if flags & (libc::O_WRONLY | libc::O_RDWR | libc::O_TRUNC | libc::O_CREAT) != 0 {
                    (self.nodes.get(&ino).map(|node| node.entry_id), "error", Some(EROFS))
                } else if let Some(node) = self.nodes.get(&ino) {
                    (Some(node.entry_id), "opened", None)
                } else {
                    (None, "error", Some(ENOENT))
                };
            eprintln!(
                "clipboard-trace event=open direction=local transfer_id={} file_id={:?} \
                 ino={} flags={} outcome={} errno={:?} elapsed_ms={}",
                self.transfer_id,
                file_id,
                ino,
                flags,
                outcome,
                errno,
                started.elapsed().as_millis()
            );
            match errno {
                Some(errno) => reply.error(errno),
                None => reply.opened(ino, 0),
            }
        }
        fn read(
            &mut self,
            _: &Request<'_>,
            ino: u64,
            _: u64,
            offset: i64,
            size: u32,
            _: i32,
            _: Option<u64>,
            reply: ReplyData,
        ) {
            let started = Instant::now();
            let file_id = self.nodes.get(&ino).map(|node| node.entry_id);
            eprintln!(
                "clipboard-trace event=read direction=local transfer_id={} file_id={:?} \
                 request_id=pending ino={} offset={} requested_bytes={} state=start",
                self.transfer_id, file_id, ino, offset, size
            );
            if self.cancelled.load(Ordering::Acquire) {
                eprintln!(
                    "clipboard-trace event=read direction=local transfer_id={} file_id={:?} \
                     ino={} offset={} requested_bytes={} returned_bytes=0 outcome=cancelled \
                     errno={} elapsed_ms={}",
                    self.transfer_id, file_id, ino, offset, size, EIO, started.elapsed().as_millis()
                );
                reply.error(EIO);
                return;
            }
            let Some(node) = self.nodes.get(&ino) else {
                eprintln!(
                    "clipboard-trace event=read direction=local transfer_id={} ino={} \
                     offset={} requested_bytes={} returned_bytes=0 outcome=error errno={} elapsed_ms={}",
                    self.transfer_id, ino, offset, size, ENOENT, started.elapsed().as_millis()
                );
                reply.error(ENOENT);
                return;
            };
            if !matches!(node.kind, EntryKind::File) || offset < 0 {
                eprintln!(
                    "clipboard-trace event=read direction=local transfer_id={} file_id={} \
                     ino={} offset={} requested_bytes={} returned_bytes=0 outcome=error errno={} elapsed_ms={}",
                    self.transfer_id, node.entry_id, ino, offset, size, EINVAL,
                    started.elapsed().as_millis()
                );
                reply.error(EINVAL);
                return;
            }
            let offset = offset as u64;
            if offset >= node.size {
                eprintln!(
                    "clipboard-trace event=read direction=local transfer_id={} file_id={} \
                     ino={} offset={} requested_bytes={} returned_bytes=0 outcome=eof errno=none elapsed_ms={}",
                    self.transfer_id, node.entry_id, ino, offset, size, started.elapsed().as_millis()
                );
                reply.data(&[]);
                return;
            }
            let total = size.min((node.size - offset) as u32);
            let transfer_id = self.transfer_id.clone();
            let entry_id = node.entry_id;
            let pending = Arc::clone(&self.pending);
            let node_size = node.size;
            let progress = Arc::clone(&self.progress);
            let progress_out = Arc::clone(&self.out);
            let cancelled = Arc::clone(&self.cancelled);
            let out = Arc::clone(&self.out);
            let reserved_requests = total.div_ceil(MAX_CHUNK) as u64 * MAX_RANGE_ATTEMPTS;
            let next_request = self.next_request.fetch_add(reserved_requests, Ordering::Relaxed);
            thread::spawn(move || {
                let mut data = Vec::with_capacity(total as usize);
                while data.len() < total as usize {
                    if cancelled.load(Ordering::Acquire) {
                        eprintln!(
                            "clipboard-trace event=read direction=local transfer_id={} file_id={} \
                             offset={} requested_bytes={} returned_bytes={} outcome=cancelled errno={} elapsed_ms={}",
                            transfer_id, entry_id, offset, total, data.len(), EIO, started.elapsed().as_millis()
                        );
                        reply.error(EIO);
                        return;
                    }
                    let chunk_offset = offset + data.len() as u64;
                    let length = (total as usize - data.len()).min(MAX_CHUNK as usize) as u32;
                    let chunk_index = data.len() as u64 / MAX_CHUNK as u64;
                    let mut response = None;
                    for attempt in 0..MAX_RANGE_ATTEMPTS {
                        let request_id = next_request + chunk_index * MAX_RANGE_ATTEMPTS + attempt;
                        eprintln!(
                            "clipboard-trace event=range-request direction=outbound transfer_id={} \
                             file_id={} request_id={} offset={} requested_bytes={} attempt={}",
                            transfer_id, entry_id, request_id, chunk_offset, length, attempt + 1
                        );
                        let (tx, rx) = mpsc::channel();
                        pending.lock().insert(request_id, tx);
                        let request = Message::RangeRequest(RangeRequest {
                            transfer_id: transfer_id.clone(), request_id, entry_id, offset: chunk_offset, length,
                        });
                        if write_message(&mut *out.lock(), &request).is_err() {
                            pending.lock().remove(&request_id);
                            eprintln!("clipboard-trace event=range-request direction=outbound transfer_id={} file_id={} request_id={} outcome=error errno={} elapsed_ms={}",
                                transfer_id, entry_id, request_id, EIO, started.elapsed().as_millis());
                            reply.error(EIO);
                            return;
                        }
                        match rx.recv_timeout(RANGE_ATTEMPT_TIMEOUT) {
                            Ok(received) => { response = Some((request_id, received)); break; }
                            Err(_) => { pending.lock().remove(&request_id); }
                        }
                    }
                    let Some((request_id, response)) = response else { reply.error(EIO); return; };
                    let Ok(bytes) = response.decode_data(length as usize) else { reply.error(EIO); return; };
                    if response.transfer_id != transfer_id || response.request_id != request_id ||
                        response.offset != chunk_offset || response.error.is_some() ||
                        bytes.is_empty() || bytes.len() > length as usize ||
                        (!response.eof && bytes.len() != length as usize) {
                        eprintln!("clipboard-trace event=range-response direction=inbound transfer_id={} file_id={} request_id={} offset={} requested_bytes={} returned_bytes={} outcome=error errno={} elapsed_ms={}",
                            transfer_id, entry_id, request_id, chunk_offset, length, bytes.len(), EIO, started.elapsed().as_millis());
                        reply.error(EIO);
                        return;
                    }
                    eprintln!("clipboard-trace event=range-response direction=inbound transfer_id={} file_id={} request_id={} offset={} requested_bytes={} returned_bytes={} outcome={} elapsed_ms={}",
                        transfer_id, entry_id, request_id, chunk_offset, length, bytes.len(), if response.eof {"eof"} else {"ok"}, started.elapsed().as_millis());
                    data.extend_from_slice(&bytes);
                    if response.eof { break; }
                }
                let returned = data.len();
                reply.data(&data);
                eprintln!("clipboard-trace event=progress direction=local transfer_id={} file_id={} offset={} requested_bytes={} returned_bytes={} outcome=progress elapsed_ms={}",
                    transfer_id, entry_id, offset, total, returned, started.elapsed().as_millis());
                if progress.lock().record(entry_id, offset, returned as u64, node_size) {
                    let (completed_entries, total_entries, completed_bytes, total_bytes) = progress.lock().snapshot();
                    let mut writer = progress_out.lock();
                    let _ = write_message(&mut *writer, &Message::Progress(Progress {
                        transfer_id: transfer_id.clone(), completed_entries, total_entries,
                        completed_bytes: Some(completed_bytes), total_bytes: Some(total_bytes),
                    }));
                }
            });
        }

        fn write(
            &mut self,
            _: &Request<'_>,
            _: u64,
            _: u64,
            _: i64,
            _: &[u8],
            _: u32,
            _: i32,
            _: Option<u64>,
            reply: fuser::ReplyWrite,
        ) {
            reply.error(EACCES);
        }
    }

    fn build_fs(
        manifest: &RemoteManifest,
        out: Arc<Mutex<BufWriter<io::Stdout>>>,
        pending: Arc<Mutex<HashMap<u64, mpsc::Sender<RangeResponse>>>>,
        cancelled: Arc<AtomicBool>,
        progress: Arc<Mutex<TransferProgress>>,
    ) -> io::Result<(Fs, Vec<String>)> {
        let mut nodes = HashMap::new();
        let mut children = HashMap::new();
        let mut paths = HashMap::new();
        nodes.insert(
            ROOT,
            Node {
                ino: ROOT,
                parent: ROOT,
                name: String::new(),
                kind: EntryKind::Directory,
                size: 0,
                children: Vec::new(),
                entry_id: 0,
            },
        );
        paths.insert(String::new(), ROOT);
        let mut next_ino = ROOT + 1;
        let mut roots = Vec::new();
        for entry in &manifest.entries {
            if !matches!(entry.kind, EntryKind::File | EntryKind::Directory) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsupported entry type",
                ));
            }
            let parts: Vec<&str> = entry.path.split('/').collect();
            if parts.is_empty() || parts.iter().any(|part| !safe_component(part)) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "unsafe path"));
            }
            let parent_path = parts[..parts.len() - 1].join("/");
            let parent = *paths.get(&parent_path).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "manifest parent must precede child",
                )
            })?;
            let name = parts[parts.len() - 1].to_owned();
            if children.contains_key(&(parent, name.as_bytes().to_vec())) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "duplicate path"));
            }
            let ino = next_ino;
            next_ino += 1;
            nodes
                .get_mut(&parent)
                .expect("parent exists")
                .children
                .push(ino);
            children.insert((parent, name.as_bytes().to_vec()), ino);
            paths.insert(entry.path.clone(), ino);
            if parent == ROOT {
                roots.push(entry.path.clone());
            }
            nodes.insert(
                ino,
                Node {
                    ino,
                    parent,
                    name,
                    kind: entry.kind.clone(),
                    size: entry.size.unwrap_or(0),
                    children: Vec::new(),
                    entry_id: entry.entry_id,
                },
            );
        }
        Ok((
            Fs {
                nodes,
                children,
                out,
                pending,
                next_request: AtomicU64::new(1),
                transfer_id: manifest.transfer_id.clone(),
                cancelled,
                progress,
            },
            roots,
        ))
    }

    fn mount_path(transfer_id: &str) -> io::Result<PathBuf> {
        if transfer_id.is_empty()
            || !transfer_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsafe transfer id",
            ));
        }
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is unset"))?;
        let path = runtime
            .join("lan-mouse")
            .join("clipboard")
            .join(transfer_id);
        fs::create_dir_all(&path)?;
        Ok(path)
    }

    fn file_uri(path: &Path) -> String {
        use percent_encoding::{NON_ALPHANUMERIC, percent_encode};
        let encoded = path
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(value) => {
                    Some(percent_encode(value.as_bytes(), NON_ALPHANUMERIC).to_string())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");
        if path.is_absolute() {
            format!("file:///{encoded}")
        } else {
            format!("file://{encoded}")
        }
    }

    pub fn run() -> io::Result<()> {
        let out = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
        write_message(
            &mut *out.lock(),
            &Message::Hello {
                protocol_version: lan_mouse_adapter_api::PROTOCOL_VERSION,
                adapter_id: "fuse".into(),
                name: "Read-only Clipboard FUSE".into(),
                capabilities: lan_mouse_adapter_api::Capabilities {
                    clipboard_read: true, paste: true, cancel: true, requires_live_mount: true,
                    mime_types: vec!["text/uri-list".into(), "x-special/gnome-copied-files".into()],
                },
            },
        ).map_err(|error| io::Error::other(error.to_string()))?;
        let mut reader = BufReader::new(io::stdin());
        let mut first_line = String::new();
        reader.read_line(&mut first_line)?;
        let manifest = match Message::decode_line(first_line.trim_end()) {
            Ok(Message::RemoteManifest(manifest)) => {
                eprintln!("clipboard-trace event=manifest-receipt direction=inbound transfer_id={} file_count={} total_bytes={} outcome=accepted elapsed_ms=0",
                    manifest.transfer_id, manifest.entries.len(), manifest.entries.iter().filter_map(|entry| entry.size).sum::<u64>());
                manifest
            }
            Ok(_) => {
                eprintln!("clipboard-trace event=manifest-receipt direction=inbound transfer_id=unknown outcome=error errno={} elapsed_ms=0", EINVAL);
                return Err(io::Error::new(io::ErrorKind::InvalidData, "expected remote manifest"));
            }
            Err(error) => {
                eprintln!("clipboard-trace event=manifest-receipt direction=inbound transfer_id=unknown outcome=error errno={} elapsed_ms=0", EINVAL);
                return Err(io::Error::new(io::ErrorKind::InvalidData, error));
            }
        };

        let pending = Arc::new(Mutex::new(
            HashMap::<u64, mpsc::Sender<RangeResponse>>::new(),
        ));
        let progress = Arc::new(Mutex::new(TransferProgress {
            covered: HashMap::new(), completed: HashSet::new(), completed_bytes: 0,
            total_bytes: manifest.entries.iter().filter_map(|e| e.size).sum(),
            completed_entries: 0,
            total_entries: manifest.entries.iter().filter(|e| matches!(e.kind, EntryKind::File)).count() as u64,
        }));
        let cancelled = Arc::new(AtomicBool::new(false));
        let (fs, roots) = build_fs(
            &manifest,
            Arc::clone(&out),
            Arc::clone(&pending),
            Arc::clone(&cancelled),
            Arc::clone(&progress),
        )?;
        let mount = mount_path(&manifest.transfer_id)?;
        let session = fuser::spawn_mount2(
            fs,
            &mount,
            &[
                MountOption::RO,
                MountOption::FSName("lan-mouse-clipboard".into()),
                MountOption::NoAtime,
                MountOption::DefaultPermissions,
            ],
        )?;

        let uris = roots
            .iter()
            .map(|path| file_uri(&mount.join(path)))
            .collect();
        write_message(
            &mut *out.lock(),
            &Message::MountReady(MountReady {
                transfer_id: manifest.transfer_id.clone(),
                mount_uri: file_uri(&mount),
                uris,
            }),
        )
        .map_err(|error| io::Error::other(error.to_string()))?;

        let transfer_id = manifest.transfer_id.clone();
        let reader_pending = Arc::clone(&pending);
        let reader_cancelled = Arc::clone(&cancelled);
        let (done_tx, done_rx) = mpsc::channel();
        thread::Builder::new()
            .name("lan-mouse-fuse-router".into())
            .spawn(move || {
                for message in read_messages(reader) {
                    match message {
                        Ok(Message::RangeResponse(response))
                            if response.transfer_id == transfer_id =>
                        {
                            if let Some(sender) = reader_pending.lock().remove(&response.request_id)
                            {
                                let _ = sender.send(response);
                            }
                        }
                        Ok(Message::Cancel { transfer_id: id }) if id == transfer_id => {
                            reader_cancelled.store(true, Ordering::Release);
                            break;
                        }
                        Ok(Message::Unmounted(event)) if event.transfer_id == transfer_id => break,
                        Ok(_) => {}
                        Err(_) => {
                            reader_cancelled.store(true, Ordering::Release);
                            break;
                        }
                    }
                }
                reader_pending.lock().clear();
                let _ = done_tx.send(());
            })?;

        let _ = done_rx.recv();
        drop(session);
        let remove_result = fs::remove_dir_all(&mount);
        let event = Message::Unmounted(Unmounted {
            transfer_id: manifest.transfer_id,
            success: remove_result.is_ok(),
            error: remove_result.err().map(|error| error.to_string()),
        });
        let result = write_message(&mut *out.lock(), &event)
            .map_err(|error| io::Error::other(error.to_string()));
        result
    }
}

#[cfg(target_os = "linux")]
fn main() -> std::io::Result<()> {
    linux::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("FUSE adapter is supported only on Linux");
}
