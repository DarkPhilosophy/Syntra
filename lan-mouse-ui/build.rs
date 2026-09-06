use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=ui/app-window.slint");
    println!("cargo:rerun-if-changed=ui/theme.slint");
    println!("cargo:rerun-if-changed=ui/localization.slint");
    println!("cargo:rerun-if-changed=ui/components");
    println!("cargo:rerun-if-changed=ui/pages");

    let root_manifest =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not set"))
            .join("../Cargo.toml");
    println!("cargo:rerun-if-changed={}", root_manifest.display());

    let manifest = fs::read_to_string(&root_manifest)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", root_manifest.display()));
    let manifest: toml::Value = toml::from_str(&manifest)
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", root_manifest.display()));
    let application_version = manifest
        .get("package")
        .and_then(|package| package.get("version"))
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| {
            panic!(
                "{} does not contain a string package.version",
                root_manifest.display()
            )
        });
    println!("cargo:rustc-env=SYNTRA_APPLICATION_VERSION={application_version}");
    slint_build::compile("ui/app-window.slint").expect("failed to compile Slint UI");
}
