//! The plugin manager: what the daemon reports, as the interface shows it, and
//! what the interface asks for in return.
//!
//! The daemon is the authority. A card shows what the daemon reported and every
//! button only sends a request; the reply, which always follows, is what
//! changes the card. Nothing here predicts an outcome.
//!
//! # One vocabulary
//!
//! A plugin is somewhere on one ladder: *installed*, *approved*, *enabled*,
//! *starting*, *ready* (or *running* for a task-sized plugin), with *failed* and
//! *unsupported* off to the side. A request the user made that the daemon has
//! not yet accepted is **pending** and is shown as pending, never as if it had
//! happened. Whether a plugin is trusted is a separate fact from whether it is
//! running, and both are shown.
//!
//! # Older daemons
//!
//! A daemon from before plugin approval sends none of the newer fields. The
//! card then shows only what that daemon understands (health, enable, restart,
//! settings) and offers no approval, role or install controls, which it could
//! not honour. [`PluginView::manages_lifecycle`] decides.

use std::sync::mpsc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};
use syntra_api::{
    FrontendRequest, PluginHealth, PluginOrigin, PluginPending, PluginRole, PluginState,
    PluginStatus, PluginTrust,
};

use super::{AppState, PluginEntry, PluginRoleEntry, PluginSettingEntry};
use crate::edge_glow;

/// Everything the card needs, decided without a window so it can be tested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PluginView {
    pub health: &'static str,
    pub state: &'static str,
    pub pending: &'static str,
    pub trust: &'static str,
    pub origin: &'static str,
    /// The daemon speaks the lifecycle vocabulary (approval, roles, install).
    pub manages_lifecycle: bool,
    pub can_approve: bool,
    pub can_revoke: bool,
    pub can_uninstall: bool,
    /// Switched on, but waiting for the user to approve its content.
    pub needs_approval: bool,
    /// The content changed since the user approved it.
    pub changed_since_approval: bool,
    /// A package the user asked to review; not installed, never launched.
    pub candidate: bool,
}

/// Stable identifier used for the health badge and its translation key.
pub(super) fn health_id(health: PluginHealth) -> &'static str {
    match health {
        PluginHealth::Disabled => "disabled",
        PluginHealth::NotInstalled => "not-installed",
        PluginHealth::Stopped => "stopped",
        PluginHealth::OnDemand => "on-demand",
        PluginHealth::Starting => "starting",
        PluginHealth::Healthy => "healthy",
        PluginHealth::Unresponsive => "unresponsive",
        PluginHealth::Failed => "failed",
    }
}

fn state_id(state: PluginState) -> &'static str {
    match state {
        PluginState::Installed => "installed",
        PluginState::Approved => "approved",
        PluginState::Enabled => "enabled",
        PluginState::Starting => "starting",
        PluginState::Ready => "ready",
        PluginState::Running => "running",
        PluginState::Failed => "failed",
        PluginState::Unsupported => "unsupported",
    }
}

fn pending_id(pending: Option<PluginPending>) -> &'static str {
    match pending {
        None => "",
        Some(PluginPending::Approving) => "approving",
        Some(PluginPending::Revoking) => "revoking",
        Some(PluginPending::Enabling) => "enabling",
        Some(PluginPending::Disabling) => "disabling",
        Some(PluginPending::Installing) => "installing",
        Some(PluginPending::Removing) => "removing",
    }
}

fn trust_id(trust: PluginTrust) -> &'static str {
    match trust {
        PluginTrust::Bundled => "bundled",
        PluginTrust::Unapproved => "unapproved",
        PluginTrust::Approved => "approved",
        PluginTrust::ChangedSinceApproval => "changed",
    }
}

fn origin_id(origin: PluginOrigin) -> &'static str {
    match origin {
        PluginOrigin::Bundled => "bundled",
        PluginOrigin::DropIn => "drop-in",
        PluginOrigin::Installed => "installed",
    }
}

fn role_id(role: PluginRole) -> &'static str {
    match role {
        PluginRole::FileClipboard => "file-clipboard",
        PluginRole::MountedFileReceive => "mounted-file-receive",
        PluginRole::Pointer => "pointer",
    }
}

