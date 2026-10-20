//! What the daemon knows about plugins, and which of them may run.
//!
//! The registry is the one place that answers: which plugins exist, who vouches
//! for each, whether the user approved exactly the content on disk now, which
//! plugin provides each role, and what state each is in. The supervisor runs
//! processes; the registry decides whether and which.
//!
//! # State
//!
//! A plugin's life is one ladder (see [`PluginState`]): installed, approved,
//! enabled, ready, running, with failed and unsupported off to the side. It is
//! derived from facts the registry holds (approval, preference, observed
//! process events), never stored as a label that can disagree with them.
//!
//! # Approval is re-derived, never remembered
//!
//! [`PluginRegistry::launch_plan`] recomputes the content digest each time it
//! is asked. A start, a restart and an automatic relaunch all go through it, so
//! a file changed after approval stops running at the next launch. A digest
//! identifies the bytes read at that moment; it does not make them immutable,
//! so the window between the read and the program starting is a limit of this
//! design and is stated rather than hidden.

use super::{
    discovery::{self, Discovered, Found, Unsupported},
    routing::{self, Candidate, client_role, manifest_role},
    supervisor::{Integrity, LaunchSpec},
    trust::{TrustError, TrustStore},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use syntra_api::{
    PluginHealth, PluginOrigin, PluginPending, PluginReview, PluginRoleStatus, PluginState,
    PluginStatus, PluginTrust,
};
use syntra_plugin_api::{PluginMetadata, PluginRole as ManifestRole, SettingDecl, digest_package};

/// Plugin protocol version this daemon speaks.
///
/// A plugin declaring a different one is still listed, so the user can see why
/// it misbehaves instead of finding an unexplained absence.
const SUPPORTED_PROTOCOL_VERSION: u32 = syntra_plugin_api::PROTOCOL_VERSION;

/// How long a launched plugin may take to complete its handshake.
///
/// Past this the process exists but is not answering, which is a different and
/// more useful statement than "starting" repeated indefinitely.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Restarts within a session after which a plugin is treated as unhealthy
/// rather than merely restarting.
const CRASH_LOOP_RESTARTS: u32 = 3;

/// What is kept on disk for one plugin.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SavedState {
    /// The user switched this plugin off.
    #[serde(default)]
    disabled: bool,
    /// The user switched this plugin on. Third-party plugins start off, so
    /// this records an explicit choice rather than the absence of one.
    #[serde(default)]
    enabled: bool,
    /// The user's choice for each setting, by key.
    #[serde(default)]
    values: HashMap<String, String>,
}

/// Reads a plugin's saved state. A missing, unreadable or malformed file is no
/// state at all: the daemon must start whatever is lying on the disk.
fn read_state(directory: &Path, id: &str) -> Option<SavedState> {
    let text = std::fs::read_to_string(directory.join(id).join("settings.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// Writes a plugin's state whole or not at all, through a uniquely named
/// exclusive temporary file in the same directory.
fn write_state(directory: &Path, id: &str, state: &SavedState) -> Result<(), &'static str> {
    use std::io::Write;
    let folder = directory.join(id);
    std::fs::create_dir_all(&folder).map_err(|_| "cannot create the directory")?;
    let text = serde_json::to_vec_pretty(state).map_err(|_| "cannot encode")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".settings-")
        .suffix(".tmp")
        .tempfile_in(&folder)
        .map_err(|_| "cannot create a temporary file")?;
    temporary.write_all(&text).map_err(|_| "cannot write")?;
    temporary.as_file().sync_all().map_err(|_| "cannot flush")?;
    temporary
        .persist(folder.join("settings.json"))
        .map_err(|_| "cannot replace the file")?;
    Ok(())
}

/// Why a plugin cannot be launched right now, ready to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// No such plugin.
    Unknown,
    /// Switched off.
    Disabled,
    /// Its content is not approved.
    NotApproved,
    /// Its content changed after it was approved.
    Changed,
    /// The executable is missing or not runnable.
    NotInstalled,
    /// The package could not be read to compute its digest.
    Unreadable(String),
    /// It cannot run here (a newer manifest or protocol version).
    Unsupported(String),
}

impl Refusal {
    /// Words a user can act on.
    pub(crate) fn text(&self) -> String {
        match self {
            Refusal::Unknown => "the plugin is not known".into(),
            Refusal::Disabled => "the plugin is switched off".into(),
            Refusal::NotApproved => "approve the plugin's content to run it".into(),
            Refusal::Changed => {
                "the plugin changed since it was approved; review and approve it again".into()
            }
            Refusal::NotInstalled => "the plugin's executable is missing or not runnable".into(),
            Refusal::Unreadable(why) => format!("the plugin cannot be checked: {why}"),
            Refusal::Unsupported(why) => why.clone(),
        }
    }
}

/// A plugin that is allowed to start, with exactly what to start.
#[derive(Debug, Clone)]
pub(crate) struct LaunchPlan {
    /// How to launch it.
    pub spec: LaunchSpec,
    /// What it should be told once it has handshaken.
    pub settings: Vec<(String, String)>,
}

/// File sizes and modification times of what a digest covers.
type Fingerprint = Vec<(PathBuf, Option<u64>, Option<std::time::SystemTime>)>;

/// A digest computed for display together with the fingerprint it was
/// computed under.
struct CachedDigest {
    fingerprint: Fingerprint,
    digest: Result<String, String>,
}

/// Tracks discovered plugins and which the user has switched on.
pub(crate) struct PluginRegistry {
    /// Discovery order is display order.
    plugins: Vec<Discovered>,
    /// Manifests that were found but cannot run here.
    unsupported: Vec<Unsupported>,
    trust: TrustStore,
    /// User preference per plugin id, once one was made.
    enabled: HashMap<String, bool>,
    /// Ids whose process has handshaken and is running.
    running: HashMap<String, bool>,
    /// When a launch was observed, so a start that never completes cannot stay
    /// pending for ever.
    starting: HashMap<String, Instant>,
    /// Restart count per id for this session.
    restarts: HashMap<String, u32>,
    /// Most recent failure reported for a plugin.
    failures: HashMap<String, String>,
    /// Process id of the running plugin, so the interface can prove the entry
    /// corresponds to a real process rather than a file on disk.
    pids: HashMap<String, u32>,
    /// What the user chose for each plugin's settings: plugin id, then key.
    chosen: HashMap<String, HashMap<String, String>>,
    /// Where each plugin's choices are kept between runs, when persisted.
    state_dir: Option<PathBuf>,
    /// Settings a plugin declared it owns, as it last reported them.
    owned: HashMap<String, Vec<syntra_plugin_api::SettingControl>>,
    /// What each running plugin declared about itself at its handshake. Shown;
    /// never a source of trust or identity.
    declared: HashMap<String, PluginMetadata>,
    /// A request the user made that the daemon has not applied yet.
    pending: HashMap<String, PluginPending>,
    /// Why the latest lifecycle request for a plugin was refused.
    action_errors: HashMap<String, String>,
    /// Recent diagnostic lines per plugin, sanitised and bounded.
    diagnostics: HashMap<String, Vec<String>>,
    /// Candidates the user asked to review, not installed.
    reviews: Vec<PluginReview>,
    /// Where Syntra keeps packages it installed.
    store: Option<PathBuf>,
    /// Digests computed for display, by plugin id. See
    /// [`PluginRegistry::cached_digest`].
    digests: std::cell::RefCell<HashMap<String, CachedDigest>>,
}

impl PluginRegistry {
    /// Builds the registry from what discovery found.
    ///
    /// Nothing here runs a plugin or reads its description from its binary.
    pub(crate) fn discover(
        daemon_executable: &Path,
        drop_in: Option<PathBuf>,
        store: Option<PathBuf>,
    ) -> Self {
        let directory = daemon_executable
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let Found {
            plugins,
            unsupported,
        } = discovery::discover(&directory, drop_in.as_deref(), store.as_deref());
        Self {
            plugins,
            unsupported,
            trust: TrustStore::in_memory(),
            enabled: HashMap::new(),
            running: HashMap::new(),
            starting: HashMap::new(),
            restarts: HashMap::new(),
            failures: HashMap::new(),
            pids: HashMap::new(),
            chosen: HashMap::new(),
            state_dir: None,
            owned: HashMap::new(),
            declared: HashMap::new(),
            pending: HashMap::new(),
            action_errors: HashMap::new(),
            diagnostics: HashMap::new(),
            reviews: Vec::new(),
            store,
            digests: std::cell::RefCell::new(HashMap::new()),
        }
    }

