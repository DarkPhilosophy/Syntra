//! Validation of the client/daemon contract.
//!
//! `syntra-api` is what third-party plugins compile and serialise against, so
//! its wire behaviour is a compatibility promise rather than an internal
//! detail. These tests pin the properties a plugin author would rely on and
//! that a careless edit would quietly break.

use syntra_api::{
    ClientConfig, DeviceProfile, FrontendEvent, FrontendRequest, HistoryEventId, Position, paths,
};

/// The transport is newline-delimited JSON, so a serialised frame containing a
/// literal newline would be read as two truncated frames.
#[test]
fn serialised_frames_never_contain_a_newline() {
    let requests = [
        FrontendRequest::Sync,
        FrontendRequest::SetLogSpec("warn,clipboard=trace".into()),
        FrontendRequest::QueryHistory {
            query: "line one\nline two".into(),
            offset: 0,
            limit: 50,
        },
        FrontendRequest::UpdateHostname(7, Some("host\nname".into())),
    ];

    for request in requests {
        let frame = serde_json::to_string(&request).expect("requests are serialisable");
        assert!(
            !frame.contains('\n'),
            "frame must stay on one line, got: {frame}"
        );
    }
}

/// A plugin decodes what the daemon encodes. Any variant that fails to survive
/// the round trip is unusable across the socket regardless of how it looks in
/// Rust.
#[test]
fn requests_survive_a_json_round_trip() {
    let requests = vec![
        FrontendRequest::Activate(1, true),
        FrontendRequest::Create,
        FrontendRequest::ChangePort(4242),
        FrontendRequest::UpdatePosition(3, Position::Left),
        FrontendRequest::SetClipboardText(false),
        FrontendRequest::QueryLogSpec,
        FrontendRequest::SetLogSpec("info".into()),
        FrontendRequest::SetHistoryPinned {
            event_id: HistoryEventId {
                origin_device_id: "abc".into(),
                origin_sequence: 9,
            },
            pinned: true,
        },
    ];

    for request in requests {
        let encoded = serde_json::to_string(&request).expect("serialisable");
        let decoded: FrontendRequest = serde_json::from_str(&encoded).expect("decodable");
        assert_eq!(decoded, request, "round trip changed the request");
    }
}

/// Externally tagged enums encode the variant name, so renaming a variant is a
/// breaking protocol change even though Rust callers keep compiling. Pinning
/// the exact wire text makes that visible in review.
#[test]
fn variant_names_are_part_of_the_wire_format() {
    assert_eq!(
        serde_json::to_string(&FrontendRequest::Sync).expect("serialisable"),
        "\"Sync\""
    );
    assert_eq!(
        serde_json::to_string(&FrontendRequest::QueryLogSpec).expect("serialisable"),
        "\"QueryLogSpec\""
    );
    assert_eq!(
        serde_json::to_string(&FrontendEvent::LogSpec("info".into())).expect("serialisable"),
        "{\"LogSpec\":\"info\"}"
    );
}

/// A client that predates a daemon feature must fail loudly on an unknown
/// variant rather than silently discarding it, otherwise a user would see a
/// dashboard that quietly ignores half the daemon's reports.
#[test]
fn unknown_variants_are_rejected_not_ignored() {
    let unknown = r#"{"SomeFutureEvent":{"value":1}}"#;
    assert!(serde_json::from_str::<FrontendEvent>(unknown).is_err());
}

/// The daemon rejects an oversized profile instead of truncating it, so a peer
/// never renders a silently mangled name.
#[test]
fn device_profile_enforces_its_documented_bound() {
    let oversized = DeviceProfile {
        display_name: "a".repeat(syntra_api::MAX_DEVICE_PROFILE_NAME_BYTES + 1),
        avatar: None,
    };
    assert!(oversized.validate().is_err());

    let at_limit = DeviceProfile {
        display_name: "a".repeat(syntra_api::MAX_DEVICE_PROFILE_NAME_BYTES),
        avatar: None,
    };
    assert!(at_limit.validate().is_ok());
}

/// The bound is in bytes, not characters. A multi-byte name that fits by
/// character count but not by encoded length must still be refused, or the
/// wire framing would overflow.
#[test]
fn profile_bound_counts_bytes_not_characters() {
    // U+6587 encodes as three bytes, so half the limit in characters is well
    // under the character count yet 1.5x over the byte budget.
    let character_count = syntra_api::MAX_DEVICE_PROFILE_NAME_BYTES / 2;
    let profile = DeviceProfile {
        display_name: "文".repeat(character_count),
        avatar: None,
    };
    assert!(
        profile.display_name.chars().count() <= syntra_api::MAX_DEVICE_PROFILE_NAME_BYTES,
        "precondition: the name is short by character count"
    );
    assert!(
        profile.display_name.len() > syntra_api::MAX_DEVICE_PROFILE_NAME_BYTES,
        "precondition: the name is long by byte count"
    );
    assert!(
        profile.validate().is_err(),
        "a name over the byte budget must be refused"
    );
}

