//! Capture fed by the app's own touchpad screen.
//!
//! On a phone there is no system input to intercept: the user drives a
//! remote computer from a touchpad surface drawn by the app. The interface
//! pushes events through [`sender`]; this backend turns them into the same
//! capture stream every desktop backend produces, so routing, key release
//! and the peer protocol are shared unchanged.

use std::pin::Pin;
use std::sync::{LazyLock, Mutex};
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures_core::Stream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::{Capture, CaptureError, CaptureEvent, Position};

type Item = (Position, CaptureEvent);

struct Channel {
    tx: UnboundedSender<Item>,
    rx: Mutex<Option<UnboundedReceiver<Item>>>,
}

static CHANNEL: LazyLock<Channel> = LazyLock::new(|| {
    let (tx, rx) = unbounded_channel();
    Channel {
        tx,
        rx: Mutex::new(Some(rx)),
    }
});

fn channel() -> &'static Channel {
    &CHANNEL
}

/// Handle the interface uses to inject touchpad input.
#[derive(Clone)]
pub struct TouchpadSender(UnboundedSender<Item>);

impl TouchpadSender {
    /// Starts controlling the device placed at `position`.
    pub fn begin(&self, position: Position) {
        let _ = self.0.send((position, CaptureEvent::Begin));
    }

    /// Sends one input event to the device at `position`.
    pub fn input(&self, position: Position, event: syntra_input_event::Event) {
        let _ = self.0.send((position, CaptureEvent::Input(event)));
    }
}

/// Returns the process-wide touchpad input handle.
pub fn sender() -> TouchpadSender {
    TouchpadSender(channel().tx.clone())
}

pub(crate) struct TouchpadInputCapture {
    rx: UnboundedReceiver<Item>,
}

impl TouchpadInputCapture {
    pub(crate) fn new() -> Result<Self, super::CaptureCreationError> {
        // One capture at a time owns the stream; a recreated capture after
        // a restart takes it over.
        let rx = channel()
            .rx
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .unwrap_or_else(|| {
                let (tx, rx) = unbounded_channel();
                drop(tx);
                rx
            });
        Ok(Self { rx })
    }
}

impl Drop for TouchpadInputCapture {
    fn drop(&mut self) {
        // Hand the receiver back so a recreated capture keeps working.
        let (_tx, empty) = unbounded_channel();
        let rx = std::mem::replace(&mut self.rx, empty);
        if let Ok(mut slot) = channel().rx.lock() {
            *slot = Some(rx);
        }
    }
}

#[async_trait(?Send)]
impl Capture for TouchpadInputCapture {
    async fn create(&mut self, _pos: Position) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn destroy(&mut self, _pos: Position) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
}

impl Stream for TouchpadInputCapture {
    type Item = Result<Item, CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx).map(|item| item.map(Ok))
    }
}
