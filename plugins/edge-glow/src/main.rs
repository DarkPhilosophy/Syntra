//! Syntra edge glow plugin.
//!
//! Started by the daemon, it says who it is and then listens: settings and
//! pointer events arrive on stdin, and it lights the screen edge they name.
//! `--settings` opens the plugin's own settings window instead. What the
//! daemon knows about the plugin before it runs is its packaged descriptor,
//! never an answer from the binary.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod settings_window;

#[cfg(target_os = "linux")]
fn main() {
    if std::env::args().any(|argument| argument == "--settings") {
        if let Err(error) = settings_window::run() {
            eprintln!("syntra-plugin-edge-glow settings: {error}");
            std::process::exit(1);
        }
        return;
    }
    linux::main();
}

/// The windows the glow is drawn in exist only for Linux so far.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("syntra-plugin-edge-glow has no window backend for this system yet");
    std::process::exit(1);
}
