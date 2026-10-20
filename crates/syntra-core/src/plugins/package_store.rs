//! Syntra's own store of installed plugin packages.
//!
//! Installing copies a plugin into a directory Syntra owns, disabled and
//! unapproved. Nothing is executed: the copy is a plain file copy, and the
//! daemon only ever *runs* a plugin after its content digest is approved.
//!
//! # Ownership
//!
//! Syntra may remove exactly what it installed. Every installed package
//! carries a marker file that only this module writes; uninstall deletes a
//! directory only when it sits directly inside the store, is not a symbolic
//! link, and carries the marker. A plugin the user dropped into the plugins
//! directory has no marker and is never removed, whatever it is called.
//!
//! # What is copied
//!
//! The manifest, the executable it names and the resources it lists, no more:
//! the same set the content digest covers, so what was reviewed is what is
//! installed. Anything that is not a regular file or directory is refused.

use std::{
    fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
};
use syntra_plugin_api::{ManifestLoad, PluginManifest, is_safe_id, manifest::MAX_MANIFEST_BYTES};
use thiserror::Error;

/// Name of the file that marks a directory as one Syntra installed.
const MARKER: &str = ".syntra-installed";

/// Why a package could not be installed or removed.
///
/// The text is safe to show a client: it never names a path.
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum StoreError {
    /// There is no store directory on this system.
    #[error("there is no place to install plugins on this system")]
    NoStore,
    /// The package is not a readable, valid manifest with its files.
    #[error("the package is not valid: {0}")]
    Invalid(String),
    /// The package contains something that is not a regular file or directory.
    #[error("the package contains a file that is not a regular file")]
    NotRegular,
    /// The copy or replacement failed.
    #[error("the package could not be installed ({0})")]
    Io(&'static str),
    /// The plugin is not one Syntra installed.
    #[error("only a plugin installed by Syntra can be uninstalled")]
    NotOwned,
}

/// Where the manifest of a package is, given what a client named.
pub(crate) fn manifest_path_of(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.join("syntra-plugin.json")
    } else {
        path.to_path_buf()
    }
}

/// Installs the package at `source` into `store`, replacing an earlier install
/// of the same id. Returns the id.
///
/// The copy is staged beside the final location. An earlier install of the
/// same id is renamed to a sibling backup, the staged copy is renamed into
/// place, and the backup is removed only afterwards; if the final rename
/// fails the backup is renamed back, so a failure leaves the previous package
/// intact. A crash between the two renames can leave the previous package under
/// a `.backup-` name in the store, never a mixture of old and new files.
pub(crate) fn install(source: &Path, store: &Path) -> Result<String, StoreError> {
    let manifest_path = manifest_path_of(source);
    let (manifest, root) = read_package(&manifest_path)?;
    fs::create_dir_all(store).map_err(|_| StoreError::Io("cannot create the store"))?;

    let staging = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(store)
        .map_err(|_| StoreError::Io("cannot stage the package"))?;

    // The manifest keeps its name so the installed directory is discovered the
    // same way a package directory always is.
    copy_regular(&manifest_path, &staging.path().join("syntra-plugin.json"))?;
    let executable = manifest.executable_path(&manifest_path);
    let relative_executable = relative_inside(&executable, &root)?;
    copy_regular(&executable, &staging.path().join(&relative_executable))?;
    for resource in &manifest.resources {
        copy_tree(&root.join(resource), &staging.path().join(resource), &root)?;
    }
    // The manifest named the executable relative to itself; the copy keeps
    // that layout, so the relative name is unchanged.
    fs::write(staging.path().join(MARKER), manifest.id.as_bytes())
        .map_err(|_| StoreError::Io("cannot mark the package"))?;

    let target = store.join(&manifest.id);
    // Only replace what Syntra installed. A directory with the same id that
    // Syntra did not create is somebody's data.
    if target.exists() && !is_owned(&target, store) {
        return Err(StoreError::NotOwned);
    }
    let staged = staging.keep();
    if let Err(error) = swap_into_place(&staged, &target, store, |from, to| fs::rename(from, to)) {
        let _ = fs::remove_dir_all(&staged);
        return Err(error);
    }
    Ok(manifest.id)
}

