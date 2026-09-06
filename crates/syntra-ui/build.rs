//! Compiles the Slint markup into Rust.
//!
//! The application version is not resolved here: Cargo already exposes it as
//! `CARGO_PKG_VERSION`, and every crate inherits it from `[workspace.package]`.

fn main() {
    // `slint_build` tracks the entry file only, so directories holding
    // imported components must be registered explicitly.
    for path in [
        "ui/app-window.slint",
        "ui/app-state.slint",
        "ui/theme.slint",
        "ui/localization.slint",
        "ui/components",
        "ui/pages",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    slint_build::compile("ui/app-window.slint").expect("failed to compile Slint UI");
}
