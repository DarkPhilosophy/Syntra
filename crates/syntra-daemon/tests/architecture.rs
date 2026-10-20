//! Executable architecture rules.
//!
//! The three-tier split is the property that makes the daemon installable
//! without a desktop, the dashboard usable without a daemon, and a stable
//! surface available to plugins. Nothing in the type system enforces it: a
//! single convenient `use` in the wrong crate collapses it silently and is
//! very hard to undo once code has grown across the boundary.
//!
//! These tests read `cargo metadata`, so they see the real resolved graph
//! rather than what the manifests appear to say.

use std::collections::{BTreeSet, VecDeque};
use std::process::Command;

/// One workspace crate and the workspace crates it depends on.
struct Package {
    name: String,
    deps: Vec<String>,
}

/// Resolves the workspace dependency graph, restricted to local crates.
fn workspace_packages() -> Vec<Package> {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata runs inside the workspace");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata emits JSON");

    let packages = metadata["packages"]
        .as_array()
        .expect("metadata contains packages");
    let local: BTreeSet<String> = packages
        .iter()
        .map(|package| {
            package["name"]
                .as_str()
                .expect("package has a name")
                .to_owned()
        })
        .collect();

    packages
        .iter()
        .map(|package| {
            let name = package["name"]
                .as_str()
                .expect("package has a name")
                .to_owned();
            let deps = package["dependencies"]
                .as_array()
                .expect("package has a dependency list")
                .iter()
                // On Android the app is one process that hosts the service,
                // so its target-only dependencies are outside these rules.
                .filter(|dep| {
                    !dep["target"]
                        .as_str()
                        .is_some_and(|target| target.contains("android"))
                })
                .filter_map(|dep| dep["name"].as_str())
                .filter(|dep| local.contains(*dep))
                .map(str::to_owned)
                .collect();
            Package { name, deps }
        })
        .collect()
}

/// Every workspace crate reachable from `root`, including `root` itself.
fn reachable_from(packages: &[Package], root: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([root.to_owned()]);
    while let Some(current) = queue.pop_front() {
        if !seen.insert(current.clone()) {
            continue;
        }
        if let Some(package) = packages.iter().find(|p| p.name == current) {
            queue.extend(package.deps.iter().cloned());
        }
    }
    seen
}

/// The daemon must be installable on a headless machine.
///
/// Depending on the presentation crate would drag Slint, a windowing backend
/// and a tray into a service that runs before any session exists.
#[test]
fn daemon_does_not_depend_on_the_user_interface() {
    let packages = workspace_packages();
    let reachable = reachable_from(&packages, "syntra-daemon");

    for forbidden in ["syntra-ui", "syntra-app"] {
        assert!(
            !reachable.contains(forbidden),
            "syntra-daemon must not depend on {forbidden}; \
             the daemon has to run without a desktop session. Reached: {reachable:?}"
        );
    }
}

/// The dashboard must be installable and runnable without the service stack.
///
/// Linking the core would also mean linking capture and emulation backends,
/// which is exactly what makes a client unable to start when a platform
/// backend is missing.
#[test]
fn dashboard_does_not_depend_on_the_daemon_core() {
    let packages = workspace_packages();
    let reachable = reachable_from(&packages, "syntra-app");

    for forbidden in [
        "syntra-core",
        "syntra-input-capture",
        "syntra-input-emulation",
    ] {
        assert!(
            !reachable.contains(forbidden),
            "syntra-app must not depend on {forbidden}; \
             the dashboard is a client and must start without the service stack. \
             Reached: {reachable:?}"
        );
    }
}

/// Both tiers must meet on the shared contract rather than on private types.
///
/// If either side stopped depending on `syntra-api`, it would be talking to
/// the other through something that is not the documented plugin surface.
#[test]
fn both_tiers_share_the_control_contract() {
    let packages = workspace_packages();

    for tier in ["syntra-daemon", "syntra-app"] {
        assert!(
            reachable_from(&packages, tier).contains("syntra-api"),
            "{tier} must reach syntra-api: it is the only sanctioned boundary \
             between the daemon and its clients"
        );
    }
}

/// The contract crate is what third parties compile against.
///
/// Pulling in the core, the UI or a backend would force a plugin author to
/// build the entire application to speak to it.
#[test]
fn the_contract_crate_stays_dependency_light() {
    let packages = workspace_packages();
    let reachable = reachable_from(&packages, "syntra-api");

    let unexpected: Vec<_> = reachable
        .iter()
        .filter(|name| name.as_str() != "syntra-api")
        .collect();
    assert!(
        unexpected.is_empty(),
        "syntra-api must not depend on other workspace crates so plugins can \
         compile against it alone, but it reaches {unexpected:?}"
    );
}

