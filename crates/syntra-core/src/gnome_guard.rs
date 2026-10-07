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

/// Whether independent pointers can run now: not on GNOME, or the running
/// GNOME Shell already has direct scanout off. Free of side effects, so it
/// can be asked whenever the state is reported.
///
/// This also decides whether a peer accepts input at all on a desktop with no
/// capture backend (gamescope): without it the peer reports "not ready" and
/// the sender refuses every entry.
pub(crate) fn independent_pointer_ready() -> bool {
    supports_independent(gnome_session(), shell_has_flag())
}

fn supports_independent(gnome: bool, shell_flag: bool) -> bool {
    !gnome || shell_flag
}

/// Whether the compositor draws a cursor of its own for each peer (GNOME's
/// pen cursors), as opposed to peers driving the shared pointer.
pub(crate) fn peers_have_own_cursor() -> bool {
    syntra_input_emulation::peers_have_pen_cursors()
}

/// Why independent pointers cannot be turned on right now, if anything.
///
/// Also installs the session setting that removes the reason, so a single
/// re-login is all the user has to do.
pub(crate) fn independent_pointer_blocker() -> Option<String> {
    if independent_pointer_ready() {
        return None;
    }
    let installed = install_drop_in();
    Some(match installed {
        Ok(()) => "Independent pointers need a GNOME setting that takes effect at the next \
                   login (direct scanout off, avoiding a GNOME Shell crash with extra \
                   cursors). It has been installed and your choice is saved: log out and \
                   back in, and independent pointers turn on by themselves."
            .to_owned(),
        Err(error) => format!(
            "Independent pointers need MUTTER_DEBUG_PAINT={FLAG} in the GNOME session to \
             avoid a GNOME Shell crash, and it could not be installed automatically: {error}"
        ),
    })
}

fn gnome_session() -> bool {
    syntra_input_emulation::gnome_shell_running()
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
    )?;
    // environment.d is read only when the systemd user manager starts. With
    // lingering it outlives logouts, so a re-login alone never applied the
    // file: set the variable in the running manager too, for the next
    // GNOME Shell it starts.
    let status = std::process::Command::new("systemctl")
        .args([
            "--user",
            "set-environment",
            &format!("MUTTER_DEBUG_PAINT={FLAG}"),
        ])
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "systemctl set-environment failed: {status}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::supports_independent;

    #[test]
    fn only_an_unprepared_gnome_shell_blocks_independent_pointers() {
        assert!(supports_independent(true, true));
        // GNOME before its next login still crashes with extra cursors.
        assert!(!supports_independent(true, false));
        // Desktops without GNOME are not blocked; gamescope has no capture
        // backend and relies on this to accept input at all.
        assert!(supports_independent(false, false));
        assert!(supports_independent(false, true));
    }
}
