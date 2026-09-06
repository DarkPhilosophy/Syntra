//! Derives a build fingerprint shared by every Syntra component.
//!
//! The recurring failure this exists to catch is version skew: a daemon from
//! one build supervising a plugin from another. The protocol version does not
//! detect it, because both sides can speak protocol 1 while disagreeing about
//! everything above it.
//!
//! The fingerprint is derived from the checkout, not from the compile, so all
//! crates built from the same source agree on it while a stale binary from an
//! earlier build does not.

use std::process::Command;

fn main() {
    // Re-run when the checkout moves, so a rebuild after a commit or a
    // working-tree change produces a new fingerprint.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SYNTRA_BUILD_FINGERPRINT");

    let fingerprint = std::env::var("SYNTRA_BUILD_FINGERPRINT")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(fingerprint_from_checkout);
    println!("cargo:rustc-env=SYNTRA_BUILD_FINGERPRINT={fingerprint}");
}

/// Describes the source the component was built from.
///
/// Falls back to the package version when git is unavailable, as in a
/// published crate or a source tarball. That is weaker but never wrong: two
/// components built from the same tarball still agree.
fn fingerprint_from_checkout() -> String {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".into());
    let Some(commit) = git(&["rev-parse", "--short=12", "HEAD"]) else {
        return version;
    };
    // A dirty tree cannot be identified by its commit alone, so it is marked.
    // Two dirty builds may still differ; that is honest, since the source may
    // genuinely differ between them.
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .map(|output| !output.is_empty())
        .unwrap_or(false);
    if dirty {
        format!("{version}+{commit}.dirty")
    } else {
        format!("{version}+{commit}")
    }
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
