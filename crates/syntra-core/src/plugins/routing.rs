//! Which plugin serves which role, and which frames it may send.
//!
//! Nothing here looks at a plugin's *name*. A plugin fills a role because the
//! capabilities the user approved (and the manifest declared) say it can, and
//! may send only the frames those capabilities allow. A third-party plugin
//! with any id can therefore supply the file clipboard or a mounted receive,
//! and takes exactly the same paths through the transfer state machine as the
//! bundled ones.
//!
//! The functions are pure: they take data and return a decision, so every rule
//! can be tested without a process.

use std::collections::HashMap;
use syntra_api::PluginRole;
use syntra_plugin_api::{Capabilities, Message, PluginRole as ManifestRole};

/// Converts the manifest's role vocabulary to the client contract's.
///
/// The two enums are separate on purpose: `syntra-plugin-api` must stay free of
/// the client contract's dependencies and the reverse. Their wire names are
/// identical and a test pins that, so they cannot drift apart unnoticed.
pub(crate) fn client_role(role: ManifestRole) -> PluginRole {
    match role {
        ManifestRole::FileClipboard => PluginRole::FileClipboard,
        ManifestRole::MountedFileReceive => PluginRole::MountedFileReceive,
        ManifestRole::Pointer => PluginRole::Pointer,
    }
}

/// The inverse of [`client_role`].
pub(crate) fn manifest_role(role: PluginRole) -> ManifestRole {
    match role {
        PluginRole::FileClipboard => ManifestRole::FileClipboard,
        PluginRole::MountedFileReceive => ManifestRole::MountedFileReceive,
        PluginRole::Pointer => ManifestRole::Pointer,
    }
}

/// A plugin that could fill a role, as far as selection is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    /// Plugin identifier.
    pub id: String,
    /// Part of the distribution, found beside the daemon executable.
    pub bundled: bool,
    /// The user switched it on.
    pub enabled: bool,
    /// Its content is approved (or it is bundled).
    pub trusted: bool,
    /// Its executable exists and is runnable on this platform.
    pub launchable: bool,
}

impl Candidate {
    /// Whether the plugin may be launched at all.
    pub(crate) fn eligible(&self) -> bool {
        self.enabled && self.trusted && self.launchable
    }
}

/// Picks the provider of one role, deterministically.
///
/// 1. The provider the user explicitly preferred, when it is eligible.
/// 2. Otherwise a bundled provider, so the default is the one that ships.
/// 3. Otherwise the eligible provider with the smallest id.
///
/// `candidates` are those that can fill the role. Returns `None` when none is
/// eligible, which is a normal state: a missing or unapproved provider only
/// means that capability is unavailable, never that input sharing is.
pub(crate) fn select_provider<'a>(
    candidates: &'a [Candidate],
    preferred: Option<&str>,
) -> Option<&'a Candidate> {
    if let Some(wanted) = preferred {
        if let Some(found) = candidates
            .iter()
            .find(|candidate| candidate.id == wanted && candidate.eligible())
        {
            return Some(found);
        }
    }
    let mut eligible: Vec<&Candidate> = candidates
        .iter()
        .filter(|candidate| candidate.eligible())
        .collect();
    eligible.sort_by(|a, b| b.bundled.cmp(&a.bundled).then_with(|| a.id.cmp(&b.id)));
    eligible.into_iter().next()
}

/// Why a candidate that could fill a role is not the active provider, in words
/// a user can act on. `None` when it is the active one.
pub(crate) fn unavailable_reason(
    candidate: &Candidate,
    active: Option<&str>,
    preferred: Option<&str>,
) -> Option<String> {
    if !candidate.enabled {
        return Some("switched off".into());
    }
    if !candidate.trusted {
        return Some("its content is not approved".into());
    }
    if !candidate.launchable {
        return Some("its executable is missing or not runnable here".into());
    }
    match active {
        Some(id) if id == candidate.id => None,
        Some(id) => Some(if preferred == Some(id) {
            format!("`{id}` is the provider you chose")
        } else {
            format!("`{id}` provides it")
        }),
        None => None,
    }
}

/// What a running plugin was approved to do, as the supervisor enforces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Grant {
    /// Capabilities the user approved (or the bundled declaration), narrowed
    /// to what the process reported in its handshake. Never wider than the
    /// approval: a handshake can describe what is available right now, not
    /// extend what was allowed.
    pub capabilities: Capabilities,
    /// Roles must be held by both declarations, not newly derived from a
    /// narrowed flag set (dropping clipboard_read must not grant mounts).
    roles: Vec<ManifestRole>,
}