    /// Keeps approvals in `<directory>/approvals.json` and each plugin's
    /// choices in `<directory>/<id>/settings.json`, and loads what is already
    /// there. Separate from `discover` so a registry can be made without
    /// touching the disk, as the tests and the first start do.
    pub(crate) fn with_state_dir(mut self, directory: PathBuf) -> Self {
        self.trust = TrustStore::load(directory.join("approvals.json"));
        for plugin in &self.plugins {
            let id = plugin.manifest.id.clone();
            if let Some(saved) = read_state(&directory, &id) {
                if saved.disabled {
                    self.enabled.insert(id.clone(), false);
                } else if saved.enabled {
                    self.enabled.insert(id.clone(), true);
                }
                if !saved.values.is_empty() {
                    self.chosen.insert(id, saved.values);
                }
            }
        }
        self.state_dir = Some(directory);
        self
    }

    fn find(&self, id: &str) -> Option<&Discovered> {
        self.plugins.iter().find(|plugin| plugin.manifest.id == id)
    }

    fn is_bundled(plugin: &Discovered) -> bool {
        plugin.origin == PluginOrigin::Bundled
    }

    /// Where the manifest file is. A bundled descriptor has no file; its
    /// package root is the directory of its executable.
    fn manifest_path_of(plugin: &Discovered) -> PathBuf {
        if plugin.manifest_path.as_os_str().is_empty() {
            plugin.executable.with_file_name("syntra-plugin.json")
        } else {
            plugin.manifest_path.clone()
        }
    }

    /// The content digest of a plugin as it is on disk now, or why it cannot
    /// be computed. Reads files only.
    fn digest_of(plugin: &Discovered) -> Result<String, String> {
        digest_package(
            &plugin.manifest,
            &Self::manifest_path_of(plugin),
            &plugin.executable,
        )
        .map(|digest| digest.digest)
        .map_err(|error| error.to_string())
    }

