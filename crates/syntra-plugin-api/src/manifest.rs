//! The one manifest contract for out-of-process Syntra plugins.
//!
//! A manifest is how a plugin is *discovered*: which file to run, with which
//! arguments, what it claims to be able to do, and the provenance a user needs
//! in order to decide whether to trust it. The daemon's discovery, the JSON
//! schema in `plugins/manifest.schema.json` and every packaged example are all
//! defined by this module, so there is exactly one definition of what a valid
//! manifest is.
//!
//! A manifest is a *claim*, never a grant. What a plugin may actually do is the
//! intersection of what the user approved for its content digest, what the
//! manifest declares and what the running process reports in its handshake.
//! Nothing in a manifest, including [`PluginManifest::bundled`], confers
//! trust.

use crate::{ApiError, Capabilities, PROTOCOL_VERSION, SettingControl, SettingControlKind};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// Highest manifest format this build understands.
pub const MANIFEST_VERSION: u32 = 1;

/// Longest id, name, label or other short text a manifest may carry.
pub const MAX_SHORT_TEXT: usize = 256;
/// Longest free-form description a manifest may carry.
pub const MAX_DESCRIPTION: usize = 2048;
/// Most arguments a manifest may pass to its executable.
pub const MAX_ARGS: usize = 32;
/// Longest single argument, in bytes.
pub const MAX_ARG_LEN: usize = 1024;
/// Most settings a manifest may declare.
pub const MAX_SETTINGS: usize = 64;
/// Most options one choice setting may declare.
pub const MAX_SETTING_OPTIONS: usize = 64;
/// Most MIME types a manifest may declare.
pub const MAX_MIME_TYPES: usize = 32;
/// Largest manifest file the daemon will read, in bytes.
pub const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
/// Most `resources` entries a manifest may list.
pub const MAX_RESOURCES: usize = 256;

/// What a plugin does for the daemon, derived from what it declares.
///
/// Roles are how the daemon picks a provider. It never branches on a plugin
/// id: a plugin that declares the capabilities of a role can fill it, whatever
/// it is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginRole {
    /// Reads the selection of files a desktop file manager copied and offers
    /// it to the daemon, and publishes remote files as a desktop clipboard
    /// offer. Long-running.
    FileClipboard,
    /// Receives files from another device, serving them to the user through a
    /// mounted read-only view for the duration of one transfer. Started per
    /// transfer.
    MountedFileReceive,
    /// Is told when a pointer crosses or presses against a screen edge.
    /// Long-running.
    Pointer,
}

impl PluginRole {
    /// Every role, in the order a deterministic default is chosen.
    pub const ALL: [PluginRole; 3] = [
        PluginRole::FileClipboard,
        PluginRole::MountedFileReceive,
        PluginRole::Pointer,
    ];

    /// Stable text form, used in state files and the interface.
    pub fn as_str(self) -> &'static str {
        match self {
            PluginRole::FileClipboard => "file_clipboard",
            PluginRole::MountedFileReceive => "mounted_file_receive",
            PluginRole::Pointer => "pointer",
        }
    }

    /// Parses [`PluginRole::as_str`] output.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.as_str() == text)
    }

    /// Whether one process serves one transfer and then exits, rather than
    /// running for as long as the plugin is enabled.
    pub fn per_transfer(self) -> bool {
        matches!(self, PluginRole::MountedFileReceive)
    }
}

impl Capabilities {
    /// Whether these capabilities are enough to fill `role`.
    ///
    /// The file clipboard needs both directions and the two MIME types file
    /// managers exchange copied files with. Mounted receive needs a live mount
    /// and the ability to paste. Neither is decided by who the plugin is.
    pub fn fills(&self, role: PluginRole) -> bool {
        let lists = |wanted: &str| self.mime_types.iter().any(|mime| mime == wanted);
        match role {
            PluginRole::FileClipboard => {
                self.clipboard_read
                    && self.paste
                    && self.cancel
                    && lists(crate::URI_LIST_MIME)
                    && lists(crate::GNOME_COPIED_FILES_MIME)
            }
            PluginRole::MountedFileReceive => {
                // A clipboard reader needs a live mount published to it; it
                // does not implement RemoteManifest/RangeRequest itself.
                // The existing receiver contract is paste-only plus a mount.
                !self.clipboard_read
                    && self.paste
                    && self.cancel
                    && self.requires_live_mount
                    && lists(crate::URI_LIST_MIME)
            }
            PluginRole::Pointer => self.pointer_events,
        }
    }