impl PluginView {
    pub(super) fn from_status(plugin: &PluginStatus) -> Self {
        // An older daemon sends none of these, so each defaults: unapproved,
        // drop-in, no digest, no roles. A plugin with a digest, an approval, a
        // role, or bundled provenance can only come from a daemon that knows
        // the lifecycle.
        let manages_lifecycle = !plugin.digest.is_empty()
            || !plugin.roles.is_empty()
            || plugin.trust != PluginTrust::Unapproved
            || plugin.origin != PluginOrigin::DropIn
            || plugin.review.is_some();
        let candidate = plugin.review.is_some();
        let pending = plugin.pending.is_some();
        let installed = plugin.installed && !candidate;
        let needs_approval = manages_lifecycle
            && plugin.enabled
            && matches!(
                plugin.trust,
                PluginTrust::Unapproved | PluginTrust::ChangedSinceApproval
            );
        Self {
            health: health_id(plugin.health),
            state: state_id(plugin.state),
            pending: pending_id(plugin.pending),
            trust: trust_id(plugin.trust),
            origin: origin_id(plugin.origin),
            manages_lifecycle,
            can_approve: manages_lifecycle
                && installed
                && !pending
                && !plugin.digest.is_empty()
                && matches!(
                    plugin.trust,
                    PluginTrust::Unapproved | PluginTrust::ChangedSinceApproval
                ),
            can_revoke: manages_lifecycle
                && installed
                && !pending
                && plugin.trust == PluginTrust::Approved,
            can_uninstall: manages_lifecycle && plugin.removable && !pending,
            needs_approval,
            changed_since_approval: plugin.trust == PluginTrust::ChangedSinceApproval,
            candidate,
        }
    }
}

/// One role row of a card.
fn role_entry(role: &syntra_api::PluginRoleStatus) -> PluginRoleEntry {
    PluginRoleEntry {
        role: role_id(role.role).into(),
        active: role.active,
        preferred: role.preferred,
        unavailable: role.unavailable.clone().unwrap_or_default().into(),
    }
}

/// A plugin as the Slint card draws it.
pub(super) fn entry(plugin: &PluginStatus, accent: &str) -> PluginEntry {
    let view = PluginView::from_status(plugin);
    let review = plugin.review.as_ref();
    PluginEntry {
        id: plugin.id.clone().into(),
        name: plugin.name.clone().into(),
        description: plugin.description.clone().into(),
        version: plugin.version.clone().into(),
        author: plugin.author.clone().into(),
        homepage: plugin.homepage.clone().unwrap_or_default().into(),
        source: plugin.source.clone().unwrap_or_default().into(),
        update_url: plugin.update_url.clone().unwrap_or_default().into(),
        license: plugin.license.clone().unwrap_or_default().into(),
        mime_types: plugin.mime_types.join(", ").into(),
        bundled: plugin.bundled,
        manifest_path: plugin.manifest_path.clone().into(),
        executable: plugin.executable.clone().into(),
        installed: plugin.installed,
        enabled: plugin.enabled,
        running: plugin.running,
        health: view.health.into(),
        restarts: plugin.restarts as i32,
        // Only meaningful once the plugin has actually connected.
        build_mismatch: !plugin.build_fingerprint.is_empty()
            && plugin.build_fingerprint != plugin.daemon_build_fingerprint,
        build_fingerprint: plugin.build_fingerprint.clone().into(),
        pid: plugin
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_default()
            .into(),
        protocol_version: plugin.protocol_version as i32,
        supported_protocol_version: plugin.supported_protocol_version as i32,
        error: plugin.error.clone().unwrap_or_default().into(),
        settings: ModelRc::new(VecModel::from(
            plugin
                .settings
                .iter()
                .map(|setting| setting_entry(setting, accent))
                .collect::<Vec<_>>(),
        )),
        owns_settings: plugin.owns_settings,
        state: view.state.into(),
        pending: view.pending.into(),
        trust: view.trust.into(),
        origin: view.origin.into(),
        digest: plugin.digest.clone().into(),
        approved_digest: plugin.approved_digest.clone().unwrap_or_default().into(),
        args: plugin.args.join(" ").into(),
        roles: ModelRc::new(VecModel::from(
            plugin.roles.iter().map(role_entry).collect::<Vec<_>>(),
        )),
        diagnostics: plugin.diagnostics.join("\n").into(),
        manages_lifecycle: view.manages_lifecycle,
        can_approve: view.can_approve,
        can_revoke: view.can_revoke,
        can_uninstall: view.can_uninstall,
        needs_approval: view.needs_approval,
        changed_since_approval: view.changed_since_approval,
        candidate: view.candidate,
        review_path: review
            .map(|review| review.path.clone())
            .unwrap_or_default()
            .into(),
        review_files: review
            .map(|review| review.files.join(", "))
            .unwrap_or_default()
            .into(),
        review_replaces: review.is_some_and(|review| review.replaces_existing),
    }
}