impl Grant {
    /// The grant for a process that announced `announced`, under an approval
    /// of `approved`.
    pub(crate) fn from_handshake(approved: &Capabilities, announced: &Capabilities) -> Self {
        Self {
            capabilities: announced.intersect(approved),
            roles: approved
                .roles()
                .into_iter()
                .filter(|role| announced.fills(*role))
                .collect(),
        }
    }

    /// Whether the grant covers `role`.
    pub(crate) fn allows(&self, role: ManifestRole) -> bool {
        self.roles.contains(&role) && self.capabilities.fills(role)
    }

    /// Whether the process may send `message` at all.
    ///
    /// Frames that carry a transfer are refused unless the grant covers the
    /// role that owns them, so a plugin approved only to listen for the
    /// pointer cannot forge clipboard or file frames. Daemon-to-plugin frames
    /// are never accepted from a plugin.
    pub(crate) fn permits(&self, message: &Message) -> bool {
        let clipboard = self.allows(ManifestRole::FileClipboard);
        let receive = self.allows(ManifestRole::MountedFileReceive);
        match message {
            // A plugin may always report an error and describe its settings.
            Message::Error { .. } | Message::DeclareSettings { .. } => true,
            // Offering copied files and answering a paste are the file
            // clipboard's.
            Message::CopyManifest(_)
            | Message::PasteDestination(_)
            | Message::Released(_)
            | Message::ClipboardData { .. } => clipboard,
            // Progress, completion and cancellation belong to whichever
            // transfer role the plugin holds.
            Message::Progress(_) | Message::Completed(_) | Message::Cancelled(_) => {
                clipboard || receive
            }
            // Reading remote bytes and announcing a mount are a mounted
            // receive's.
            Message::RangeRequest(_) | Message::MountReady(_) | Message::Unmounted(_) => receive,
            // Everything else flows from the daemon to a plugin, or is the
            // handshake, which is handled before a grant exists.
            Message::Hello { .. }
            | Message::PublishFileClipboard(_)
            | Message::RemoteManifest(_)
            | Message::RangeResponse(_)
            | Message::Cancel { .. }
            | Message::Settings { .. }
            | Message::ChangeSetting { .. }
            | Message::OpenSettings { .. }
            | Message::Pointer(_) => false,
        }
    }
}

/// Checks a handshake before the process is treated as ready.
///
/// A plugin must announce the id it was launched under (or one of the aliases
/// its manifest declares), speak this protocol version, and declare
/// capabilities that cover at least one role the user approved it for. What it
/// announces beyond the approval is ignored rather than refused: it is
/// narrowed away by [`Grant::from_handshake`].
pub(crate) fn validate_handshake(
    launched_as: &str,
    accepts_id: impl Fn(&str) -> bool,
    approved: &Capabilities,
    message: &Message,
    expected_role: Option<ManifestRole>,
) -> Result<Grant, &'static str> {
    let Message::Hello {
        protocol_version,
        adapter_id,
        capabilities,
        ..
    } = message
    else {
        return Err("the first message was not a handshake");
    };
    if *protocol_version != syntra_plugin_api::PROTOCOL_VERSION {
        return Err("the plugin speaks a different protocol version");
    }
    if !accepts_id(adapter_id) {
        return Err("the handshake names a different plugin than the one launched");
    }
    let grant = Grant::from_handshake(approved, capabilities);
    let serves_something = ManifestRole::ALL.into_iter().any(|role| grant.allows(role));
    if !serves_something {
        return Err("the plugin announced nothing it was approved to do");
    }
    if let Some(role) = expected_role {
        if !grant.allows(role) {
            return Err("the plugin does not provide the role it was started for");
        }
    }
    let _ = launched_as;
    Ok(grant)
}

/// Index of providers per role, from the full set of candidates.
pub(crate) fn assign_roles(
    candidates: &HashMap<ManifestRole, Vec<Candidate>>,
    preferences: &HashMap<ManifestRole, String>,
) -> HashMap<ManifestRole, String> {
    let mut active = HashMap::new();
    for role in ManifestRole::ALL {
        let Some(list) = candidates.get(&role) else {
            continue;
        };
        let preferred = preferences.get(&role).map(String::as_str);
        if let Some(chosen) = select_provider(list, preferred) {
            active.insert(role, chosen.id.clone());
        }
    }
    active
}

#[cfg(test)]
mod tests {
    use super::*;
    use syntra_plugin_api::{
        Cancelled, CopyManifest, GNOME_COPIED_FILES_MIME, Operation, URI_LIST_MIME,
    };