    /// Every role these capabilities can fill.
    pub fn roles(&self) -> Vec<PluginRole> {
        PluginRole::ALL
            .into_iter()
            .filter(|role| self.fills(*role))
            .collect()
    }

    /// The part of `self` that `approved` also grants.
    ///
    /// A handshake can describe what a process is able to do right now, but it
    /// can never widen what the user approved: anything not in `approved` is
    /// dropped.
    pub fn intersect(&self, approved: &Capabilities) -> Capabilities {
        Capabilities {
            clipboard_read: self.clipboard_read && approved.clipboard_read,
            paste: self.paste && approved.paste,
            cancel: self.cancel && approved.cancel,
            requires_live_mount: self.requires_live_mount && approved.requires_live_mount,
            pointer_events: self.pointer_events && approved.pointer_events,
            mime_types: self
                .mime_types
                .iter()
                .filter(|mime| approved.mime_types.contains(mime))
                .cloned()
                .collect(),
        }
    }
}

/// One setting as a manifest declares it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingDecl {
    /// Name the plugin receives the value under.
    pub key: String,
    /// Label shown to the user.
    pub label: String,
    /// `toggle`, `choice`, `color` or `number`.
    pub kind: String,
    /// Value until the user chooses one, in its wire form.
    #[serde(default)]
    pub default: String,
    /// For `choice`: the options as `value` and `label`.
    #[serde(default)]
    pub options: Vec<ChoiceDecl>,
    /// For `number`: the lowest accepted value. Optional; omitted means no
    /// lower bound.
    #[serde(default)]
    pub min: Option<i64>,
    /// For `number`: the highest accepted value. Optional; omitted means no
    /// upper bound.
    #[serde(default)]
    pub max: Option<i64>,
}

/// One option of a `choice` setting in a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceDecl {
    /// What the plugin receives when this is picked.
    pub value: String,
    /// What the user reads.
    pub label: String,
}

impl SettingDecl {
    /// The inclusive range of a `number` setting.
    ///
    /// An omitted bound is the extreme of `i64`, so a setting that declares
    /// none accepts any whole number rather than only zero. The plugin
    /// protocol's number control carries both bounds, and this is the one place
    /// they are produced, so every consumer agrees on what omission means.
    pub fn range(&self) -> (i64, i64) {
        (self.min.unwrap_or(i64::MIN), self.max.unwrap_or(i64::MAX))
    }