    /// Ids of every plugin with a live process, started or ready. Reads only
    /// the observed process state, so unlike a snapshot it computes no digest
    /// and touches no file.
    pub(crate) fn running_plugins(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .pids
            .keys()
            .chain(self.running.iter().filter(|(_, up)| **up).map(|(id, _)| id))
            .cloned()
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// A digest for display and provider selection, reused while nothing in
    /// the package has visibly changed.
    ///
    /// Hashing a package reads every file in it, which must not happen on each
    /// status broadcast or route lookup while input is being forwarded on the
    /// same thread. The cache is keyed by a fingerprint of file sizes and
    /// modification times, a few `stat` calls. A stale value is harmless: it
    /// can only make the interface show a change one step late. It is never
    /// used to *allow* a launch: [`PluginRegistry::launch_plan`], approval and
    /// the supervisor all recompute the digest from the files.
    fn cached_digest(&self, plugin: &Discovered) -> Result<String, String> {
        let fingerprint = Self::fingerprint(plugin);
        let id = plugin.manifest.id.clone();
        if let Some(cached) = self.digests.borrow().get(&id) {
            if cached.fingerprint == fingerprint {
                return cached.digest.clone();
            }
        }
        let digest = Self::digest_of(plugin);
        self.digests.borrow_mut().insert(
            id,
            CachedDigest {
                fingerprint,
                digest: digest.clone(),
            },
        );
        digest
    }

    /// Sizes and modification times of everything a digest covers, at the top
    /// level of each resource. Cheap; not a proof of sameness.
    fn fingerprint(
        plugin: &Discovered,
    ) -> Vec<(PathBuf, Option<u64>, Option<std::time::SystemTime>)> {
        let manifest_path = Self::manifest_path_of(plugin);
        let root = manifest_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let mut paths = vec![manifest_path, plugin.executable.clone()];
        paths.extend(
            plugin
                .manifest
                .resources
                .iter()
                .map(|resource| root.join(resource)),
        );
        paths
            .into_iter()
            .map(|path| {
                let metadata = std::fs::metadata(&path).ok();
                (
                    path,
                    metadata.as_ref().map(std::fs::Metadata::len),
                    metadata.and_then(|metadata| metadata.modified().ok()),
                )
            })
            .collect()
    }

    fn trust_now(&self, plugin: &Discovered) -> (PluginTrust, String) {
        if Self::is_bundled(plugin) {
            return (PluginTrust::Bundled, String::new());
        }
        let digest = self.cached_digest(plugin).unwrap_or_default();
        (
            self.trust.trust_of(&plugin.manifest.id, &digest, false),
            digest,
        )
    }

    /// Whether the user has this plugin switched on.
    ///
    /// A bundled plugin is on until the user turns it off. A third-party one
    /// is off until the user turns it on, so approving or installing it never
    /// starts it by itself. Unknown ids are reported as off, so a stale or
    /// hostile client cannot make the daemon launch something it never
    /// discovered.
    pub(crate) fn is_enabled(&self, id: &str) -> bool {
        let Some(plugin) = self.find(id) else {
            return false;
        };
        self.enabled
            .get(id)
            .copied()
            .unwrap_or(Self::is_bundled(plugin))
    }

    /// Records a user preference. Returns `false` for an unknown id.
    pub(crate) fn set_enabled(&mut self, id: &str, enabled: bool) -> bool {
        if self.find(id).is_none() {
            return false;
        }
        self.enabled.insert(id.to_owned(), enabled);
        self.pending.remove(id);
        if !enabled {
            self.running.insert(id.to_owned(), false);
        }
        self.save_state(id);
        true
    }

    // ----- settings --------------------------------------------------------

    /// The settings of a plugin with the value in force for each, in the order
    /// the manifest declared them. What a plugin is told when it starts.
    pub(crate) fn settings_of(&self, id: &str) -> Vec<(String, String)> {
        let Some(plugin) = self.find(id) else {
            return Vec::new();
        };
        plugin
            .manifest
            .settings
            .iter()
            .map(|decl| {
                let chosen = self.chosen.get(id).and_then(|values| values.get(&decl.key));
                (decl.key.clone(), decl.effective(chosen))
            })
            .collect()
    }

    /// Records the user's choice for one setting. Returns `false` when the
    /// plugin or the key is unknown, or the value is not acceptable for it:
    /// what is stored goes back to the plugin, so only valid values are kept.
    pub(crate) fn set_setting(&mut self, id: &str, key: &str, value: &str) -> bool {
        let accepted = self
            .find(id)
            .and_then(|plugin| plugin.manifest.settings.iter().find(|decl| decl.key == key))
            .is_some_and(|decl| decl.accepts(value));
        if accepted {
            self.chosen
                .entry(id.to_owned())
                .or_default()
                .insert(key.to_owned(), value.to_owned());
            self.save_state(id);
        }
        accepted
    }

    /// Whether `id` keeps its own settings, so a change goes to the process.
    pub(crate) fn owns_settings(&self, id: &str) -> bool {
        self.owned.contains_key(id)
            || self
                .plugins
                .iter()
                .any(|plugin| plugin.manifest.id == id && plugin.manifest.owns_settings)
    }

    /// Shows a requested value at once; the plugin's next declaration, which
    /// it sends when it takes or refuses the change, replaces it. Returns
    /// whether `key` is one the plugin declared.
    pub(crate) fn request_owned_setting(&mut self, id: &str, key: &str, value: &str) -> bool {
        let Some(control) = self
            .owned
            .get_mut(id)
            .and_then(|settings| settings.iter_mut().find(|control| control.key == key))
        else {
            return false;
        };
        control.value = value.to_owned();
        true
    }

    /// Takes what a running plugin says its settings are now.
    pub(crate) fn adopt_owned_settings(
        &mut self,
        id: &str,
        settings: Vec<syntra_plugin_api::SettingControl>,
    ) {
        if self.find(id).is_some() {
            self.owned.insert(id.to_owned(), settings);
        }
    }

    fn save_state(&self, id: &str) {
        let Some(directory) = &self.state_dir else {
            return;
        };
        let state = SavedState {
            disabled: self.enabled.get(id) == Some(&false),
            enabled: self.enabled.get(id) == Some(&true),
            values: self.chosen.get(id).cloned().unwrap_or_default(),
        };
        if let Err(reason) = write_state(directory, id, &state) {
            log::warn!("could not save the settings of plugin {id}: {reason}");
        }
    }

    // ----- launching -------------------------------------------------------

    /// Decides whether `id` may start, and if so exactly how.
    ///
    /// The content digest is recomputed here, on every call, so approval is
    /// re-verified at each start, restart and relaunch rather than remembered.
    /// `role` is the role the process is being started for, when it is started
    /// for one.
    pub(crate) fn launch_plan(
        &self,
        id: &str,
        role: Option<ManifestRole>,
    ) -> Result<LaunchPlan, Refusal> {
        if let Some(entry) = self
            .unsupported
            .iter()
            .find(|entry| entry.id.as_deref() == Some(id))
        {
            if self.find(id).is_none() {
                return Err(Refusal::Unsupported(entry.reason.clone()));
            }
        }
        let plugin = self.find(id).ok_or(Refusal::Unknown)?;
        if !self.is_enabled(id) {
            return Err(Refusal::Disabled);
        }
        if !plugin.executable.is_file() {
            return Err(Refusal::NotInstalled);
        }
        let digest = if Self::is_bundled(plugin) {
            String::new()
        } else {
            let digest = Self::digest_of(plugin).map_err(Refusal::Unreadable)?;
            match self.trust.trust_of(id, &digest, false) {
                PluginTrust::Approved => {}
                PluginTrust::ChangedSinceApproval => return Err(Refusal::Changed),
                _ => return Err(Refusal::NotApproved),
            }
            digest
        };
        if let Some(wanted) = role {
            if !plugin.manifest.capabilities.fills(wanted) {
                return Err(Refusal::Unsupported(
                    "the plugin does not declare the capability for this role".into(),
                ));
            }
        }
        Ok(LaunchPlan {
            spec: LaunchSpec {
                executable: plugin.executable.clone(),
                args: plugin.manifest.argv().to_vec(),
                approved: plugin.manifest.capabilities.clone(),
                handshake_ids: plugin.manifest.handshake_ids.clone(),
                role,
                digest,
                integrity: (!Self::is_bundled(plugin)).then(|| Integrity {
                    manifest: plugin.manifest.clone(),
                    manifest_path: Self::manifest_path_of(plugin),
                }),
            },
            settings: self.settings_of(id),
        })
    }

    /// Absolute path of a plugin's executable, whether or not it exists.
    pub(crate) fn executable(&self, id: &str) -> Option<PathBuf> {
        self.find(id).map(|plugin| plugin.executable.clone())
    }

    /// The executable of an installed plugin.
    pub(crate) fn executable_of(&self, id: &str) -> Option<PathBuf> {
        self.executable(id).filter(|path| path.is_file())
    }

    // ----- roles -----------------------------------------------------------

    fn candidates(&self, role: ManifestRole) -> Vec<Candidate> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.manifest.capabilities.fills(role))
            .map(|plugin| {
                let id = plugin.manifest.id.as_str();
                let (trust, _) = self.trust_now(plugin);
                Candidate {
                    id: id.to_owned(),
                    bundled: Self::is_bundled(plugin),
                    enabled: self.is_enabled(id),
                    trusted: matches!(trust, PluginTrust::Bundled | PluginTrust::Approved),
                    launchable: plugin.executable.is_file(),
                }
            })
            .collect()
    }

    /// The plugin that provides `role` now, chosen deterministically from the
    /// eligible ones: the user's explicit preference, else a bundled provider,
    /// else the smallest id. `None` when nothing can: that capability is then
    /// unavailable, which never prevents input sharing.
    pub(crate) fn active_provider(&self, role: ManifestRole) -> Option<String> {
        let candidates = self.candidates(role);
        let preferred = self.trust.preferred(client_role(role));
        routing::select_provider(&candidates, preferred).map(|candidate| candidate.id.clone())
    }

    /// Records the user's choice of provider for `role`. Refused when the
    /// plugin cannot fill the role or the choice cannot be saved.
    pub(crate) fn set_role_preference(
        &mut self,
        role: syntra_api::PluginRole,
        id: Option<&str>,
    ) -> Result<(), String> {
        if let Some(id) = id {
            let fills = self
                .find(id)
                .is_some_and(|plugin| plugin.manifest.capabilities.fills(manifest_role(role)));
            if !fills {
                return Err("that plugin does not provide this role".into());
            }
        }
        self.trust
            .set_preferred(role, id)
            .map_err(|error| error.to_string())
    }

    /// Ids of every plugin that should be running as a resident process now:
    /// the active provider of each resident role, and the pointer plugins that
    /// are enabled and allowed. A role nobody can fill contributes nothing.
    pub(crate) fn resident_plugins(&self) -> Vec<(String, ManifestRole)> {
        let mut wanted: Vec<(String, ManifestRole)> = Vec::new();
        if let Some(id) = self.active_provider(ManifestRole::FileClipboard) {
            wanted.push((id, ManifestRole::FileClipboard));
        }
        // Every eligible pointer plugin runs: several can listen at once.
        for candidate in self.candidates(ManifestRole::Pointer) {
            if candidate.eligible() && !wanted.iter().any(|(id, _)| *id == candidate.id) {
                wanted.push((candidate.id, ManifestRole::Pointer));
            }
        }
        wanted
    }

    /// Ids of every enabled, allowed plugin that wants pointer events.
    pub(crate) fn pointer_plugins(&self) -> Vec<String> {
        self.candidates(ManifestRole::Pointer)
            .into_iter()
            .filter(Candidate::eligible)
            .map(|candidate| candidate.id)
            .collect()
    }

    /// Whether `id` is a discovered plugin that wants pointer events.
    pub(crate) fn listens_to_pointer(&self, id: &str) -> bool {
        self.find(id)
            .is_some_and(|plugin| plugin.manifest.capabilities.pointer_events)
    }

    /// Whether the plugin only runs while something needs it.
    pub(crate) fn is_on_demand(&self, id: &str) -> bool {
        self.find(id)
            .is_some_and(|plugin| plugin.manifest.on_demand)
    }

    // ----- approval and lifecycle -----------------------------------------

    /// Approves the digest the user was shown for `id`.
    pub(crate) fn approve(&mut self, id: &str, shown: &str) -> Result<(), String> {
        let plugin = self.find(id).cloned().ok_or("the plugin is not known")?;
        if Self::is_bundled(&plugin) {
            return Err("a bundled plugin needs no approval".into());
        }
        let current =
            Self::digest_of(&plugin).map_err(|why| format!("cannot be checked: {why}"))?;
        let result = self
            .trust
            .approve(id, shown, &current)
            .map_err(|error: TrustError| error.to_string());
        self.note_result(id, &result);
        result
    }

    /// Withdraws an approval.
    pub(crate) fn revoke(&mut self, id: &str) -> Result<(), String> {
        let result = self.trust.revoke(id).map_err(|error| error.to_string());
        self.note_result(id, &result);
        result
    }

    /// Records the outcome of a lifecycle request: clears the pending marker,
    /// and keeps the reason when it was refused.
    pub(crate) fn note_result(&mut self, id: &str, result: &Result<(), String>) {
        self.pending.remove(id);
        match result {
            Ok(()) => {
                self.action_errors.remove(id);
            }
            Err(reason) => {
                self.action_errors.insert(id.to_owned(), reason.clone());
            }
        }
    }

    /// Marks a request as made but not yet applied.
    pub(crate) fn mark_pending(&mut self, id: &str, pending: PluginPending) {
        self.pending.insert(id.to_owned(), pending);
    }

    /// Reads and digests a package without running it, for the user to review,
    /// and remembers the result so it is listed as a candidate.
    #[cfg(test)]
    pub(crate) fn review(&mut self, path: &Path) -> PluginReview {
        let review = inspect_package(path);
        self.record_review(review)
    }

    /// Remembers a review so it is listed as a candidate, noting whether it
    /// would replace a plugin that already exists.
    pub(crate) fn record_review(&mut self, mut review: PluginReview) -> PluginReview {
        review.replaces_existing = !review.id.is_empty() && self.find(&review.id).is_some();
        self.reviews.retain(|existing| existing.path != review.path);
        self.reviews.push(review.clone());
        if self.reviews.len() > 8 {
            self.reviews.remove(0);
        }
        review
    }

    /// The resident role a plugin should be started for: the one it is the
    /// active provider of, or the pointer role when it is a pointer plugin.
    /// `None` when it provides no resident role right now.
    pub(crate) fn resident_role_of(&self, id: &str) -> Option<ManifestRole> {
        self.resident_plugins()
            .into_iter()
            .find(|(wanted, _)| wanted == id)
            .map(|(_, role)| role)
    }

    /// Drops a candidate that was installed, so it is no longer listed as
    /// something to review.
    pub(crate) fn forget_review(&mut self, id: &str) {
        self.reviews.retain(|review| review.id != id);
    }
}

