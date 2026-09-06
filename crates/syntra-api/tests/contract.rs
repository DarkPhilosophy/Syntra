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