/// Syntra's present colours, as `#rrggbb`, for a plugin drawing its own window.
pub(super) fn theme(app: &super::AppWindow) -> Vec<(String, String)> {
    use slint::Color;
    let theme = app.global::<super::Theme>();
    let translations = app.global::<super::Translations>();
    // The shared controls name their own buttons; the plugin's window gets
    // these texts in the user's language as `tr:<key>` entries.
    let texts = [
        "action-edit",
        "action-close",
        "settings-titlebar-default",
        "color-hue",
        "color-saturation",
        "color-brightness",
        "plugin-settings-close",
    ]
    .into_iter()
    .map(|key| {
        (
            format!("tr:{key}"),
            translations.invoke_tr(key.into()).to_string(),
        )
    });
    let hex = |colour: Color| edge_glow::to_hex(colour.red(), colour.green(), colour.blue());
    vec![
        ("background".into(), hex(theme.get_background())),
        ("surface".into(), hex(theme.get_surface())),
        ("elevated-surface".into(), hex(theme.get_elevated_surface())),
        ("accent".into(), hex(theme.get_accent())),
        ("text".into(), hex(theme.get_text())),
        ("muted-text".into(), hex(theme.get_muted_text())),
        ("border".into(), hex(theme.get_border())),
        ("selection".into(), hex(theme.get_selection())),
        ("hover-surface".into(), hex(theme.get_hover_surface())),
        ("pressed-surface".into(), hex(theme.get_pressed_surface())),
        ("on-accent".into(), hex(theme.get_on_accent())),
        ("base-mode".into(), theme.get_base_mode().to_string()),
    ]
    .into_iter()
    .chain(texts)
    .collect()
}

/// One plugin setting as the Slint row draws it: the control is chosen by the
/// kind, and a colour is shown as the accent while its value is empty.
pub(super) fn setting_entry(
    setting: &syntra_api::PluginSetting,
    accent: &str,
) -> PluginSettingEntry {
    use syntra_api::PluginSettingKind;
    let (kind, labels, values, min, max) = match &setting.kind {
        PluginSettingKind::Toggle => ("toggle", Vec::new(), Vec::new(), 0, 0),
        PluginSettingKind::Color => ("color", Vec::new(), Vec::new(), 0, 0),
        PluginSettingKind::Choice(options) => (
            "choice",
            options.iter().map(|option| option.label.clone()).collect(),
            options.iter().map(|option| option.value.clone()).collect(),
            0,
            0,
        ),
        PluginSettingKind::Number { min, max } => (
            "number",
            Vec::new(),
            Vec::new(),
            (*min).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            (*max).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        ),
    };
    let fallback = edge_glow::setting_colour(accent, (113, 131, 85));
    let (red, green, blue) = edge_glow::setting_colour(&setting.value, fallback);
    PluginSettingEntry {
        key: setting.key.clone().into(),
        label: setting.label.clone().into(),
        kind: kind.into(),
        value: setting.value.clone().into(),
        option_values: ModelRc::new(VecModel::from(
            values
                .into_iter()
                .map(Into::into)
                .collect::<Vec<slint::SharedString>>(),
        )),
        option_labels: ModelRc::new(VecModel::from(
            labels
                .into_iter()
                .map(Into::into)
                .collect::<Vec<slint::SharedString>>(),
        )),
        min,
        max,
        colour: slint::Color::from_rgb_u8(red, green, blue),
    }
}

/// Whether two setting rows would be drawn the same.
///
/// The option lists are separate models, and a model is compared by identity,
/// so two rows with the same options would never be equal; the text is.
fn same_setting(left: &PluginSettingEntry, right: &PluginSettingEntry) -> bool {
    let texts = |model: &ModelRc<slint::SharedString>| model.iter().collect::<Vec<_>>();
    left.key == right.key
        && left.label == right.label
        && left.kind == right.kind
        && left.value == right.value
        && left.min == right.min
        && left.max == right.max
        && left.colour == right.colour
        && texts(&left.option_values) == texts(&right.option_values)
        && texts(&left.option_labels) == texts(&right.option_labels)
}