/// Reads and digests a package without running it.
///
/// A free function holding no registry state, so it can run on a blocking
/// thread while the service loop keeps forwarding input.
pub(crate) fn inspect_package(path: &Path) -> PluginReview {
    let manifest_path = if path.is_dir() {
        path.join("syntra-plugin.json")
    } else {
        path.to_path_buf()
    };
    let mut review = PluginReview {
        path: path.display().to_string(),
        valid: false,
        error: None,
        unsupported: false,
        id: String::new(),
        name: String::new(),
        version: String::new(),
        author: String::new(),
        source: None,
        license: None,
        executable: String::new(),
        args: Vec::new(),
        mime_types: Vec::new(),
        roles: Vec::new(),
        pointer_events: false,
        requires_live_mount: false,
        digest: String::new(),
        files: Vec::new(),
        replaces_existing: false,
    };
    match discovery::read_manifest(&manifest_path, PluginOrigin::DropIn) {
        Err(error) => review.error = Some(error),
        Ok(discovery::Read::Unsupported(entry)) => {
            review.unsupported = true;
            review.error = Some(entry.reason);
            review.id = entry.id.unwrap_or_default();
            review.name = entry.name.unwrap_or_default();
        }
        Ok(discovery::Read::Plugin(found)) => {
            let manifest = &found.manifest;
            review.id = manifest.id.clone();
            review.name = manifest.name.clone();
            review.version = manifest.version.clone();
            review.author = manifest.author.clone();
            review.source = manifest.source.clone();
            review.license = manifest.license.clone();
            review.executable = found.executable.display().to_string();
            review.args = manifest.argv().to_vec();
            review.mime_types = manifest.capabilities.mime_types.clone();
            review.roles = manifest.roles().into_iter().map(client_role).collect();
            review.pointer_events = manifest.capabilities.pointer_events;
            review.requires_live_mount = manifest.capabilities.requires_live_mount;
            match digest_package(manifest, &manifest_path, &found.executable) {
                Ok(digest) => {
                    review.valid = true;
                    review.digest = digest.digest;
                    review.files = digest.labels;
                }
                Err(error) => review.error = Some(error.to_string()),
            }
        }
    }
    review
}

impl PluginRegistry {
    /// Whether no plugin with this id is known, so a failed request about it
    /// has no entry to carry its reason.
    pub(crate) fn is_unknown(&self, id: &str) -> bool {
        self.find(id).is_none()
    }

    /// Re-reads the directories, keeping what the registry knows about
    /// processes and choices for ids that still exist.
    pub(crate) fn rediscover(&mut self, daemon_executable: &Path, drop_in: Option<&Path>) {
        let directory = daemon_executable
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let found = discovery::discover(&directory, drop_in, self.store.as_deref());
        self.plugins = found.plugins;
        self.unsupported = found.unsupported;
        for plugin in &self.plugins {
            if let Some(directory) = &self.state_dir {
                let id = &plugin.manifest.id;
                if !self.chosen.contains_key(id) {
                    if let Some(saved) = read_state(directory, id) {
                        if !saved.values.is_empty() {
                            self.chosen.insert(id.clone(), saved.values);
                        }
                    }
                }
            }
        }
    }

    /// The directory Syntra installs packages into.
    pub(crate) fn store(&self) -> Option<&Path> {
        self.store.as_deref()
    }

    /// Whether `id` is a package Syntra installed itself, which only Syntra
    /// may uninstall.
    pub(crate) fn is_installed_package(&self, id: &str) -> bool {
        self.find(id)
            .is_some_and(|plugin| plugin.origin == PluginOrigin::Installed)
    }

    /// Forgets a plugin after its package was removed: approval, preference,
    /// and every observation. Saved settings are kept so a reinstall finds
    /// them again.
    pub(crate) fn forget(&mut self, id: &str) -> Result<(), String> {
        self.trust.forget(id).map_err(|error| error.to_string())?;
        self.running.remove(id);
        self.starting.remove(id);
        self.restarts.remove(id);
        self.failures.remove(id);
        self.pids.remove(id);
        self.owned.remove(id);
        self.declared.remove(id);
        self.pending.remove(id);
        self.diagnostics.remove(id);
        self.enabled.remove(id);
        Ok(())
    }

    // ----- observed process state -----------------------------------------

    /// Whether the plugin's process has handshaken and is running.
    pub(crate) fn is_running(&self, id: &str) -> bool {
        self.running.get(id).copied().unwrap_or(false)
    }

    /// Records that a plugin's process has become ready, or has stopped.
    pub(crate) fn set_running(&mut self, id: &str, running: bool) {
        self.running.insert(id.to_owned(), running);
        if running {
            self.starting.remove(id);
            self.failures.remove(id);
        }
    }

    /// Records that a plugin's process has been launched, with its pid.
    ///
    /// Only the supervisor calls this, and only once a process genuinely
    /// exists. Health is derived from observed processes, never from an
    /// intention to start one.
    pub(crate) fn set_starting(&mut self, id: &str, pid: u32) {
        self.starting.insert(id.to_owned(), Instant::now());
        self.running.insert(id.to_owned(), false);
        self.pids.insert(id.to_owned(), pid);
    }

    /// Records that a plugin stopped because it was asked to. Clears the
    /// process state without a failure, so the interface shows a stopped
    /// plugin rather than a broken one.
    pub(crate) fn set_stopped(&mut self, id: &str) {
        self.running.insert(id.to_owned(), false);
        self.starting.remove(id);
        self.failures.remove(id);
        self.pids.remove(id);
    }

    /// Replaces a plugin's displayed description with what the plugin itself
    /// declared at its handshake. A manifest file is only how a plugin is
    /// discovered; what the process says is what the user sees. It is shown
    /// and never trusted: it cannot change the plugin's provenance, its
    /// approved capabilities or its identity.
    pub(crate) fn adopt_declared(&mut self, id: &str, declared: PluginMetadata) {
        if self.find(id).is_some() {
            self.declared.insert(id.to_owned(), declared);
        }
    }

    /// Records a plugin's recent diagnostic lines.
    pub(crate) fn set_diagnostics(&mut self, id: &str, lines: Vec<String>) {
        if self.find(id).is_some() {
            self.diagnostics.insert(id.to_owned(), lines);
        }
    }

    /// Records that a plugin's process ended.
    ///
    /// For an on-demand plugin an exit is the normal end of its work, not a
    /// fault: the FUSE helper serves one transfer and leaves. Recording that
    /// as a failure left a red state and a "Starting" badge that nothing could
    /// ever clear, because nothing relaunches it until the next transfer.
    pub(crate) fn set_exited(&mut self, id: &str, reason: impl Into<String>) {
        if self.is_on_demand(id) {
            self.set_stopped(id);
            return;
        }
        self.set_failed(id, reason);
    }

    /// Records a failure reported for a plugin.
    pub(crate) fn set_failed(&mut self, id: &str, reason: impl Into<String>) {
        self.failures.insert(id.to_owned(), reason.into());
        self.running.insert(id.to_owned(), false);
        self.starting.remove(id);
        // The process is gone: keeping its pid would advertise a dead one.
        self.pids.remove(id);
    }

    /// Counts a restart requested by a client, used to spot a crash loop.
    ///
    /// The pid is cleared rather than kept: the old process is on its way out
    /// and the supervisor reports the new one when it launches, so showing the
    /// previous pid meanwhile would be a lie. This records only that the old
    /// process is going away. It must NOT claim the plugin is starting: an
    /// on-demand plugin is not relaunched until something needs it, and
    /// marking an intention as a state left it reading "Starting" for ever
    /// with nothing able to clear the flag.
    pub(crate) fn record_restart(&mut self, id: &str) {
        *self.restarts.entry(id.to_owned()).or_default() += 1;
        self.starting.remove(id);
        self.running.insert(id.to_owned(), false);
        self.pids.remove(id);
        self.failures.remove(id);
    }

    // ----- presentation ----------------------------------------------------

    /// Health of one plugin, derived from its connection to the daemon.
    ///
    /// `Healthy` means the plugin completed its handshake over the stdio
    /// contract and is answering. It is never inferred from an executable
    /// existing on disk: a file that is present but never connects is not a
    /// working plugin, and reporting it as one is exactly the lie this state
    /// machine exists to prevent. `installed` only distinguishes "declared but
    /// absent" from "declared and launchable".
    fn health(&self, id: &str, installed: bool, enabled: bool, on_demand: bool) -> PluginHealth {
        if !enabled {
            return PluginHealth::Disabled;
        }
        if self.failures.contains_key(id) {
            return PluginHealth::Failed;
        }
        // Answering the handshake is the only evidence of health.
        if self.running.get(id).copied().unwrap_or(false) {
            let restarts = self.restarts.get(id).copied().unwrap_or(0);
            // A plugin that keeps coming back, but does not work.
            if restarts >= CRASH_LOOP_RESTARTS {
                return PluginHealth::Unresponsive;
            }
            return PluginHealth::Healthy;
        }
        if let Some(since) = self.starting.get(id) {
            // A launch that never handshakes must not stay pending for ever;
            // past the deadline the process exists but is not answering.
            return if since.elapsed() < HANDSHAKE_TIMEOUT {
                PluginHealth::Starting
            } else {
                PluginHealth::Unresponsive
            };
        }
        if !installed {
            return PluginHealth::NotInstalled;
        }
        if on_demand {
            return PluginHealth::OnDemand;
        }
        PluginHealth::Stopped
    }

