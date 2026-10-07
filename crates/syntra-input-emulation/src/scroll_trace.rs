//! A small log of the scroll events the emulation receives and what it did
//! with them.
//!
//! Pointer motion is logged nowhere because it arrives hundreds of times a
//! second, which left scrolling, a handful of events a second at most, as the
//! one input that could fail without a trace. This file is that trace. It
//! records only the axis, the amount and the route taken, never what is being
//! scrolled, and is cut back when it passes a fixed size.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::Mutex,
    time::SystemTime,
};

/// The file is cut back to nothing when it passes this size.
const MAX_BYTES: u64 = 128 * 1024;

static FILE: Mutex<Option<File>> = Mutex::new(None);

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
    Some(base.join("syntra").join("scroll.log"))
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

/// Appends one line. A log that cannot be written never stops the input.
pub(crate) fn line(text: &str) {
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
    if file.metadata().is_ok_and(|meta| meta.len() > MAX_BYTES) {
        let _ = file.set_len(0);
    }
}
