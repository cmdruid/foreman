# Testing foreman

## Environment assumptions

- `codex` binary available and runnable with `app-server` support
- `cargo` toolchain installed
- Foreman unix-socket path available locally (set via `FOREMAN_SOCKET_PATH`)

## Smoke tests

1. Validate service config:

```bash
foreman --validate-config
```

Expected:

- process exits `0`
- prints loaded callback profiles
- prints warnings if any

2. Initialize a sample project scaffold:

```bash
foreman --init-project /tmp/example-project
```

Expected generated files:

- `project.toml`
- `FOREMAN.md`
- `WORKER.md`
- `RUNBOOK.md`
- `HANDOFF.md`
- `MANUAL.md` (only with `--with-manual`)

3. Launch service:

```bash
foreman \
  --socket-path /tmp/cf-foreman.sock \
  --project /tmp/example-project/project.toml
```

Defaults:

- `--codex-binary` defaults to `codex`
- `--service-config` defaults to `~/.foreman/config.toml`

```bash
export FOREMAN_SOCKET_PATH="/tmp/cf-foreman.sock"
```

4. Health:

```bash
curl --unix-socket "$FOREMAN_SOCKET_PATH" -s http://localhost/health
```

Expected: `ok`

5. Restart recovery smoke:

```bash
curl --unix-socket "$FOREMAN_SOCKET_PATH" -s -X POST http://localhost/projects \
  -H "Content-Type: application/json" \
  -d '{
    "path": "/tmp/example-project",
    "start_prompt": "recovery test"
  }'
```

- Capture returned `project_id` and `foreman_agent_id`.
- Stop and restart `foreman` with same project-local state path (`--state-path`).
- Confirm `GET /projects/<project_id>` returns persisted metadata.
- Confirm callback dispatch continues to execute after restart.
- Confirm `GET /projects/<project_id>/callback-status` reports lifecycle callback state.

## Agent flow

### Spawn + callback profile

```bash
curl --unix-socket "$FOREMAN_SOCKET_PATH" -s -X POST http://localhost/agents \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "Write a one-line status update",
    "callback_profile": "openclaw"
  }'
```

Save returned `id` as `<agent_id>`.

### Callback-only `/send`

```bash
curl --unix-socket "$FOREMAN_SOCKET_PATH" -s -X POST "http://localhost/agents/<agent_id>/send" \
  -H "Content-Type: application/json" \
  -d '{
    "callback_events": ["turn/completed", "turn/aborted"],
    "callback_vars": {"operator":"ci"}
  }'
```

### Update callback and callback prefix

```bash
curl --unix-socket "$FOREMAN_SOCKET_PATH" -s -X POST "http://localhost/agents/<agent_id>/send" \
  -H "Content-Type: application/json" \
  -d '{
    "callback_profile": "openclaw_webhook",
    "callback_prompt_prefix": "OpenClaw event ingest. Follow these rules before action:\n",
    "callback_events": ["turn/completed"]
  }'
```

### Steering + interrupt

```bash
curl --unix-socket "$FOREMAN_SOCKET_PATH" -s -X POST "http://localhost/agents/<agent_id>/steer" \
  -H "Content-Type: application/json" \
  -d '{"prompt":"Please keep this short"}'

curl --unix-socket "$FOREMAN_SOCKET_PATH" -s -X POST "http://localhost/agents/<agent_id>/interrupt" \
  -H "Content-Type: application/json" \
  -d '{}'
```

### Cleanup

```bash
curl --unix-socket "$FOREMAN_SOCKET_PATH" -s -X DELETE "http://localhost/agents/<agent_id>"
```

## Project flow

Use a real project folder with:
- `FOREMAN.md`, `WORKER.md`, `RUNBOOK.md`
- optional `project.toml`

### Create project

```bash
curl --unix-socket "$FOREMAN_SOCKET_PATH" -s -X POST http://localhost/projects \
  -H "Content-Type: application/json" \
  -d '{
    "path": "/tmp/my-project",
    "start_prompt": "Start with the latest ticket list.",
    "callback_overrides": { "callback_profile": "openclaw" }
  }'
```

Save `project_id` and `foreman_agent_id`.

### Spawn workers and steer foreman