    /// Where the plugin is in its life, in the shared vocabulary.
    fn state(
        &self,
        id: &str,
        trust: PluginTrust,
        installed: bool,
        enabled: bool,
        health: PluginHealth,
        on_demand: bool,
    ) -> PluginState {
        if !installed {
            return PluginState::Unsupported;
        }
        if matches!(health, PluginHealth::Failed | PluginHealth::Unresponsive) {
            return PluginState::Failed;
        }
        match health {
            PluginHealth::Starting => return PluginState::Starting,
            PluginHealth::Healthy => {
                return if on_demand {
                    PluginState::Running
                } else {
                    PluginState::Ready
                };
            }
            _ => {}
        }
        if !matches!(trust, PluginTrust::Bundled | PluginTrust::Approved) {
            return PluginState::Installed;
        }
        if enabled {
            PluginState::Enabled
        } else {
            PluginState::Approved
        }
    }

    fn owned_status(control: &syntra_plugin_api::SettingControl) -> syntra_api::PluginSetting {
        use syntra_api::{PluginChoice, PluginSetting, PluginSettingKind};
        use syntra_plugin_api::SettingControlKind;
        let kind = match &control.kind {
            SettingControlKind::Toggle => PluginSettingKind::Toggle,
            SettingControlKind::Color => PluginSettingKind::Color,
            SettingControlKind::Choice { options } => PluginSettingKind::Choice(
                options
                    .iter()
                    .map(|option| PluginChoice {
                        value: option.value.clone(),
                        label: option.label.clone(),
                    })
                    .collect(),
            ),
            SettingControlKind::Number { min, max } => PluginSettingKind::Number {
                min: *min,
                max: *max,
            },
        };
        PluginSetting {
            key: control.key.clone(),
            label: control.label.clone(),
            kind,
            default: control.default.clone(),
            value: control.value.clone(),
        }
    }

    fn setting_status(&self, id: &str, decl: &SettingDecl) -> syntra_api::PluginSetting {
        use syntra_api::{PluginChoice, PluginSetting, PluginSettingKind};
        let kind = match decl.kind.as_str() {
            "choice" => PluginSettingKind::Choice(
                decl.options
                    .iter()
                    .map(|option| PluginChoice {
                        value: option.value.clone(),
                        label: option.label.clone(),
                    })
                    .collect(),
            ),
            "color" => PluginSettingKind::Color,
            "number" => {
                let (min, max) = decl.range();
                PluginSettingKind::Number { min, max }
            }
            _ => PluginSettingKind::Toggle,
        };
        let chosen = self.chosen.get(id).and_then(|values| values.get(&decl.key));
        PluginSetting {
            key: decl.key.clone(),
            label: decl.label.clone(),
            kind,
            default: decl.default.clone(),
            value: decl.effective(chosen),
        }
    }

    fn role_statuses(&self, plugin: &Discovered) -> Vec<PluginRoleStatus> {
        let id = plugin.manifest.id.as_str();
        plugin
            .manifest
            .roles()
            .into_iter()
            .map(|role| {
                let candidates = self.candidates(role);
                let active = self.active_provider(role);
                let preferred = self.trust.preferred(client_role(role));
                let me = candidates.iter().find(|candidate| candidate.id == id);
                PluginRoleStatus {
                    role: client_role(role),
                    active: active.as_deref() == Some(id),
                    preferred: preferred == Some(id),
                    unavailable: me.and_then(|candidate| {
                        routing::unavailable_reason(candidate, active.as_deref(), preferred)
                    }),
                }
            })
            .collect()
    }

    /// The manifest as it is displayed: what the file says, overlaid with what
    /// the running plugin declared about itself.
    fn displayed(&self, plugin: &Discovered) -> DisplayFields {
        let manifest = &plugin.manifest;
        let mut fields = DisplayFields {
            name: manifest.name.clone(),
            description: manifest.description.clone(),
            version: manifest.version.clone(),
            author: manifest.author.clone(),
            homepage: manifest.homepage.clone(),
            source: manifest.source.clone(),
            update_url: manifest.update_url.clone(),
            license: manifest.license.clone(),
            on_demand: manifest.on_demand,
            build_fingerprint: String::new(),
        };
        if let Some(declared) = self.declared.get(&manifest.id) {
            if !declared.description.is_empty() {
                fields.description = declared.description.clone();
            }
            if !declared.version.is_empty() {
                fields.version = declared.version.clone();
            }
            if !declared.author.is_empty() {
                fields.author = declared.author.clone();
            }
            if declared.homepage.is_some() {
                fields.homepage = declared.homepage.clone();
            }
            if declared.source.is_some() {
                fields.source = declared.source.clone();
            }
            if declared.update_url.is_some() {
                fields.update_url = declared.update_url.clone();
            }
            if declared.license.is_some() {
                fields.license = declared.license.clone();
            }
            fields.on_demand = declared.on_demand;
            fields.build_fingerprint = declared.build_fingerprint.clone();
        }
        fields
    }

    /// Snapshot for the interface, in discovery order.
    pub(crate) fn snapshot(&self) -> Vec<PluginStatus> {
        let mut list: Vec<PluginStatus> = self
            .plugins
            .iter()
            .map(|plugin| self.status_of(plugin))
            .collect();
        list.extend(self.unsupported.iter().map(Self::unsupported_status));
        list.extend(self.reviews.iter().map(Self::candidate_status));
        list
    }

    fn status_of(&self, plugin: &Discovered) -> PluginStatus {
        let manifest = &plugin.manifest;
        let id = manifest.id.as_str();
        let installed = plugin.executable.is_file();
        let enabled = self.is_enabled(id);
        let display = self.displayed(plugin);
        let health = self.health(id, installed, enabled, display.on_demand);
        let (trust, digest) = self.trust_now(plugin);
        let state = self.state(id, trust, installed, enabled, health, display.on_demand);
        let running = installed && enabled && self.running.get(id).copied().unwrap_or(false);
        // Why an enabled plugin is not running, when the reason is approval, so
        // the user knows what to do. Derived from the trust already computed
        // above: asking `launch_plan` here would hash the package again.
        let approval_hint = if enabled {
            match trust {
                PluginTrust::Unapproved => Some(Refusal::NotApproved.text()),
                PluginTrust::ChangedSinceApproval => Some(Refusal::Changed.text()),
                PluginTrust::Bundled | PluginTrust::Approved => None,
            }
        } else {
            None
        };
        let error = self
            .action_errors
            .get(id)
            .cloned()
            .or_else(|| self.failures.get(id).cloned())
            .or_else(|| {
                (!installed).then(|| {
                    format!(
                        "{} was not found; the plugin is declared but not installed",
                        plugin.executable.display()
                    )
                })
            })
            .or(approval_hint);
        PluginStatus {
            id: manifest.id.clone(),
            name: display.name,
            description: display.description,
            version: display.version,
            protocol_version: manifest.protocol_version,
            supported_protocol_version: SUPPORTED_PROTOCOL_VERSION,
            health,
            pid: self.pids.get(id).copied(),
            restarts: self.restarts.get(id).copied().unwrap_or(0),
            author: display.author,
            homepage: display.homepage,
            source: display.source,
            update_url: display.update_url,
            license: display.license,
            mime_types: manifest.capabilities.mime_types.clone(),
            bundled: plugin.origin == PluginOrigin::Bundled,
            owns_settings: self.owns_settings(id),
            settings: match self.owned.get(id) {
                Some(owned) => owned.iter().map(Self::owned_status).collect(),
                None => manifest
                    .settings
                    .iter()
                    .map(|decl| self.setting_status(id, decl))
                    .collect(),
            },
            manifest_path: plugin.manifest_path.display().to_string(),
            build_fingerprint: display.build_fingerprint,
            daemon_build_fingerprint: syntra_plugin_api::BUILD_FINGERPRINT.to_owned(),
            executable: plugin.executable.display().to_string(),
            installed,
            enabled,
            running,
            error,
            trust,
            origin: plugin.origin,
            digest,
            approved_digest: self.trust.approved_digest(id).map(str::to_owned),
            args: manifest.argv().to_vec(),
            roles: self.role_statuses(plugin),
            diagnostics: self.diagnostics.get(id).cloned().unwrap_or_default(),
            removable: plugin.origin == PluginOrigin::Installed,
            state,
            pending: self.pending.get(id).copied(),
            review: None,
            action_error: self.action_errors.get(id).cloned(),
        }
    }

