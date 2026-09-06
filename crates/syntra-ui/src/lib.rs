//! Syntra presentation layer: view models and the Slint dashboard.
//!
//! This crate is a client. It renders snapshots and sends typed intents; it
//! owns no networking, capture, emulation, transfer or authorisation state
//! machine. Anything it appears to decide is really the daemon's decision
//! echoed back.
//!
//! # Pipeline
//!
//! ```text
//! daemon --> syntra-api --> AppViewState --> Slint
//!        <-- syntra-api <-- UiIntent    <-- callbacks
//! ```
//!
//! * [`bridge`] — the transport traits. [`bridge::EventSource`] and
//!   [`bridge::RequestSink`] are what let the same presentation code run over
//!   IPC on the desktop and over in-process channels on Android.
//! * [`models`] — [`models::AppViewState`] reduces events into the single
//!   snapshot the UI renders.
//! * [`app`] — window construction, projection and callbacks.
//! * [`platform`] — per-OS tray, service control and window behaviour.
//!
//! The dashboard is expected to outlive any particular daemon: it starts
//! without one, reports the disconnected state, and reattaches when one
//! appears.

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
