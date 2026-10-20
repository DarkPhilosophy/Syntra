//! A third-party plugin, from a directory on disk to a running provider.
//!
//! These tests follow the path the service takes: discovery reads the package,
//! the registry decides whether it may run, the supervisor launches exactly
//! that plan, and the plugin's frames enter the same transfer state machine
//! the bundled clipboard plugin uses. The plugin is the packaged example in
//! `plugins/examples/file-provider`, not a stub written for the test, so the
//! example is itself guaranteed to run.
//!
//! The service loop is not built here (it needs capture, emulation and
//! sockets); everything it calls on the plugin side is.

use super::{
    registry::{PluginRegistry, Refusal},
    supervisor::{AdapterId, AdapterProcessManager, ManagerCommand, ManagerEvent},
};
use crate::transfer_manager::{TransferAction, TransferFrontendEvent, TransferState};
use std::{path::Path, time::Duration};
use syntra_api::{PluginState, PluginTrust};
use syntra_plugin_api::{Message, PluginRole};
use syntra_proto::ProtoEvent;
use tokio::sync::mpsc;

const EXAMPLE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../plugins/examples/file-provider"
);

/// Copies the packaged example into a directory the test owns, so the test can
/// change files without touching the repository.
fn copy_example(into: &Path) {
    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap().flatten() {
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
            }
        }
    }
    copy(Path::new(EXAMPLE), into);
}

struct Daemon {
    root: tempfile::TempDir,
}

impl Daemon {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("daemon")).unwrap();
        std::fs::create_dir_all(root.path().join("plugins")).unwrap();
        copy_example(&root.path().join("plugins/example-file-provider"));
        Self { root }
    }

    fn registry(&self) -> PluginRegistry {
        PluginRegistry::discover(
            &self.root.path().join("daemon/syntra-daemon"),
            Some(self.root.path().join("plugins")),
            Some(self.root.path().join("store")),
        )
        .with_state_dir(self.root.path().join("state"))
    }
}

async fn next(events: &mut mpsc::Receiver<ManagerEvent>) -> ManagerEvent {
    tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("no event within 10s")
        .expect("supervisor stopped")
}

const ID: &str = "example-file-provider";

#[test]
fn the_packaged_example_is_a_valid_third_party_package_with_the_file_clipboard_role() {
    let daemon = Daemon::new();
    let registry = daemon.registry();
    let listed = registry
        .snapshot()
        .into_iter()
        .find(|plugin| plugin.id == ID)
        .expect("the example is discovered");
    assert_eq!(listed.trust, PluginTrust::Unapproved);
    assert!(!listed.bundled && !listed.removable);
    assert_eq!(listed.args, ["--serve"]);
    assert!(
        listed
            .roles
            .iter()
            .any(|role| { role.role == syntra_api::PluginRole::FileClipboard && !role.active })
    );
    assert!(!listed.digest.is_empty());
}

/// Nothing runs until the user approves the digest they were shown, and the
/// provider is selected by capability, not by name.
#[test]
fn approval_makes_a_new_id_the_active_file_clipboard_provider() {
    let daemon = Daemon::new();
    let mut registry = daemon.registry();
    assert_eq!(registry.active_provider(PluginRole::FileClipboard), None);

    registry.set_enabled(ID, true);
    assert_eq!(
        registry.launch_plan(ID, None).unwrap_err(),
        Refusal::NotApproved
    );
    assert_eq!(registry.active_provider(PluginRole::FileClipboard), None);

    let shown = registry
        .snapshot()
        .into_iter()
        .find(|plugin| plugin.id == ID)
        .unwrap()
        .digest;
    registry.approve(ID, &shown).unwrap();
    assert_eq!(
        registry
            .active_provider(PluginRole::FileClipboard)
            .as_deref(),
        Some(ID)
    );
    let state = registry
        .snapshot()
        .into_iter()
        .find(|plugin| plugin.id == ID)
        .unwrap()
        .state;
    assert_eq!(state, PluginState::Enabled);
}