```bash
curl --unix-socket "$FOREMAN_SOCKET_PATH" -s -X POST http://localhost/projects/<project_id>/workers \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "Investigate test failure in module X."
  }'

curl --unix-socket "$FOREMAN_SOCKET_PATH" -s -X POST http://localhost/projects/<project_id>/foreman/send \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "Produce handoff notes and current risk list."
  }'

curl --unix-socket "$FOREMAN_SOCKET_PATH" -s -X POST "http://localhost/projects/<project_id>/foreman/steer" \
  -H "Content-Type: application/json" \
  -d '{ "prompt":"Focus on blockers only." }'
```

### Compact + close

```bash
curl --unix-socket "$FOREMAN_SOCKET_PATH" -s -X POST http://localhost/projects/<project_id>/compact \
  -H "Content-Type: application/json" \
  -d '{ "reason": "Threshold policy trigger", "prompt": "Condense context and open next tasks." }'

curl --unix-socket "$FOREMAN_SOCKET_PATH" -s -X DELETE "http://localhost/projects/<project_id>"
```

## Callback verification

- For command callbacks, confirm callback receives:
  - rendered shell args
  - env values
  - `event_prompt` contains your configured prefix
- For webhook callbacks, confirm payload includes `callback_vars` and `x-foreman-secret` when configured.

## Release validation checklist

- Run `cargo fmt --all`
- Run `cargo clippy --all-targets -- -D warnings`
- Run commands in this document against a live codex app-server
- Confirm `--validate-config` exits successfully and lists expected profiles
- Confirm `/agents/:id/send` with no turn or callback changes returns `400`
- Confirm project endpoints return expected IDs and statuses
- Confirm service starts with both:
  - `foreman` uses local stdio `app-server` by default
- Confirm API auth is enforced when `security.auth.enabled = true` and `/health` remains open.
- Confirm command/webhook callback timeout paths do not block requests.
- Confirm crash recovery restores state after process restart.

## Workflow hardening checks

- Ensure all workflow `uses:` references are pinned to full commit SHAs before release:

```bash
./scripts/verify_action.sh
```

- Expected result:
  - **success**: `All workflow action references are pinned to full commit SHAs.`
  - **failure**: list of offending workflows and action refs, exit code non-zero.

- Negative test:
  - Temporarily add a non-pinned workflow reference (for example `actions/checkout@v4`) and confirm the script fails.
  - Restore the file before pushing.

- Validate release workflow parsing:

```bash
yq eval '.' .github/workflows/release.yml
```

- Validate CI workflow parsing:

```bash
yq eval '.' .github/workflows/ci.yml
```

### Live mock demo (release gate)

After endpoint-level tests pass, run a real mixed-or-worktree orchestration smoke:

```bash
CODEX_BIN=/home/cmd/.npm-global/bin/codex \
FOREMAN_BIN=target/release/foreman \
JOB_TIMEOUT_MS=300000 \
JOB_POLL_MS=500 \
WORKTREE_CLEANUP=false \
RUN_MOCK_DEMO_MODE=worktree \
./contrib/demo/run_demo.sh
```

Success criteria:

- exit code `0`
- `result: success`
- for `RUN_MOCK_DEMO_MODE=worktree`: three workers complete
- for `RUN_MOCK_DEMO_MODE=mixed`: four workers complete
- all deliverable snapshots are created under:
  - `contrib/demo/.audit/reports/`
- non-fatal warnings only (e.g. empty `final_text` for `turn/completed`)

To run mixed mode explicitly:

```bash
RUN_MOCK_DEMO_MODE=mixed ./contrib/demo/run_demo.sh
```

Or use the helper wrapper:

```bash
./contrib/demo/run_mixed.sh
```

Run the full release gate script when preparing a release:

```bash
./scripts/release_gate.sh
```

## Test Assets

### Mocks

- `test/bin/fake_codex.rs` provides a deterministic JSON-RPC stub for tests.
- Local webhook capture endpoints are started inside integration tests and record callback payloads.

### Fixtures

- `test/fixtures/project-valid/` — complete scaffold for project lifecycle tests.
- `test/fixtures/project-missing-worker/` — missing prompt file for validation failures.
- `test/fixtures/project-invalid-config/` — malformed `project.toml`.

## Test Commands

- Unit + integration:
  - `cargo test --workspace --tests -- --nocapture`
  - Protocol boundary crate only: `cargo test -p codex-api --tests -- --nocapture`

- End-to-end:
  - `cargo test --test e2e -- --nocapture`
- Release checks:
  - `cargo test --test release -- --nocapture`

For root-level full verification (all suites):

```bash
cargo test --tests
```

For CI-style runs, prefer:

```bash
cargo fmt --all -- --check
cargo test --all -- --nocapture
```
