use std::{collections::HashMap, path::PathBuf};

/// The one directory of the dashboard's UI the settings window may build from.
///
/// It is reached by the library name `syntra-kit`, which resolves to the
/// directory holding `kit.slint`, the dashboard's declared UI surface. The
/// window imports `@syntra-kit/kit.slint` and nothing else from there, so the
/// dashboard's private files can move without breaking this plugin.
const KIT_DIRECTORY: &str = "../../crates/syntra-ui/ui";

fn main() {
    // The settings window exists only where the plugin has a window backend.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        // Everything under the kit directory is an input of the compile, so a
        // change to a shared control rebuilds the window.
        println!("cargo:rerun-if-changed={KIT_DIRECTORY}");
        println!("cargo:rerun-if-changed=ui");
        let manifest = std::env::var_os("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .expect("cargo sets CARGO_MANIFEST_DIR");
        let libraries = HashMap::from([("syntra-kit".to_owned(), manifest.join(KIT_DIRECTORY))]);
        let configuration = slint_build::CompilerConfiguration::new().with_library_paths(libraries);
        slint_build::compile_with_config("ui/settings.slint", configuration)
            .expect("settings window");
    }
}
