//! Finding plugins: reading manifests and building the bundled set.
//!
//! Discovery only reads files. It never runs a plugin, so looking at a
//! directory full of unknown programs executes none of them. What a plugin
//! says about itself at run time (its handshake) may narrow what it can do
//! but never changes what was discovered or who it is.
//!
//! # Provenance
//!
//! Where a plugin was found decides who vouches for it, and nothing the plugin
//! says can change that:
//!
//! * **Bundled** plugins are the three the distribution ships. Their
//!   descriptors are compiled into the daemon from the very files packaged in
//!   `plugins/`, and the executable is the one beside the daemon. A file merely
//!   lying in that directory is not bundled: unless its id is one of the
//!   built-in ids *and* it is the expected executable, it is ignored.
//! * **Installed** plugins were copied into Syntra's own package store by an
//!   install action. Syntra may uninstall exactly these.
//! * **Drop-in** plugins were placed by the user in the plugins directory.
//!   They are listed and inspectable but never launched until approved, and
//!   Syntra never removes them.
//!
//! A user-supplied executable for a built-in id is a *drop-in* with that id: it
//! replaces the bundled entry, is third-party, and needs approval of its own
//! digest.

use std::{
    fs,
    path::{Path, PathBuf},
};
use syntra_api::PluginOrigin;
use syntra_plugin_api::{ManifestLoad, PluginManifest, is_safe_id, manifest::MAX_MANIFEST_BYTES};

/// Identifier of the bundled clipboard plugin.
pub(crate) const CLIPBOARD_PLUGIN_ID: &str = "clipboard";
/// Identifier of the bundled FUSE plugin.
pub(crate) const FUSE_PLUGIN_ID: &str = "fuse";
/// Identifier of the bundled edge glow plugin.
pub(crate) const EDGE_GLOW_PLUGIN_ID: &str = "edge-glow";

/// The plugins the distribution ships: id, packaged descriptor and the file
/// name of the executable beside the daemon.
///
/// The descriptor is compiled in from the packaged manifest, so there is one
/// file that describes a bundled plugin and it cannot be missing, stale or
/// hand-edited beside the binary. The daemon never executes the binary to ask
/// what it is.
const BUNDLED: [(&str, &str, &str); 3] = [
    (
        CLIPBOARD_PLUGIN_ID,
        include_str!("../../../../plugins/clipboard/syntra-plugin.json"),
        "syntra-plugin-clipboard",
    ),
    (
        FUSE_PLUGIN_ID,
        include_str!("../../../../plugins/fuse/syntra-plugin.json"),
        "syntra-plugin-fuse",
    ),
    (
        EDGE_GLOW_PLUGIN_ID,
        include_str!("../../../../plugins/edge-glow/syntra-plugin.json"),
        "syntra-plugin-edge-glow",
    ),
];

/// A plugin found on disk, or compiled in, and where it came from.
#[derive(Debug, Clone)]
pub(crate) struct Discovered {
    /// What the manifest claims. `bundled` in it is a claim, never trust.
    pub manifest: PluginManifest,
    /// Where the daemon found it, which alone decides provenance.
    pub origin: PluginOrigin,
    /// Path of the manifest file; empty for a bundled descriptor.
    pub manifest_path: PathBuf,
    /// Exact file the daemon would launch, whether or not it exists.
    pub executable: PathBuf,
}

/// A manifest the daemon could read but does not understand.
#[derive(Debug, Clone)]
pub(crate) struct Unsupported {
    /// Identifier, when it could be read safely.
    pub id: Option<String>,
    /// Display name, when present.
    pub name: Option<String>,
    /// Where it was found.
    pub origin: PluginOrigin,
    /// Path of the manifest file.
    pub manifest_path: PathBuf,
    /// Why it is unsupported, ready to show.
    pub reason: String,
}

/// Everything one discovery pass found.
#[derive(Debug, Default)]
pub(crate) struct Found {
    /// Plugins this daemon can run, in display order.
    pub plugins: Vec<Discovered>,
    /// Manifests it listed but cannot run.
    pub unsupported: Vec<Unsupported>,
}