    /// Whether `value` is acceptable for this setting.
    ///
    /// Checked by the daemon, never trusted from the interface: what is saved
    /// goes back to the plugin, so a value it cannot understand must never be
    /// stored.
    pub fn accepts(&self, value: &str) -> bool {
        match self.kind.as_str() {
            "toggle" => value == "true" || value == "false",
            "choice" => self.options.iter().any(|option| option.value == value),
            "number" => value.parse::<i64>().is_ok_and(|number| {
                let (min, max) = self.range();
                min <= number && number <= max
            }),
            "color" => {
                value.is_empty()
                    || (value.len() == 7
                        && value.starts_with('#')
                        && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit()))
            }
            _ => false,
        }
    }

    /// The value in force: the user's choice if it is still acceptable,
    /// otherwise the default. A stale choice therefore never reaches the
    /// plugin.
    pub fn effective(&self, chosen: Option<&String>) -> String {
        match chosen {
            Some(value) if self.accepts(value) => value.clone(),
            _ => self.default.clone(),
        }
    }

    /// This declaration as the typed control the plugin protocol uses.
    pub fn control(&self, value: String) -> Option<SettingControl> {
        let kind = match self.kind.as_str() {
            "toggle" => SettingControlKind::Toggle,
            "color" => SettingControlKind::Color,
            "choice" => SettingControlKind::Choice {
                options: self
                    .options
                    .iter()
                    .map(|option| crate::SettingOption {
                        value: option.value.clone(),
                        label: option.label.clone(),
                    })
                    .collect(),
            },
            "number" => {
                let (min, max) = self.range();
                SettingControlKind::Number { min, max }
            }
            _ => return None,
        };
        Some(SettingControl {
            key: self.key.clone(),
            label: self.label.clone(),
            kind,
            value,
            default: self.default.clone(),
        })
    }

    fn validate(&self) -> Result<(), ApiError> {
        let invalid = |reason: &str| {
            Err(ApiError::InvalidManifest(format!(
                "setting `{}`: {reason}",
                self.key
            )))
        };
        let mut chars = self.key.chars();
        let key_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !key_ok || self.key.len() > MAX_SHORT_TEXT {
            return invalid(
                "key must be lowercase letters, digits and `_`, starting with a letter",
            );
        }
        if self.label.trim().is_empty() || self.label.len() > MAX_SHORT_TEXT {
            return invalid("label is required");
        }
        if self.default.len() > MAX_SHORT_TEXT {
            return invalid("default is too long");
        }
        match self.kind.as_str() {
            "toggle" | "color" => {}
            "choice" => {
                if self.options.is_empty() || self.options.len() > MAX_SETTING_OPTIONS {
                    return invalid("a choice needs between 1 and 64 options");
                }
                for option in &self.options {
                    if option.value.is_empty()
                        || option.label.trim().is_empty()
                        || option.value.len() > MAX_SHORT_TEXT
                        || option.label.len() > MAX_SHORT_TEXT
                    {
                        return invalid("every option needs a value and a label");
                    }
                }
            }
            "number" => {
                // Bounds are optional in a manifest: an omitted one is the
                // extreme of the type, so a setting without a range accepts any
                // whole number. The only invalid range is a reversed one.
                let (min, max) = self.range();
                if min > max {
                    return invalid("min is greater than max");
                }
            }
            _ => return invalid("kind must be toggle, choice, color or number"),
        }
        if !self.default.is_empty() && !self.accepts(&self.default) {
            return invalid("default is not a value the setting accepts");
        }
        Ok(())
    }
}

/// The manifest of an out-of-process plugin.
///
/// Every field the daemon, the schema or an example uses is here. Fields added
/// after manifest version 1 are optional, so a manifest written earlier stays
/// valid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Manifest format version. A version above [`MANIFEST_VERSION`] is shown
    /// as unsupported rather than guessed at.
    pub manifest_version: u32,
    /// Plugin protocol version the plugin speaks.
    pub protocol_version: u32,
    /// Stable identifier, unique per daemon.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// File to run, relative to the manifest or absolute.
    pub executable: PathBuf,
    /// Arguments passed to the executable at launch, in order.
    #[serde(default)]
    pub args: Vec<String>,
    /// One or two sentences describing what the plugin adds.
    #[serde(default)]
    pub description: String,
    /// Plugin version.
    #[serde(default)]
    pub version: String,
    /// Person or organisation responsible for it.
    #[serde(default)]
    pub author: String,
    /// Project or documentation page.
    #[serde(default)]
    pub homepage: Option<String>,
    /// Where the source can be audited.
    #[serde(default)]
    pub source: Option<String>,
    /// Where newer releases are published.
    #[serde(default)]
    pub update_url: Option<String>,
    /// SPDX licence identifier.
    #[serde(default)]
    pub license: Option<String>,
    /// A *claim* that the plugin ships with Syntra. Ignored for trust: the
    /// daemon decides provenance from where it found the file, never from
    /// this.
    #[serde(default)]
    pub bundled: bool,
    /// Started only while something needs it, then exits.
    #[serde(default)]
    pub on_demand: bool,
    /// Keeps and validates its own settings, declared over the API.
    #[serde(default)]
    pub owns_settings: bool,
    /// What the plugin claims to be able to do.
    pub capabilities: Capabilities,
    /// What the user can change, drawn by the interface from this.
    #[serde(default)]
    pub settings: Vec<SettingDecl>,
    /// Other adapter ids this plugin's handshake may use besides its own `id`.
    ///
    /// Exists so a plugin released before ids were enforced keeps working: the
    /// bundled clipboard plugin has always announced itself as
    /// `gtk-clipboard`.
    #[serde(default)]
    pub handshake_ids: Vec<String>,
    /// Files and directories beside the manifest that are part of the plugin
    /// and are therefore covered by its content digest, in addition to the
    /// executable. Paths are relative to the manifest and may not climb out
    /// of it.
    #[serde(default)]
    pub resources: Vec<String>,
}

