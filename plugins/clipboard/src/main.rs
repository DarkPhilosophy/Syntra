//! Syntra clipboard plugin for Linux desktops.
//!
//! Runs as a separate process so the daemon never links GTK: the toolkit
//! needs a session bus and a display, which a service starting at boot has
//! neither of. It observes and publishes file-clipboard selections and
//! speaks [`syntra_plugin_api`] over stdio.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

/// The plugin bridges the GTK clipboard, which only exists on Linux
/// desktops; elsewhere the service handles the clipboard itself.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("syntra-plugin-clipboard is only available on Linux");
    std::process::exit(1);
}
