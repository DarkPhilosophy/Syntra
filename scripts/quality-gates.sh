#!/usr/bin/env bash
# Runs the repository's quality gates locally, in the order CI runs them, so a
# change can be checked before it is pushed.
#
#   scripts/quality-gates.sh            every gate this machine can run
#   scripts/quality-gates.sh fmt check  only the named gates
#
# Gates: fmt, architecture, scripts, check, test, clippy, dependencies.
# dependencies requires cargo-deny >= 0.20; CARGO_DENY may override its executable.
#
# The Linux backend feature set is the same one CI uses and needs the system
# libraries listed in .github/workflows/rust.yml. Other platforms build
# without the backend features. Everything runs with --locked: a gate that
# silently rewrites Cargo.lock is not checking the lockfile anyone else gets.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

features=()
if [ "$(uname -s)" = "Linux" ]; then
  features=(--features
    "syntra-daemon/layer_shell_capture,syntra-daemon/x11_capture,syntra-daemon/libei_capture,syntra-daemon/wlroots_emulation,syntra-daemon/libei_emulation,syntra-daemon/rdp_emulation,syntra-daemon/uinput_emulation,syntra-daemon/x11_emulation")
fi

gate_fmt() { cargo fmt --check; }

# Reads cargo metadata, so it is the fastest signal that a boundary was crossed.
# (`cargo test` still compiles the daemon crate's own dependencies, which on
# Linux include the native capture crates: the system libraries are needed.)
gate_architecture() { cargo test --locked -p syntra-daemon --test architecture; }

# Generator and release-script tests; they use only Node's built-in runner.
gate_scripts() {
  node --test scripts/test/*.test.js
  node scripts/sync-licenses.js --check
}

gate_check() { cargo check --locked --workspace --all-targets ${features[@]+"${features[@]}"}; }
gate_test() { cargo test --locked --workspace ${features[@]+"${features[@]}"}; }

# The dependency-free contract crates are held at zero warnings; the rest
# report. `-D warnings` reaches path dependencies, so only crates without
# internal dependencies can be gated this way.
gate_clippy() {
  cargo clippy --locked --all-targets -p syntra-api -p syntra-plugin-api -p syntra-log -- -D warnings
  cargo clippy --locked --workspace --all-targets ${features[@]+"${features[@]}"}
}

gate_dependencies() {
  local audit="${CARGO_DENY:-cargo-deny}"
  command -v "$audit" >/dev/null 2>&1 || { echo "cargo-deny >= 0.20 is required for the dependencies gate" >&2; return 127; }
  "$audit" --locked check
}

gates=("$@")
if [ ${#gates[@]} -eq 0 ]; then
  gates=(fmt architecture scripts check test clippy)
fi

for gate in "${gates[@]}"; do
  if ! declare -F "gate_${gate}" >/dev/null; then
    echo "unknown gate: ${gate} (fmt architecture scripts check test clippy dependencies)" >&2
    exit 2
  fi
  echo "==> ${gate}"
  "gate_${gate}"
done
