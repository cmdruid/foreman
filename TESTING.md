# Testing codex-foreman

## Environment assumptions

- `codex` binary available and runnable with `app-server` support
- `cargo` toolchain installed
- Foreman bind address reachable locally

## Smoke tests

1. Validate service config:

```bash
cargo run -- --service-config /etc/codex-foreman/config.toml --validate-config
```

Expected:

- process exits `0`
- prints loaded callback profiles
- prints warnings if any

2. Initialize a sample project scaffold:

```bash
cargo run -- --init-project /tmp/example-project
```

Expected generated files:

- `project.toml`
- `FOREMAN.md`
- `WORKER.md`
- `RUNBOOK.md`
- `HANDOFF.md`
- `MANUAL.md` (only with `--init-project-manual`)

3. Launch service:

```bash
cargo run -- \
  --bind 127.0.0.1:8787 \
  --codex-binary /usr/local/bin/codex \
  --service-config /etc/codex-foreman/config.toml \
  --state-path /tmp/codex-foreman/foreman-state.json
```

4. Health:

```bash
curl -s http://127.0.0.1:8787/health
```

Expected: `ok`

5. Restart recovery smoke:

```bash
curl -s -X POST http://127.0.0.1:8787/projects \
  -H "Content-Type: application/json" \
  -d '{
    "path": "/tmp/my-project",
    "start_prompt": "recovery test"
  }'
```

- Capture returned `project_id` and `foreman_agent_id`.
- Stop and restart `codex-foreman` with same `--state-path`.
- Confirm `GET /projects/<project_id>` returns persisted metadata.
- Confirm callbacks/hooks continue to trigger after restart.

## Agent flow

### Spawn + callback profile

```bash
curl -s -X POST http://127.0.0.1:8787/agents \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "Write a one-line status update",
    "callback_profile": "openclaw"
  }'
```

Save returned `id` as `<agent_id>`.

### Callback-only `/send`

```bash
curl -s -X POST "http://127.0.0.1:8787/agents/<agent_id>/send" \
  -H "Content-Type: application/json" \
  -d '{
    "callback_events": ["turn/completed", "turn/aborted"],
    "callback_vars": {"operator":"ci"}
  }'
```

### Update callback and callback prefix

```bash
curl -s -X POST "http://127.0.0.1:8787/agents/<agent_id>/send" \
  -H "Content-Type: application/json" \
  -d '{
    "callback_profile": "openclaw_webhook",
    "callback_prompt_prefix": "OpenClaw event ingest. Follow these rules before action:\n",
    "callback_events": ["turn/completed"]
  }'
```

### Steering + interrupt

```bash
curl -s -X POST "http://127.0.0.1:8787/agents/<agent_id>/steer" \
  -H "Content-Type: application/json" \
  -d '{"prompt":"Please keep this short"}'

curl -s -X POST "http://127.0.0.1:8787/agents/<agent_id>/interrupt" \
  -H "Content-Type: application/json" \
  -d '{}'
```

### Cleanup

```bash
curl -s -X DELETE "http://127.0.0.1:8787/agents/<agent_id>"
```

## Project flow

Use a real project folder with:
- `FOREMAN.md`, `WORKER.md`, `RUNBOOK.md`
- optional `project.toml`

### Create project

```bash
curl -s -X POST http://127.0.0.1:8787/projects \
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
curl -s -X POST http://127.0.0.1:8787/projects/<project_id>/workers \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "Investigate test failure in module X."
  }'

curl -s -X POST http://127.0.0.1:8787/projects/<project_id>/foreman/send \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "Produce handoff notes and current risk list."
  }'

curl -s -X POST "http://127.0.0.1:8787/projects/<project_id>/foreman/steer" \
  -H "Content-Type: application/json" \
  -d '{ "prompt":"Focus on blockers only." }'
```

### Compact + close

```bash
curl -s -X POST http://127.0.0.1:8787/projects/<project_id>/compact \
  -H "Content-Type: application/json" \
  -d '{ "reason": "Threshold policy trigger", "prompt": "Condense context and open next tasks." }'

curl -s -X DELETE "http://127.0.0.1:8787/projects/<project_id>"
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
  - `codex-foreman` uses local stdio `app-server` by default
- Confirm API auth is enforced when `security.auth.enabled = true` and `/health` remains open.
- Confirm command/webhook callback timeout paths do not block requests.
- Confirm crash recovery restores state after process restart.

## Workflow hardening checks

- Ensure all workflow `uses:` references are pinned to full commit SHAs before release:

```bash
./scripts/verify_action_shas.sh
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

After endpoint-level tests pass, run a real mixed orchestration smoke:

```bash
CODEX_BIN=/home/cmd/.npm-global/bin/codex \
FOREMAN_BIN=target/release/codex-foreman \
JOB_TIMEOUT_MS=300000 \
JOB_POLL_MS=500 \
WORKTREE_CLEANUP=true \
RUN_MOCK_DEMO_MODE=mixed \
./contrib/run_mock_demo.sh
```

Success criteria:

- exit code `0`
- `result: success`
- exactly four workers complete for mixed mode
- all deliverable snapshots are created under:
  - `contrib/mock/.audit/reports/`
- non-fatal warnings only (e.g. empty `final_text` for `turn/completed`)

## Test Assets

### Mocks

- `src/bin/fake_codex.rs` provides a deterministic JSON-RPC stub for tests.
- Local webhook capture endpoints are started inside integration tests and record callback payloads.

### Fixtures

- `tests/fixtures/project-valid/` — complete scaffold for project lifecycle tests.
- `tests/fixtures/project-missing-worker/` — missing prompt file for validation failures.
- `tests/fixtures/project-invalid-config/` — malformed `project.toml`.

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