/// The core is a service library, not a command-line application.
///
/// Argument parsing and subcommand dispatch belong to the binary that owns
/// the command line. If the core reached the CLI frontend, every host of the
/// service (including the in-process Android service) would drag in a client
/// and see a dependency pointing from the engine back to one of its clients.
#[test]
fn the_core_does_not_depend_on_the_cli_frontend() {
    let packages = workspace_packages();
    let reachable = reachable_from(&packages, "syntra-core");

    for forbidden in ["syntra-cli", "syntra-ui", "syntra-app"] {
        assert!(
            !reachable.contains(forbidden),
            "syntra-core must not depend on {forbidden}; the core receives typed \
             configuration overrides from its host. Reached: {reachable:?}"
        );
    }
}

/// The presentation crate talks to the daemon, not to its storage.
///
/// History is persisted by the daemon. A UI that linked the store could open
/// the database directly and bypass the control contract, and it would pull
/// SQLite into every desktop client.
#[test]
fn the_user_interface_does_not_depend_on_the_store() {
    let packages = workspace_packages();

    for tier in ["syntra-ui", "syntra-app"] {
        let reachable = reachable_from(&packages, tier);
        assert!(
            !reachable.contains("syntra-store"),
            "{tier} must not depend on syntra-store; history reaches the UI as \
             syntra-api DTOs. Reached: {reachable:?}"
        );
    }
}

/// The plugin protocol crate must be usable by any plugin on its own.
///
/// It may not reach the application, the core, or the dashboard, otherwise a
/// plugin author would have to build the product to speak the protocol.
#[test]
fn the_plugin_protocol_crate_stays_neutral() {
    let packages = workspace_packages();
    let reachable = reachable_from(&packages, "syntra-plugin-api");

    for forbidden in [
        "syntra-core",
        "syntra-ui",
        "syntra-app",
        "syntra-daemon",
        "syntra-store",
        "syntra-cli",
    ] {
        assert!(
            !reachable.contains(forbidden),
            "syntra-plugin-api must not reach {forbidden}. Reached: {reachable:?}"
        );
    }
}

/// Bundled plugins are separate products that use the published surface.
///
/// A plugin may depend on the protocol crate (and the wire protocol where it
/// serves peers), never on the daemon core or the dashboard crates.
#[test]
fn bundled_plugins_use_only_the_published_surface() {
    let packages = workspace_packages();

    for plugin in packages
        .iter()
        .filter(|package| package.name.starts_with("syntra-plugin-"))
        .filter(|package| package.name != "syntra-plugin-api")
    {
        let reachable = reachable_from(&packages, &plugin.name);
        for forbidden in [
            "syntra-core",
            "syntra-ui",
            "syntra-app",
            "syntra-daemon",
            "syntra-store",
        ] {
            assert!(
                !reachable.contains(forbidden),
                "{} must not reach {forbidden}; plugins build against \
                 syntra-plugin-api only. Reached: {reachable:?}",
                plugin.name
            );
        }
    }
}

/// The edge-glow settings window may only import the dashboard's declared UI
/// surface (`kit.slint`), never its private files.
///
/// The dashboard can then reorganise its own UI without breaking a plugin
/// that is built and shipped separately.
#[test]
fn edge_glow_imports_only_the_declared_ui_surface() {
    let plugin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/edge-glow");
    let mut stack = vec![plugin.join("ui")];
    let mut checked = 0;
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory).expect("edge-glow ui directory is readable") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("slint") {
                continue;
            }
            checked += 1;
            let source = std::fs::read_to_string(&path).expect("slint source is readable");
            for line in source
                .lines()
                .filter(|line| line.trim_start().starts_with("import"))
            {
                assert!(
                    !line.contains("syntra-ui") && !line.contains("../../crates"),
                    "{} imports a private dashboard file: {line}",
                    path.display()
                );
                if line.contains("syntra-kit") {
                    assert!(
                        line.contains("@syntra-kit/kit.slint"),
                        "{} must import only @syntra-kit/kit.slint: {line}",
                        path.display()
                    );
                }
            }
        }
    }
    assert!(checked > 0, "no edge-glow .slint sources were inspected");

    let build = std::fs::read_to_string(plugin.join("build.rs")).expect("build script is readable");
    assert!(
        build.contains("../../crates/syntra-ui/ui\""),
        "the single sanctioned UI path is the kit directory"
    );
    assert_eq!(
        build.matches("crates/syntra-ui").count(),
        1,
        "build.rs may name the dashboard UI directory exactly once"
    );
}

/// The peer protocol is a wire format, nothing more.
///
/// It may use the input event types it carries, and no other workspace
/// crate: depending on the daemon, the UI or the plugin protocol would tie the
/// bytes two machines exchange to the release cycle of an unrelated part.
#[test]
fn the_peer_protocol_depends_only_on_the_input_event_types() {
    let packages = workspace_packages();
    let reachable = reachable_from(&packages, "syntra-proto");
    let unexpected: Vec<_> = reachable
        .iter()
        .filter(|name| !matches!(name.as_str(), "syntra-proto" | "syntra-input-event"))
        .collect();
    assert!(
        unexpected.is_empty(),
        "syntra-proto must depend on no workspace crate but syntra-input-event, \
         yet it reaches {unexpected:?}"
    );
}