/// Puts `staged` at `target`, keeping the previous package until the new one
/// is in place.
///
/// An existing package is first renamed to a sibling backup, then the staged
/// copy is renamed over the freed name. If that second rename fails the
/// backup is renamed back, so a failed replacement leaves the previous package
/// exactly as it was. The backup is deleted only after the new package is in
/// place. `rename` is a parameter so the failure path can be exercised.
fn swap_into_place(
    staged: &Path,
    target: &Path,
    store: &Path,
    rename: impl Fn(&Path, &Path) -> io::Result<()>,
) -> Result<(), StoreError> {
    if !target.exists() {
        return rename(staged, target)
            .map_err(|_| StoreError::Io("cannot move the package into place"));
    }
    let backup = tempfile::Builder::new()
        .prefix(".backup-")
        .tempdir_in(store)
        .map_err(|_| StoreError::Io("cannot keep the old version"))?
        .keep();
    // `rename` onto an existing empty directory is allowed on Unix but not
    // everywhere; the reservation directory is removed so the name is free.
    fs::remove_dir(&backup).map_err(|_| StoreError::Io("cannot keep the old version"))?;
    rename(target, &backup).map_err(|_| StoreError::Io("cannot replace the old version"))?;
    if rename(staged, target).is_err() {
        // Put the previous package back before reporting the failure.
        return match rename(&backup, target) {
            Ok(()) => Err(StoreError::Io("cannot move the package into place")),
            Err(_) => Err(StoreError::Io(
                "cannot move the package into place; the previous version is kept in the store",
            )),
        };
    }
    // The new package is live; a leftover backup is only wasted space.
    let _ = fs::remove_dir_all(&backup);
    Ok(())
}

/// Removes the package installed under `id`, and nothing else.
pub(crate) fn uninstall(id: &str, store: &Path) -> Result<(), StoreError> {
    if !is_safe_id(id) {
        return Err(StoreError::NotOwned);
    }
    let target = store.join(id);
    if !is_owned(&target, store) {
        return Err(StoreError::NotOwned);
    }
    fs::remove_dir_all(&target).map_err(|_| StoreError::Io("cannot remove the package"))
}

/// Whether `target` is a package this module installed: a real directory
/// directly inside `store`, carrying the marker, that is not a link.
fn is_owned(target: &Path, store: &Path) -> bool {
    if target.parent() != Some(store) {
        return false;
    }
    let Ok(metadata) = fs::symlink_metadata(target) else {
        return false;
    };
    if !metadata.file_type().is_dir() {
        return false;
    }
    match fs::symlink_metadata(target.join(MARKER)) {
        Ok(marker) => marker.file_type().is_file(),
        Err(_) => false,
    }
}

/// Reads and validates a manifest, returning it with its package root.
fn read_package(manifest_path: &Path) -> Result<(PluginManifest, PathBuf), StoreError> {
    let metadata = fs::metadata(manifest_path)
        .map_err(|_| StoreError::Invalid("the manifest cannot be read".into()))?;
    if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES {
        return Err(StoreError::Invalid(
            "the manifest is not a regular file of a sensible size".into(),
        ));
    }
    let text = fs::read_to_string(manifest_path)
        .map_err(|_| StoreError::Invalid("the manifest cannot be read".into()))?;
    let manifest = match PluginManifest::parse(&text) {
        Ok(ManifestLoad::Valid(manifest)) => *manifest,
        Ok(ManifestLoad::Unsupported { reason, .. }) => return Err(StoreError::Invalid(reason)),
        Err(error) => return Err(StoreError::Invalid(error.to_string())),
    };
    let root = manifest_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    Ok((manifest, root))
}

/// `path` relative to `root`, refusing anything that is not inside it.
fn relative_inside(path: &Path, root: &Path) -> Result<PathBuf, StoreError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| StoreError::Invalid("the executable is outside the package".into()))?;
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(StoreError::Invalid(
            "the executable path is not contained in the package".into(),
        ));
    }
    Ok(relative.to_path_buf())
}

/// Copies one regular file, preserving the executable bit, refusing a link.
fn copy_regular(from: &Path, to: &Path) -> Result<(), StoreError> {
    let metadata = fs::symlink_metadata(from).map_err(|_| StoreError::Io("cannot read a file"))?;
    if !metadata.file_type().is_file() {
        return Err(StoreError::NotRegular);
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|_| StoreError::Io("cannot create a directory"))?;
    }
    let mut source = fs::File::open(from).map_err(|_| StoreError::Io("cannot read a file"))?;
    let mut target = fs::File::create(to).map_err(|_| StoreError::Io("cannot write a file"))?;
    io::copy(&mut source, &mut target).map_err(|_| StoreError::Io("cannot copy a file"))?;
    target
        .flush()
        .map_err(|_| StoreError::Io("cannot write a file"))?;
    fs::set_permissions(to, metadata.permissions())
        .map_err(|_| StoreError::Io("cannot set permissions"))?;
    Ok(())
}