    /// A manifest that names a version this daemon does not understand.
    ///
    /// Reported through the legacy health vocabulary as `NotInstalled` (the
    /// closest value an older client knows) with the reason in `error`; the
    /// shared `state` says `Unsupported`.
    fn unsupported_status(entry: &Unsupported) -> PluginStatus {
        PluginStatus {
            id: entry.id.clone().unwrap_or_default(),
            name: entry
                .name
                .clone()
                .unwrap_or_else(|| "Unsupported plugin".into()),
            description: String::new(),
            version: String::new(),
            protocol_version: 0,
            supported_protocol_version: SUPPORTED_PROTOCOL_VERSION,
            health: PluginHealth::NotInstalled,
            pid: None,
            restarts: 0,
            author: String::new(),
            homepage: None,
            source: None,
            update_url: None,
            license: None,
            mime_types: Vec::new(),
            bundled: false,
            owns_settings: false,
            settings: Vec::new(),
            manifest_path: entry.manifest_path.display().to_string(),
            build_fingerprint: String::new(),
            daemon_build_fingerprint: syntra_plugin_api::BUILD_FINGERPRINT.to_owned(),
            executable: String::new(),
            installed: false,
            enabled: false,
            running: false,
            error: Some(entry.reason.clone()),
            trust: PluginTrust::Unapproved,
            origin: entry.origin,
            digest: String::new(),
            approved_digest: None,
            args: Vec::new(),
            roles: Vec::new(),
            diagnostics: Vec::new(),
            removable: false,
            state: PluginState::Unsupported,
            pending: None,
            review: None,
            action_error: None,
        }
    }

    /// A reviewed package that is not installed, shown so the user can decide.
    fn candidate_status(review: &PluginReview) -> PluginStatus {
        let mut status = Self::unsupported_status(&Unsupported {
            id: Some(review.id.clone()).filter(|id| !id.is_empty()),
            name: Some(review.name.clone()).filter(|name| !name.is_empty()),
            origin: PluginOrigin::DropIn,
            manifest_path: PathBuf::from(&review.path),
            reason: review.error.clone().unwrap_or_default(),
        });
        status.version = review.version.clone();
        status.author = review.author.clone();
        status.source = review.source.clone();
        status.license = review.license.clone();
        status.mime_types = review.mime_types.clone();
        status.executable = review.executable.clone();
        status.args = review.args.clone();
        status.digest = review.digest.clone();
        status.error = review.error.clone();
        status.state = if review.unsupported {
            PluginState::Unsupported
        } else {
            PluginState::Installed
        };
        status.review = Some(review.clone());
        status
    }
}

/// The text fields of a plugin as they are displayed.
struct DisplayFields {
    name: String,
    description: String,
    version: String,
    author: String,
    homepage: Option<String>,
    source: Option<String>,
    update_url: Option<String>,
    license: Option<String>,
    on_demand: bool,
    build_fingerprint: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::{CLIPBOARD_PLUGIN_ID, EDGE_GLOW_PLUGIN_ID, FUSE_PLUGIN_ID};
    use std::fs;
    use syntra_api::PluginHealth;