/// Brings a model of settings to `wanted` in place, touching only rows that
/// differ, so the rows of an open popup keep their state.
fn sync_settings(model: &VecModel<PluginSettingEntry>, wanted: Vec<PluginSettingEntry>) {
    let shared = model.row_count().min(wanted.len());
    for (row, item) in wanted.iter().enumerate().take(shared) {
        if model
            .row_data(row)
            .is_none_or(|current| !same_setting(&current, item))
        {
            model.set_row_data(row, item.clone());
        }
    }
    for item in wanted.iter().skip(shared) {
        model.push(item.clone());
    }
    while model.row_count() > wanted.len() {
        model.remove(model.row_count() - 1);
    }
}

/// Sets the plugin list on the interface without replacing the model.
///
/// Replacing it recreates every card and every row of an open settings popup,
/// which closed the popup and sent the scroll position back to the top on each
/// press of `+`, since every change makes the daemon send the list again. A
/// plugin keeps the settings model its row already has, updated in place.
pub(super) fn sync(global: &AppState, mut wanted: Vec<PluginEntry>) {
    let existing = global.get_plugins();
    let Some(model) = existing.as_any().downcast_ref::<VecModel<PluginEntry>>() else {
        global.set_plugins(ModelRc::new(VecModel::from(wanted)));
        return;
    };
    for (index, entry) in wanted.iter_mut().enumerate() {
        let Some(current) = model.row_data(index) else {
            continue;
        };
        if current.id != entry.id {
            continue;
        }
        let Some(inner) = current
            .settings
            .as_any()
            .downcast_ref::<VecModel<PluginSettingEntry>>()
        else {
            continue;
        };
        sync_settings(inner, entry.settings.iter().collect());
        entry.settings = current.settings.clone();
    }
    // The rows are compared by value; the settings, now the same model, by
    // identity, which is equal exactly when nothing else differs.
    let count = model.row_count().min(wanted.len());
    for (row, entry) in wanted.iter().enumerate().take(count) {
        if model.row_data(row).as_ref() != Some(entry) {
            model.set_row_data(row, entry.clone());
        }
    }
    for entry in wanted.iter().skip(count) {
        model.push(entry.clone());
    }
    while model.row_count() > wanted.len() {
        model.remove(model.row_count() - 1);
    }
}

/// The role named by the interface, as the request needs it.
fn parse_role(text: &str) -> Option<PluginRole> {
    match text {
        "file-clipboard" => Some(PluginRole::FileClipboard),
        "mounted-file-receive" => Some(PluginRole::MountedFileReceive),
        "pointer" => Some(PluginRole::Pointer),
        _ => None,
    }
}

