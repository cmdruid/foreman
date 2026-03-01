# codex-foreman Manual

This manual documents the alpha `codex-foreman` service: a control-plane that runs one `codex app-server` process and orchestrates many agents and project workflows.

## 1) What codex-foreman Does

`codex-foreman` is an HTTP server (`127.0.0.1:8787` by default) with:

- `/agents` API for direct agent lifecycles
- `/projects` API for long-running project scopes
- callback dispatch for webhook and command profiles
- project-scoped prompts, event callbacks, hooks, and compaction policy

Agents and project foremen are backed by active Codex thread IDs.

## 2) Run and Validate

```bash
cargo run -- \
  --bind 0.0.0.0:8787 \
  --codex-binary /usr/local/bin/codex \
  --config /path/to/codex/config/override.toml \
  --service-config /etc/codex-foreman/config.toml
```

`codex-foreman` always starts and manages a local `codex app-server` process using
JSON-RPC over stdio.

State is persisted to `--state-path` after most mutations and reloaded automatically at startup.

Initialize a starter project scaffold:

```bash
cargo run -- --init-project /tmp/example-project
```

Include an agent operating manual in the scaffold:

```bash
cargo run -- --init-project /tmp/example-project --init-project-manual
```

Force overwrite existing scaffold files:

```bash
cargo run -- --init-project /tmp/example-project --init-project-overwrite
```

Validate service callback config:

```bash
cargo run -- --service-config /etc/codex-foreman/config.toml --validate-config
```

`--validate-config` loads and validates the service callback config and exits without starting the daemon.

## 2.1 Sandbox And Permission Constraints For Project Runs

When running from a restricted Codex execution context, the following permissions are required for codex-agent project runs:

- Bind/listen on a local TCP socket (`--bind`, default `127.0.0.1:8787`).
- Start a child `codex app-server` process.
- Read/write `--state-path` (durable state recovery).
- Read project files (`FOREMAN.md`, `WORKER.md`, `RUNBOOK.md`, `HANDOFF.md`, `project.toml`, reports).
- Execute callback hooks/commands (if using command callbacks) and perform outbound webhook calls (if configured).
- Write callback artifacts (if callbacks include file/command targets).
- Run foreground/background process management (start, inspect, terminate).

Required action before launch:

If the runtime is restricted, request one-time external privileges for the launch command before creating project workers:

```text
Do you want to run `codex-foreman` with external privileges for this project execution?
```

Then execute the launch command with the caller-approved privilege path so the API server can bind and receive callbacks for the running project.

If escalation is unavailable, use the existing test harness (`cargo test --test integration`) for validation instead, because tests execute without requiring a bound service process.

### Workspace test commands

```bash
cargo test --workspace --tests -- --nocapture
cargo test -p codex-api --tests -- --nocapture
cargo test --tests
```

### Audit Remediation Runbook

For a full exercise of parallel review + consolidation using this repository’s `.audit` templates:

```bash
./scripts/run_audit_remediation_demo.sh
```

Output includes:
- project id
- job id
- per-category worker results
- foreman wait output
- final job payload

You can tune timing by setting:

- `PORT`
- `JOB_TIMEOUT_MS`
- `JOB_POLL_MS`
- `WAIT_FOREMAN_SECONDS`
- `CLEANUP_PROJECT=false` (preserve state for manual follow-up)

See [audit remediation runbook](/home/cmd/repos/codex-foreman/docs/audit-remediation-runbook.md) for the full step-by-step manual and troubleshooting notes.

### Real-Backend Smoke Test

The following validates the service against a real `codex` binary (not `fake_codex`):

```bash
set -euo pipefail

CODEX_BIN=/home/cmd/.npm-global/bin/codex
FOREMAN_BIN=target/debug/codex-foreman
PORT=8895
STATE_FILE=/tmp/codex-foreman-state-smoke.json
SERV_CONF=/tmp/cf-smoke.toml
PROJECT_DIR=/tmp/codex-foreman-smoke-project

cat >"$SERV_CONF" <<'EOF'
[callbacks]
[callbacks.profiles]
EOF

# Optional: initialize scaffolds used by the project flow
${FOREMAN_BIN} --init-project "$PROJECT_DIR" --init-project-overwrite

# Start foreman in background for manual verification
${FOREMAN_BIN} \
  --bind 127.0.0.1:$PORT \
  --codex-binary "$CODEX_BIN" \
  --service-config "$SERV_CONF" \
  --state-path "$STATE_FILE" \
  > /tmp/codex-foreman-smoke.log 2>&1 &
FOREMAN_PID=$!

# Wait until server responds
for i in {1..20}; do
  if curl -fsS "http://127.0.0.1:$PORT/health" > /dev/null; then
    break
  fi
  sleep 0.25
done

# Health check
curl -sS "http://127.0.0.1:$PORT/health"

# Standalone agent
AGENT_ID=$(curl -sS -X POST "http://127.0.0.1:$PORT/agents" \
  -H 'Content-Type: application/json' \
  -d '{"prompt":"real path smoke: quick status probe"}' \
  | jq -r '.id')
curl -sS "http://127.0.0.1:$PORT/agents/$AGENT_ID"

# Project route smoke
PROJECT_ID=$(curl -sS -X POST "http://127.0.0.1:$PORT/projects" \
  -H 'Content-Type: application/json' \
  -d "$(jq -cn --arg p "$PROJECT_DIR" '{path:$p}')" \
  | jq -r '.project_id')
curl -sS -X POST "http://127.0.0.1:$PORT/projects/$PROJECT_ID/workers" \
  -H 'Content-Type: application/json' \
  -d '{"prompt":"real project worker smoke"}'
```