/// The whole route: approve, launch the registry's plan through the real
/// supervisor, receive the plugin's copy announcement, and see it accepted by
/// the transfer state machine under the plugin's own id.
#[cfg(unix)]
#[tokio::test]
async fn an_approved_third_party_provider_runs_and_its_frames_reach_the_transfer_state() {
    use std::os::unix::fs::PermissionsExt;
    let daemon = Daemon::new();
    // The example only announces itself; give this copy a body that also
    // announces a copied file, as a real provider would after a user copies.
    // The file exists, because the daemon builds its offer from the real disk.
    let copied = daemon.root.path().join("copied.txt");
    std::fs::write(&copied, b"abc").unwrap();
    let executable = daemon
        .root
        .path()
        .join("plugins/example-file-provider/bin/example-file-provider");
    let copy = format!(
        r#"{{"type":"copy_manifest","data":{{"transfer_id":"copy-1","operation":"copy","entries":[{{"uri":"file://{}","kind":"file","size":3}}]}}}}"#,
        copied.display()
    );
    let script = std::fs::read_to_string(&executable).unwrap();
    let with_copy = script.replace(
        "# Frames from the daemon are ignored",
        &format!("printf '%s\\n' '{copy}'\n# Frames from the daemon are ignored"),
    );
    std::fs::write(&executable, with_copy).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut registry = daemon.registry();
    let shown = registry
        .snapshot()
        .into_iter()
        .find(|plugin| plugin.id == ID)
        .unwrap()
        .digest;
    registry.set_enabled(ID, true);
    registry.approve(ID, &shown).unwrap();
    let plan = registry
        .launch_plan(ID, Some(PluginRole::FileClipboard))
        .expect("an approved plugin may launch");
    assert_eq!(
        plan.spec.args,
        ["--serve"],
        "the manifest's argv is what is launched"
    );

    let (manager, mut events) = AdapterProcessManager::start();
    manager
        .try_send(ManagerCommand::StartPlugin {
            id: ID.into(),
            spec: plan.spec,
            settings: plan.settings,
        })
        .unwrap();

    let mut state = TransferState::<u8>::new();
    state.set_file_clipboard_adapter(ID);
    let mut announced = None;
    loop {
        match next(&mut events).await {
            ManagerEvent::Started(adapter, pid) => {
                registry.set_starting(adapter.plugin(), pid);
            }
            ManagerEvent::Ready(adapter, _, grant) => {
                assert_eq!(adapter, AdapterId::Resident(ID.into()));
                assert!(grant.allows(PluginRole::FileClipboard));
                registry.set_running(adapter.plugin(), true);
            }
            ManagerEvent::Message {
                adapter,
                message: Message::CopyManifest(manifest),
            } => {
                assert_eq!(
                    adapter.plugin(),
                    ID,
                    "the frame is attributed to its plugin"
                );
                announced = Some(manifest);
                break;
            }
            ManagerEvent::Exited { status, .. } => panic!("the provider exited: {status}"),
            ManagerEvent::Rejected { reason, .. } => panic!("rejected: {reason}"),
            _ => {}
        }
    }
    let manifest = announced.expect("the provider announced a copy");
    assert_eq!(manifest.transfer_id, "copy-1");
    let listed = registry
        .snapshot()
        .into_iter()
        .find(|plugin| plugin.id == ID)
        .unwrap();
    assert_eq!(listed.state, PluginState::Ready);
    assert!(listed.running && listed.pid.is_some());

    // What the service does with the announcement: build the offer from the
    // announced URIs on disk, then register it with the state machine under
    // the provider's own id, for one clipboard peer.
    let uri_text = manifest
        .entries
        .iter()
        .map(|entry| entry.uri.as_str())
        .collect::<Vec<_>>()
        .join("\r\n");
    let mut offer = crate::file_transfer::FileOffer::from_uri_list(1, &uri_text)
        .expect("the announced file is on disk");
    let wire_id = 7;
    offer.transfer_id = wire_id;
    let actions = state
        .local_copy_manifest(1u8, wire_id, ID.to_owned(), manifest, offer)
        .expect("the state machine accepts a copy from the active provider");

    // Consumer-visible effect: the peer is sent exactly the announced file.
    let sent: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            TransferAction::Peer {
                peer,
                event:
                    ProtoEvent::ClipboardManifest {
                        transfer_id,
                        entries,
                    },
            } => Some((*peer, *transfer_id, entries.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(sent.len(), 1, "{actions:?}");
    let (peer, announced_wire_id, entries) = &sent[0];
    assert_eq!((*peer, *announced_wire_id), (1u8, wire_id));
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "copied.txt");
    assert_eq!(entries[0].size, 3);

    // Attribution: the transfer is keyed by the provider's id and the
    // provider's own transfer id, and by nothing else.
    assert_eq!(state.owners_for_adapter(ID, "copy-1").len(), 1);
    assert!(
        state
            .owners_for_adapter("some-other-plugin", "copy-1")
            .is_empty(),
        "another plugin does not own the transfer"
    );

    // Ending the transfer under the wrong plugin id is refused and leaves it
    // in place; under the provider's id it cancels the peer's offer and
    // reports the cancellation to clients.
    let wrong = state.adapter_released(
        "some-other-plugin",
        syntra_plugin_api::Released {
            transfer_id: "copy-1".into(),
        },
    );
    assert!(wrong.is_err());
    assert_eq!(state.owners_for_adapter(ID, "copy-1").len(), 1);
    let released = state
        .adapter_released(
            ID,
            syntra_plugin_api::Released {
                transfer_id: "copy-1".into(),
            },
        )
        .expect("the provider's own release is applied");
    assert!(released.iter().any(|action| matches!(
        action,
        TransferAction::Peer {
            event: ProtoEvent::ClipboardTransferCancel { transfer_id, .. },
            ..
        } if *transfer_id == wire_id
    )));
    assert!(released.iter().any(|action| matches!(
        action,
        TransferAction::Frontend(TransferFrontendEvent::Cancelled { .. })
    )));
    assert!(state.owners_for_adapter(ID, "copy-1").is_empty());
    manager.shutdown().await.unwrap();
}

/// A provider whose files change after approval is not launched again, whether
/// the launch is a restart, a relaunch or a fresh start.
#[cfg(unix)]
#[test]
fn a_provider_changed_after_approval_needs_a_new_approval() {
    let daemon = Daemon::new();
    let mut registry = daemon.registry();
    let shown = registry
        .snapshot()
        .into_iter()
        .find(|plugin| plugin.id == ID)
        .unwrap()
        .digest;
    registry.set_enabled(ID, true);
    registry.approve(ID, &shown).unwrap();
    assert!(registry.launch_plan(ID, None).is_ok());

    let executable = daemon
        .root
        .path()
        .join("plugins/example-file-provider/bin/example-file-provider");
    std::fs::write(&executable, "#!/bin/sh\necho tampered\n").unwrap();
    assert_eq!(
        registry.launch_plan(ID, None).unwrap_err(),
        Refusal::Changed
    );
    assert_eq!(registry.active_provider(PluginRole::FileClipboard), None);
    let status = registry
        .snapshot()
        .into_iter()
        .find(|plugin| plugin.id == ID)
        .unwrap();
    assert_eq!(status.trust, PluginTrust::ChangedSinceApproval);
}

/// Competing providers: the user's explicit choice wins, deterministically,
/// and withdrawing it returns to the default.
#[cfg(unix)]
#[test]
fn an_explicit_preference_selects_between_competing_providers() {
    use std::os::unix::fs::PermissionsExt;
    let daemon = Daemon::new();
    // A second provider with another id and the same capabilities.
    let second = daemon.root.path().join("plugins/aaa-second");
    copy_example(&second);
    let manifest = std::fs::read_to_string(second.join("syntra-plugin.json"))
        .unwrap()
        .replace(&format!("\"id\": \"{ID}\""), "\"id\": \"aaa-second\"");
    std::fs::write(second.join("syntra-plugin.json"), manifest).unwrap();
    std::fs::set_permissions(
        second.join("bin/example-file-provider"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let mut registry = daemon.registry();
    for id in [ID, "aaa-second"] {
        let digest = registry
            .snapshot()
            .into_iter()
            .find(|plugin| plugin.id == id)
            .unwrap()
            .digest;
        registry.set_enabled(id, true);
        registry.approve(id, &digest).unwrap();
    }
    // Neither is bundled, so the smallest id is the deterministic default.
    assert_eq!(
        registry
            .active_provider(PluginRole::FileClipboard)
            .as_deref(),
        Some("aaa-second")
    );
    registry
        .set_role_preference(syntra_api::PluginRole::FileClipboard, Some(ID))
        .unwrap();
    assert_eq!(
        registry
            .active_provider(PluginRole::FileClipboard)
            .as_deref(),
        Some(ID)
    );
    registry
        .set_role_preference(syntra_api::PluginRole::FileClipboard, None)
        .unwrap();
    assert_eq!(
        registry
            .active_provider(PluginRole::FileClipboard)
            .as_deref(),
        Some("aaa-second")
    );
}

/// The state machine is told which plugin provides the file clipboard, and a
/// release is looked up under the process that sent it, so a third-party
/// provider is keyed by its own id like any other.
#[test]
fn the_state_machine_keys_a_third_party_provider_by_its_own_id() {
    use crate::transfer_manager::TransferStateError;
    let mut state = TransferState::<u8>::new();
    state.set_file_clipboard_adapter(ID);
    assert_eq!(state.file_clipboard_adapter(), ID);
    // A release nobody registered under this provider is refused, naming the
    // provider, rather than being applied to some other plugin's transfer.
    let error = state
        .adapter_released(
            ID,
            syntra_plugin_api::Released {
                transfer_id: "never-registered".into(),
            },
        )
        .unwrap_err();
    assert!(
        matches!(
            error,
            TransferStateError::UnknownAdapterTransfer { ref adapter_id, .. } if adapter_id.as_str() == ID
        ),
        "{error:?}"
    );
}