    fn candidate(id: &str, bundled: bool) -> Candidate {
        Candidate {
            id: id.into(),
            bundled,
            enabled: true,
            trusted: true,
            launchable: true,
        }
    }

    fn file_clipboard() -> Capabilities {
        Capabilities {
            clipboard_read: true,
            paste: true,
            cancel: true,
            requires_live_mount: false,
            pointer_events: false,
            mime_types: vec![URI_LIST_MIME.into(), GNOME_COPIED_FILES_MIME.into()],
        }
    }

    fn pointer_only() -> Capabilities {
        Capabilities {
            pointer_events: true,
            ..Capabilities::default()
        }
    }

    fn hello(id: &str, capabilities: Capabilities) -> Message {
        Message::Hello {
            protocol_version: syntra_plugin_api::PROTOCOL_VERSION,
            adapter_id: id.into(),
            name: "x".into(),
            capabilities,
            metadata: None,
        }
    }

    fn copy_manifest() -> Message {
        Message::CopyManifest(CopyManifest {
            transfer_id: "t".into(),
            operation: Operation::Copy,
            entries: Vec::new(),
        })
    }

    #[test]
    fn the_two_role_enums_agree_on_names_and_convert_both_ways() {
        for role in ManifestRole::ALL {
            let client = client_role(role);
            assert_eq!(manifest_role(client), role);
            let wire = serde_json::to_string(&client).unwrap();
            assert_eq!(wire, format!("\"{}\"", role.as_str()));
        }
    }

    #[test]
    fn the_bundled_provider_is_the_default_and_an_explicit_preference_wins() {
        let list = [
            candidate("zeta", false),
            candidate("clipboard", true),
            candidate("alpha", false),
        ];
        assert_eq!(select_provider(&list, None).unwrap().id, "clipboard");
        assert_eq!(select_provider(&list, Some("zeta")).unwrap().id, "zeta");

        // A preference for something that cannot run falls back, instead of
        // leaving the role empty.
        let mut broken = list.clone();
        broken[0].trusted = false;
        assert_eq!(
            select_provider(&broken, Some("zeta")).unwrap().id,
            "clipboard"
        );
    }

    #[test]
    fn without_a_bundled_provider_the_smallest_id_is_chosen_every_time() {
        let list = [candidate("zeta", false), candidate("alpha", false)];
        for _ in 0..3 {
            assert_eq!(select_provider(&list, None).unwrap().id, "alpha");
        }
    }

    #[test]
    fn no_eligible_provider_is_not_an_error() {
        let mut only = candidate("a", false);
        only.trusted = false;
        assert!(select_provider(&[only.clone()], None).is_none());
        only.trusted = true;
        only.enabled = false;
        assert!(select_provider(&[only], None).is_none());
        assert!(select_provider(&[], None).is_none());
    }

    #[test]
    fn a_plugin_with_any_id_can_fill_the_file_clipboard_role() {
        let approved = file_clipboard();
        let grant = validate_handshake(
            "totally-new-id",
            |id| id == "totally-new-id",
            &approved,
            &hello("totally-new-id", approved.clone()),
            Some(ManifestRole::FileClipboard),
        )
        .unwrap();
        assert!(grant.allows(ManifestRole::FileClipboard));
        assert!(grant.permits(&copy_manifest()));
    }

    #[test]
    fn a_mounted_receiver_cannot_publish_clipboard_frames() {
        let approved = Capabilities {
            paste: true,
            cancel: true,
            requires_live_mount: true,
            mime_types: vec![syntra_plugin_api::URI_LIST_MIME.into()],
            ..Capabilities::default()
        };
        let grant = Grant::from_handshake(&approved, &approved);
        assert!(grant.allows(ManifestRole::MountedFileReceive));
        assert!(!grant.allows(ManifestRole::FileClipboard));
        assert!(!grant.permits(&copy_manifest()));
        assert!(!grant.permits(&Message::ClipboardData {
            transfer_id: "t".into(),
            mime_type: "text/plain".into(),
            value: "x".into(),
        }));
        assert!(
            grant.permits(&Message::RangeRequest(syntra_plugin_api::RangeRequest {
                transfer_id: "t".into(),
                request_id: 1,
                entry_id: 2,
                offset: 0,
                length: 1,
            }))
        );
    }