/// Connects the plugin callbacks of the page to requests.
///
/// Plugin management is entirely the daemon's decision; the interface only
/// forwards the intent and waits for the authoritative reply.
pub(super) fn bind(app: &super::AppWindow, tx: &mpsc::Sender<FrontendRequest>) {
    let global = app.global::<AppState>();
    {
        let requests = tx.clone();
        global.on_set_plugin_enabled(move |id, enabled| {
            let _ = requests.send(FrontendRequest::SetPluginEnabled {
                id: id.to_string(),
                enabled,
            });
        });
    }
    {
        let requests = tx.clone();
        global.on_restart_plugin(move |id| {
            let _ = requests.send(FrontendRequest::RestartPlugin { id: id.to_string() });
        });
    }
    {
        // A setting is changed by the daemon alone: this forwards the intent
        // and the reply, with the value that is in force, redraws the control.
        let requests = tx.clone();
        global.on_change_plugin_setting(move |id, key, value| {
            let _ = requests.send(FrontendRequest::SetPluginSetting {
                id: id.to_string(),
                key: key.to_string(),
                value: value.to_string(),
            });
        });
    }
    {
        let requests = tx.clone();
        global.on_change_plugin_setting_colour(move |id, key, colour| {
            let _ = requests.send(FrontendRequest::SetPluginSetting {
                id: id.to_string(),
                key: key.to_string(),
                value: edge_glow::to_hex(colour.red(), colour.green(), colour.blue()),
            });
        });
    }
    {
        let requests = tx.clone();
        global.on_refresh_plugins(move || {
            let _ = requests.send(FrontendRequest::QueryPlugins);
        });
    }
    {
        // Lends the plugin Syntra's look: its window uses Syntra's controls,
        // coloured as this window is right now.
        let requests = tx.clone();
        let weak = app.as_weak();
        global.on_open_plugin_settings(move |id| {
            let theme = weak.upgrade().map(|app| theme(&app)).unwrap_or_default();
            let _ = requests.send(FrontendRequest::OpenPluginSettings {
                id: id.to_string(),
                theme,
            });
        });
    }
    {
        // Approval is bound to the digest the card showed, so the daemon can
        // refuse it if the content changed in the meantime.
        let requests = tx.clone();
        global.on_approve_plugin(move |id, digest| {
            let _ = requests.send(FrontendRequest::ApprovePlugin {
                id: id.to_string(),
                digest: digest.to_string(),
            });
        });
    }
    {
        let requests = tx.clone();
        global.on_revoke_plugin_approval(move |id| {
            let _ = requests.send(FrontendRequest::RevokePluginApproval { id: id.to_string() });
        });
    }
    {
        let requests = tx.clone();
        global.on_uninstall_plugin(move |id| {
            let _ = requests.send(FrontendRequest::UninstallPlugin { id: id.to_string() });
        });
    }
    {
        let requests = tx.clone();
        global.on_review_plugin(move |path| {
            let path = path.trim().to_owned();
            if !path.is_empty() {
                let _ = requests.send(FrontendRequest::ReviewPlugin { path });
            }
        });
    }
    {
        let requests = tx.clone();
        global.on_install_plugin(move |path| {
            let path = path.trim().to_owned();
            if !path.is_empty() {
                let _ = requests.send(FrontendRequest::InstallPlugin { path });
            }
        });
    }
    {
        let requests = tx.clone();
        global.on_update_plugin(move |id, path| {
            let path = path.trim().to_owned();
            if !path.is_empty() {
                let _ = requests.send(FrontendRequest::UpdatePlugin {
                    id: id.to_string(),
                    path,
                });
            }
        });
    }
    {
        let requests = tx.clone();
        global.on_set_plugin_role_preference(move |role, id| {
            if let Some(role) = parse_role(role.as_str()) {
                let id = id.to_string();
                let _ = requests.send(FrontendRequest::SetPluginRolePreference {
                    role,
                    id: (!id.is_empty()).then_some(id),
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> PluginStatus {
        PluginStatus {
            id: "acme".into(),
            name: "Acme".into(),
            description: String::new(),
            version: "1".into(),
            protocol_version: 1,
            supported_protocol_version: 1,
            health: PluginHealth::Stopped,
            pid: None,
            restarts: 0,
            author: String::new(),
            homepage: None,
            source: None,
            update_url: None,
            license: None,
            mime_types: Vec::new(),
            bundled: false,
            settings: Vec::new(),
            owns_settings: false,
            manifest_path: String::new(),
            build_fingerprint: String::new(),
            daemon_build_fingerprint: String::new(),
            executable: "/x".into(),
            installed: true,
            enabled: false,
            running: false,
            error: None,
            trust: PluginTrust::Unapproved,
            origin: PluginOrigin::DropIn,
            digest: "d1".into(),
            approved_digest: None,
            args: Vec::new(),
            roles: Vec::new(),
            diagnostics: Vec::new(),
            removable: false,
            state: PluginState::Installed,
            pending: None,
            review: None,
            action_error: None,
        }
    }

    /// A daemon from before plugin approval sends none of the new fields, so
    /// they decode to their defaults. The card then offers only what that
    /// daemon understands.
    #[test]
    fn an_older_daemons_status_shows_no_lifecycle_controls() {
        let mut old = status();
        old.digest = String::new();
        let view = PluginView::from_status(&old);
        assert!(!view.manages_lifecycle);
        assert!(!view.can_approve && !view.can_revoke && !view.can_uninstall);
        assert!(!view.needs_approval);
        assert_eq!(view.health, "stopped");
    }

    #[test]
    fn an_unapproved_plugin_can_be_approved_but_not_withdrawn() {
        let view = PluginView::from_status(&status());
        assert!(view.manages_lifecycle);
        assert!(view.can_approve && !view.can_revoke);
        assert_eq!(view.trust, "unapproved");
        assert_eq!(view.state, "installed");
    }

    #[test]
    fn approval_needs_a_digest_to_bind_to() {
        let mut no_digest = status();
        no_digest.digest = String::new();
        no_digest.roles = vec![syntra_api::PluginRoleStatus {
            role: PluginRole::Pointer,
            active: false,
            preferred: false,
            unavailable: None,
        }];
        let view = PluginView::from_status(&no_digest);
        assert!(view.manages_lifecycle, "a role proves a lifecycle daemon");
        assert!(!view.can_approve, "nothing to approve without a digest");
    }

    #[test]
    fn an_approved_plugin_can_be_withdrawn_and_a_changed_one_must_be_reapproved() {
        let mut approved = status();
        approved.trust = PluginTrust::Approved;
        let view = PluginView::from_status(&approved);
        assert!(view.can_revoke && !view.can_approve);

        let mut changed = status();
        changed.trust = PluginTrust::ChangedSinceApproval;
        changed.enabled = true;
        let view = PluginView::from_status(&changed);
        assert!(view.can_approve && view.changed_since_approval && view.needs_approval);
    }

    /// A request the daemon has not accepted is pending: its controls are
    /// frozen, so the user cannot stack a second request on an unknown state.
    #[test]
    fn a_pending_request_freezes_the_lifecycle_controls() {
        let mut pending = status();
        pending.pending = Some(PluginPending::Approving);
        let view = PluginView::from_status(&pending);
        assert_eq!(view.pending, "approving");
        assert!(!view.can_approve && !view.can_revoke && !view.can_uninstall);
        // The trust itself is unchanged: pending is not accepted.
        assert_eq!(view.trust, "unapproved");
    }

    #[test]
    fn only_a_syntra_installed_package_offers_uninstall() {
        let mut dropped = status();
        dropped.removable = false;
        assert!(!PluginView::from_status(&dropped).can_uninstall);
        let mut installed = status();
        installed.removable = true;
        installed.origin = PluginOrigin::Installed;
        assert!(PluginView::from_status(&installed).can_uninstall);
    }

    #[test]
    fn a_bundled_plugin_needs_no_approval_and_is_not_removable() {
        let mut bundled = status();
        bundled.bundled = true;
        bundled.trust = PluginTrust::Bundled;
        bundled.origin = PluginOrigin::Bundled;
        let view = PluginView::from_status(&bundled);
        assert!(!view.can_approve && !view.can_revoke && !view.can_uninstall);
        assert!(!view.needs_approval);
        assert_eq!(view.origin, "bundled");
    }

    #[test]
    fn an_enabled_but_unapproved_plugin_says_it_is_waiting_for_approval() {
        let mut waiting = status();
        waiting.enabled = true;
        assert!(PluginView::from_status(&waiting).needs_approval);
    }

    #[test]
    fn a_review_candidate_is_never_treated_as_installed() {
        let mut candidate = status();
        candidate.installed = true;
        candidate.review = Some(syntra_api::PluginReview {
            path: "/p".into(),
            valid: true,
            error: None,
            unsupported: false,
            id: "acme".into(),
            name: "Acme".into(),
            version: "1".into(),
            author: String::new(),
            source: None,
            license: None,
            executable: String::new(),
            args: Vec::new(),
            mime_types: Vec::new(),
            roles: Vec::new(),
            pointer_events: false,
            requires_live_mount: false,
            digest: "d".into(),
            files: Vec::new(),
            replaces_existing: false,
        });
        let view = PluginView::from_status(&candidate);
        assert!(view.candidate);
        assert!(
            !view.can_approve && !view.can_revoke,
            "a candidate is not a plugin yet"
        );
    }

    #[test]
    fn every_role_and_state_has_a_stable_identifier() {
        for role in [
            PluginRole::FileClipboard,
            PluginRole::MountedFileReceive,
            PluginRole::Pointer,
        ] {
            assert_eq!(parse_role(role_id(role)), Some(role));
        }
        assert_eq!(parse_role("nonsense"), None);
        assert_eq!(state_id(PluginState::Unsupported), "unsupported");
        assert_eq!(pending_id(None), "");
    }
}
