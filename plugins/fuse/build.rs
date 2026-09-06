//! Places this plugin's manifest beside the built executable.
//!
//! The daemon discovers plugins by reading manifests from the directory it
//! runs from. Without this step a freshly built tree contains the plugin
//! binaries but no manifests, so the plugin manager shows nothing and the
//! capability silently does not exist.

fn main() {
    println!("cargo:rerun-if-changed=syntra-plugin.json");
    if let Err(error) = syntra_plugin_manifest::install("syntra-plugin.json") {
        // A missing manifest degrades discovery, it does not break the build;
        // failing here would make an unrelated packaging problem look like a
        // compilation error.
        println!("cargo:warning=could not stage the plugin manifest: {error}");
    }
}

/// Copies a manifest next to the compiled binaries.
///
/// Duplicated in each plugin rather than shared through a crate: a build
/// script cannot depend on a workspace member without making that member a
/// build dependency of every consumer, which is a far larger cost than these
/// twenty lines.
mod syntra_plugin_manifest {
    use std::path::{Path, PathBuf};
    use std::{env, fs, io};

    /// Copies `manifest` from the crate root into the target profile
    /// directory, where the daemon looks for it.
    pub fn install(manifest: &str) -> io::Result<()> {
        let source = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or_else(missing_env)?)
            .join(manifest);
        let target = profile_directory()?;
        fs::create_dir_all(&target)?;
        let name = Path::new(manifest)
            .file_name()
            .ok_or_else(|| io::Error::other("manifest has no file name"))?;
        // Name the copy after the plugin so two plugins cannot overwrite each
        // other's manifest in a shared target directory.
        let stem =
            env::var("CARGO_PKG_NAME").unwrap_or_else(|_| name.to_string_lossy().into_owned());
        fs::copy(&source, target.join(format!("{stem}.json")))?;
        Ok(())
    }

    /// Resolves `target/<profile>` from `OUT_DIR`.
    ///
    /// Cargo exposes no direct variable for it, but `OUT_DIR` is always
    /// `<target>/<profile>/build/<crate>-<hash>/out`, so three levels up is
    /// the directory the binaries land in.
    fn profile_directory() -> io::Result<PathBuf> {
        let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or_else(missing_env)?);
        out_dir
            .ancestors()
            .nth(3)
            .map(Path::to_path_buf)
            .ok_or_else(|| io::Error::other("OUT_DIR is not inside a target directory"))
    }

    fn missing_env() -> io::Error {
        io::Error::other("cargo did not provide the expected build environment")
    }
}
