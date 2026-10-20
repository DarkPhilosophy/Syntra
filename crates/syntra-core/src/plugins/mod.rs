//! Out-of-process plugins: discovery, trust, routing and supervision.
//!
//! Plugins are separate processes that add optional capabilities. The daemon is
//! the master: it owns the list, decides what may run, and starts and stops the
//! processes. Clients render this state and ask for changes; they never touch a
//! plugin process themselves.
//!
//! # Structure
//!
//! | module | owns |
//! |---|---|
//! | [`discovery`] | reading manifests and building the bundled set; reads files, never runs anything |
//! | [`trust`] | content-bound approvals and role preferences, persisted |
//! | [`package_store`] | the packages Syntra installed itself, and removing exactly those |
//! | [`registry`] | what is known, who vouches for it, which plugin provides each role, the state shown to clients |
//! | [`routing`] | pure rules: provider selection, handshake validation, what a grant permits |
//! | [`process`] | how a process is started, contained and ended; environment, diagnostics |
//! | [`supervisor`] | the processes themselves: protocol, generations, restarts |
//!
//! # The contract
//!
//! One [`syntra_plugin_api::PluginManifest`] describes a plugin for discovery,
//! the schema in `plugins/manifest.schema.json` and every packaged example. A
//! manifest is a claim, never a grant: what a plugin may do is what the user
//! approved for its content digest, narrowed by what its process announces.
//! Three directories are read, in order, and a later source replaces an
//! earlier plugin of the same id:
//!
//! 1. the three plugins the distribution ships, whose descriptors are compiled
//!    in (their executables sit beside the daemon);
//! 2. `<config>/plugins`, where a user drops third-party plugins;
//! 3. `<config>/plugin-store`, where Syntra installs packages it owns.

pub(crate) mod discovery;
pub(crate) mod package_store;
pub(crate) mod process;
pub(crate) mod registry;
pub(crate) mod routing;
pub(crate) mod supervisor;
pub(crate) mod trust;

#[cfg(test)]
mod e2e_tests;

pub(crate) use discovery::{CLIPBOARD_PLUGIN_ID, EDGE_GLOW_PLUGIN_ID, FUSE_PLUGIN_ID};
pub(crate) use registry::PluginRegistry;
