pub mod service;

use std::fmt;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloseBehavior {
    Hide,
    Exit,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlatformCapabilities {
    pub tray: bool,
    pub close_behavior: CloseBehavior,
    pub flatpak_permissions: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlatpakApplication {
    pub application_id: String,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlatformAction {
    Show,
    Quit,
    RequestPermissions,
    OpenPermissionSettings,
    RelaunchAfterPermissionGrant,
    GrantFlatpakFilesystemAccess { application_id: String },
    NotifyRunningInBackground,
}
#[derive(Clone)]
pub struct PlatformCallbacks {
    show_callback: Arc<dyn Fn() + Send + Sync>,
    quit_callback: Arc<dyn Fn() + Send + Sync>,
}
impl PlatformCallbacks {
    pub fn new(
        show: impl Fn() + Send + Sync + 'static,
        quit: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            show_callback: Arc::new(show),
            quit_callback: Arc::new(quit),
        }
    }
    pub fn show(&self) {
        (self.show_callback)();
    }
    pub fn quit(&self) {
        (self.quit_callback)();
    }
}
#[derive(Debug)]
pub enum PlatformError {
    UnsupportedAction(&'static str),
    Tray(String),
    Permission(String),
    Flatpak(String),
}
impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for PlatformError {}
pub trait PlatformTray: Send {}
pub trait PlatformActions: Send + Sync {
    fn capabilities(&self) -> PlatformCapabilities;
    fn installed_flatpak_applications(&self) -> Result<Vec<FlatpakApplication>, PlatformError> {
        Err(PlatformError::UnsupportedAction(
            "Flatpak application discovery is Linux-only",
        ))
    }
    fn perform(&self, action: PlatformAction) -> Result<(), PlatformError>;
    fn start_tray(
        &self,
        callbacks: PlatformCallbacks,
    ) -> Result<Box<dyn PlatformTray>, PlatformError>;
}
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::LinuxPlatform;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;
pub fn platform() -> Box<dyn PlatformActions> {
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LinuxPlatform::new())
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacPlatform::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsPlatform::new())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    panic!("unsupported platform")
}
