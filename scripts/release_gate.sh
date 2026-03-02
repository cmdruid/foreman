#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DOCS_ROOT="$REPO_ROOT"

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found" >&2
  exit 1
fi

echo "[release-gate] verifying workflow pinning"
if [[ -x "$REPO_ROOT/scripts/verify_action_shas.sh" ]]; then
  "$REPO_ROOT/scripts/verify_action_shas.sh"
else
  echo "missing verify_action_shas.sh" >&2
  exit 1
fi

echo "[release-gate] running release metadata test"
cd "$REPO_ROOT"
cargo test --test release --quiet

echo "[release-gate] validating release documentation anchors"
for pattern in \
  "Live mock demo (release gate)" \
  "result: success" \
  "mixed mode" \
  "before tagging" \
  "contrib/run_mock_demo.sh"; do
  if ! rg -q "$pattern" "$DOCS_ROOT"/TESTING.md "$DOCS_ROOT"/RELEASES.md "$DOCS_ROOT"/README.md "$DOCS_ROOT"/docs/manual.md; then
    echo "required documentation pattern not found: $pattern" >&2
    exit 1
  fi
done

if [[ "${RUN_LIVE_MOCK_SMOKE:-false}" == "true" ]]; then
  echo "[release-gate] running live mixed mock smoke"
  CODEX_BIN="${CODEX_BIN:-$(command -v codex 2>/dev/null || true)}"
  if [[ -z "$CODEX_BIN" ]]; then
    echo "RUN_LIVE_MOCK_SMOKE is true but CODEX_BIN is not set and codex is not on PATH" >&2
    exit 1
  fi
  FOREMAN_BIN="${FOREMAN_BIN:-$REPO_ROOT/target/release/codex-foreman}"
  if [[ ! -x "$FOREMAN_BIN" ]]; then
    echo "[release-gate] building release binary"
    cargo build --locked --release --bin codex-foreman
  fi

  export CODEX_BIN="$CODEX_BIN"
  export FOREMAN_BIN="$FOREMAN_BIN"
  export JOB_TIMEOUT_MS="${JOB_TIMEOUT_MS:-300000}"
  export JOB_POLL_MS="${JOB_POLL_MS:-500}"
  export WORKTREE_CLEANUP="${WORKTREE_CLEANUP:-true}"
  export RUN_MOCK_DEMO_MODE="${RUN_MOCK_DEMO_MODE:-mixed}"

  if ! "$REPO_ROOT/contrib/run_mock_demo.sh"; then
    echo "live mock demo failed" >&2
    exit 1
  fi
else
  echo "[release-gate] RUN_LIVE_MOCK_SMOKE=false; skipping live demo."
fi

echo "[release-gate] complete"