/// What reading a manifest file produced.
#[derive(Debug, Clone)]
pub enum ManifestLoad {
    /// A manifest this build fully understands.
    Valid(Box<PluginManifest>),
    /// Valid JSON that names a manifest or protocol version newer than this
    /// build understands. It is listed as unsupported with what could be read,
    /// never launched.
    Unsupported {
        /// Identifier, when it could be read safely.
        id: Option<String>,
        /// Name, when present.
        name: Option<String>,
        /// Manifest version found.
        manifest_version: u32,
        /// Protocol version found, `0` when absent.
        protocol_version: u32,
        /// Why it is unsupported, ready to show.
        reason: String,
    },
}

/// Whether `id` can safely name a directory and a file: letters, digits, `.`,
/// `_` and `-`, not `.` or `..`, and of bounded length.
pub fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SHORT_TEXT
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

impl PluginManifest {
    /// Parses and fully validates a manifest, without touching the file
    /// system.
    ///
    /// A manifest that names a future manifest or protocol version is returned
    /// as [`ManifestLoad::Unsupported`] instead of an error, so it can be
    /// displayed. Anything else invalid is an error.
    pub fn parse(text: &str) -> Result<ManifestLoad, ApiError> {
        let value: serde_json::Value = serde_json::from_str(text)?;
        let manifest_version = value
            .get("manifest_version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| ApiError::InvalidManifest("manifest_version is required".into()))?;
        let protocol_version = value
            .get("protocol_version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let manifest_version = u32::try_from(manifest_version).unwrap_or(u32::MAX);
        let protocol_version = u32::try_from(protocol_version).unwrap_or(u32::MAX);
        if manifest_version > MANIFEST_VERSION || protocol_version > PROTOCOL_VERSION {
            let text_of = |key: &str| {
                value
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .map(|text| text.chars().take(MAX_SHORT_TEXT).collect::<String>())
            };
            return Ok(ManifestLoad::Unsupported {
                id: text_of("id").filter(|id| is_safe_id(id)),
                name: text_of("name"),
                manifest_version,
                protocol_version,
                reason: format!(
                    "needs manifest version {manifest_version} and protocol version \
                     {protocol_version}; this Syntra understands manifest {MANIFEST_VERSION} and \
                     protocol {PROTOCOL_VERSION}"
                ),
            });
        }
        let manifest: PluginManifest = serde_json::from_value(value)?;
        manifest.validate()?;
        Ok(ManifestLoad::Valid(Box::new(manifest)))
    }

    /// Checks everything a manifest declares, independent of the file system.
    ///
    /// Required: identity, both versions, name, executable and a capability
    /// block that does something. Bounded: every text, list and argument.
    pub fn validate(&self) -> Result<(), ApiError> {
        let invalid = |reason: String| Err(ApiError::InvalidManifest(reason));
        if self.manifest_version != MANIFEST_VERSION || self.protocol_version != PROTOCOL_VERSION {
            return invalid(format!(
                "unsupported manifest version {} or protocol version {}",
                self.manifest_version, self.protocol_version
            ));
        }
        if !is_safe_id(&self.id) {
            return invalid(format!(
                "plugin id {:?} must use only letters, digits, `.`, `_` and `-`, and not be `.` \
                 or `..`",
                self.id
            ));
        }
        if self.name.trim().is_empty() || self.name.len() > MAX_SHORT_TEXT {
            return invalid("name is required".into());
        }
        if self.executable.as_os_str().is_empty() {
            return invalid("executable is required".into());
        }
        if self.description.len() > MAX_DESCRIPTION {
            return invalid("description is too long".into());
        }
        for (field, value) in [
            ("version", Some(&self.version)),
            ("author", Some(&self.author)),
            ("homepage", self.homepage.as_ref()),
            ("source", self.source.as_ref()),
            ("update_url", self.update_url.as_ref()),
            ("license", self.license.as_ref()),
        ] {
            if value.is_some_and(|text| text.len() > MAX_SHORT_TEXT) {
                return invalid(format!("{field} is too long"));
            }
        }
        if self.args.len() > MAX_ARGS
            || self
                .args
                .iter()
                .any(|argument| argument.len() > MAX_ARG_LEN || argument.contains('\0'))
        {
            return invalid(format!(
                "at most {MAX_ARGS} arguments of {MAX_ARG_LEN} bytes without NUL are allowed"
            ));
        }
        self.validate_capabilities()?;
        if self.settings.len() > MAX_SETTINGS {
            return invalid(format!("at most {MAX_SETTINGS} settings are allowed"));
        }
        let mut keys = std::collections::HashSet::new();
        for setting in &self.settings {
            setting.validate()?;
            if !keys.insert(setting.key.as_str()) {
                return invalid(format!("setting `{}` is declared twice", setting.key));
            }
        }
        if self.handshake_ids.len() > 8 || self.handshake_ids.iter().any(|id| !is_safe_id(id)) {
            return invalid("handshake_ids must be at most 8 identifier-safe ids".into());
        }
        if self.resources.len() > MAX_RESOURCES {
            return invalid(format!("at most {MAX_RESOURCES} resources are allowed"));
        }
        for resource in &self.resources {
            if !is_contained_relative(Path::new(resource)) {
                return invalid(format!(
                    "resource {resource:?} must be a relative path that stays inside the plugin \
                     directory"
                ));
            }
        }
        Ok(())
    }

    fn validate_capabilities(&self) -> Result<(), ApiError> {
        let caps = &self.capabilities;
        let invalid = |reason: &str| Err(ApiError::InvalidManifest(reason.into()));
        if caps.mime_types.len() > MAX_MIME_TYPES
            || caps
                .mime_types
                .iter()
                .any(|mime| mime.trim().is_empty() || mime.len() > MAX_SHORT_TEXT)
        {
            return invalid("MIME types must be non-empty and at most 32 in number");
        }
        let mut seen = std::collections::HashSet::new();
        if !caps
            .mime_types
            .iter()
            .all(|mime| seen.insert(mime.as_str()))
        {
            return invalid("MIME types must be unique");
        }
        // A plugin must do something, but not necessarily with the clipboard:
        // one that only watches the pointer has no MIME types to declare.
        let handles_clipboard = caps.clipboard_read || caps.paste;
        let clipboard_ok = handles_clipboard && !caps.mime_types.is_empty();
        let pointer_ok = caps.pointer_events && !handles_clipboard && caps.mime_types.is_empty();
        if !(clipboard_ok || pointer_ok) {
            return invalid(
                "capabilities must advertise a clipboard operation with unique MIME types, or \
                 pointer events alone",
            );
        }
        Ok(())
    }

    /// Resolves the executable against the manifest file, without touching the
    /// file system.
    pub fn executable_path(&self, manifest_path: &Path) -> PathBuf {
        if self.executable.is_absolute() {
            self.executable.clone()
        } else {
            manifest_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(&self.executable)
        }
    }

    /// Validates this manifest and resolves its executable to a runnable
    /// file.
    pub fn resolve_executable(&self, manifest_path: &Path) -> Result<PathBuf, ApiError> {
        self.validate()?;
        let path = self.executable_path(manifest_path);
        let metadata = std::fs::metadata(&path)
            .map_err(|e| ApiError::InvalidManifest(format!("executable unavailable: {e}")))?;
        if !metadata.is_file() {
            return Err(ApiError::InvalidManifest("executable is not a file".into()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                return Err(ApiError::InvalidManifest(
                    "executable is not executable".into(),
                ));
            }
        }
        Ok(path)
    }

    /// The exact argument vector that is delivered to the process, after the
    /// program name.
    pub fn argv(&self) -> &[String] {
        &self.args
    }

    /// Every role the manifest's capabilities can fill.
    pub fn roles(&self) -> Vec<PluginRole> {
        self.capabilities.roles()
    }

    /// Whether `adapter_id`, as announced in a handshake, names this plugin.
    pub fn accepts_handshake_id(&self, adapter_id: &str) -> bool {
        adapter_id == self.id || self.handshake_ids.iter().any(|id| id == adapter_id)
    }
}

/// A relative path that never leaves the directory it is relative to.
fn is_contained_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.is_relative()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

/// Alias kept for code written against the earlier manifest name.
pub type AdapterManifest = PluginManifest;

#[cfg(test)]
mod tests {
    use super::*;

    const FILE_PROVIDER: &str = r#"{
        "manifest_version": 1, "protocol_version": 1,
        "id": "cool-files", "name": "Cool files", "executable": "bin/cool",
        "args": ["--mode", "sync", "--flag=a b"],
        "version": "2.0.0", "author": "Someone",
        "capabilities": {
            "clipboard_read": true, "paste": true, "cancel": true,
            "requires_live_mount": false,
            "mime_types": ["text/uri-list", "x-special/gnome-copied-files"]
        }
    }"#;

    fn valid(text: &str) -> PluginManifest {
        match PluginManifest::parse(text).expect("manifest should parse") {
            ManifestLoad::Valid(manifest) => *manifest,
            ManifestLoad::Unsupported { reason, .. } => panic!("unsupported: {reason}"),
        }
    }

    /// The arguments in the manifest are the arguments the process gets, in
    /// order and unaltered; they are not dropped on the way to the supervisor.
    #[test]
    fn arguments_are_kept_verbatim_and_in_order() {
        let manifest = valid(FILE_PROVIDER);
        assert_eq!(manifest.argv(), ["--mode", "sync", "--flag=a b"]);
    }

    #[test]
    fn the_roles_come_from_declared_capabilities_not_from_the_id() {
        let manifest = valid(FILE_PROVIDER);
        assert_eq!(manifest.id, "cool-files");
        assert_eq!(manifest.roles(), vec![PluginRole::FileClipboard]);

        let clipboard_with_mount = valid(&FILE_PROVIDER.replace(
            "\"requires_live_mount\": false",
            "\"requires_live_mount\": true",
        ));
        assert_eq!(
            clipboard_with_mount.roles(),
            vec![PluginRole::FileClipboard]
        );

        let receiver = valid(
            &FILE_PROVIDER
                .replace("\"clipboard_read\": true", "\"clipboard_read\": false")
                .replace(
                    "\"requires_live_mount\": false",
                    "\"requires_live_mount\": true",
                )
                .replace("cool-files", "anything-else"),
        );
        assert_eq!(receiver.roles(), vec![PluginRole::MountedFileReceive]);
    }

    /// The bundled pointer plugin declares only `pointer_events`; it must stay
    /// readable although it has no clipboard fields.
    #[test]
    fn a_pointer_only_manifest_without_clipboard_fields_is_valid() {
        let manifest = valid(
            r#"{"manifest_version":1,"protocol_version":1,"id":"edge-glow","name":"Edge glow",
            "executable":"syntra-plugin-edge-glow","owns_settings":true,
            "capabilities":{"pointer_events":true,"mime_types":[]}}"#,
        );
        assert_eq!(manifest.roles(), vec![PluginRole::Pointer]);
    }

    #[test]
    fn a_handshake_can_narrow_but_never_widen_what_was_approved() {
        let approved = valid(FILE_PROVIDER).capabilities;
        let mut claimed = approved.clone();
        claimed.pointer_events = true;
        claimed.requires_live_mount = true;
        claimed.mime_types.push("application/x-extra".into());
        let granted = claimed.intersect(&approved);
        assert!(!granted.pointer_events);
        assert!(!granted.requires_live_mount);
        assert!(
            !granted
                .mime_types
                .iter()
                .any(|mime| mime == "application/x-extra")
        );
        assert!(granted.fills(PluginRole::FileClipboard));

        let mut narrower = approved.clone();
        narrower.paste = false;
        assert!(
            !narrower
                .intersect(&approved)
                .fills(PluginRole::FileClipboard)
        );
    }

    #[test]
    fn a_future_manifest_or_protocol_is_unsupported_not_an_error() {
        let future = FILE_PROVIDER.replace("\"manifest_version\": 1", "\"manifest_version\": 7");
        match PluginManifest::parse(&future).unwrap() {
            ManifestLoad::Unsupported {
                id,
                manifest_version,
                reason,
                ..
            } => {
                assert_eq!(id.as_deref(), Some("cool-files"));
                assert_eq!(manifest_version, 7);
                assert!(reason.contains("manifest version 7"), "{reason}");
            }
            ManifestLoad::Valid(_) => panic!("a future manifest must not load"),
        }
        let future_protocol =
            FILE_PROVIDER.replace("\"protocol_version\": 1", "\"protocol_version\": 9");
        assert!(matches!(
            PluginManifest::parse(&future_protocol).unwrap(),
            ManifestLoad::Unsupported { .. }
        ));
    }

    /// A manifest written before provenance, settings or args existed stays
    /// readable.
    #[test]
    fn a_legacy_manifest_remains_valid() {
        let legacy = r#"{"manifest_version":1,"protocol_version":1,"id":"coolfilebrowser",
            "name":"CoolFileBrowser","executable":"coolfilebrowser-syntra-plugin","args":[],
            "capabilities":{"clipboard_read":true,"paste":true,"cancel":true,
            "requires_live_mount":false,
            "mime_types":["text/uri-list","x-special/gnome-copied-files"]}}"#;
        let manifest = valid(legacy);
        assert!(manifest.settings.is_empty() && manifest.resources.is_empty());
        assert!(manifest.author.is_empty());
    }