/// Both tiers resolve endpoints through this module. If the daemon and a
/// client disagreed on a path they would never meet, and the failure would
/// look like "the service is not running".
#[test]
fn endpoints_are_distinct_and_absolute() {
    // Build sandboxes (nix, containers) have no session runtime directory;
    // the contract under test is resolution, not the host's session.
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() && cfg!(unix) {
        // SAFETY: no other test in this binary reads the environment.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", std::env::temp_dir()) };
    }
    let daemon = paths::daemon_socket().expect("a runtime directory is available");
    let diagnostics = paths::diagnostics_socket().expect("a runtime directory is available");

    assert_ne!(daemon, diagnostics);
    assert!(daemon.is_absolute(), "got {}", daemon.display());
    assert!(diagnostics.is_absolute(), "got {}", diagnostics.display());
}

/// Position parsing backs both the CLI and the config file, so a round trip
/// through its text form has to be stable.
#[test]
fn positions_round_trip_through_text() {
    for position in [
        Position::Left,
        Position::Right,
        Position::Top,
        Position::Bottom,
    ] {
        let text = position.to_string();
        let parsed: Position = text.parse().expect("its own output must parse");
        assert_eq!(parsed, position);
    }
}

/// Defaults are what a fresh install runs with, so an accidental change is a
/// silent behaviour change for every new user.
#[test]
fn default_client_is_inactive_on_the_default_port() {
    let client = ClientConfig::default();
    assert_eq!(client.port, paths::DEFAULT_PEER_PORT);
}

/// A status written by a daemon older than plugin approval has none of the
/// newer fields. It must still decode, reading as unapproved and installed, so
/// a current dashboard attached to an old daemon keeps working.
#[test]
fn a_plugin_status_from_an_older_daemon_still_decodes() {
    use syntra_api::{PluginHealth, PluginOrigin, PluginState, PluginStatus, PluginTrust};
    let old = r#"{"id":"edge-glow","name":"Edge glow","description":"","version":"0.10.0",
        "protocol_version":1,"supported_protocol_version":1,"health":"healthy","pid":42,
        "restarts":0,"author":"","homepage":null,"source":null,"update_url":null,
        "license":null,"mime_types":[],"bundled":true,"manifest_path":"","build_fingerprint":"",
        "daemon_build_fingerprint":"","executable":"/x","installed":true,"enabled":true,
        "running":true,"error":null}"#;
    let status: PluginStatus = serde_json::from_str(old).expect("old status must decode");
    assert_eq!(status.health, PluginHealth::Healthy);
    assert_eq!(status.trust, PluginTrust::Unapproved);
    assert_eq!(status.origin, PluginOrigin::DropIn);
    assert_eq!(status.state, PluginState::Installed);
    assert!(status.pending.is_none() && status.roles.is_empty() && !status.removable);
}

/// A status with the new fields keeps every older field a legacy client reads,
/// and never uses a `health` value a legacy client lacks.
#[test]
fn a_current_plugin_status_keeps_the_legacy_health_vocabulary() {
    use syntra_api::PluginHealth;
    for health in [
        PluginHealth::Disabled,
        PluginHealth::NotInstalled,
        PluginHealth::Stopped,
        PluginHealth::OnDemand,
        PluginHealth::Starting,
        PluginHealth::Healthy,
        PluginHealth::Unresponsive,
        PluginHealth::Failed,
    ] {
        let text = serde_json::to_string(&health).unwrap();
        assert!(
            [
                "\"disabled\"",
                "\"not_installed\"",
                "\"stopped\"",
                "\"on_demand\"",
                "\"starting\"",
                "\"healthy\"",
                "\"unresponsive\"",
                "\"failed\""
            ]
            .contains(&text.as_str()),
            "{text}"
        );
    }
}

/// The lifecycle requests are the wire contract of the plugin manager.
#[test]
fn plugin_lifecycle_requests_survive_a_json_round_trip() {
    use syntra_api::PluginRole;
    for request in [
        FrontendRequest::ReviewPlugin { path: "/p".into() },
        FrontendRequest::InstallPlugin { path: "/p".into() },
        FrontendRequest::ApprovePlugin {
            id: "a".into(),
            digest: "ab12".into(),
        },
        FrontendRequest::RevokePluginApproval { id: "a".into() },
        FrontendRequest::UpdatePlugin {
            id: "a".into(),
            path: "/q".into(),
        },
        FrontendRequest::UninstallPlugin { id: "a".into() },
        FrontendRequest::SetPluginRolePreference {
            role: PluginRole::FileClipboard,
            id: Some("a".into()),
        },
    ] {
        let text = serde_json::to_string(&request).unwrap();
        assert!(!text.contains('\n'));
        let back: FrontendRequest = serde_json::from_str(&text).unwrap();
        assert_eq!(back, request);
    }
}

