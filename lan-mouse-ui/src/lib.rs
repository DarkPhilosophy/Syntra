pub mod app;
pub mod bridge;
pub mod diagnostics;
#[cfg(not(target_os = "android"))]
pub mod file_drop;
pub mod history;
pub mod localization;
pub mod models;
pub mod platform;
pub mod settings;

#[cfg(any(target_os = "android", test))]
pub mod android;
pub(crate) mod avatar;
pub mod device_identity;
pub mod lifecycle;
pub(crate) mod manual_ui;
#[cfg(not(target_os = "android"))]
pub mod single_instance;
pub mod updates;
