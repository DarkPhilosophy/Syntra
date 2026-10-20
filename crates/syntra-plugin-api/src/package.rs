//! The content digest a user approves.
//!
//! Approval is bound to *content*, not to a name or a path: the digest covers
//! the manifest's security-relevant fields, the executable, every listed
//! resource and the argument vector the daemon would launch with. Change any
//! of them and the digest changes, so changed code can never ride on an earlier
//! approval.
//!
//! Computing a digest only reads files. It never runs the plugin.
//!
//! # What a digest does and does not guarantee
//!
//! It identifies the bytes that were read *at that moment*. It does not make
//! the files immutable: a process with write access to the package can change
//! them after the digest was computed and before the program is executed. The
//! caller narrows that window by recomputing the digest immediately before
//! every launch. A daemon-owned store prevents accidental package mixing, not
//! another process running as the same user from writing those files.

use crate::PluginManifest;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
};
use thiserror::Error;

/// Most bytes of one file that are hashed. Larger files are refused rather
/// than silently truncated, because a prefix digest would approve unseen code.
pub const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
/// Most files one package may contain across its resources.
pub const MAX_FILES: usize = 4096;
/// Deepest directory nesting walked inside a resource directory.
pub const MAX_DEPTH: usize = 16;

/// Why a package could not be digested.
#[derive(Debug, Error)]
pub enum PackageError {
    /// A file or directory could not be read.
    #[error("cannot read {path}: {source}")]
    Io {
        /// Path relative to the package, as it appears in the manifest.
        path: String,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// A resource points outside the package, through a symbolic link or
    /// otherwise.
    #[error("{0} resolves outside the plugin directory")]
    Escapes(String),
    /// The package has more files or nesting than is accepted.
    #[error("the package is too large to approve: {0}")]
    TooLarge(String),
    /// An entry is not a regular file or directory (a FIFO, socket or device),
    /// so reading it could block or return unbounded data.
    #[error("{0} is not a regular file or directory")]
    NotRegular(String),
    /// A file changed while it was being read, so no consistent digest of it
    /// exists.
    #[error("{0} changed while it was being read")]
    Unstable(String),
}

/// A content digest together with what it covers, for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageDigest {
    /// Lowercase hexadecimal SHA-256 over everything listed in `files`, the
    /// argument vector and the declared capabilities.
    pub digest: String,
    /// Every file that went into the digest, in the order it was hashed.
    pub files: Vec<PathBuf>,
    /// The same files as they are labelled in the digest: the executable by
    /// its path, resources relative to the manifest. This is what a user is
    /// shown.
    pub labels: Vec<String>,
}

impl PackageDigest {
    /// A short form safe to show next to an approval button.
    pub fn short(&self) -> &str {
        &self.digest[..self.digest.len().min(16)]
    }
}