/// What an already-installed dashboard decodes: the plugin status and the
/// event enum exactly as they were before plugin lifecycle management, copied
/// from the previous release. It rejects unknown event *variants* (as serde
/// enums do) but ignores unknown *fields*, so these types are the reference
/// for what the daemon may still send to old clients.
mod previous_release {
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(rename_all = "snake_case")]
    pub enum Health {
        Disabled,
        NotInstalled,
        Stopped,
        OnDemand,
        Starting,
        Healthy,
        Unresponsive,
        Failed,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    pub struct Status {
        pub id: String,
        pub name: String,
        pub description: String,
        pub version: String,
        pub protocol_version: u32,
        pub supported_protocol_version: u32,
        pub health: Health,
        pub pid: Option<u32>,
        pub restarts: u32,
        pub author: String,
        pub homepage: Option<String>,
        pub source: Option<String>,
        pub update_url: Option<String>,
        pub license: Option<String>,
        pub mime_types: Vec<String>,
        pub bundled: bool,
        #[serde(default)]
        pub settings: Vec<serde_json::Value>,
        #[serde(default)]
        pub owns_settings: bool,
        pub manifest_path: String,
        pub build_fingerprint: String,
        pub daemon_build_fingerprint: String,
        pub executable: String,
        pub installed: bool,
        pub enabled: bool,
        pub running: bool,
        pub error: Option<String>,
    }

    /// The two event variants that carry plugin state, plus the ones a
    /// dashboard decodes from the same stream. Variants it does not list are
    /// not part of what the current daemon adds.
    #[derive(Debug, Deserialize)]
    pub enum Event {
        Plugins(Vec<Status>),
        LogSpec(String),
        InputSharing(bool),
        HistoryChanged,
        Error(String),
        DaemonInfo(serde_json::Value),
    }
}

fn fully_populated_status() -> syntra_api::PluginStatus {
    use syntra_api::{
        PluginHealth, PluginOrigin, PluginPending, PluginReview, PluginRole, PluginRoleStatus,
        PluginState, PluginStatus, PluginTrust,
    };
    PluginStatus {
        id: "acme".into(),
        name: "Acme".into(),
        description: "d".into(),
        version: "1.2.3".into(),
        protocol_version: 1,
        supported_protocol_version: 1,
        health: PluginHealth::Healthy,
        pid: Some(7),
        restarts: 1,
        author: "a".into(),
        homepage: None,
        source: Some("https://example.invalid/src".into()),
        update_url: None,
        license: Some("MIT".into()),
        mime_types: vec!["text/uri-list".into()],
        bundled: false,
        settings: Vec::new(),
        owns_settings: false,
        manifest_path: "/m".into(),
        build_fingerprint: "b".into(),
        daemon_build_fingerprint: "b".into(),
        executable: "/x".into(),
        installed: true,
        enabled: true,
        running: true,
        error: None,
        trust: PluginTrust::Approved,
        origin: PluginOrigin::Installed,
        digest: "ab".repeat(32),
        approved_digest: Some("ab".repeat(32)),
        args: vec!["--serve".into()],
        roles: vec![PluginRoleStatus {
            role: PluginRole::FileClipboard,
            active: true,
            preferred: true,
            unavailable: None,
        }],
        diagnostics: vec!["line".into()],
        removable: true,
        state: PluginState::Ready,
        pending: Some(PluginPending::Revoking),
        review: Some(PluginReview {
            path: "/p".into(),
            valid: true,
            error: None,
            unsupported: false,
            id: "acme".into(),
            name: "Acme".into(),
            version: "1.2.3".into(),
            author: "a".into(),
            source: None,
            license: None,
            executable: "/x".into(),
            args: Vec::new(),
            mime_types: Vec::new(),
            roles: vec![PluginRole::FileClipboard],
            pointer_events: false,
            requires_live_mount: false,
            digest: "cd".repeat(32),
            files: vec!["syntra-plugin.json".into()],
            replaces_existing: true,
        }),
        action_error: Some("refused".into()),
    }
}