/// Copies a file or a directory tree, refusing links, devices and anything
/// that leaves `root`.
fn copy_tree(from: &Path, to: &Path, root: &Path) -> Result<(), StoreError> {
    let metadata = fs::symlink_metadata(from).map_err(|_| StoreError::Io("cannot read a file"))?;
    let kind = metadata.file_type();
    if kind.is_symlink() {
        return Err(StoreError::NotRegular);
    }
    let real = fs::canonicalize(from).map_err(|_| StoreError::Io("cannot read a file"))?;
    let real_root = fs::canonicalize(root).map_err(|_| StoreError::Io("cannot read a file"))?;
    if !real.starts_with(&real_root) {
        return Err(StoreError::Invalid(
            "a resource is outside the package".into(),
        ));
    }
    if kind.is_dir() {
        fs::create_dir_all(to).map_err(|_| StoreError::Io("cannot create a directory"))?;
        let mut names: Vec<_> = fs::read_dir(from)
            .map_err(|_| StoreError::Io("cannot read a directory"))?
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        names.sort();
        for name in names {
            copy_tree(&from.join(&name), &to.join(&name), root)?;
        }
        Ok(())
    } else {
        copy_regular(from, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"{
        "manifest_version": 1, "protocol_version": 1,
        "id": "acme", "name": "Acme", "executable": "bin/acme",
        "args": ["--serve"], "resources": ["lib"],
        "capabilities": {"pointer_events": true, "mime_types": []}
    }"#;

    /// Lays out a plugin package and returns its directory.
    fn package(directory: &Path) -> PathBuf {
        let package = directory.join("source");
        fs::create_dir_all(package.join("bin")).unwrap();
        fs::create_dir_all(package.join("lib")).unwrap();
        fs::write(package.join("syntra-plugin.json"), MANIFEST).unwrap();
        fs::write(package.join("bin/acme"), "#!/bin/sh\n").unwrap();
        fs::write(package.join("lib/data"), "resource").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(package.join("bin/acme"), fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        package
    }

    #[test]
    fn install_copies_the_package_marks_it_and_executes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let source = package(directory.path());
        let witness = directory.path().join("ran");
        fs::write(
            source.join("bin/acme"),
            format!("#!/bin/sh\ntouch '{}'\n", witness.display()),
        )
        .unwrap();
        let store = directory.path().join("store");

        let id = install(&source, &store).unwrap();
        assert_eq!(id, "acme");
        let installed = store.join("acme");
        assert!(installed.join("syntra-plugin.json").is_file());
        assert!(installed.join("bin/acme").is_file());
        assert!(installed.join("lib/data").is_file());
        assert!(installed.join(MARKER).is_file());
        assert!(!witness.exists(), "installing must never run the plugin");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(installed.join("bin/acme"))
                .unwrap()
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0, "the executable bit survives the copy");
        }
    }

    #[test]
    fn only_the_files_the_digest_covers_are_copied() {
        let directory = tempfile::tempdir().unwrap();
        let source = package(directory.path());
        fs::write(source.join("unrelated.txt"), "not part of the plugin").unwrap();
        let store = directory.path().join("store");
        install(&source, &store).unwrap();
        assert!(!store.join("acme/unrelated.txt").exists());
    }

    #[test]
    fn reinstalling_replaces_the_installed_package() {
        let directory = tempfile::tempdir().unwrap();
        let source = package(directory.path());
        let store = directory.path().join("store");
        install(&source, &store).unwrap();
        fs::write(source.join("lib/data"), "newer").unwrap();
        install(&source, &store).unwrap();
        assert_eq!(
            fs::read_to_string(store.join("acme/lib/data")).unwrap(),
            "newer"
        );
        let leftovers: Vec<_> = fs::read_dir(&store)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, ["acme"], "no staging directory is left behind");
    }

    /// If moving the new package into place fails, the previous package is
    /// restored exactly, and nothing is left beside it.
    #[test]
    fn a_failed_replacement_restores_the_previous_package() {
        let directory = tempfile::tempdir().unwrap();
        let source = package(directory.path());
        let store = directory.path().join("store");
        install(&source, &store).unwrap();
        let installed_before = fs::read_to_string(store.join("acme/lib/data")).unwrap();

        let staged = store.join(".staged-new");
        fs::create_dir_all(staged.join("lib")).unwrap();
        fs::write(staged.join("lib/data"), "must not appear").unwrap();
        let target = store.join("acme");
        // Moving the staged copy fails; every other rename is real.
        let result = swap_into_place(&staged, &target, &store, |from, to| {
            if from == staged {
                Err(io::Error::other("simulated failure"))
            } else {
                fs::rename(from, to)
            }
        });

        assert!(matches!(result, Err(StoreError::Io(_))), "{result:?}");
        assert_eq!(
            fs::read_to_string(target.join("lib/data")).unwrap(),
            installed_before,
            "the previous package is back"
        );
        assert!(
            target.join(MARKER).is_file(),
            "the ownership marker is back too"
        );
        let mut names: Vec<_> = fs::read_dir(&store)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, [".staged-new", "acme"], "no backup is left behind");
    }

    /// A directory Syntra did not install is never replaced, and is left
    /// untouched by the refusal.
    #[test]
    fn a_refused_replacement_leaves_the_existing_directory_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let source = package(directory.path());
        let store = directory.path().join("store");
        fs::create_dir_all(store.join("acme")).unwrap();
        fs::write(store.join("acme/mine"), "user data").unwrap();

        assert_eq!(install(&source, &store), Err(StoreError::NotOwned));
        assert_eq!(
            fs::read_to_string(store.join("acme/mine")).unwrap(),
            "user data"
        );
        let names: Vec<_> = fs::read_dir(&store)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["acme"], "the staging directory was cleaned up");
    }

    #[test]
    fn uninstall_removes_exactly_the_installed_package() {
        let directory = tempfile::tempdir().unwrap();
        let source = package(directory.path());
        let store = directory.path().join("store");
        install(&source, &store).unwrap();
        fs::create_dir_all(store.join("other")).unwrap();
        fs::write(store.join("other/keep"), "x").unwrap();

        uninstall("acme", &store).unwrap();
        assert!(!store.join("acme").exists());
        assert!(
            source.join("syntra-plugin.json").exists(),
            "the source is untouched"
        );
        assert!(store.join("other/keep").exists());
    }

    /// A directory that Syntra did not install is never removed, even inside
    /// the store and even with a matching name.
    #[test]
    fn uninstall_refuses_what_syntra_did_not_install() {
        let directory = tempfile::tempdir().unwrap();
        let store = directory.path().join("store");
        fs::create_dir_all(store.join("acme")).unwrap();
        fs::write(store.join("acme/precious"), "x").unwrap();
        assert_eq!(uninstall("acme", &store), Err(StoreError::NotOwned));
        assert!(store.join("acme/precious").exists());
        assert_eq!(uninstall("missing", &store), Err(StoreError::NotOwned));
    }

    #[test]
    fn uninstall_refuses_ids_that_could_leave_the_store() {
        let directory = tempfile::tempdir().unwrap();
        let store = directory.path().join("store");
        fs::create_dir_all(&store).unwrap();
        let outside = directory.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join(MARKER), "x").unwrap();
        for id in ["../outside", "..", ".", "a/b", ""] {
            assert_eq!(uninstall(id, &store), Err(StoreError::NotOwned), "{id:?}");
        }
        assert!(outside.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_in_the_store_is_never_followed_by_uninstall() {
        let directory = tempfile::tempdir().unwrap();
        let store = directory.path().join("store");
        fs::create_dir_all(&store).unwrap();
        let outside = directory.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join(MARKER), "x").unwrap();
        fs::write(outside.join("precious"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, store.join("acme")).unwrap();
        assert_eq!(uninstall("acme", &store), Err(StoreError::NotOwned));
        assert!(outside.join("precious").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_package_with_a_symbolic_link_is_refused_and_nothing_is_left() {
        let directory = tempfile::tempdir().unwrap();
        let source = package(directory.path());
        let secret = directory.path().join("secret");
        fs::write(&secret, "x").unwrap();
        std::os::unix::fs::symlink(&secret, source.join("lib/link")).unwrap();
        let store = directory.path().join("store");
        assert!(install(&source, &store).is_err());
        let entries = fs::read_dir(&store).map_or(0, |dir| dir.flatten().count());
        assert_eq!(entries, 0, "a failed install leaves no partial package");
    }

    #[test]
    fn an_invalid_or_unsupported_manifest_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let source = package(directory.path());
        let store = directory.path().join("store");
        fs::write(source.join("syntra-plugin.json"), "{ not json").unwrap();
        assert!(matches!(
            install(&source, &store),
            Err(StoreError::Invalid(_))
        ));
        fs::write(
            source.join("syntra-plugin.json"),
            MANIFEST.replace("\"manifest_version\": 1", "\"manifest_version\": 9"),
        )
        .unwrap();
        assert!(matches!(
            install(&source, &store),
            Err(StoreError::Invalid(_))
        ));
    }

    #[test]
    fn errors_never_name_a_path() {
        let directory = tempfile::tempdir().unwrap();
        let store = directory.path().join("store");
        let message = install(&directory.path().join("missing"), &store)
            .unwrap_err()
            .to_string();
        assert!(
            !message.contains(directory.path().to_str().unwrap()),
            "{message}"
        );
    }
}
