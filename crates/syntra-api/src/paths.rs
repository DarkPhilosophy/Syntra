//! Single source of truth for every runtime-visible name and path.
//!
//! Both tiers resolve these through this module: the daemon creates the
//! endpoints and the clients connect to them. Defining a socket name in two
//! places is a silent-failure bug, so nothing outside this module may spell
//! one out.
//!
//! Every location honours an environment override, which keeps test runs,
//! sandboxes and side-by-side installations from colliding without needing a
//! rebuild.

use std::env::{self, VarError};
use std::path::PathBuf;

use thiserror::Error;

/// Reverse-DNS application identifier.
///
/// Used for the XDG app id, desktop-portal handshakes, the macOS bundle and
/// the Android application id. Changing it invalidates portal permissions
/// previously granted by the user.
pub const APPLICATION_ID: &str = "io.syntra.Syntra";

/// Short, lowercase name used for directories, sockets and service units.
pub const APPLICATION_SLUG: &str = "syntra";

/// Overrides [`daemon_socket`].
pub const ENV_DAEMON_SOCKET: &str = "SYNTRA_DAEMON_SOCKET";
/// Overrides [`diagnostics_socket`].
pub const ENV_DIAGNOSTICS_SOCKET: &str = "SYNTRA_DIAGNOSTICS_SOCKET";
/// Overrides [`config_dir`].
pub const ENV_CONFIG_DIR: &str = "SYNTRA_CONFIG_DIR";
/// Selects the log filter for every Syntra process.
pub const ENV_LOG: &str = "SYNTRA_LOG";

/// Loopback port used by the Windows IPC transport, which has no Unix sockets.
pub const DEFAULT_IPC_PORT: u16 = 5252;

/// Default UDP/TCP port used between peers.
pub const DEFAULT_PEER_PORT: u16 = 4242;

const DAEMON_SOCKET_FILE: &str = "syntra-daemon.sock";
const DIAGNOSTICS_SOCKET_FILE: &str = "syntra-diagnostics.sock";

/// Failure to locate a per-user runtime or configuration directory.
#[derive(Debug, Error)]
pub enum PathError {
    /// `$XDG_RUNTIME_DIR` is required on Linux and BSD but was not set.
    #[error("could not determine $XDG_RUNTIME_DIR: `{0}`")]
    XdgRuntimeDirNotFound(VarError),
    /// `$HOME` is required on macOS but was not set.
    #[error("could not determine $HOME: `{0}`")]
    HomeDirNotFound(VarError),
    /// No per-user configuration directory could be determined.
    #[error("could not determine a configuration directory")]
    ConfigDirNotFound,
    /// Android sandboxes the filesystem and offers no Unix socket transport.
    #[error("unix socket transport is unavailable on Android")]
    AndroidUnavailable,
}

/// Directory holding sockets and other per-user runtime state.
#[cfg(all(unix, not(target_os = "android"), not(target_os = "macos")))]
fn runtime_dir() -> Result<PathBuf, PathError> {
    env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map_err(PathError::XdgRuntimeDirNotFound)
}

#[cfg(all(unix, target_os = "macos"))]
fn runtime_dir() -> Result<PathBuf, PathError> {
    env::var("HOME")
        .map(|home| PathBuf::from(home).join("Library").join("Caches"))
        .map_err(PathError::HomeDirNotFound)
}

#[cfg(target_os = "android")]
fn runtime_dir() -> Result<PathBuf, PathError> {
    Err(PathError::AndroidUnavailable)
}

#[cfg(windows)]
fn runtime_dir() -> Result<PathBuf, PathError> {
    Ok(env::temp_dir())
}

fn resolve(env_key: &str, file: &str) -> Result<PathBuf, PathError> {
    if let Some(value) = env::var_os(env_key) {
        return Ok(PathBuf::from(value));
    }
    Ok(runtime_dir()?.join(file))
}

/// Control socket a client uses to reach the daemon.
///
/// Override with [`ENV_DAEMON_SOCKET`].
pub fn daemon_socket() -> Result<PathBuf, PathError> {
    resolve(ENV_DAEMON_SOCKET, DAEMON_SOCKET_FILE)
}

/// Datagram socket the daemon publishes structured log records on.
///
/// Clients bind it to render a live diagnostics view. Override with
/// [`ENV_DIAGNOSTICS_SOCKET`].
pub fn diagnostics_socket() -> Result<PathBuf, PathError> {
    resolve(ENV_DIAGNOSTICS_SOCKET, DIAGNOSTICS_SOCKET_FILE)
}

/// Per-user configuration directory holding the config file and certificate.
///
/// Override with [`ENV_CONFIG_DIR`].
pub fn config_dir() -> Result<PathBuf, PathError> {
    if let Some(value) = env::var_os(ENV_CONFIG_DIR) {
        return Ok(PathBuf::from(value));
    }
    dirs::config_dir()
        .map(|base| base.join(APPLICATION_SLUG))
        .ok_or(PathError::ConfigDirNotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An override must win over the platform default, so parallel test runs
    /// and sandboxes can relocate the endpoint without a rebuild.
    #[test]
    fn environment_override_replaces_default_socket() {
        // SAFETY: single-threaded test, restored before returning.
        let previous = env::var_os(ENV_DAEMON_SOCKET);
        unsafe { env::set_var(ENV_DAEMON_SOCKET, "/tmp/custom-syntra.sock") };
        let resolved = daemon_socket().expect("override is always resolvable");
        match previous {
            Some(value) => unsafe { env::set_var(ENV_DAEMON_SOCKET, value) },
            None => unsafe { env::remove_var(ENV_DAEMON_SOCKET) },
        }
        assert_eq!(resolved, PathBuf::from("/tmp/custom-syntra.sock"));
    }

    /// The daemon and the diagnostics stream must never resolve to the same
    /// endpoint, otherwise log records would be parsed as control traffic.
    #[test]
    fn daemon_and_diagnostics_endpoints_differ() {
        assert_ne!(DAEMON_SOCKET_FILE, DIAGNOSTICS_SOCKET_FILE);
    }
}
