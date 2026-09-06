#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleState {
    Background,
    BackgroundDisconnected,
    Foreground,
    Reconnecting,
    Resynchronizing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleEvent {
    Foregrounded,
    Backgrounded,
    TransportConnected,
    TransportLost,
    ResyncCompleted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleAction {
    Connect,
    RequestResync,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    Available,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostCapabilities {
    pub capture: Capability,
    pub emulation: Capability,
    pub clipboard_files: Capability,
}

impl HostCapabilities {
    pub const fn android_default() -> Self {
        Self {
            capture: Capability::Unavailable,
            emulation: Capability::Unavailable,
            clipboard_files: Capability::Unavailable,
        }
    }
}

impl From<HostCapabilities> for crate::models::PlatformCapabilities {
    fn from(capabilities: HostCapabilities) -> Self {
        Self {
            capture: capabilities.capture == Capability::Available,
            emulation: capabilities.emulation == Capability::Available,
            clipboard_files: capabilities.clipboard_files == Capability::Available,
        }
    }
}

impl LifecycleState {
    pub const fn presentation_event(self) -> crate::models::TransportLifecycleEvent {
        use crate::models::TransportLifecycleEvent::*;

        match self {
            Self::Foreground | Self::Background => Resynchronized,
            Self::Reconnecting | Self::Resynchronizing => Reconnecting,
            Self::BackgroundDisconnected => DaemonUnavailable,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Visibility {
    Foreground,
    Background,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    Disconnected,
    Connecting,
    Connected,
    Resynchronizing,
}

pub struct Lifecycle {
    visibility: Visibility,
    link: Link,
}

impl Lifecycle {
    pub fn new(connected: bool) -> Self {
        Self {
            visibility: Visibility::Background,
            link: if connected {
                Link::Connected
            } else {
                Link::Disconnected
            },
        }
    }

    pub fn state(&self) -> LifecycleState {
        match (self.visibility, self.link) {
            (Visibility::Foreground, Link::Connected) => LifecycleState::Foreground,
            (Visibility::Foreground, Link::Resynchronizing) => LifecycleState::Resynchronizing,
            (Visibility::Foreground, _) => LifecycleState::Reconnecting,
            (Visibility::Background, Link::Connected) => LifecycleState::Background,
            (Visibility::Background, _) => LifecycleState::BackgroundDisconnected,
        }
    }

    pub fn transition(&mut self, event: LifecycleEvent) -> Vec<LifecycleAction> {
        use LifecycleAction::*;

        match event {
            LifecycleEvent::Foregrounded => {
                self.visibility = Visibility::Foreground;
                if self.link == Link::Disconnected {
                    self.link = Link::Connecting;
                    vec![Connect]
                } else {
                    vec![]
                }
            }
            LifecycleEvent::Backgrounded => {
                self.visibility = Visibility::Background;
                vec![]
            }
            LifecycleEvent::TransportLost => {
                if matches!(self.link, Link::Connected | Link::Resynchronizing) {
                    self.link = Link::Connecting;
                    vec![Connect]
                } else {
                    vec![]
                }
            }
            LifecycleEvent::TransportConnected => {
                if self.link == Link::Connecting {
                    self.link = Link::Resynchronizing;
                    vec![RequestResync]
                } else {
                    vec![]
                }
            }
            LifecycleEvent::ResyncCompleted => {
                if self.link == Link::Resynchronizing {
                    self.link = Link::Connected;
                }
                vec![]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_foreground_starts_connection_once() {
        let mut lifecycle = Lifecycle::new(false);

        assert_eq!(lifecycle.state(), LifecycleState::BackgroundDisconnected);
        assert_eq!(
            lifecycle.transition(LifecycleEvent::Foregrounded),
            vec![LifecycleAction::Connect]
        );
        assert_eq!(lifecycle.state(), LifecycleState::Reconnecting);
        assert!(
            lifecycle
                .transition(LifecycleEvent::Foregrounded)
                .is_empty()
        );
    }

    #[test]
    fn connected_foreground_does_not_start_another_connection() {
        let mut lifecycle = Lifecycle::new(true);

        assert!(
            lifecycle
                .transition(LifecycleEvent::Foregrounded)
                .is_empty()
        );
        assert_eq!(lifecycle.state(), LifecycleState::Foreground);
    }

    #[test]
    fn connection_resynchronizes_before_becoming_ready() {
        let mut lifecycle = Lifecycle::new(false);
        lifecycle.transition(LifecycleEvent::Foregrounded);

        assert_eq!(
            lifecycle.transition(LifecycleEvent::TransportConnected),
            vec![LifecycleAction::RequestResync]
        );
        assert_eq!(lifecycle.state(), LifecycleState::Resynchronizing);
        assert!(
            lifecycle
                .transition(LifecycleEvent::ResyncCompleted)
                .is_empty()
        );
        assert_eq!(lifecycle.state(), LifecycleState::Foreground);
    }

    #[test]
    fn duplicate_transport_loss_starts_one_reconnection() {
        let mut lifecycle = Lifecycle::new(true);

        assert_eq!(
            lifecycle.transition(LifecycleEvent::TransportLost),
            vec![LifecycleAction::Connect]
        );
        assert!(
            lifecycle
                .transition(LifecycleEvent::TransportLost)
                .is_empty()
        );
    }

    #[test]
    fn background_preserves_connection_progress_without_duplicate_connect() {
        let mut lifecycle = Lifecycle::new(false);
        lifecycle.transition(LifecycleEvent::Foregrounded);

        assert!(
            lifecycle
                .transition(LifecycleEvent::Backgrounded)
                .is_empty()
        );
        assert_eq!(lifecycle.state(), LifecycleState::BackgroundDisconnected);
        assert!(
            lifecycle
                .transition(LifecycleEvent::Foregrounded)
                .is_empty()
        );
    }

    #[test]
    fn capabilities_are_explicit() {
        let capabilities = HostCapabilities::android_default();
        assert_eq!(
            crate::models::PlatformCapabilities::from(capabilities),
            crate::models::PlatformCapabilities::default()
        );
    }
}
