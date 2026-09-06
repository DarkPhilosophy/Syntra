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
