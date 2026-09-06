//! Installation of the freedesktop application entry.
//!
//! Without a `.desktop` file the dashboard has no launcher entry, no icon in
//! the task switcher and no application name: the compositor matches a window
//! to an entry by application id, so a running window is otherwise shown as an
//! anonymous client.
//!
//! The entry is written to the per-user application directory. That needs no
//! privileges and matches how the binaries themselves are installed, which may
//! be anywhere including a build tree.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use syntra_api::paths::APPLICATION_ID;
use thiserror::Error;

/// Why installing or removing the application entry failed.
#[derive(Debug, Error)]
pub enum DesktopEntryError {
    /// No per-user data directory could be determined.
    #[error("could not determine the per-user application directory")]
    DirectoryNotFound,
    /// The running executable could not be located.
    #[error("could not determine the path of the running application: {0}")]
    ExecutableNotFound(io::Error),
    /// Writing or removing the entry failed.
    #[error("could not update the application entry: {0}")]
    Io(#[from] io::Error),
}

/// Whether the entry is present, and where it would be written.
#[derive(Debug, Clone)]
pub struct DesktopEntryStatus {
    /// True when an entry written by this application exists.
    pub installed: bool,
    /// Absolute path of the entry, whether or not it exists.
    pub path: PathBuf,
}

fn applications_directory() -> Result<PathBuf, DesktopEntryError> {
    dirs::data_dir()
        .map(|base| base.join("applications"))
        .ok_or(DesktopEntryError::DirectoryNotFound)
}

fn entry_path() -> Result<PathBuf, DesktopEntryError> {
    Ok(applications_directory()?.join(format!("{APPLICATION_ID}.desktop")))
}

/// Reports whether the application entry is installed.
pub fn status() -> Result<DesktopEntryStatus, DesktopEntryError> {
    let path = entry_path()?;
    Ok(DesktopEntryStatus {
        installed: path.is_file(),
        path,
    })
}
/// Writes the application entry pointing at `executable`.
///
/// The caller passes the installed copy rather than letting this read
/// `current_exe`, so the entry never points at a build tree that may move.
/// The entry is rewritten if it exists, so reinstalling repairs a stale
/// `Exec` line.
pub fn install(executable: &Path) -> Result<PathBuf, DesktopEntryError> {
    let directory = applications_directory()?;
    fs::create_dir_all(&directory)?;
    let path = entry_path()?;
    fs::write(&path, entry_contents(executable))?;
    refresh_database(&directory);
    Ok(path)
}

/// Removes the application entry, if present.
///
/// Removing an entry that is already absent succeeds: the caller asked for a
/// state, not for an event.
pub fn uninstall() -> Result<(), DesktopEntryError> {
    let path = entry_path()?;
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    if let Ok(directory) = applications_directory() {
        refresh_database(&directory);
    }
    Ok(())
}

/// Asks the desktop to reindex, so the launcher updates without a re-login.
///
/// Best effort: the tool is absent on minimal systems, and the entry is still
/// valid without it.
fn refresh_database(directory: &Path) {
    let _ = std::process::Command::new("update-desktop-database")
        .arg(directory)
        .status();
}

fn entry_contents(executable: &Path) -> String {
    // StartupWMClass must equal the application id the window sets, or the
    // compositor cannot associate the running window with this entry and the
    // task switcher shows a generic icon.
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Syntra\n\
         GenericName=Input and clipboard sharing\n\
         Comment=Share mouse, keyboard, clipboard and files across your devices\n\
         Exec={executable} %U\n\
         Icon={APPLICATION_ID}\n\
         Terminal=false\n\
         Categories=Utility;RemoteAccess;Network;\n\
         Keywords=kvm;mouse;keyboard;clipboard;sharing;\n\
         StartupNotify=true\n\
         StartupWMClass={APPLICATION_ID}\n\
         SingleMainWindow=true\n\
         X-GNOME-UsesNotifications=true\n",
        executable = executable.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compositor matches a window to its entry by application id; if
    /// these drift the launcher shows a generic icon for a running window.
    #[test]
    fn entry_declares_the_application_id_as_its_window_class() {
        let contents = entry_contents(Path::new("/usr/bin/syntra"));

        assert!(contents.contains(&format!("StartupWMClass={APPLICATION_ID}")));
        assert!(contents.contains(&format!("Icon={APPLICATION_ID}")));
    }

    /// The binary may live in a build tree or a portable directory, so the
    /// entry must point at wherever it actually is.
    #[test]
    fn entry_points_at_the_running_executable() {
        let contents = entry_contents(Path::new("/opt/syntra/syntra"));

        assert!(contents.contains("Exec=/opt/syntra/syntra %U"));
    }
}