/// Computes the digest of a plugin package without executing anything.
///
/// Covers, in a fixed order with length prefixes so no two inputs collide:
/// the manifest's identity, versions, argument vector, capabilities and
/// settings; the executable; then each resource, files in sorted order.
///
/// `manifest_path` anchors relative paths. `executable` is the already
/// resolved path the daemon would launch, so the digest covers the exact file
/// that will run, including a user override.
pub fn digest_package(
    manifest: &PluginManifest,
    manifest_path: &Path,
    executable: &Path,
) -> Result<PackageDigest, PackageError> {
    let mut hasher = Sha256::new();
    let mut files = Vec::new();
    let mut labels = Vec::new();

    section(&mut hasher, "identity", manifest.id.as_bytes());
    section(&mut hasher, "version", manifest.version.as_bytes());
    section(
        &mut hasher,
        "protocol",
        &manifest.protocol_version.to_le_bytes(),
    );
    section(
        &mut hasher,
        "flags",
        &[
            u8::from(manifest.on_demand),
            u8::from(manifest.owns_settings),
        ],
    );
    for argument in manifest.argv() {
        section(&mut hasher, "arg", argument.as_bytes());
    }
    // Declared capabilities are part of what is approved, so widening them is
    // a change of content even if no file changed.
    let capabilities = &manifest.capabilities;
    section(
        &mut hasher,
        "capabilities",
        &[
            u8::from(capabilities.clipboard_read),
            u8::from(capabilities.paste),
            u8::from(capabilities.cancel),
            u8::from(capabilities.requires_live_mount),
            u8::from(capabilities.pointer_events),
        ],
    );
    let mut mime_types = capabilities.mime_types.clone();
    mime_types.sort();
    for mime in &mime_types {
        section(&mut hasher, "mime", mime.as_bytes());
    }
    for setting in &manifest.settings {
        let text = serde_json::to_vec(setting).unwrap_or_default();
        section(&mut hasher, "setting", &text);
    }
    for handshake_id in &manifest.handshake_ids {
        section(&mut hasher, "handshake_id", handshake_id.as_bytes());
    }

    let root = manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let executable_label = executable.display().to_string();
    hash_file(
        &mut hasher,
        "executable",
        executable,
        &executable_label,
        &mut files,
        &mut labels,
    )?;

    let root_real = fs::canonicalize(&root).map_err(|source| PackageError::Io {
        path: root.display().to_string(),
        source,
    })?;
    for resource in &manifest.resources {
        let path = root.join(resource);
        let real = fs::canonicalize(&path).map_err(|source| PackageError::Io {
            path: resource.clone(),
            source,
        })?;
        if !real.starts_with(&root_real) {
            return Err(PackageError::Escapes(resource.clone()));
        }
        walk(
            &mut hasher,
            &root_real,
            resource,
            &real,
            0,
            &mut files,
            &mut labels,
        )?;
    }

    Ok(PackageDigest {
        digest: hex(&hasher.finalize()),
        files,
        labels,
    })
}

fn walk(
    hasher: &mut Sha256,
    root: &Path,
    label: &str,
    path: &Path,
    depth: usize,
    files: &mut Vec<PathBuf>,
    labels: &mut Vec<String>,
) -> Result<(), PackageError> {
    if depth > MAX_DEPTH {
        return Err(PackageError::TooLarge(format!(
            "{label} is nested too deeply"
        )));
    }
    let metadata = fs::metadata(path).map_err(|source| PackageError::Io {
        path: label.to_owned(),
        source,
    })?;
    if metadata.is_dir() {
        let mut entries = fs::read_dir(path)
            .map_err(|source| PackageError::Io {
                path: label.to_owned(),
                source,
            })?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| PackageError::Io {
                path: label.to_owned(),
                source,
            })?;
        entries.sort();
        for name in entries {
            let child_label = format!("{label}/{}", name.to_string_lossy());
            let child = fs::canonicalize(path.join(&name)).map_err(|source| PackageError::Io {
                path: child_label.clone(),
                source,
            })?;
            // A symbolic link inside a resource directory may not reach out
            // of the package: that would let approved content change without
            // the digest noticing.
            if !child.starts_with(root) {
                return Err(PackageError::Escapes(child_label));
            }
            walk(hasher, root, &child_label, &child, depth + 1, files, labels)?;
        }
        Ok(())
    } else {
        hash_file(hasher, "resource", path, label, files, labels)
    }
}

