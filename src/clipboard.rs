use arboard::ImageData;
use std::{
    borrow::Cow,
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
    },
};
use tokio::sync::mpsc;
#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) enum ClipboardContent {
    Text(String),
    Image {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Observation {
    Local,
    Remote,
}

#[derive(Default)]
pub(crate) struct ClipboardState {
    last: Option<ClipboardContent>,
    remote_origin: bool,
}

impl ClipboardState {
    fn observe(&mut self, content: ClipboardContent) -> Observation {
        if self.last.as_ref() == Some(&content) {
            return if self.remote_origin {
                Observation::Remote
            } else {
                Observation::Local
            };
        }
        self.last = Some(content);
        self.remote_origin = false;
        Observation::Local
    }
    fn queue_remote(&self, content: ClipboardContent) -> QueuedRemote {
        QueuedRemote {
            baseline: self.last.clone(),
            content,
        }
    }
    fn commit_remote(&mut self, queued: &QueuedRemote, current: Option<&ClipboardContent>) -> bool {
        if let Some(baseline) = queued.baseline.as_ref() {
            if Some(baseline) != current {
                if let Some(current) = current {
                    self.last = Some(current.clone());
                    self.remote_origin = false;
                }
                return false;
            }
        }
        self.last = Some(queued.content.clone());
        self.remote_origin = true;
        true
    }
}

struct QueuedRemote {
    baseline: Option<ClipboardContent>,
    content: ClipboardContent,
}

pub(crate) struct Clipboard {
    read_tx: SyncSender<()>,
    write_tx: SyncSender<QueuedRemote>,
    read_rx: mpsc::Receiver<Result<ClipboardContent, arboard::Error>>,
    state: Arc<Mutex<ClipboardState>>,
}

impl Clipboard {
    pub(crate) fn new() -> Self {
        let (read_tx, read_requests) = sync_channel(1);
        let (write_tx, write_requests) = sync_channel(1);
        let (result_tx, read_rx) = mpsc::channel(1);
        let state = Arc::new(Mutex::new(ClipboardState::default()));
        spawn_read_worker(read_requests, result_tx, state.clone());
        spawn_write_worker(write_requests, state.clone());
        Self {
            read_tx,
            write_tx,
            read_rx,
            state,
        }
    }
    pub(crate) fn read_once(&self) {
        match self.read_tx.try_send(()) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => log::warn!("clipboard read worker stopped"),
        }
    }
    pub(crate) fn write(&self, content: ClipboardContent) {
        let queued = self
            .state
            .lock()
            .expect("clipboard state poisoned")
            .queue_remote(content);
        match self.write_tx.try_send(queued) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => log::warn!("clipboard write worker stopped"),
        }
    }
    pub(crate) async fn next_read(&mut self) -> Result<ClipboardContent, arboard::Error> {
        self.read_rx
            .recv()
            .await
            .expect("clipboard read worker stopped")
    }
}

fn read_clipboard() -> Result<ClipboardContent, arboard::Error> {
    arboard::Clipboard::new().and_then(|mut clipboard| match clipboard.get_text() {
        Ok(text) => Ok(ClipboardContent::Text(text)),
        Err(arboard::Error::ContentNotAvailable) => {
            let image = clipboard.get_image()?;
            let width =
                u32::try_from(image.width).map_err(|_| arboard::Error::ConversionFailure)?;
            let height =
                u32::try_from(image.height).map_err(|_| arboard::Error::ConversionFailure)?;
            Ok(ClipboardContent::Image {
                width,
                height,
                rgba: image.into_owned_bytes().into_owned(),
            })
        }
        Err(error) => Err(error),
    })
}

fn spawn_read_worker(
    requests: Receiver<()>,
    results: mpsc::Sender<Result<ClipboardContent, arboard::Error>>,
    state: Arc<Mutex<ClipboardState>>,
) {
    std::thread::Builder::new()
        .name("lan-mouse-clipboard-read".into())
        .spawn(move || {
            while requests.recv().is_ok() {
                let result = read_clipboard().and_then(|content| {
                    let observation = state
                        .lock()
                        .expect("clipboard state poisoned")
                        .observe(content.clone());
                    if observation == Observation::Remote {
                        Err(arboard::Error::ContentNotAvailable)
                    } else {
                        Ok(content)
                    }
                });
                if results.blocking_send(result).is_err() {
                    break;
                }
            }
        })
        .expect("failed to start clipboard read worker");
}