Expected outcome:

- `health` returns `ok`.
- `POST /agents` returns `201` and an agent `id`.
- `GET /agents/{id}` returns a valid agent state.
- `POST /projects` returns `201` with a `project_id`.
- `POST /projects/{project_id}/workers` returns `201`.

Clean up:

```bash
kill "$FOREMAN_PID"
rm -f "$STATE_FILE" "$SERV_CONF"
```

## 3) Service Configuration

`codex-foreman` uses a global callback config file, defaulting to `/etc/codex-foreman/config.toml`.

Example:

```toml
[app_server]
initialize_timeout_ms = 5_000
request_timeout_ms = 30_000
```

```toml
[protocol]
expected_codex_version = "0.9.2"
```

```toml
[callbacks]
default_profile = "openclaw"

[callbacks.profiles.openclaw]
type = "command"
program = "/usr/local/bin/openclaw-callback"
args = ["--message", "{{message}}", "--thread-id", "{{thread_id}}"]
prompt_prefix = "OpenClaw event:"
event_prompt_variable = "message"
timeout_ms = 10_000

[callbacks.profiles.openclaw.env]
OPENCLAW_TOKEN = "{{openclaw_token}}"

[callbacks.profiles.openclaw_webhook]
type = "webhook"
url = "https://hooks.example.local/codex"
secret_env = "OPENCLAW_WEBHOOK_SECRET"
events = ["turn/completed", "turn/aborted"]
```

Notes:

- `default_profile` is optional.
- If an agent/project does not set a callback profile, there is no callback.
- `events` is an allow-list filter (`"*"` means all events).
- `timeout_ms` defaults to `5000` when omitted.
- Set `timeout_ms = 0` for no timeout.

Security and API auth are also configurable:

```toml
[security.auth]
enabled = true
token = "replace-this"
header_name = "authorization"
header_scheme = "Bearer" # optional
token_env = "CODEX_FOREMAN_API_TOKEN" # optional alternative token source
skip_paths = ["/health"] # optional
```

When auth is enabled, all non-health routes require the configured header.

Service config schema is implemented in `src/config.rs` as:

- `callbacks.default_profile: Option<String>`
- `app_server.initialize_timeout_ms` controls how long startup waits for `initialize`
- `app_server.request_timeout_ms` controls per-request RPC timeout
- `callbacks.profiles.<name>.type = "webhook" | "command"`
- `security.auth` controls API auth requirements.

## 4) API

Base URL examples below: `http://127.0.0.1:8787`.

### 4.1 Health

- `GET /health` → `"ok"`

### 4.2 Agents API

- `POST /agents` → create standalone agent
- `GET /agents` → list agents
- `GET /agents/:id` → get agent state/events
- `GET /agents/:id/result` → get deterministic terminal result (`final_text`, `summary`, `error`, `completed_at`, `event_count`)
- `GET /agents/:id/wait` → wait for terminal result with optional timeout and polling control
- `GET /agents/:id/events` → fetch filtered/tail event history for an agent
- `POST /agents/:id/send` → start turn or update callback settings
- `POST /agents/:id/steer` → steer active turn
- `POST /agents/:id/interrupt` → interrupt by turn or current active turn
- `DELETE /agents/:id` → remove local tracking
- `GET /status` → service status and runtime metadata

#### 4.2.1 Request/response examples

Wait for terminal result with event preview:

```bash
curl "http://127.0.0.1:8787/agents/8a6f.../wait?timeout_ms=120000&poll_ms=500&include_events=true"
```

Fetch only the last 20 events:

```bash
curl "http://127.0.0.1:8787/agents/8a6f.../events?tail=20"
```

Get deterministic result only:

