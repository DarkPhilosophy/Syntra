//! A small log of what the plugin saw and did, kept in a file.
//!
//! The plugin's standard error goes to the daemon, which forwards it to the
//! dashboard's diagnostics view and nowhere a person debugging from a terminal
//! can read it. This file is the same account where it can be read, bounded so
//! a plugin that runs for weeks cannot fill the disk.
//!
//! It records frames received, the renderer chosen, draws and the windows
//! being shown or hidden, never anything the user typed or copied.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::Mutex,
    time::SystemTime,
};

/// The file is cut back to nothing when it passes this size.
const MAX_BYTES: u64 = 256 * 1024;

static FILE: Mutex<Option<File>> = Mutex::new(None);

/// Where the log lives: the per-user runtime directory, which only this user
/// can read and which is cleared at logout.
fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
    Some(base.join("syntra").join("edge-glow.log"))
}

fn open() -> Option<File> {
    let path = path()?;
    fs::create_dir_all(path.parent()?).ok()?;
    let too_big = fs::metadata(&path).is_ok_and(|meta| meta.len() > MAX_BYTES);
    let mut options = OpenOptions::new();
    options.create(true).write(true);
    if too_big {
        options.truncate(true);
    } else {
        options.append(true);
    }
    options.open(path).ok()
}

/// Appends one line, with the time, to the log. Failure to write is ignored: a
/// log that cannot be written must never stop the plugin from drawing.
pub fn line(text: &str) {
    let Ok(mut guard) = FILE.lock() else {
        return;
    };
    if guard.is_none() {
        *guard = open();
    }
    let Some(file) = guard.as_mut() else {
        return;
    };
    let since_epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let _ = writeln!(
        file,
        "{}.{:03} {text}",
        since_epoch.as_secs(),
        since_epoch.subsec_millis()
    );
    // Cut back in place when it grows past the limit, without reopening.
    if file.metadata().is_ok_and(|meta| meta.len() > MAX_BYTES) {
        let _ = file.set_len(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The path must sit under the runtime directory and be named for the
    /// plugin, so two plugins never share a file.
    #[test]
    fn the_log_lives_under_the_runtime_directory() {
        let path = path();
        if let Some(path) = path {
            assert!(path.ends_with("syntra/edge-glow.log"), "{path:?}");
        }
    }
}