/// Identity of the file a metadata describes, where the platform has one.
#[cfg(unix)]
fn identity(metadata: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn identity(_: &fs::Metadata) -> (u64, u64) {
    (0, 0)
}

/// Hashes one regular file, refusing anything that is not stable.
///
/// Only regular files are read. A FIFO, socket or device could block the read
/// forever or produce unbounded data, and no declared size bounds it. The file
/// is checked before it is opened and again through the open handle, must be
/// the same file both times, must yield exactly its stated length, and must
/// have the same length and modification time afterwards.
fn hash_file(
    hasher: &mut Sha256,
    kind: &str,
    path: &Path,
    label: &str,
    files: &mut Vec<PathBuf>,
    labels: &mut Vec<String>,
) -> Result<(), PackageError> {
    if files.len() >= MAX_FILES {
        return Err(PackageError::TooLarge(format!(
            "more than {MAX_FILES} files"
        )));
    }
    let io_error = |source| PackageError::Io {
        path: label.to_owned(),
        source,
    };
    let before = fs::symlink_metadata(path).map_err(io_error)?;
    // Paths reaching here are canonical, so a symbolic link means the file
    // was swapped after canonicalisation: refuse rather than follow it.
    if !before.file_type().is_file() {
        return Err(PackageError::NotRegular(label.to_owned()));
    }
    let mut file = File::open(path).map_err(io_error)?;
    let opened = file.metadata().map_err(io_error)?;
    if !opened.is_file() {
        return Err(PackageError::NotRegular(label.to_owned()));
    }
    if identity(&before) != identity(&opened) {
        return Err(PackageError::Unstable(label.to_owned()));
    }
    let length = opened.len();
    if length > MAX_FILE_BYTES {
        return Err(PackageError::TooLarge(format!(
            "{label} is larger than {MAX_FILE_BYTES} bytes"
        )));
    }
    section(hasher, kind, label.as_bytes());
    hasher.update(length.to_le_bytes());
    let mut buffer = [0u8; 64 * 1024];
    let mut remaining = length;
    while remaining > 0 {
        let wanted = buffer.len().min(remaining as usize);
        let read = file.read(&mut buffer[..wanted]).map_err(io_error)?;
        if read == 0 {
            // The file shrank while it was being read; a digest of a moving
            // target approves nothing.
            return Err(PackageError::Unstable(label.to_owned()));
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    // Any byte past the stated length means the file grew: a prefix is not
    // the file.
    let mut extra = [0u8; 1];
    if file.read(&mut extra).map_err(io_error)? != 0 {
        return Err(PackageError::Unstable(label.to_owned()));
    }
    let after = file.metadata().map_err(io_error)?;
    if after.len() != length || after.modified().ok() != opened.modified().ok() {
        return Err(PackageError::Unstable(label.to_owned()));
    }
    files.push(path.to_path_buf());
    labels.push(label.to_owned());
    Ok(())
}

/// Length-prefixed, tagged input, so concatenations cannot be confused.
fn section(hasher: &mut Sha256, tag: &str, bytes: &[u8]) {
    hasher.update((tag.len() as u64).to_le_bytes());
    hasher.update(tag.as_bytes());
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ManifestLoad, PluginManifest};

    fn manifest(args: &str, resources: &str) -> PluginManifest {
        let text = format!(
            r#"{{"manifest_version":1,"protocol_version":1,"id":"acme","name":"Acme",
            "executable":"run","args":{args},"resources":{resources},
            "capabilities":{{"pointer_events":true,"mime_types":[]}}}}"#
        );
        match PluginManifest::parse(&text).unwrap() {
            ManifestLoad::Valid(manifest) => *manifest,
            ManifestLoad::Unsupported { .. } => panic!("expected a valid manifest"),
        }
    }

    fn digest(directory: &Path, manifest: &PluginManifest) -> Result<PackageDigest, PackageError> {
        digest_package(
            manifest,
            &directory.join("m.json"),
            &directory.join(&manifest.executable),
        )
    }

    #[test]
    fn the_digest_changes_with_the_executable_the_resources_and_the_arguments() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path();
        fs::write(dir.join("run"), b"#!/bin/sh\n").unwrap();
        fs::create_dir_all(dir.join("lib")).unwrap();
        fs::write(dir.join("lib/a.so"), b"one").unwrap();

        let base = manifest("[]", r#"["lib"]"#);
        let first = digest(dir, &base).unwrap();
        assert_eq!(
            first.digest,
            digest(dir, &base).unwrap().digest,
            "digest is deterministic"
        );
        assert_eq!(first.labels.last().map(String::as_str), Some("lib/a.so"));

        fs::write(dir.join("lib/a.so"), b"two").unwrap();
        let resource_changed = digest(dir, &base).unwrap().digest;
        assert_ne!(first.digest, resource_changed);

        fs::write(dir.join("run"), b"#!/bin/sh\nexit 1\n").unwrap();
        let executable_changed = digest(dir, &base).unwrap().digest;
        assert_ne!(resource_changed, executable_changed);

        let with_args = manifest(r#"["--flag"]"#, r#"["lib"]"#);
        assert_ne!(executable_changed, digest(dir, &with_args).unwrap().digest);
    }

    #[test]
    fn the_digest_does_not_depend_on_directory_listing_order() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path();
        fs::write(dir.join("run"), b"x").unwrap();
        fs::create_dir_all(dir.join("lib")).unwrap();
        // Created in an order different from the sorted one.
        for name in ["z.so", "a.so", "m.so"] {
            fs::write(dir.join("lib").join(name), name.as_bytes()).unwrap();
        }
        let digest = digest(dir, &manifest("[]", r#"["lib"]"#)).unwrap();
        assert_eq!(
            digest.labels[1..],
            ["lib/a.so", "lib/m.so", "lib/z.so"],
            "files are hashed in sorted order"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_resource_that_climbs_out_of_the_package_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let dir = directory.path();
        fs::write(dir.join("run"), b"x").unwrap();
        fs::write(outside.path().join("secret"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.join("link")).unwrap();
        let error = digest(dir, &manifest("[]", r#"["link"]"#)).unwrap_err();
        assert!(matches!(error, PackageError::Escapes(_)), "{error}");

        // A link inside an otherwise contained resource directory is refused
        // the same way.
        fs::create_dir_all(dir.join("lib")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.join("lib/s")).unwrap();
        let error = digest(dir, &manifest("[]", r#"["lib"]"#)).unwrap_err();
        assert!(matches!(error, PackageError::Escapes(_)), "{error}");
    }

    /// A FIFO would block a read for ever. It must be refused without being
    /// opened for reading.
    #[cfg(unix)]
    #[test]
    fn a_fifo_in_a_resource_directory_is_refused_not_read() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path();
        fs::write(dir.join("run"), b"x").unwrap();
        fs::create_dir_all(dir.join("lib")).unwrap();
        let fifo = CString::new(dir.join("lib/pipe").as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo` is a valid NUL-terminated path that lives for the call.
        let made = unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) };
        assert_eq!(made, 0, "the test needs a FIFO");
        let error = digest(dir, &manifest("[]", r#"["lib"]"#)).unwrap_err();
        assert!(matches!(error, PackageError::NotRegular(_)), "{error}");
    }

    #[test]
    fn a_missing_resource_or_executable_is_an_error_not_an_empty_digest() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path();
        fs::write(dir.join("run"), b"x").unwrap();
        let error = digest(dir, &manifest("[]", r#"["absent"]"#)).unwrap_err();
        assert!(matches!(error, PackageError::Io { .. }), "{error}");

        fs::remove_file(dir.join("run")).unwrap();
        let error = digest(dir, &manifest("[]", "[]")).unwrap_err();
        assert!(matches!(error, PackageError::Io { .. }), "{error}");
    }

    #[test]
    fn a_manifest_resource_that_is_not_contained_is_invalid() {
        for bad in [r#"["../x"]"#, r#"["/etc"]"#, r#"[""]"#] {
            let text = format!(
                r#"{{"manifest_version":1,"protocol_version":1,"id":"a","name":"A",
                "executable":"run","resources":{bad},
                "capabilities":{{"pointer_events":true,"mime_types":[]}}}}"#
            );
            assert!(PluginManifest::parse(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_directory_nested_too_deeply_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path();
        fs::write(dir.join("run"), b"x").unwrap();
        let mut deep = dir.join("lib");
        for _ in 0..=MAX_DEPTH + 1 {
            deep = deep.join("d");
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("f"), b"x").unwrap();
        let error = digest(dir, &manifest("[]", r#"["lib"]"#)).unwrap_err();
        assert!(matches!(error, PackageError::TooLarge(_)), "{error}");
    }
}
