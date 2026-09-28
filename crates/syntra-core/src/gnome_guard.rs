//! Keeps independent pointers from crashing GNOME Shell.
//!
//! Mutter 50 gives every extra tablet cursor a secondary cursor renderer
//! whose `update_cursor` reports "needs overlay" even without a cursor. When
//! a fullscreen window is a direct-scanout candidate, the compositor then
//! measures that missing cursor and dereferences NULL
//! (`has_overlapping_cursor_overlay_foreach` → `clutter_cursor_realize_texture`),
//! taking the whole session down. With direct scanout disabled that code
//! path never runs, so the pen cursors are safe.

use std::path::PathBuf;

/// Environment drop-in read by the user session at the next login.
const DROP_IN: &str = "syntra-independent-pointers.conf";
const FLAG: &str = "disable-direct-scanout";

/// Why independent pointers cannot be turned on right now, if anything.
///
/// Also installs the session setting that removes the reason, so a single
/// re-login is all the user has to do.
pub(crate) fn independent_pointer_blocker() -> Option<String> {
    if !gnome_session() || shell_has_flag() {
        return None;
    }
    let installed = install_drop_in();
    Some(match installed {
        Ok(()) => "Independent pointers need a GNOME setting that takes effect at the next \
                   login (direct scanout off, avoiding a GNOME Shell crash with extra \
                   cursors). It has been installed: log out and back in, then enable this \
                   option again."
            .to_owned(),
        Err(error) => format!(
            "Independent pointers need MUTTER_DEBUG_PAINT={FLAG} in the GNOME session to \
             avoid a GNOME Shell crash, and it could not be installed automatically: {error}"
        ),
    })
}

fn gnome_session() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|desktops| desktops.split(':').any(|d| d.eq_ignore_ascii_case("gnome")))
        .unwrap_or(false)
}

/// Whether the running GNOME Shell already has direct scanout disabled.
fn shell_has_flag() -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_shell = std::fs::read_to_string(path.join("comm"))
            .map(|comm| comm.trim() == "gnome-shell")
            .unwrap_or(false);
        if !is_shell {
            continue;
        }
        if let Ok(environ) = std::fs::read(path.join("environ")) {
            return environ.split(|b| *b == 0).any(|var| {
                std::str::from_utf8(var)
                    .ok()
                    .and_then(|v| v.strip_prefix("MUTTER_DEBUG_PAINT="))
                    .is_some_and(|flags| flags.split(',').any(|f| f.trim() == FLAG))
            });
        }
    }
    false
}

fn install_drop_in() -> std::io::Result<()> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| std::io::Error::other("no configuration directory"))?
        .join("environment.d");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join(DROP_IN),
        format!(
            "# Written by Syntra: independent pointers crash GNOME Shell (Mutter 50)\n\
             # unless direct scanout is disabled. Remove this file to undo.\n\
             MUTTER_DEBUG_PAINT={FLAG}\n"
        ),
    )
}