    #[test]
    fn identity_and_bounds_are_enforced() {
        for (label, text) in [
            ("unsafe id", FILE_PROVIDER.replace("cool-files", "../evil")),
            ("empty name", FILE_PROVIDER.replace("Cool files", " ")),
            (
                "nul in argument",
                FILE_PROVIDER.replace("--mode", "--mo\\u0000de"),
            ),
            (
                "no capability",
                FILE_PROVIDER
                    .replace("\"clipboard_read\": true", "\"clipboard_read\": false")
                    .replace("\"paste\": true", "\"paste\": false"),
            ),
            (
                "duplicate mime",
                FILE_PROVIDER.replace("\"x-special/gnome-copied-files\"", "\"text/uri-list\""),
            ),
        ] {
            assert!(
                PluginManifest::parse(&text).is_err(),
                "{label} must be refused"
            );
        }
        let many_args = format!(
            "[{}]",
            (0..=MAX_ARGS)
                .map(|n| format!("\"a{n}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        let text = FILE_PROVIDER.replace(r#"["--mode", "sync", "--flag=a b"]"#, &many_args);
        assert!(PluginManifest::parse(&text).is_err(), "too many arguments");
    }

    #[test]
    fn typed_settings_validate_and_constrain_values() {
        let text = FILE_PROVIDER.replace(
            "\"capabilities\"",
            r##""settings": [
                {"key":"enabled","label":"On","kind":"toggle","default":"true"},
                {"key":"mode","label":"Mode","kind":"choice","default":"a",
                 "options":[{"value":"a","label":"A"},{"value":"b","label":"B"}]},
                {"key":"size","label":"Size","kind":"number","default":"5","min":1,"max":9},
                {"key":"tint","label":"Tint","kind":"color","default":""}
            ], "capabilities""##,
        );
        let manifest = valid(&text);
        let by = |key: &str| manifest.settings.iter().find(|s| s.key == key).unwrap();
        assert!(by("enabled").accepts("false") && !by("enabled").accepts("yes"));
        assert!(by("mode").accepts("b") && !by("mode").accepts("c"));
        assert!(by("size").accepts("9") && !by("size").accepts("10"));
        assert!(by("tint").accepts("#33ccff") && !by("tint").accepts("red"));
        assert_eq!(by("mode").effective(Some(&"gone".to_owned())), "a");

        let bad_default = text.replace("\"default\":\"5\"", "\"default\":\"50\"");
        assert!(PluginManifest::parse(&bad_default).is_err());
        let duplicate = text.replace("\"key\":\"mode\"", "\"key\":\"size\"");
        assert!(PluginManifest::parse(&duplicate).is_err());
    }

    /// An omitted bound must not become 0: a range of `0..=0` would accept only
    /// zero and look like a working setting. A bound that is left out is the
    /// extreme of the type, so a manifest that declares none stays valid and
    /// accepts any whole number; the only invalid range is a reversed one.
    #[test]
    fn omitted_number_bounds_are_unbounded_not_zero() {
        let with = |range: &str| {
            FILE_PROVIDER.replace(
                "\"capabilities\"",
                &format!(
                    r##""settings": [{{"key":"size","label":"Size","kind":"number","default":"5"{range}}}],
                    "capabilities""##
                ),
            )
        };
        let size = |range: &str| valid(&with(range)).settings.remove(0);

        let unbounded = size("");
        assert_eq!(unbounded.range(), (i64::MIN, i64::MAX));
        assert!(unbounded.accepts("5") && unbounded.accepts("-9000000000"));
        assert!(!unbounded.accepts("") && !unbounded.accepts("1.5") && !unbounded.accepts("x"));

        let only_min = size(r#","min":1"#);
        assert_eq!(only_min.range(), (1, i64::MAX));
        assert!(only_min.accepts("1") && only_min.accepts("100") && !only_min.accepts("0"));

        let only_max = size(r#","max":9"#);
        assert_eq!(only_max.range(), (i64::MIN, 9));
        assert!(only_max.accepts("9") && only_max.accepts("-100") && !only_max.accepts("10"));

        // A full range is exactly what accepts values, at its edges too.
        let bounded =
            valid(&with(r#","min":-3,"max":3"#).replace(r#""default":"5""#, r#""default":"0""#))
                .settings
                .remove(0);
        assert_eq!(bounded.range(), (-3, 3));
        assert!(bounded.accepts("-3") && bounded.accepts("3") && bounded.accepts("0"));
        assert!(!bounded.accepts("-4") && !bounded.accepts("4"));
        assert!(PluginManifest::parse(&with(r#","min":-3,"max":3"#)).is_err());

        // The typed control that goes to the plugin carries the same range.
        let control = bounded.control("0".into()).unwrap();
        assert_eq!(control.kind, SettingControlKind::Number { min: -3, max: 3 });

        let error = PluginManifest::parse(&with(r#","min":9,"max":1"#))
            .unwrap_err()
            .to_string();
        assert!(error.contains("min is greater than max"), "{error}");
    }

    #[test]
    fn the_handshake_id_is_the_plugin_id_or_a_declared_alias() {
        let mut manifest = valid(FILE_PROVIDER);
        assert!(manifest.accepts_handshake_id("cool-files"));
        assert!(!manifest.accepts_handshake_id("gtk-clipboard"));
        manifest.handshake_ids.push("gtk-clipboard".into());
        assert!(manifest.accepts_handshake_id("gtk-clipboard"));
    }

    #[test]
    fn the_bundled_claim_is_data_only() {
        let claimed = valid(&FILE_PROVIDER.replace("\"id\"", "\"bundled\": true, \"id\""));
        assert!(claimed.bundled, "kept so it can be displayed as a claim");
        // Nothing in validation or roles depends on it.
        assert_eq!(claimed.roles(), valid(FILE_PROVIDER).roles());
    }
}