    const THIRD_PARTY: &str = r#"{
        "manifest_version": 1, "protocol_version": 1,
        "id": "acme-files", "name": "Acme files", "executable": "acme",
        "args": ["--serve", "--mode=sync"],
        "version": "1.0.0", "author": "Acme",
        "settings": [
            {"key":"enabled","label":"On","kind":"toggle","default":"true"},
            {"key":"renderer","label":"Drawing","kind":"choice","default":"gl",
             "options":[{"value":"gl","label":"OpenGL"},{"value":"cpu","label":"Software"}]},
            {"key":"width","label":"Width","kind":"number","default":"24","min":4,"max":96}
        ],
        "capabilities": {"clipboard_read": true, "paste": true, "cancel": true,
            "mime_types": ["text/uri-list", "x-special/gnome-copied-files"]}
    }"#;

    /// A daemon directory, a drop-in directory and a registry over them, with
    /// a runnable third-party plugin installed in the drop-in directory.
    struct Fixture {
        root: tempfile::TempDir,
    }

    fn make_executable(path: &Path) {
        fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            fs::create_dir_all(root.path().join("daemon")).unwrap();
            fs::create_dir_all(root.path().join("plugins")).unwrap();
            let fixture = Self { root };
            fixture.write_plugin("acme.json", THIRD_PARTY, "acme");
            fixture
        }

        fn drop_in(&self) -> PathBuf {
            self.root.path().join("plugins")
        }

        fn state(&self) -> PathBuf {
            self.root.path().join("state")
        }

        fn daemon(&self) -> PathBuf {
            self.root.path().join("daemon/syntra-daemon")
        }

        /// Makes a bundled plugin's executable exist beside the daemon.
        fn install_bundled(&self, name: &str) {
            make_executable(&self.root.path().join("daemon").join(name));
        }

        /// Writes a manifest and an executable it names.
        fn write_plugin(&self, manifest: &str, text: &str, executable: &str) {
            fs::write(self.drop_in().join(manifest), text).unwrap();
            make_executable(&self.drop_in().join(executable));
        }

        fn registry(&self) -> PluginRegistry {
            PluginRegistry::discover(&self.daemon(), Some(self.drop_in()), None)
                .with_state_dir(self.state())
        }
    }

    fn entry(registry: &PluginRegistry, id: &str) -> PluginStatus {
        registry
            .snapshot()
            .into_iter()
            .find(|plugin| plugin.id == id)
            .unwrap_or_else(|| panic!("no plugin with id {id}"))
    }

    #[test]
    fn a_third_party_plugin_is_listed_but_stopped_until_its_content_is_approved() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry();
        let listed = entry(&registry, "acme-files");
        assert_eq!(listed.origin, PluginOrigin::DropIn);
        assert_eq!(listed.trust, PluginTrust::Unapproved);
        assert!(!listed.enabled, "a third-party plugin starts switched off");
        assert!(!listed.digest.is_empty());

        // Switching it on does not run it: it is not approved.
        registry.set_enabled("acme-files", true);
        assert_eq!(
            registry.launch_plan("acme-files", None).unwrap_err(),
            Refusal::NotApproved
        );

        registry.approve("acme-files", &listed.digest).unwrap();
        let plan = registry.launch_plan("acme-files", None).unwrap();
        assert_eq!(plan.spec.args, ["--serve", "--mode=sync"]);
        assert!(plan.spec.executable.ends_with("acme"));
        assert_eq!(entry(&registry, "acme-files").trust, PluginTrust::Approved);
    }

    #[test]
    fn approving_a_digest_that_is_not_the_one_on_disk_is_refused() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry();
        assert!(registry.approve("acme-files", "0123abcd").is_err());
        assert_eq!(
            entry(&registry, "acme-files").trust,
            PluginTrust::Unapproved
        );
    }

    /// Approval is bound to content: changing the executable, the arguments or
    /// the declared capabilities after approval stops the next launch.
    #[test]
    fn changed_content_needs_a_fresh_approval_on_every_launch() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry();
        let shown = entry(&registry, "acme-files").digest;
        registry.set_enabled("acme-files", true);
        registry.approve("acme-files", &shown).unwrap();
        assert!(registry.launch_plan("acme-files", None).is_ok());

        // The executable changes under the registry's feet.
        fs::write(fixture.drop_in().join("acme"), "#!/bin/sh\necho changed\n").unwrap();
        assert_eq!(
            registry.launch_plan("acme-files", None).unwrap_err(),
            Refusal::Changed,
            "the digest is recomputed at launch, not remembered from approval"
        );
        let status = entry(&registry, "acme-files");
        assert_eq!(status.trust, PluginTrust::ChangedSinceApproval);
        assert_eq!(status.approved_digest.as_deref(), Some(shown.as_str()));
        assert_ne!(status.digest, shown);

        // A new approval of the new digest runs it again.
        registry.approve("acme-files", &status.digest).unwrap();
        assert!(registry.launch_plan("acme-files", None).is_ok());
    }

    #[test]
    fn changed_arguments_or_capabilities_also_change_the_digest() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry();
        let shown = entry(&registry, "acme-files").digest;
        registry.set_enabled("acme-files", true);
        registry.approve("acme-files", &shown).unwrap();

        fixture.write_plugin(
            "acme.json",
            &THIRD_PARTY.replace("--mode=sync", "--mode=other"),
            "acme",
        );
        let mut reread = fixture.registry();
        reread.set_enabled("acme-files", true);
        assert_eq!(
            reread.launch_plan("acme-files", None).unwrap_err(),
            Refusal::Changed
        );

        let widened = THIRD_PARTY.replace(
            "\"cancel\": true,",
            "\"cancel\": true, \"requires_live_mount\": true,",
        );
        fixture.write_plugin("acme.json", &widened, "acme");
        let mut wider = fixture.registry();
        wider.set_enabled("acme-files", true);
        assert_eq!(
            wider.launch_plan("acme-files", None).unwrap_err(),
            Refusal::Changed
        );
    }

    #[test]
    fn approvals_and_enabled_state_survive_a_restart() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry();
        let shown = entry(&registry, "acme-files").digest;
        registry.set_enabled("acme-files", true);
        registry.approve("acme-files", &shown).unwrap();
        drop(registry);

        let reborn = fixture.registry();
        assert!(reborn.is_enabled("acme-files"));
        assert!(reborn.launch_plan("acme-files", None).is_ok());
    }

    #[test]
    fn a_disabled_plugin_does_not_launch_and_disabling_is_remembered() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry();
        let shown = entry(&registry, "acme-files").digest;
        registry.set_enabled("acme-files", true);
        registry.approve("acme-files", &shown).unwrap();
        registry.set_enabled("acme-files", false);
        assert_eq!(
            registry.launch_plan("acme-files", None).unwrap_err(),
            Refusal::Disabled
        );
        let reborn = fixture.registry();
        assert!(!reborn.is_enabled("acme-files"));
    }

    #[test]
    fn bundled_plugins_need_no_approval_but_cannot_launch_without_an_executable() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        for id in [CLIPBOARD_PLUGIN_ID, FUSE_PLUGIN_ID, EDGE_GLOW_PLUGIN_ID] {
            let listed = entry(&registry, id);
            assert_eq!(listed.trust, PluginTrust::Bundled, "{id}");
            assert_eq!(listed.origin, PluginOrigin::Bundled, "{id}");
            assert!(listed.bundled && !listed.removable, "{id}");
            // Nothing was built beside the daemon in this fixture.
            assert_eq!(
                registry.launch_plan(id, None).unwrap_err(),
                Refusal::NotInstalled,
                "{id}"
            );
            assert!(!listed.installed && listed.state == PluginState::Unsupported);
        }
    }

    #[test]
    fn bundled_clipboard_consumes_mounts_but_fuse_provides_them() {
        let fixture = Fixture::new();
        let clipboard_exe = if cfg!(windows) {
            "syntra-plugin-clipboard.exe"
        } else {
            "syntra-plugin-clipboard"
        };
        let fuse_exe = if cfg!(windows) {
            "syntra-plugin-fuse.exe"
        } else {
            "syntra-plugin-fuse"
        };
        fixture.install_bundled(clipboard_exe);
        fixture.install_bundled(fuse_exe);
        let registry = fixture.registry();
        assert_eq!(
            registry
                .active_provider(ManifestRole::FileClipboard)
                .as_deref(),
            Some(CLIPBOARD_PLUGIN_ID)
        );
        assert_eq!(
            registry
                .active_provider(ManifestRole::MountedFileReceive)
                .as_deref(),
            Some(FUSE_PLUGIN_ID)
        );
        assert!(
            registry
                .launch_plan(CLIPBOARD_PLUGIN_ID, Some(ManifestRole::MountedFileReceive))
                .is_err()
        );
        let mount = registry
            .launch_plan(FUSE_PLUGIN_ID, Some(ManifestRole::MountedFileReceive))
            .unwrap();
        assert!(mount.spec.executable.ends_with(fuse_exe));
    }

    #[test]
    fn an_installed_bundled_plugin_launches_with_the_descriptors_settings() {
        let fixture = Fixture::new();
        fixture.install_bundled("syntra-plugin-edge-glow");
        let registry = fixture.registry();
        let plan = registry.launch_plan(EDGE_GLOW_PLUGIN_ID, None).unwrap();
        assert!(plan.spec.executable.ends_with("syntra-plugin-edge-glow"));
        assert!(plan.spec.approved.pointer_events);
        assert!(
            !plan.settings.is_empty(),
            "the descriptor's settings are sent"
        );
    }

    /// A manifest cannot make itself bundled, and an executable that replaces a
    /// bundled id is third-party and needs its own approval.
    #[test]
    fn a_replacement_for_a_bundled_id_is_third_party() {
        let fixture = Fixture::new();
        fixture.write_plugin(
            "fuse.json",
            &THIRD_PARTY
                .replace("acme-files", "fuse")
                .replace("\"id\"", "\"bundled\": true, \"id\""),
            "acme",
        );
        let mut registry = fixture.registry();
        let listed = entry(&registry, "fuse");
        assert_eq!(listed.origin, PluginOrigin::DropIn);
        assert_eq!(listed.trust, PluginTrust::Unapproved);
        assert!(!listed.removable);
        registry.set_enabled("fuse", true);
        assert_eq!(
            registry.launch_plan("fuse", None).unwrap_err(),
            Refusal::NotApproved
        );
    }

    #[test]
    fn the_default_provider_is_bundled_and_a_preference_overrides_it() {
        let fixture = Fixture::new();
        fixture.install_bundled("syntra-plugin-clipboard");
        let mut registry = fixture.registry();
        let shown = entry(&registry, "acme-files").digest;
        registry.set_enabled("acme-files", true);
        registry.approve("acme-files", &shown).unwrap();

        assert_eq!(
            registry
                .active_provider(ManifestRole::FileClipboard)
                .as_deref(),
            Some(CLIPBOARD_PLUGIN_ID),
            "the bundled provider is the default"
        );
        registry
            .set_role_preference(syntra_api::PluginRole::FileClipboard, Some("acme-files"))
            .unwrap();
        assert_eq!(
            registry
                .active_provider(ManifestRole::FileClipboard)
                .as_deref(),
            Some("acme-files")
        );
        let roles = entry(&registry, "acme-files").roles;
        assert!(roles.iter().any(|role| role.active && role.preferred));

        registry
            .set_role_preference(syntra_api::PluginRole::FileClipboard, None)
            .unwrap();
        assert_eq!(
            registry
                .active_provider(ManifestRole::FileClipboard)
                .as_deref(),
            Some(CLIPBOARD_PLUGIN_ID)
        );
    }

    #[test]
    fn no_provider_for_a_role_is_a_normal_state() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        // Nothing is approved or built: no provider for any role.
        assert_eq!(registry.active_provider(ManifestRole::FileClipboard), None);
        assert_eq!(
            registry.active_provider(ManifestRole::MountedFileReceive),
            None
        );
        assert!(registry.resident_plugins().is_empty());
        assert!(registry.pointer_plugins().is_empty());
    }

    #[test]
    fn a_preference_for_a_plugin_that_cannot_fill_the_role_is_refused() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry();
        assert!(
            registry
                .set_role_preference(syntra_api::PluginRole::Pointer, Some("acme-files"))
                .is_err()
        );
        assert!(
            registry
                .set_role_preference(syntra_api::PluginRole::Pointer, Some("nobody"))
                .is_err()
        );
    }

    #[test]
    fn unknown_plugins_are_neither_enabled_nor_launchable() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry();
        assert!(!registry.is_enabled("something-else"));
        assert!(!registry.set_enabled("something-else", true));
        assert_eq!(
            registry.launch_plan("something-else", None).unwrap_err(),
            Refusal::Unknown
        );
    }

    #[test]
    fn a_future_manifest_is_listed_as_unsupported_and_never_launched() {
        let fixture = Fixture::new();
        fixture.write_plugin(
            "future.json",
            &THIRD_PARTY
                .replace("acme-files", "from-the-future")
                .replace("\"manifest_version\": 1", "\"manifest_version\": 9"),
            "acme",
        );
        let registry = fixture.registry();
        let listed = entry(&registry, "from-the-future");
        assert_eq!(listed.state, PluginState::Unsupported);
        assert_eq!(listed.health, PluginHealth::NotInstalled);
        assert!(
            listed
                .error
                .as_deref()
                .unwrap()
                .contains("manifest version 9")
        );
        assert!(matches!(
            registry.launch_plan("from-the-future", None),
            Err(Refusal::Unsupported(_))
        ));
    }

    #[test]
    fn health_is_derived_from_observed_process_events_only() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry();
        let shown = entry(&registry, "acme-files").digest;
        registry.set_enabled("acme-files", true);
        registry.approve("acme-files", &shown).unwrap();
        assert_eq!(entry(&registry, "acme-files").health, PluginHealth::Stopped);
        assert_eq!(entry(&registry, "acme-files").state, PluginState::Enabled);

        registry.set_starting("acme-files", 4242);
        let starting = entry(&registry, "acme-files");
        assert_eq!(starting.health, PluginHealth::Starting);
        assert_eq!(starting.pid, Some(4242));
        assert_eq!(starting.state, PluginState::Starting);

        registry.set_running("acme-files", true);
        let ready = entry(&registry, "acme-files");
        assert_eq!(ready.health, PluginHealth::Healthy);
        assert_eq!(ready.state, PluginState::Ready);
        assert!(ready.running);

        registry.set_failed("acme-files", "killed by signal 9");
        let failed = entry(&registry, "acme-files");
        assert_eq!(failed.health, PluginHealth::Failed);
        assert_eq!(failed.state, PluginState::Failed);
        assert_eq!(failed.error.as_deref(), Some("killed by signal 9"));
        assert_eq!(failed.pid, None, "a dead process has no pid");
    }

    #[test]
    fn an_executable_alone_is_never_reported_healthy() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let listed = entry(&registry, "acme-files");
        assert!(listed.installed);
        assert!(!listed.running);
        assert_ne!(listed.health, PluginHealth::Healthy);
    }

    #[test]
    fn disabling_clears_the_running_flag() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry();
        registry.set_running("acme-files", true);
        registry.set_enabled("acme-files", false);
        let listed = entry(&registry, "acme-files");
        assert!(!listed.enabled && !listed.running);
        assert_eq!(listed.health, PluginHealth::Disabled);
    }

    #[test]
    fn an_on_demand_plugin_exiting_is_not_a_failure() {
        let fixture = Fixture::new();
        fixture.install_bundled("syntra-plugin-fuse");
        let mut registry = fixture.registry();
        registry.set_starting(FUSE_PLUGIN_ID, 4242);
        registry.set_exited(FUSE_PLUGIN_ID, "exited with status 0");
        let fuse = entry(&registry, FUSE_PLUGIN_ID);
        assert_eq!(fuse.health, PluginHealth::OnDemand);
        assert!(fuse.error.is_none(), "a normal exit is not an error");
        assert!(fuse.pid.is_none());
    }

    #[test]
    fn a_persistent_plugin_exiting_is_reported_as_failed() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry();
        let shown = entry(&registry, "acme-files").digest;
        registry.set_enabled("acme-files", true);
        registry.approve("acme-files", &shown).unwrap();
        registry.set_starting("acme-files", 4242);
        registry.set_running("acme-files", true);
        registry.set_exited("acme-files", "killed by signal 9");
        let listed = entry(&registry, "acme-files");
        assert_eq!(listed.health, PluginHealth::Failed);
        assert_eq!(listed.error.as_deref(), Some("killed by signal 9"));
    }

    #[test]
    fn restarting_an_on_demand_plugin_does_not_leave_it_starting() {
        let fixture = Fixture::new();
        fixture.install_bundled("syntra-plugin-fuse");
        let mut registry = fixture.registry();
        registry.record_restart(FUSE_PLUGIN_ID);
        let fuse = entry(&registry, FUSE_PLUGIN_ID);
        assert_eq!(fuse.health, PluginHealth::OnDemand);
        assert!(fuse.pid.is_none());
    }

    #[test]
    fn the_handshake_description_is_displayed_but_cannot_change_provenance() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry();
        registry.adopt_declared(
            "acme-files",
            PluginMetadata {
                description: "declared by the process".into(),
                version: "9.9.9".into(),
                bundled: true,
                build_fingerprint: "fp".into(),
                ..PluginMetadata::default()
            },
        );
        let listed = entry(&registry, "acme-files");
        assert_eq!(listed.description, "declared by the process");
        assert_eq!(listed.version, "9.9.9");
        assert_eq!(listed.build_fingerprint, "fp");
        // It claimed to be bundled; nothing about trust or origin moved.
        assert!(!listed.bundled);
        assert_eq!(listed.origin, PluginOrigin::DropIn);
        assert_eq!(listed.trust, PluginTrust::Unapproved);
    }

    #[test]
    fn settings_are_typed_validated_and_survive_a_restart() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry();
        assert!(registry.set_setting("acme-files", "renderer", "cpu"));
        assert!(registry.set_setting("acme-files", "width", "40"));
        for (id, key, value) in [
            ("no-such-plugin", "renderer", "cpu"),
            ("acme-files", "no-such-key", "cpu"),
            ("acme-files", "enabled", "yes"),
            ("acme-files", "renderer", "vulkan"),
            ("acme-files", "width", "3"),
            ("acme-files", "width", "97"),
            ("acme-files", "width", ""),
        ] {
            assert!(
                !registry.set_setting(id, key, value),
                "{id} {key} {value:?}"
            );
        }
        assert_eq!(
            registry.settings_of("acme-files"),
            vec![
                ("enabled".to_owned(), "true".to_owned()),
                ("renderer".to_owned(), "cpu".to_owned()),
                ("width".to_owned(), "40".to_owned()),
            ]
        );
        drop(registry);
        let reborn = fixture.registry();
        let renderer = reborn
            .settings_of("acme-files")
            .into_iter()
            .find(|(key, _)| key == "renderer")
            .unwrap()
            .1;
        assert_eq!(renderer, "cpu");
    }

    #[test]
    fn a_damaged_state_file_falls_back_to_the_defaults() {
        let fixture = Fixture::new();
        let plugin_state = fixture.state().join("acme-files");
        fs::create_dir_all(&plugin_state).unwrap();
        for body in [
            "{ not json",
            "[]",
            "",
            r#"{"values":{"renderer":"vulkan","width":"9000"}}"#,
        ] {
            fs::write(plugin_state.join("settings.json"), body).unwrap();
            let registry = fixture.registry();
            let settings = registry.settings_of("acme-files");
            let value = |key: &str| {
                settings
                    .iter()
                    .find(|(name, _)| name == key)
                    .unwrap()
                    .1
                    .clone()
            };
            assert_eq!(value("renderer"), "gl", "{body:?}");
            assert_eq!(value("width"), "24", "{body:?}");
        }
    }

    #[test]
    fn a_manifest_whose_id_climbs_out_is_never_listed_or_written() {
        let fixture = Fixture::new();
        fixture.write_plugin(
            "evil.json",
            &THIRD_PARTY.replace("acme-files", "../../escaped"),
            "acme",
        );
        let mut registry = fixture.registry();
        assert!(
            registry
                .snapshot()
                .iter()
                .all(|plugin| !plugin.id.contains("escaped"))
        );
        assert!(!registry.set_enabled("../../escaped", false));
        assert!(!fixture.root.path().join("escaped").exists());
    }

    #[test]
    fn the_snapshot_carries_the_args_and_a_review_never_installs() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry();
        assert_eq!(
            entry(&registry, "acme-files").args,
            ["--serve", "--mode=sync"]
        );

        let outside = tempfile::tempdir().unwrap();
        fs::write(
            outside.path().join("syntra-plugin.json"),
            THIRD_PARTY.replace("acme-files", "newcomer"),
        )
        .unwrap();
        make_executable(&outside.path().join("acme"));
        let review = registry.review(outside.path());
        assert!(review.valid, "{:?}", review.error);
        assert_eq!(review.id, "newcomer");
        assert_eq!(review.args, ["--serve", "--mode=sync"]);
        assert!(!review.digest.is_empty());
        assert!(!review.replaces_existing);

        // It is shown as a candidate, and is not a plugin the daemon knows.
        let candidate = entry(&registry, "newcomer");
        assert!(candidate.review.is_some() && !candidate.enabled);
        assert!(registry.launch_plan("newcomer", None).is_err());
    }

    #[test]
    fn a_pending_request_is_distinct_from_an_accepted_one() {
        let fixture = Fixture::new();
        let mut registry = fixture.registry();
        registry.mark_pending("acme-files", PluginPending::Approving);
        assert_eq!(
            entry(&registry, "acme-files").pending,
            Some(PluginPending::Approving)
        );
        assert_eq!(
            entry(&registry, "acme-files").trust,
            PluginTrust::Unapproved
        );
        registry.note_result("acme-files", &Err("not saved".into()));
        let refused = entry(&registry, "acme-files");
        assert_eq!(refused.pending, None);
        assert_eq!(refused.action_error.as_deref(), Some("not saved"));
        assert_eq!(refused.error.as_deref(), Some("not saved"));
    }
}