```bash
curl "http://127.0.0.1:8787/agents/8a6f.../result"
```


Create agent:

```json
POST /agents
{
  "prompt": "Write a concise status update",
  "callback_profile": "openclaw",
  "callback_prompt_prefix": "Handle this event:",
  "callback_vars": {
    "openclaw_token": "env:OPENCLAW_TOKEN"
  }
}
```

Response:

```json
{
  "id": "8a6f...",
  "thread_id": "thread-id",
  "turn_id": "turn-id",
  "status": "running",
  "project_id": null,
  "role": "standalone",
  "foreman_id": null
}
```

Send turn and override callback profile at once:

```json
POST /agents/8a6f...
{
  "prompt": "Continue with section 2",
  "callback_profile": "openclaw_webhook",
  "callback_events": ["turn/completed"]
}
```

Callback-only update (valid):

```json
POST /agents/8a6f...
{
  "callback_events": ["turn/completed", "turn/aborted"],
  "callback_vars": {"operator": "ci"}
}
```

Steer:

```json
POST /agents/8a6f.../steer
{
  "prompt": "Prefer a bullet style with short sentences."
}
```

Interrupt:

```json
POST /agents/8a6f.../interrupt
{ "turn_id": "turn-id" }
```

`turn_id` is optional; if omitted the active turn is used.

### 4.3 Service Status

- `GET /status` returns service metadata and runtime diagnostics:
  - `status`
  - `foreman_pid`
  - `app_server_pid`
  - `bind`
  - configured callback counts
  - `agent_count`, `project_count`
  - `uptime_seconds`

Example:

```json
{
  "status": "ready",
  "foreman_pid": 12345,
  "bind": "127.0.0.1:8787",
  "version": "0.1.0",
  "codex_binary": "/usr/local/bin/codex",
  "state_path": "/var/lib/codex-foreman/foreman-state.json",
  "app_server_pid": 12346,
  "callback_profiles": 2,
  "default_callback_profile": "openclaw",
  "agent_count": 3,
  "project_count": 1,
  "started_at": 1710000000,
  "uptime_seconds": 42
}
```

### 4.4 Projects API

Project management is the new alpha control-plane layer.

- `POST /projects` → create project and spawn foreman agent
- `GET /projects` → list projects
- `GET /projects/:id` → get project state
- `DELETE /projects/:id` → close project and all agents
- `POST /projects/:id/workers` → spawn a worker under project
- `POST /projects/:id/foreman/send` → send prompt/callback updates to foreman
- `POST /projects/:id/foreman/steer` → steer foreman turn
- `POST /projects/:id/compact` → force a compaction handoff
- `POST /projects/:id/jobs` → create a labeled job with multiple workers

#### 4.4.1 Create project

Request:

```json
POST /projects
{
  "path": "/path/to/project",
  "start_prompt": "Boot with context from onboarding notes.",
  "model": "gpt-4o-mini",
  "callback_overrides": {
    "callback_profile": "openclaw"
  }
}
```

Response:

```json
{
  "project_id": "3f02...",
  "path": "/path/to/project",
  "foreman_agent_id": "9f0a...",
  "status": "running"
}
```

#### 4.4.2 Spawn project worker

```json
POST /projects/3f02.../workers
{
  "prompt": "Investigate latest test failure logs and summarize.",
  "model": "gpt-4o-mini"
}
```

#### 4.4.3 Send / steer project foreman

```json
POST /projects/3f02.../foreman/send
{
  "prompt": "Please rotate this project and produce a concise handoff."
}
```

```json
POST /projects/3f02.../foreman/steer
{ "prompt": "Focus only on blockers." }
```

#### 4.4.4 Compact project

```json
POST /projects/3f02.../compact
{
  "reason": "Periodic policy compaction.",
  "prompt": "Preserve only key decisions and unresolved risks."
}
```

`compact` is designed to be a bounded “handoff” action to the project foreman.

### 4.5 Jobs API

- `GET /jobs` → list job records
- `GET /jobs/:id` → get job state and completion status
- `GET /jobs/:id/result` → get aggregate + per-worker result payload
- `GET /jobs/:id/wait` → wait for job terminal completion with optional `include_workers`, `timeout_ms`, `poll_ms`

## 5) Project Configuration and Files

Each project folder may include `project.toml` (optional; defaults are used if missing). The folder must contain prompt files (`FOREMAN.md`, `WORKER.md`, `RUNBOOK.md`) unless changed via config.

### 5.1 Layout

- `FOREMAN.md` — baseline prompt for project foreman
- `WORKER.md` — baseline prompt for worker tasks
- `RUNBOOK.md` — runbook executed/embedded for the foreman lifecycle
- `HANDOFF.md` (optional) — default handoff text for compact actions
- `project.toml` — optional project settings
- `MANUAL.md` (optional when generated) — agent operating instructions