/// File name of the executable for a bundled plugin on this platform.
fn platform_executable(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

/// The bundled set, with executables beside `daemon_directory`.
fn bundled(daemon_directory: &Path) -> Vec<Discovered> {
    BUNDLED
        .into_iter()
        .filter_map(|(id, descriptor, executable)| {
            // A descriptor that fails to parse is a build mistake and the
            // test below pins it, so it is never listed as an empty plugin.
            let manifest = match PluginManifest::parse(descriptor) {
                Ok(ManifestLoad::Valid(manifest)) if manifest.id == id => *manifest,
                _ => {
                    log::error!("the built-in descriptor for `{id}` is invalid");
                    return None;
                }
            };
            Some(Discovered {
                executable: daemon_directory.join(platform_executable(executable)),
                manifest,
                origin: PluginOrigin::Bundled,
                manifest_path: PathBuf::new(),
            })
        })
        .collect()
}

/// Builds the plugin list from the daemon's directory, the user's drop-in
/// directory and Syntra's own package store.
///
/// Order is display order: bundled first, then everything else by id. A later
/// source replaces an earlier plugin of the same id, so a user can substitute
/// a bundled plugin; the substitute is third-party and starts unapproved.
pub(crate) fn discover(
    daemon_directory: &Path,
    drop_in: Option<&Path>,
    store: Option<&Path>,
) -> Found {
    let mut found = Found {
        plugins: bundled(daemon_directory),
        unsupported: Vec::new(),
    };
    for (directory, origin) in [
        (drop_in, PluginOrigin::DropIn),
        (store, PluginOrigin::Installed),
    ] {
        let Some(directory) = directory else { continue };
        let (plugins, unsupported) = read_directory(directory, origin);
        for discovered in plugins {
            let id = discovered.manifest.id.clone();
            match found
                .plugins
                .iter_mut()
                .find(|existing| existing.manifest.id == id)
            {
                Some(existing) => *existing = discovered,
                None => found.plugins.push(discovered),
            }
        }
        found.unsupported.extend(unsupported);
    }
    found
}

/// Reads every `*.json` manifest and every `<dir>/syntra-plugin.json` in one
/// directory.
///
/// Both layouts are accepted: a loose `name.json` beside its executable (how
/// plugins have always been dropped in) and a package directory holding
/// `syntra-plugin.json` and the files it needs (how an install lays one out).
pub(crate) fn read_directory(
    directory: &Path,
    origin: PluginOrigin,
) -> (Vec<Discovered>, Vec<Unsupported>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return (Vec::new(), Vec::new());
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // `file_type` does not follow symbolic links, so a link planted in the
        // directory is never read as if it were a manifest here.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            // `.staging-*` and `.backup-*` are the package store's own scratch
            // directories; a leftover backup holds a complete older package
            // and must not be read as a second copy of the plugin.
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let inside = path.join("syntra-plugin.json");
            if inside.is_file() {
                paths.push(inside);
            }
        } else if kind.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            paths.push(path);
        }
    }
    // Directory order is arbitrary; sort so the list does not reshuffle
    // between runs.
    paths.sort();

    let mut plugins = Vec::new();
    let mut unsupported = Vec::new();
    for path in paths {
        match read_manifest(&path, origin) {
            Ok(Read::Plugin(discovered)) => plugins.push(*discovered),
            Ok(Read::Unsupported(entry)) => unsupported.push(entry),
            Err(error) => log::warn!("ignoring plugin manifest {}: {error}", path.display()),
        }
    }
    plugins.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
    (plugins, unsupported)
}

/// The two things reading one manifest can yield.
pub(crate) enum Read {
    /// A plugin this daemon understands.
    Plugin(Box<Discovered>),
    /// A manifest that names a newer manifest or protocol version.
    Unsupported(Unsupported),
}

