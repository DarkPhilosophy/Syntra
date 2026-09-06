//! Copying the Syntra binaries into a stable location.
//!
//! Installing a service or a launcher entry that points at wherever the
//! application happens to be running is wrong: a build tree, a download
//! directory or a mounted image can move or disappear, leaving a unit that
//! silently fails at every login. Installation therefore copies the binaries
//! to a durable directory first, and the unit and the desktop entry are
//! written against the copies.
//!
//! The default is the per-user executable directory, which needs no
//! privileges. It can be overridden with [`ENV_INSTALL_DIR`] for anyone who
//! keeps binaries elsewhere.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Overrides the directory binaries are installed into.
pub const ENV_INSTALL_DIR: &str = "SYNTRA_INSTALL_DIR";

/// Executable name of the dashboard.
pub const APPLICATION_EXECUTABLE: &str = if cfg!(windows) {
    "syntra.exe"
} else {
    "syntra"
};
/// Executable name of the background service.
pub const DAEMON_EXECUTABLE: &str = if cfg!(windows) {
    "syntra-daemon.exe"
} else {
    "syntra-daemon"
};

/// Why installing the binaries failed.
#[derive(Debug, Error)]
pub enum InstallError {
    /// No per-user executable directory could be determined.
    #[error("could not determine an installation directory; set {ENV_INSTALL_DIR}")]
    DirectoryNotFound,
    /// The running executable could not be located.
    #[error("could not determine the path of the running application: {0}")]
    ExecutableNotFound(io::Error),
    /// A binary that must be installed was not found beside the running one.
    #[error("{0} was not found next to the running application; install both binaries together")]
    MissingBinary(String),
    /// Copying failed.
    #[error("could not install {path}: {source}")]
    Copy {
        /// Destination that could not be written.
        path: String,
        /// Underlying error.
        source: io::Error,
    },
}

/// Where binaries are installed, honouring [`ENV_INSTALL_DIR`].
///
/// Defaults to the per-user executable directory (`~/.local/bin` on Linux),
/// which is on `PATH` for most desktop sessions and needs no privileges.
pub fn install_directory() -> Result<PathBuf, InstallError> {
    if let Some(value) = std::env::var_os(ENV_INSTALL_DIR) {
        return Ok(PathBuf::from(value));
    }
    dirs::executable_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("bin")))
        .ok_or(InstallError::DirectoryNotFound)
}

/// Directory the running executable lives in.
fn running_directory() -> Result<PathBuf, InstallError> {
    std::env::current_exe()
        .map_err(InstallError::ExecutableNotFound)?
        .parent()
        .map(Path::to_path_buf)
        .ok_or(InstallError::DirectoryNotFound)
}

/// Paths of the installed binaries.
#[derive(Debug, Clone)]
pub struct InstalledBinaries {
    /// Installed dashboard.
    pub application: PathBuf,
    /// Installed background service.
    pub daemon: PathBuf,
}

/// Whether the binaries are already installed in [`install_directory`].
pub fn status() -> Result<InstalledBinaries, InstallError> {
    let directory = install_directory()?;
    Ok(InstalledBinaries {
        application: directory.join(APPLICATION_EXECUTABLE),
        daemon: directory.join(DAEMON_EXECUTABLE),
    })
}

/// Reports whether the running executable already is the installed one.
///
/// Installing over a running binary is refused rather than attempted: on Unix
/// the copy would succeed and silently replace the file being executed.
pub fn running_from_install_directory() -> bool {
    match (running_directory(), install_directory()) {
        (Ok(running), Ok(target)) => running == target,
        _ => false,
    }
}

/// Copies both binaries into [`install_directory`] and returns their paths.
///
/// Existing copies are replaced, so this doubles as an upgrade. Nothing is
/// removed if a later step fails; a partially installed pair is still
/// runnable, and reinstalling repairs it.
pub fn install_binaries() -> Result<InstalledBinaries, InstallError> {
    let source = running_directory()?;
    let target = install_directory()?;
    fs::create_dir_all(&target).map_err(|error| InstallError::Copy {
        path: target.display().to_string(),
        source: error,
    })?;

    if source == target {
        // Already in place; treat as installed rather than copying onto self.
        return status();
    }

    let mut installed = Vec::new();
    for name in [APPLICATION_EXECUTABLE, DAEMON_EXECUTABLE] {
        let from = source.join(name);
        if !from.is_file() {
            return Err(InstallError::MissingBinary(name.to_owned()));
        }
        let to = target.join(name);
        // Remove first: overwriting a file that is currently executing fails
        // with ETXTBSY on some systems, whereas unlinking always works and
        // leaves running processes with the old inode.
        let _ = fs::remove_file(&to);
        fs::copy(&from, &to).map_err(|error| InstallError::Copy {
            path: to.display().to_string(),
            source: error,
        })?;
        set_executable(&to)?;
        installed.push(to);
    }

    Ok(InstalledBinaries {
        application: installed[0].clone(),
        daemon: installed[1].clone(),
    })
}

/// Removes the installed copies, leaving the running binaries alone.
pub fn remove_binaries() -> Result<(), InstallError> {
    let target = install_directory()?;
    if running_from_install_directory() {
        // Removing the binary that is executing would leave the user with no
        // way to start the application again.
        return Err(InstallError::Copy {
            path: target.display().to_string(),
            source: io::Error::other("refusing to remove the running installation"),
        });
    }
    for name in [APPLICATION_EXECUTABLE, DAEMON_EXECUTABLE] {
        let path = target.join(name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(InstallError::Copy {
                    path: path.display().to_string(),
                    source: error,
                });
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), InstallError> {
    use std::os::unix::fs::PermissionsExt;
    // `fs::copy` preserves the mode, but a source stripped of the execute bit
    // (an artefact extracted from an archive, for example) would install a
    // file nothing can run.
    let mut permissions = fs::metadata(path)
        .map_err(|error| InstallError::Copy {
            path: path.display().to_string(),
            source: error,
        })?
        .permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    fs::set_permissions(path, permissions).map_err(|error| InstallError::Copy {
        path: path.display().to_string(),
        source: error,
    })
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), InstallError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The override exists so a user who keeps binaries outside the default
    /// location is not forced into it.
    #[test]
    fn environment_override_selects_the_install_directory() {
        let previous = std::env::var_os(ENV_INSTALL_DIR);
        // SAFETY: single-threaded test, restored before returning.
        unsafe { std::env::set_var(ENV_INSTALL_DIR, "/opt/syntra/bin") };
        let resolved = install_directory();
        match previous {
            Some(value) => unsafe { std::env::set_var(ENV_INSTALL_DIR, value) },
            None => unsafe { std::env::remove_var(ENV_INSTALL_DIR) },
        }
        assert_eq!(resolved.unwrap(), PathBuf::from("/opt/syntra/bin"));
    }

    /// Both binaries must land in the same directory, or the dashboard cannot
    /// find the daemon beside itself at runtime.
    #[test]
    fn both_binaries_share_one_directory() {
        let paths = status().expect("a per-user directory exists in the test environment");

        assert_eq!(paths.application.parent(), paths.daemon.parent());
    }
}