`project.toml` fields:

```toml
name = "checkout-service"

[prompts]
foreman_file = "FOREMAN.md"
worker_file = "WORKER.md"
runbook_file = "RUNBOOK.md"
handoff_file = "HANDOFF.md"

[callbacks.worker]
callback_profile = "openclaw"

[callbacks.foreman]
callback_profile = "openclaw_webhook"

[callbacks.bubble_up]
callback_profile = "openclaw"
callback_events = ["turn/completed", "turn/aborted"]

[hooks]
on_project_start = "scripts/on_project_start.sh"
on_worker_completed = "scripts/worker_done.sh"
on_worker_aborted = "scripts/worker_aborted.sh"
on_project_compaction = "scripts/on_compact.sh"
on_project_stop = "scripts/on_stop.sh"

[policy]
compact_after_turns = 10
bubble_up_events = ["turn/completed", "turn/aborted"]
```

### 5.2 `project.toml` execution behavior

- `callbacks.worker`: applies to project workers
- `callbacks.foreman`: applies to project foreman
- `callbacks.bubble_up`: executes through the same callback system when configured
- `hooks.on_*`: shell commands run via `sh -lc` with environment variable template injection.
  Hook templates use `{{token}}` replacement and fail if any token cannot be resolved.
- `policy.compact_after_turns`: triggers auto-compaction after N completed/aborted worker turns
- `policy.bubble_up_events`: event allow-list for optional bubble-up callbacks

Hook payload variables available to shell hooks:

- `project_id`, `project_path`, `project_name`, `project_status`
- `event_payload` (where available)
- any callback variables resolved from the matching callback spec

## 6) Callback Behavior

There are two callback modes:

- **Webhook mode** (callback profile type `webhook`): POST with JSON payload
- **Command mode** (`callback_profile` command): run local command with templated args/env/prompt

### 6.1 Webhook payload

Both agent and project callbacks share a common base payload:

```json
{
  "agent_id": "uuid",
  "thread_id": "thread-id",
  "turn_id": "turn-id",
  "event_id": "event-id",
  "ts": 1710000000,
  "method": "turn/completed",
  "params": {"thread_id":"...", "turn_id":"..."},
  "agent_role": "worker",
  "project_id": "project-uuid",
  "foreman_id": "foreman-agent-uuid",
  "callback_vars": {
    "worker_id": "uuid",
    "thread_id": "thread-id",
    "method": "turn/completed",
    "project_id": "project-uuid",
    "project_name": "checkout-service",
    "ts": "1710000000"
  }
}
```

`callback_vars` may not include all keys in every mode, but the above fields are canonical.

Result payload includes an optional canonical result object on completion-oriented events:

```json
{
  "result": {
    "agent_id": "uuid",
    "status": "completed",
    "completion_method": "turn/completed",
    "turn_id": "turn-1",
    "final_text": "concise handoff",
    "summary": "compact summary",
    "completed_at": 1710000000,
    "event_id": "uuid",
    "error": null,
    "event_count": 42
  }
}
```

### 6.2 Command callback prefix and payload injection

For command profiles, `codex-foreman` builds a text payload:

- `event_prompt = <callback_prompt_prefix> + <compact JSON of params>`
- this is injected as `event_prompt` by default
- can be remapped by service `event_prompt_variable` field

Examples:

```toml
event_prompt_variable = "payload"
args = ["--payload", "{{payload}}", "--thread", "{{thread_id}}"]
```

Template replacement applies to:

- profile `program`
- profile/command `args`
- profile `env`
- project hook command strings

Useful variable names:

- `event_prompt`, `event_json`, `event_pretty`, `event_payload`
- `agent_id`, `thread_id`, `turn_id`, `project_id`, `project_name`, `project_path`
- plus custom `callback_vars` from request/config

### 6.3 Callback prefix guidance

Always send a prompt prefix before raw JSON so the receiving orchestrator has context. Example:

- Prefix: `"You are an agent message processor. The JSON below is a codex event:\n"`
- Template event: `{{event_prompt}}`

This is the simplest reliable format for OpenClaw-style callback agents.

## 7) Lifecycle and Cleanup

- Closing a project calls its configured stop hook, then closes tracked workers and foreman.
- Worker/agent callbacks are best-effort: callback failures are logged and do not fail the originating API call.
- Agent/project metadata is persisted to disk for best-effort restart recovery.

## 8) Open Questions for Your Next Iteration

- Add webhook retry/dead-letter behavior
- Add worker/project deletion audit logs and retention
- Add explicit per-project callback override endpoint
