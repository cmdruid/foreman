#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DOCS_ROOT="$REPO_ROOT"

contains_text() {
  local pattern="$1"
  shift

  if command -v rg >/dev/null 2>&1; then
    rg -Fq "$pattern" "$@"
  else
    grep -Fq "$pattern" "$@" || return 1
  fi
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
  cat <<'EOF'
Usage: scripts/release_gate.sh

Run release-readiness checks for foreman:
- workflow action pinning verification
- release metadata/version check
- documentation anchor checks
- optional live mixed mock smoke

Environment:
- RUN_LIVE_MOCK_SMOKE=true (default: false)
- CODEX_BIN (required when live smoke enabled)
- FOREMAN_BIN (defaults to target/release/foreman; may be set explicitly)
- JOB_TIMEOUT_MS (default: 300000)
- JOB_POLL_MS (default: 500)
- WORKTREE_CLEANUP (default: true)
- RUN_MOCK_DEMO_MODE (default: mixed)
EOF
  exit 0
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found" >&2
  exit 1
fi

echo "[release-gate] verifying workflow pinning"
if [[ -x "$REPO_ROOT/scripts/verify_action.sh" ]]; then
  "$REPO_ROOT/scripts/verify_action.sh"
else
  echo "missing verify_action.sh" >&2
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
  "Before tagging" \
  "contrib/demo/run_demo.sh"; do
  if ! contains_text "$pattern" "$DOCS_ROOT"/TESTING.md "$DOCS_ROOT"/RELEASE.md "$DOCS_ROOT"/README.md "$DOCS_ROOT"/docs/manual.md; then
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
  if [[ -x "${FOREMAN_BIN:-}" ]]; then
    :
  elif [[ -x "$REPO_ROOT/target/release/foreman" ]]; then
    FOREMAN_BIN="$REPO_ROOT/target/release/foreman"
  else
    FOREMAN_BIN="${FOREMAN_BIN:-$REPO_ROOT/target/release/foreman}"
  fi
  if [[ ! -x "$FOREMAN_BIN" ]]; then
    echo "[release-gate] building release binary"
    cargo build --locked --release --bin foreman
  fi

  export CODEX_BIN="$CODEX_BIN"
  export FOREMAN_BIN="$FOREMAN_BIN"
  export JOB_TIMEOUT_MS="${JOB_TIMEOUT_MS:-300000}"
  export JOB_POLL_MS="${JOB_POLL_MS:-500}"
  export WORKTREE_CLEANUP="${WORKTREE_CLEANUP:-true}"
  export RUN_MOCK_DEMO_MODE="${RUN_MOCK_DEMO_MODE:-mixed}"

  if ! "$REPO_ROOT/contrib/demo/run_demo.sh"; then
    echo "live mock demo failed" >&2
    exit 1
  fi
elif [[ "${RUN_LIVE_MOCK_SMOKE:-false}" != "false" ]]; then
  echo "RUN_LIVE_MOCK_SMOKE must be true or false" >&2
  exit 1
else
  echo "[release-gate] RUN_LIVE_MOCK_SMOKE=false; skipping live demo."
fi

echo "[release-gate] complete"
