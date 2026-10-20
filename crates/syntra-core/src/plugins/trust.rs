//! What the user has approved, bound to content.
//!
//! An approval names a plugin id *and* the content digest the user was shown.
//! It is never an implicit permission: a plugin whose executable, resources,
//! arguments or declared capabilities changed has a different digest, so it is
//! [`PluginTrust::ChangedSinceApproval`] and stays stopped until the new digest
//! is approved.
//!
//! Plugins found beside the daemon executable are [`PluginTrust::Bundled`].
//! That is decided from *where they were found*, by this module's caller, and
//! never from `metadata.bundled`, which a plugin can claim for itself.
//!
//! # Persistence
//!
//! The record is a small JSON file kept apart from the manifests, so what a
//! user drops into the plugins directory is never overwritten by what the
//! daemon saves. A change is reported as made only when it was *saved*: every
//! mutating method returns a [`Result`], and on failure the in-memory record
//! is restored, so what the daemon believes is approved is never more than
//! what survives a restart.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};
use syntra_api::{PluginRole, PluginTrust};
use thiserror::Error;

/// Why a trust change was not made.
///
/// The text is safe to show a client and to log: it names the kind of failure
/// but never a path.
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum TrustError {
    /// The digest the user was shown is not the digest of the content now.
    #[error("the plugin changed since it was reviewed; review it again")]
    Stale,
    /// The plugin has nothing approved.
    #[error("the plugin has no approval to withdraw")]
    NotApproved,
    /// The record could not be saved, so nothing was changed.
    #[error("approvals could not be saved ({0})")]
    Save(&'static str),
}

/// One approval: exactly the content the user was shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Approval {
    /// Content digest approved.
    pub digest: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Record {
    /// Approved digest per plugin id.
    #[serde(default)]
    approved: BTreeMap<String, Approval>,
    /// Explicit provider preference per role, by [`PluginRole`] text form.
    #[serde(default)]
    preferred: BTreeMap<String, String>,
}

/// Approvals and role preferences, persisted.
#[derive(Debug, Default)]
pub(crate) struct TrustStore {
    path: Option<PathBuf>,
    record: Record,
}

impl TrustStore {
    /// An in-memory store that persists nothing.
    pub(crate) fn in_memory() -> Self {
        Self::default()
    }

    /// Loads the store from `path`. A missing, unreadable or malformed file is
    /// an empty store: the daemon must start whatever is lying on the disk,
    /// and an empty store approves nothing, which is the safe reading.
    pub(crate) fn load(path: PathBuf) -> Self {
        let record = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            path: Some(path),
            record,
        }
    }

    /// Applies `change` to the record and saves it. If saving fails the
    /// record is restored and the failure returned, so the caller never
    /// reports a change that would not survive a restart.
    fn commit(&mut self, change: impl FnOnce(&mut Record)) -> Result<(), TrustError> {
        let before = self.record.clone();
        change(&mut self.record);
        if let Some(path) = &self.path {
            if let Err(kind) = write_atomic(path, &self.record) {
                self.record = before;
                log::warn!("could not save plugin approvals: {kind}");
                return Err(TrustError::Save(kind));
            }
        }
        Ok(())
    }

    /// The trust a plugin of this content has.
    ///
    /// `bundled` is the caller's judgement from where the file was found.
    pub(crate) fn trust_of(&self, id: &str, digest: &str, bundled: bool) -> PluginTrust {
        if bundled {
            return PluginTrust::Bundled;
        }
        match self.record.approved.get(id) {
            None => PluginTrust::Unapproved,
            Some(approval) if !digest.is_empty() && approval.digest == digest => {
                PluginTrust::Approved
            }
            Some(_) => PluginTrust::ChangedSinceApproval,
        }
    }

    /// The digest last approved for `id`, if any.
    pub(crate) fn approved_digest(&self, id: &str) -> Option<&str> {
        self.record
            .approved
            .get(id)
            .map(|approval| approval.digest.as_str())
    }

    /// Approves `shown` for `id`, once it is saved.
    ///
    /// `current` is the digest of the content on disk now. If it differs from
    /// what the user was shown, the approval is refused: an approval can never
    /// cover content the user did not see.
    pub(crate) fn approve(
        &mut self,
        id: &str,
        shown: &str,
        current: &str,
    ) -> Result<(), TrustError> {
        if shown.is_empty() || shown != current {
            return Err(TrustError::Stale);
        }
        self.commit(|record| {
            record.approved.insert(
                id.to_owned(),
                Approval {
                    digest: shown.to_owned(),
                },
            );
        })
    }

    /// Withdraws an approval, once that is saved.
    pub(crate) fn revoke(&mut self, id: &str) -> Result<(), TrustError> {
        if !self.record.approved.contains_key(id) {
            return Err(TrustError::NotApproved);
        }
        self.commit(|record| {
            record.approved.remove(id);
        })
    }

    /// Forgets everything about `id`: its approval and any role preference
    /// naming it. Succeeds without writing when there is nothing to forget.
    pub(crate) fn forget(&mut self, id: &str) -> Result<(), TrustError> {
        let has_approval = self.record.approved.contains_key(id);
        let has_preference = self
            .record
            .preferred
            .values()
            .any(|preferred| preferred == id);
        if !has_approval && !has_preference {
            return Ok(());
        }
        self.commit(|record| {
            record.approved.remove(id);
            record.preferred.retain(|_, preferred| preferred != id);
        })
    }

    /// The provider the user chose for `role`.
    pub(crate) fn preferred(&self, role: PluginRole) -> Option<&str> {
        self.record
            .preferred
            .get(role_key(role))
            .map(String::as_str)
    }

    /// Sets or clears the preferred provider of `role`, once that is saved.
    pub(crate) fn set_preferred(
        &mut self,
        role: PluginRole,
        id: Option<&str>,
    ) -> Result<(), TrustError> {
        self.commit(|record| match id {
            Some(id) => {
                record
                    .preferred
                    .insert(role_key(role).to_owned(), id.to_owned());
            }
            None => {
                record.preferred.remove(role_key(role));
            }
        })
    }
}