fn spawn_write_worker(requests: Receiver<QueuedRemote>, state: Arc<Mutex<ClipboardState>>) {
    std::thread::Builder::new()
        .name("lan-mouse-clipboard-write".into())
        .spawn(move || {
            while let Ok(QueuedRemote { baseline, content }) = requests.recv() {
                let queued = QueuedRemote { baseline, content };
                let mut clipboard = match arboard::Clipboard::new() {
                    Ok(clipboard) => clipboard,
                    Err(e) => {
                        log::warn!("failed to open clipboard for writing: {e}");
                        continue;
                    }
                };
                let mut state = state.lock().expect("clipboard state poisoned");
                let current = read_clipboard().ok();
                if !state.commit_remote(&queued, current.as_ref()) {
                    continue;
                }
                let result = match queued.content {
                    ClipboardContent::Text(text) => clipboard.set_text(text),
                    ClipboardContent::Image {
                        width,
                        height,
                        rgba,
                    } => clipboard.set_image(ImageData {
                        width: width as usize,
                        height: height as usize,
                        bytes: Cow::Owned(rgba),
                    }),
                };
                if let Err(e) = result {
                    log::warn!("failed to write clipboard: {e}");
                    state.last = current;
                    state.remote_origin = false;
                }
            }
        })
        .expect("failed to start clipboard write worker");
}

#[cfg(test)]
mod tests {
    use super::{ClipboardContent, ClipboardState, Observation};
    fn text(value: &str) -> ClipboardContent {
        ClipboardContent::Text(value.into())
    }
    fn image(rgba: &[u8]) -> ClipboardContent {
        ClipboardContent::Image {
            width: 1,
            height: 1,
            rgba: rgba.into(),
        }
    }
    #[test]
    fn remote_content_is_never_observed_as_outbound() {
        let mut state = ClipboardState::default();
        let local = text("local");
        let remote = text("remote");
        assert_eq!(state.observe(local.clone()), Observation::Local);
        let queued = state.queue_remote(remote.clone());
        assert!(state.commit_remote(&queued, Some(&local)));
        assert_eq!(state.observe(remote.clone()), Observation::Remote);
        assert_eq!(state.observe(remote), Observation::Remote);
    }
    #[test]
    fn local_text_change_after_remote_content_is_outbound() {
        let mut state = ClipboardState::default();
        let remote = text("remote");
        let queued = state.queue_remote(remote.clone());
        assert!(state.commit_remote(&queued, None));
        assert_eq!(state.observe(remote), Observation::Remote);
        assert_eq!(state.observe(text("local")), Observation::Local);
    }
    #[test]
    fn local_image_change_after_remote_content_is_outbound() {
        let mut state = ClipboardState::default();
        let remote = image(&[0, 1, 2, 3]);
        let queued = state.queue_remote(remote.clone());
        assert!(state.commit_remote(&queued, None));
        assert_eq!(state.observe(remote), Observation::Remote);
        assert_eq!(state.observe(image(&[4, 5, 6, 7])), Observation::Local);
    }
    #[test]
    fn first_remote_write_allows_existing_clipboard() {
        let mut state = ClipboardState::default();
        let queued = state.queue_remote(text("remote"));
        assert!(state.commit_remote(&queued, Some(&text("existing"))));
    }
    #[test]
    fn stale_remote_write_is_rejected_after_local_mutation() {
        let mut state = ClipboardState::default();
        assert_eq!(state.observe(text("before")), Observation::Local);
        let queued = state.queue_remote(text("remote"));
        assert!(!state.commit_remote(&queued, Some(&text("after"))));
        assert_eq!(state.observe(text("after")), Observation::Local);
    }
    #[test]
    fn restored_remote_content_is_not_retransmitted() {
        let remote = text("remote");
        let mut state = ClipboardState {
            last: Some(remote.clone()),
            remote_origin: true,
        };
        assert_eq!(state.observe(remote), Observation::Remote);
        assert_eq!(state.observe(text("new local")), Observation::Local);
    }
}