    #[test]
    fn dropping_clipboard_read_in_a_handshake_cannot_grant_mounts() {
        let mut approved = file_clipboard();
        approved.requires_live_mount = true;
        let mut announced = approved.clone();
        announced.clipboard_read = false;
        let grant = Grant::from_handshake(&approved, &announced);
        assert!(!grant.allows(ManifestRole::MountedFileReceive));
        assert!(
            !grant.permits(&Message::RangeRequest(syntra_plugin_api::RangeRequest {
                transfer_id: "t".into(),
                request_id: 1,
                entry_id: 2,
                offset: 0,
                length: 1,
            }))
        );
        assert!(
            validate_handshake(
                "clipboard",
                |id| id == "clipboard",
                &approved,
                &hello("clipboard", announced),
                Some(ManifestRole::MountedFileReceive),
            )
            .is_err()
        );
    }

    #[test]
    fn a_pointer_only_grant_cannot_forge_transfer_frames() {
        let approved = pointer_only();
        let grant = validate_handshake(
            "glow",
            |id| id == "glow",
            &approved,
            &hello("glow", pointer_only()),
            None,
        )
        .unwrap();
        assert!(!grant.permits(&copy_manifest()));
        assert!(!grant.permits(&Message::Cancelled(Cancelled {
            transfer_id: "t".into()
        })));
        assert!(grant.permits(&Message::Error {
            message: "oops".into()
        }));
    }

    #[test]
    fn the_handshake_cannot_widen_what_was_approved() {
        // Approved for the pointer only; the process announces file clipboard
        // too. The extra is dropped.
        let approved = pointer_only();
        let mut announced = file_clipboard();
        announced.pointer_events = true;
        let grant =
            validate_handshake("p", |id| id == "p", &approved, &hello("p", announced), None)
                .unwrap();
        assert!(grant.allows(ManifestRole::Pointer));
        assert!(!grant.allows(ManifestRole::FileClipboard));
        assert!(!grant.permits(&copy_manifest()));
    }

    #[test]
    fn a_handshake_for_another_plugin_or_version_or_nothing_is_refused() {
        let approved = file_clipboard();
        let accepts = |id: &str| id == "mine";
        let wrong_id = validate_handshake(
            "mine",
            accepts,
            &approved,
            &hello("someone-else", approved.clone()),
            None,
        );
        assert!(wrong_id.is_err());

        let mut old = hello("mine", approved.clone());
        if let Message::Hello {
            protocol_version, ..
        } = &mut old
        {
            *protocol_version = 0;
        }
        assert!(validate_handshake("mine", accepts, &approved, &old, None).is_err());

        let nothing = validate_handshake(
            "mine",
            accepts,
            &approved,
            &hello("mine", Capabilities::default()),
            None,
        );
        assert!(nothing.is_err());

        let not_a_hello = validate_handshake("mine", accepts, &approved, &copy_manifest(), None);
        assert!(not_a_hello.is_err());
    }

    #[test]
    fn a_legacy_handshake_id_declared_by_the_manifest_is_accepted() {
        let approved = file_clipboard();
        let result = validate_handshake(
            "clipboard",
            |id| id == "clipboard" || id == "gtk-clipboard",
            &approved,
            &hello("gtk-clipboard", approved.clone()),
            Some(ManifestRole::FileClipboard),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn daemon_to_plugin_frames_are_never_accepted_from_a_plugin() {
        let grant = Grant::from_handshake(&file_clipboard(), &file_clipboard());
        assert!(!grant.permits(&Message::Cancel {
            transfer_id: "t".into()
        }));
        assert!(!grant.permits(&Message::Settings { values: Vec::new() }));
    }

    #[test]
    fn roles_are_assigned_independently_per_role() {
        let mut candidates = HashMap::new();
        candidates.insert(
            ManifestRole::FileClipboard,
            vec![candidate("clipboard", true), candidate("third", false)],
        );
        candidates.insert(ManifestRole::Pointer, vec![candidate("glow", true)]);
        let mut preferences = HashMap::new();
        preferences.insert(ManifestRole::FileClipboard, "third".to_owned());
        let active = assign_roles(&candidates, &preferences);
        assert_eq!(active[&ManifestRole::FileClipboard], "third");
        assert_eq!(active[&ManifestRole::Pointer], "glow");
        assert!(!active.contains_key(&ManifestRole::MountedFileReceive));
    }

    #[test]
    fn unavailable_reasons_say_what_to_do() {
        let mut unapproved = candidate("a", false);
        unapproved.trusted = false;
        assert_eq!(
            unavailable_reason(&unapproved, None, None).as_deref(),
            Some("its content is not approved")
        );
        let ok = candidate("b", false);
        assert_eq!(unavailable_reason(&ok, Some("b"), None), None);
        assert!(
            unavailable_reason(&ok, Some("c"), Some("c"))
                .unwrap()
                .contains("you chose")
        );
    }
}