/// Parses one manifest file and resolves its executable relative to it.
///
/// The file is size-bounded before it is read, so an enormous file cannot be
/// used to exhaust memory.
pub(crate) fn read_manifest(path: &Path, origin: PluginOrigin) -> Result<Read, String> {
    let metadata = fs::metadata(path).map_err(|error| error.kind().to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES {
        return Err("not a regular file, or larger than a manifest may be".into());
    }
    let text = fs::read_to_string(path).map_err(|error| error.kind().to_string())?;
    match PluginManifest::parse(&text).map_err(|error| error.to_string())? {
        ManifestLoad::Valid(manifest) => {
            let executable = manifest.executable_path(path);
            Ok(Read::Plugin(Box::new(Discovered {
                manifest: *manifest,
                origin,
                manifest_path: path.to_path_buf(),
                executable,
            })))
        }
        ManifestLoad::Unsupported {
            id, name, reason, ..
        } => Ok(Read::Unsupported(Unsupported {
            id,
            name,
            origin,
            manifest_path: path.to_path_buf(),
            reason,
        })),
    }
}

/// Whether `id` may name a plugin directory and a state file.
pub(crate) fn id_is_safe(id: &str) -> bool {
    is_safe_id(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const THIRD_PARTY: &str = r#"{
        "manifest_version": 1, "protocol_version": 1,
        "id": "acme-scanner", "name": "Acme scanner",
        "executable": "scanner", "args": ["--serve", "--quiet"],
        "version": "1.0.0", "author": "Acme",
        "capabilities": {"clipboard_read": true, "paste": true, "cancel": true,
            "mime_types": ["text/uri-list", "x-special/gnome-copied-files"]}
    }"#;

    fn write(directory: &Path, name: &str, contents: &str) {
        fs::create_dir_all(directory).unwrap();
        fs::write(directory.join(name), contents).unwrap();
    }

    fn ids(found: &Found) -> Vec<&str> {
        found
            .plugins
            .iter()
            .map(|plugin| plugin.manifest.id.as_str())
            .collect()
    }

    /// The compiled-in descriptors are the packaged files. If one stops
    /// parsing, the build is wrong, not the user's machine.
    #[test]
    fn every_bundled_descriptor_is_valid_and_names_its_own_id() {
        let directory = tempfile::tempdir().unwrap();
        let found = discover(directory.path(), None, None);
        assert_eq!(
            ids(&found),
            [CLIPBOARD_PLUGIN_ID, FUSE_PLUGIN_ID, EDGE_GLOW_PLUGIN_ID]
        );
        for plugin in &found.plugins {
            assert_eq!(plugin.origin, PluginOrigin::Bundled);
            assert!(plugin.manifest_path.as_os_str().is_empty());
            assert!(
                !plugin.manifest.roles().is_empty(),
                "{}",
                plugin.manifest.id
            );
        }
    }

    #[test]
    fn the_bundled_roles_come_from_the_packaged_descriptors() {
        use syntra_plugin_api::PluginRole;
        let directory = tempfile::tempdir().unwrap();
        let found = discover(directory.path(), None, None);
        let roles = |id: &str| {
            found
                .plugins
                .iter()
                .find(|plugin| plugin.manifest.id == id)
                .unwrap()
                .manifest
                .roles()
        };
        assert_eq!(roles(CLIPBOARD_PLUGIN_ID), vec![PluginRole::FileClipboard]);
        assert!(roles(FUSE_PLUGIN_ID).contains(&PluginRole::MountedFileReceive));
        assert_eq!(roles(EDGE_GLOW_PLUGIN_ID), vec![PluginRole::Pointer]);
    }

    /// Looking at a directory must not run anything in it.
    #[cfg(unix)]
    #[test]
    fn discovery_reads_files_and_never_executes_a_plugin() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let dropped = directory.path().join("plugins");
        let witness = directory.path().join("ran");
        write(&dropped, "acme.json", THIRD_PARTY);
        let program = dropped.join("scanner");
        fs::write(
            &program,
            format!("#!/bin/sh\ntouch '{}'\n", witness.display()),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();

        let found = discover(directory.path(), Some(&dropped), None);
        assert!(ids(&found).contains(&"acme-scanner"));
        assert!(!witness.exists(), "a plugin was executed during discovery");
    }

    #[test]
    fn a_drop_in_is_third_party_and_its_args_reach_the_entry() {
        let directory = tempfile::tempdir().unwrap();
        let dropped = directory.path().join("plugins");
        write(&dropped, "acme.json", THIRD_PARTY);
        let found = discover(directory.path(), Some(&dropped), None);
        let plugin = found
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == "acme-scanner")
            .unwrap();
        assert_eq!(plugin.origin, PluginOrigin::DropIn);
        assert_eq!(plugin.manifest.argv(), ["--serve", "--quiet"]);
        assert_eq!(plugin.executable, dropped.join("scanner"));
    }

    /// A manifest cannot make itself bundled by saying so.
    #[test]
    fn the_bundled_claim_in_a_manifest_does_not_confer_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let dropped = directory.path().join("plugins");
        write(
            &dropped,
            "claim.json",
            &THIRD_PARTY.replace("\"id\"", "\"bundled\": true, \"id\""),
        );
        let found = discover(directory.path(), Some(&dropped), None);
        let plugin = found
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == "acme-scanner")
            .unwrap();
        assert!(plugin.manifest.bundled, "kept as a displayed claim");
        assert_eq!(plugin.origin, PluginOrigin::DropIn);
    }

    /// A stray manifest beside the daemon does not become a bundled plugin and
    /// does not displace a real one.
    #[test]
    fn a_leftover_manifest_beside_the_daemon_is_not_discovered() {
        let directory = tempfile::tempdir().unwrap();
        write(directory.path(), "stale-clipboard.json", THIRD_PARTY);
        let found = discover(directory.path(), None, None);
        assert!(!ids(&found).contains(&"acme-scanner"));
    }

    #[test]
    fn a_user_manifest_with_a_bundled_id_substitutes_it_as_third_party() {
        let directory = tempfile::tempdir().unwrap();
        let dropped = directory.path().join("plugins");
        write(
            &dropped,
            "fuse.json",
            &THIRD_PARTY.replace("acme-scanner", "fuse"),
        );
        let found = discover(directory.path(), Some(&dropped), None);
        let fuse: Vec<_> = found
            .plugins
            .iter()
            .filter(|plugin| plugin.manifest.id == "fuse")
            .collect();
        assert_eq!(fuse.len(), 1, "the id appears once");
        assert_eq!(fuse[0].origin, PluginOrigin::DropIn);
        assert_eq!(fuse[0].manifest.name, "Acme scanner");
    }

    #[test]
    fn a_future_manifest_is_listed_as_unsupported_and_never_as_a_plugin() {
        let directory = tempfile::tempdir().unwrap();
        let dropped = directory.path().join("plugins");
        write(
            &dropped,
            "future.json",
            &THIRD_PARTY.replace("\"manifest_version\": 1", "\"manifest_version\": 99"),
        );
        let found = discover(directory.path(), Some(&dropped), None);
        assert!(!ids(&found).contains(&"acme-scanner"));
        assert_eq!(found.unsupported.len(), 1);
        assert_eq!(found.unsupported[0].id.as_deref(), Some("acme-scanner"));
        assert!(found.unsupported[0].reason.contains("manifest version 99"));
    }

    #[test]
    fn a_malformed_or_unsafe_manifest_is_skipped_not_fatal() {
        let directory = tempfile::tempdir().unwrap();
        let dropped = directory.path().join("plugins");
        write(&dropped, "broken.json", "{ not json");
        write(
            &dropped,
            "evil.json",
            &THIRD_PARTY.replace("acme-scanner", "../../escaped"),
        );
        write(&dropped, "good.json", THIRD_PARTY);
        let found = discover(directory.path(), Some(&dropped), None);
        assert!(ids(&found).contains(&"acme-scanner"));
        assert!(!ids(&found).iter().any(|id| id.contains("escaped")));
    }

    #[test]
    fn a_package_directory_is_discovered_through_syntra_plugin_json() {
        let directory = tempfile::tempdir().unwrap();
        let store = directory.path().join("store");
        write(
            &store.join("acme-scanner"),
            "syntra-plugin.json",
            THIRD_PARTY,
        );
        let found = discover(directory.path(), None, Some(&store));
        let plugin = found
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == "acme-scanner")
            .unwrap();
        assert_eq!(plugin.origin, PluginOrigin::Installed);
        assert_eq!(
            plugin.executable,
            store.join("acme-scanner").join("scanner")
        );
    }

    #[test]
    fn an_oversized_manifest_is_refused_without_being_read() {
        let directory = tempfile::tempdir().unwrap();
        let dropped = directory.path().join("plugins");
        let padding = " ".repeat(MAX_MANIFEST_BYTES as usize + 1);
        write(&dropped, "big.json", &format!("{THIRD_PARTY}{padding}"));
        let found = discover(directory.path(), Some(&dropped), None);
        assert!(!ids(&found).contains(&"acme-scanner"));
    }

    #[test]
    fn discovery_order_is_stable() {
        let directory = tempfile::tempdir().unwrap();
        let dropped = directory.path().join("plugins");
        for id in ["zz", "aa", "mm"] {
            write(
                &dropped,
                &format!("{id}.json"),
                &THIRD_PARTY.replace("acme-scanner", id),
            );
        }
        let first = ids(&discover(directory.path(), Some(&dropped), None))
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let second = ids(&discover(directory.path(), Some(&dropped), None))
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(first, second);
        assert_eq!(&first[3..], ["aa", "mm", "zz"]);
    }

    /// The package store's scratch directories hold complete older or
    /// half-built packages; they must never be listed as plugins.
    #[test]
    fn the_stores_scratch_directories_are_not_plugins() {
        let directory = tempfile::tempdir().unwrap();
        let store = directory.path().join("store");
        for name in ["acme", ".backup-1a2b", ".staging-3c4d"] {
            let package = store.join(name);
            fs::create_dir_all(&package).unwrap();
            fs::write(package.join("syntra-plugin.json"), THIRD_PARTY).unwrap();
            fs::write(package.join("scanner"), "").unwrap();
        }
        let (plugins, _) = read_directory(&store, PluginOrigin::Installed);
        assert_eq!(plugins.len(), 1, "only the real package is discovered");
        assert!(plugins[0].executable.starts_with(store.join("acme")));
    }
}