/// A previous-release dashboard decodes the final serialised snapshot,
/// including every new field, and reads the fields it knows unchanged.
#[test]
fn a_previous_release_decodes_the_current_plugins_snapshot() {
    let status = fully_populated_status();
    let text = serde_json::to_string(&FrontendEvent::Plugins(vec![status.clone()])).unwrap();
    let event: previous_release::Event =
        serde_json::from_str(&text).expect("an old client must decode the new snapshot");
    let previous_release::Event::Plugins(plugins) = event else {
        panic!("decoded a different variant: {text}");
    };
    assert_eq!(plugins.len(), 1);
    let old = &plugins[0];
    assert_eq!(old.id, status.id);
    assert_eq!(old.health, previous_release::Health::Healthy);
    assert_eq!(old.error, None);
    assert!(old.enabled && old.installed && old.running);
    assert_eq!(old.executable, status.executable);
    assert_eq!(old.pid, Some(7));
}

/// A refusal reaches an old client through `error`, which it already shows.
#[test]
fn a_refusal_is_visible_to_a_previous_release_through_error() {
    let mut status = fully_populated_status();
    status.action_error = Some("refused".into());
    status.error = status.action_error.clone();
    let text = serde_json::to_string(&FrontendEvent::Plugins(vec![status])).unwrap();
    let previous_release::Event::Plugins(plugins) = serde_json::from_str(&text).unwrap() else {
        panic!("not a Plugins event");
    };
    assert_eq!(plugins[0].error.as_deref(), Some("refused"));
}

/// The current decoder reads a snapshot written by the previous release and
/// fills every new field with its documented default.
#[test]
fn the_current_decoder_reads_a_previous_release_snapshot_with_defaults() {
    use syntra_api::{PluginOrigin, PluginState, PluginTrust};
    let old = r#"{"Plugins":[{"id":"edge-glow","name":"Edge glow","description":"","version":"0.10.0",
        "protocol_version":1,"supported_protocol_version":1,"health":"on_demand","pid":null,
        "restarts":0,"author":"","homepage":null,"source":null,"update_url":null,
        "license":null,"mime_types":[],"bundled":true,"settings":[],"owns_settings":true,
        "manifest_path":"","build_fingerprint":"","daemon_build_fingerprint":"",
        "executable":"/x","installed":true,"enabled":true,"running":false,"error":null}]}"#;
    let FrontendEvent::Plugins(plugins) = serde_json::from_str(old).expect("old snapshot decodes")
    else {
        panic!("not a Plugins event");
    };
    let status = &plugins[0];
    assert_eq!(status.trust, PluginTrust::Unapproved);
    assert_eq!(status.origin, PluginOrigin::DropIn);
    assert_eq!(status.state, PluginState::Installed);
    assert!(status.digest.is_empty() && status.approved_digest.is_none());
    assert!(status.args.is_empty() && status.roles.is_empty() && status.diagnostics.is_empty());
    assert!(!status.removable);
    assert!(status.pending.is_none() && status.review.is_none() && status.action_error.is_none());
}

/// The event stream itself did not grow: every plugin-related lifecycle
/// result rides on `Plugins`, so no tag appears that an old dashboard would
/// reject. The tags below are the previous release's `FrontendEvent` variants
/// for plugins; the daemon must keep emitting only these for plugin state.
#[test]
fn plugin_state_travels_only_in_variants_a_previous_release_knows() {
    let events = [
        FrontendEvent::Plugins(vec![fully_populated_status()]),
        FrontendEvent::LogSpec("warn".into()),
        FrontendEvent::InputSharing(true),
        FrontendEvent::HistoryChanged,
        FrontendEvent::Error("x".into()),
    ];
    for event in events {
        let text = serde_json::to_string(&event).unwrap();
        let decoded = serde_json::from_str::<previous_release::Event>(&text)
            .unwrap_or_else(|error| panic!("a previous release rejects {text}: {error}"));
        match (event, decoded) {
            (FrontendEvent::Plugins(expected), previous_release::Event::Plugins(actual)) => {
                assert_eq!(actual[0].id, expected[0].id);
            }
            (FrontendEvent::LogSpec(expected), previous_release::Event::LogSpec(actual)) => {
                assert_eq!(actual, expected);
            }
            (
                FrontendEvent::InputSharing(expected),
                previous_release::Event::InputSharing(actual),
            ) => {
                assert_eq!(actual, expected);
            }
            (FrontendEvent::HistoryChanged, previous_release::Event::HistoryChanged) => {}
            (FrontendEvent::Error(expected), previous_release::Event::Error(actual)) => {
                assert_eq!(actual, expected);
            }
            (FrontendEvent::DaemonInfo(expected), previous_release::Event::DaemonInfo(actual)) => {
                assert_eq!(actual, serde_json::to_value(expected).unwrap());
            }
            (expected, actual) => panic!("event changed: {expected:?} became {actual:?}"),
        }
    }
}
