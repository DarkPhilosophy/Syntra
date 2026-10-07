//! Syntra edge glow plugin.
//!
//! Started by the daemon, it says who it is and then listens: settings and
//! pointer events arrive on stdin, and it lights the screen edge they name.
//! `--describe` prints the handshake and exits, which is how the daemon reads a
//! plugin's description without running it.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

/// The windows the glow is drawn in exist only for Linux so far.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("syntra-plugin-edge-glow has no window backend for this system yet");
    std::process::exit(1);
}