fn role_key(role: PluginRole) -> &'static str {
    match role {
        PluginRole::FileClipboard => "file_clipboard",
        PluginRole::MountedFileReceive => "mounted_file_receive",
        PluginRole::Pointer => "pointer",
    }
}

/// Writes `record` whole or not at all.
///
/// The bytes go to a uniquely named file created exclusively in the same
/// directory with owner-only permissions, are flushed to disk, and are then
/// renamed over the real file. A fixed temporary name would let a symbolic
/// link planted there redirect the write onto an unrelated file; an exclusive
/// unique one cannot be pre-planted.
///
/// The error is a short fixed description, never a path or the system's
/// message, so it can be shown to a client.
fn write_atomic(path: &Path, record: &Record) -> Result<(), &'static str> {
    let directory = path.parent().ok_or("no directory")?;
    std::fs::create_dir_all(directory).map_err(|_| "cannot create the directory")?;
    let text = serde_json::to_vec_pretty(record).map_err(|_| "cannot encode")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".approvals-")
        .suffix(".tmp")
        .tempfile_in(directory)
        .map_err(|_| "cannot create a temporary file")?;
    temporary.write_all(&text).map_err(|_| "cannot write")?;
    temporary.as_file().sync_all().map_err(|_| "cannot flush")?;
    temporary
        .persist(path)
        .map_err(|_| "cannot replace the file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in(directory: &tempfile::TempDir) -> TrustStore {
        TrustStore::load(directory.path().join("approvals.json"))
    }

    #[test]
    fn nothing_is_approved_until_the_user_approves_the_digest_shown() {
        let mut store = TrustStore::in_memory();
        assert_eq!(store.trust_of("a", "d1", false), PluginTrust::Unapproved);
        assert_eq!(store.approve("a", "d1", "d2"), Err(TrustError::Stale));
        assert_eq!(store.approve("a", "", ""), Err(TrustError::Stale));
        assert_eq!(store.trust_of("a", "d1", false), PluginTrust::Unapproved);
        assert_eq!(store.approve("a", "d1", "d1"), Ok(()));
        assert_eq!(store.trust_of("a", "d1", false), PluginTrust::Approved);
    }

    #[test]
    fn changed_content_loses_its_approval_without_being_forgotten() {
        let mut store = TrustStore::in_memory();
        store.approve("a", "d1", "d1").unwrap();
        assert_eq!(
            store.trust_of("a", "d2", false),
            PluginTrust::ChangedSinceApproval
        );
        // The old digest is still what is shown as approved, so the user can
        // see what changed.
        assert_eq!(store.approved_digest("a"), Some("d1"));
        // Going back to the approved content is approved again.
        assert_eq!(store.trust_of("a", "d1", false), PluginTrust::Approved);
        store.approve("a", "d2", "d2").unwrap();
        assert_eq!(store.trust_of("a", "d2", false), PluginTrust::Approved);
    }

    #[test]
    fn bundled_is_the_callers_judgement_and_needs_no_approval() {
        let store = TrustStore::in_memory();
        assert_eq!(store.trust_of("clipboard", "d", true), PluginTrust::Bundled);
        assert_eq!(
            store.trust_of("clipboard", "d", false),
            PluginTrust::Unapproved
        );
    }

    #[test]
    fn approvals_and_preferences_survive_a_restart_and_a_damaged_file_is_empty() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = store_in(&directory);
        store.approve("a", "d1", "d1").unwrap();
        store
            .set_preferred(PluginRole::FileClipboard, Some("a"))
            .unwrap();
        drop(store);

        let reloaded = store_in(&directory);
        assert_eq!(reloaded.trust_of("a", "d1", false), PluginTrust::Approved);
        assert_eq!(reloaded.preferred(PluginRole::FileClipboard), Some("a"));
        let leftovers: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            leftovers,
            ["approvals.json"],
            "written whole, nothing left over"
        );

        std::fs::write(directory.path().join("approvals.json"), "{ not json").unwrap();
        let damaged = store_in(&directory);
        assert_eq!(damaged.trust_of("a", "d1", false), PluginTrust::Unapproved);
    }

    /// A change that cannot be saved must not be reported, and must not stay
    /// in memory: otherwise the daemon would run code on an approval that a
    /// restart silently forgets.
    #[cfg(unix)]
    #[test]
    fn a_change_that_cannot_be_saved_is_refused_and_rolled_back() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let mut store = store_in(&directory);
        store.approve("a", "d1", "d1").unwrap();

        // A read-only directory: nothing can be created in it.
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let writable = std::fs::File::create(directory.path().join("probe")).is_ok();
        let result = store.approve("b", "d2", "d2");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        if writable {
            // Running with enough privilege to ignore the mode (for example
            // as root): the situation cannot be produced, so say nothing.
            return;
        }
        assert!(matches!(result, Err(TrustError::Save(_))), "{result:?}");
        assert_eq!(
            store.trust_of("b", "d2", false),
            PluginTrust::Unapproved,
            "an unsaved approval does not take effect"
        );
        assert_eq!(store.trust_of("a", "d1", false), PluginTrust::Approved);
        assert!(
            !result
                .unwrap_err()
                .to_string()
                .contains(directory.path().to_str().unwrap()),
            "the error must not disclose a path"
        );
    }

    /// The temporary file is created exclusively with a fresh name, so a link
    /// planted at any fixed name cannot redirect the write.
    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_at_a_fixed_temporary_name_is_not_followed() {
        let directory = tempfile::tempdir().unwrap();
        let victim = directory.path().join("victim");
        std::fs::write(&victim, "keep me").unwrap();
        for name in ["approvals.json.tmp", ".approvals.tmp", "approvals.tmp"] {
            std::os::unix::fs::symlink(&victim, directory.path().join(name)).unwrap();
        }
        let mut store = store_in(&directory);
        store.approve("a", "d1", "d1").unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
    }

    #[test]
    fn forgetting_a_plugin_drops_its_approval_and_preferences() {
        let mut store = TrustStore::in_memory();
        store.approve("a", "d1", "d1").unwrap();
        store.set_preferred(PluginRole::Pointer, Some("a")).unwrap();
        store.forget("a").unwrap();
        assert_eq!(store.trust_of("a", "d1", false), PluginTrust::Unapproved);
        assert_eq!(store.preferred(PluginRole::Pointer), None);
        assert_eq!(store.revoke("a"), Err(TrustError::NotApproved));
    }
}
