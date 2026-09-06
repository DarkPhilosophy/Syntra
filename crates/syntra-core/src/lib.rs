//! Syntra daemon core: everything the service does, with no user interface.
//!
//! This crate is the tier-1 implementation. It owns input capture and
//! emulation, peer discovery and transport, clipboard synchronisation, file
//! transfers and clipboard history. It never depends on a presentation
//! crate, which is what allows the daemon to run on a headless machine and
//! start before any desktop session exists.
//!
//! # Structure
//!
//! [`service::Service`] is the orchestrator: it owns the subsystems and
//! drives a single event loop over them. The subsystems are:
//!
//! * [`capture`] and [`emulation`] — pointer and keyboard input in and out.
//! * [`connect`], [`listen`], [`discovery`], [`dns`] — reaching peers.
//! * [`clipboard`], [`history`], [`history_sync`] — shared clipboard state.
//! * [`file_transfer`], [`transfer_manager`], [`manual_transfer`] — moving
//!   files: byte-level primitives, the clipboard-driven state machine, and
//!   the explicitly approved one respectively. They stay separate because
//!   their lifecycles and approval rules genuinely differ.
//! * [`adapter_manager`] — supervises out-of-process plugins.
//!
//! # Talking to clients
//!
//! The core does not decide how state is presented. It emits
//! [`syntra_api::FrontendEvent`] and applies [`syntra_api::FrontendRequest`],
//! and any number of clients may attach, detach and reattach at will.

mod adapter_manager;
mod capture;
pub mod capture_test;
pub mod client;
mod clipboard;
pub mod config;
mod connect;
mod crypto;
mod discovery;
mod dns;
mod emulation;
pub mod emulation_test;
mod file_transfer;
mod history;
mod history_sync;
mod listen;
mod manual_transfer;
mod peer_profile;
pub mod service;
mod transfer_manager;
