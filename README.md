# codex-foreman

A Rust control-plane for orchestrating multiple long-lived `codex app-server` agents from a single process.

`codex-foreman` is designed to:

- multiplex one Codex `app-server` transport for many logical workers
- provide a simple HTTP control API (`/agents`, `/projects`)
- dispatch event-driven callbacks (webhooks or shell commands)
- support OpenClaw-style callback routing with prompt prefixes and templating
- run project foremen with scoped prompts, hooks, and compaction policy
- persist lightweight agent/project state for faster recovery after restart

## Build

```bash
cd ~/repos/codex-foreman
cargo build
```

`codex-foreman` now also includes the `codex-api` protocol crate as a workspace member, so all workspace commands can target that crate directly.

## Run

```bash
cargo run -- \
  --bind 0.0.0.0:8787 \
  --codex-binary /usr/local/bin/codex \
  --service-config /etc/codex-foreman/config.toml \
  --state-path /var/lib/codex-foreman/foreman-state.json
```

`codex-foreman` now always starts and uses a local `codex app-server` via stdio.
- `--state-path`: path for persisted state recovery across restarts (default `/var/lib/codex-foreman/foreman-state.json`)

Create a project scaffold:

```bash
cargo run -- --init-project /tmp/example-project
```

Default scaffold templates are read from `templates`:

- `templates/project.toml`
- `templates/FOREMAN.md`
- `templates/WORKER.md`
- `templates/RUNBOOK.md`
- `templates/HANDOFF.md`
- `templates/MANUAL.md` (only when `--init-project-manual` is set)

Set template root with CLI flag or environment variable:

```bash
--template-dir /usr/share/codex-foreman/templates
```

or

```bash
export CODEX_FOREMAN_TEMPLATE_DIR=/usr/share/codex-foreman/templates
```

Include a tool-operating manual for agents:

```bash
cargo run -- --init-project /tmp/example-project --init-project-manual
```

Overwrite existing scaffold files:

```bash
cargo run -- --init-project /tmp/example-project --init-project-overwrite
```

By default it assumes `codex` is on `PATH`.

Validate config:

```bash
cargo run -- --service-config /etc/codex-foreman/config.toml --validate-config
```

### Live mock demo (contrib)

Run a real codex-agent orchestration smoke using the local repository fixture:

```bash
./contrib/run_mock_demo.sh
./contrib/run_mock_mixed_demo.sh
```

See [`docs/manual.md`](docs/manual.md#mock-project-demo-live) for timing knobs and success criteria.

### Permissions for Codex Agent Project Runs

Running `codex-foreman` to execute projects in a codex-agent context requires:

- Socket bind/listen access on the configured `--bind` address.
- Permission to spawn a child `codex app-server` process.
- Read/write access to `--state-path` and project workspace files.
- Permission for callback execution paths (if command/webhook callbacks are enabled).

In restricted environments, request one-time process privilege before startup if project execution requires the above.

## API

- `GET /health`
- `POST /agents` → spawn a standalone agent
- `GET /agents` → list agents
- `GET /agents/:id` → get agent state
- `GET /agents/:id/result` → get terminal result summary
- `GET /agents/:id/wait` → wait for completion (`timeout_ms`, `poll_ms`, `include_events`)
- `GET /agents/:id/events` → fetch recent events (`tail`)
- `POST /agents/:id/send` → start turn or callback-only update
- `POST /agents/:id/steer` → steer active turn
- `POST /agents/:id/interrupt` → interrupt a turn
- `DELETE /agents/:id` → remove local tracking
- `GET /status` → service status and runtime metadata
- `POST /projects` → create project and spawn project foreman
- `GET /projects` → list projects
- `GET /projects/:id` → get project state
- `DELETE /projects/:id` → close project
- `POST /projects/:id/workers` → spawn worker in project
- `POST /projects/:id/foreman/send` → send to project foreman
- `POST /projects/:id/foreman/steer` → steer project foreman
- `POST /projects/:id/compact` → force project compaction handoff
- `POST /projects/:id/jobs` → create a batch job for a project
  - `workers` is explicit and complete: each item defines one worker prompt to execute.
- `GET /jobs` → list project jobs
- `GET /jobs/:id` → get job state
- `GET /jobs/:id/result` → get per-worker result summary
- `GET /jobs/:id/wait` → wait for job terminal state (`timeout_ms`, `poll_ms`, `include_workers`)

For deterministic orchestration, send explicit worker prompts in `jobs` payloads and avoid free-form “figure this out” instructions.

See [`docs/manual.md`](docs/manual.md) for full endpoint schemas and usage examples.

## Service config

`codex-foreman` reads service defaults from `--service-config` (defaults to `/etc/codex-foreman/config.toml`):

```toml
[app_server]
# Optional: tune codex app-server startup and request behavior (milliseconds)
initialize_timeout_ms = 5_000
request_timeout_ms = 30_000
```

```toml
[protocol]
# Optional: fail startup unless the local codex binary reports this version.
expected_codex_version = "0.9.2"
```

```toml
[worker_monitoring]
enabled = false
inactivity_timeout_ms = 3_000
max_restarts = 1
watch_interval_ms = 750
```

`worker_monitoring` is optional:

- set `enabled = true` to automatically detect stalled workers (no events for more than `inactivity_timeout_ms`)
- `max_restarts` limits automatic worker restart attempts while stalled
- `watch_interval_ms` controls how often Foreman checks for stale workers

Service security is optional but recommended for shared environments:

```toml
[security.auth]
enabled = true
token = "replace-me"
header_name = "authorization"
# header_scheme = "Bearer" # optional
# skip_paths = ["/health"]
```

When `security.auth.enabled = true`, every request except `/health` must include the
configured auth header. If `header_scheme` is set, requests must provide
`<scheme> <token>`.

```toml
[callbacks]
default_profile = "openclaw"

[callbacks.profiles.openclaw]
type = "command"
program = "/usr/local/bin/openclaw-callback"
args = ["--message", "{{event_prompt}}", "--thread-id", "{{thread_id}}"]
prompt_prefix = "Handle this event payload from codex:\n"
event_prompt_variable = "message"
timeout_ms = 10_000

[callbacks.profiles.openclaw.env]
OPENCLAW_TOKEN = "{{openclaw_token}}"

[callbacks.profiles.openclaw_webhook]
type = "webhook"
url = "https://example.local/events"
secret_env = "OPENCLAW_WEBHOOK_SECRET"
events = ["turn/completed", "turn/aborted"]
```

`timeout_ms` defaults to `5000` when omitted. Set `timeout_ms = 0` to disable callback timeouts (allowed for command and webhook profiles).

Project-specific prompts and callbacks are loaded from `project.toml` in each project folder (optional).

## Webhook payload

Callback payload shape:

```json
{
  "agent_id": "uuid",
  "thread_id": "thread-id",
  "turn_id": "turn-id",
  "event_id": "event-id",
  "ts": 1710000000,
  "method": "turn/completed",
  "params": {"thread_id":"...", "turn_id":"..."},
  "callback_vars": {
    "agent_id": "uuid",
    "thread_id": "thread-id",
    "method": "turn/completed"
  }
}
```

For command profiles, `event_prompt` is created from `prompt_prefix + compact(event_json)` and exposed to command templates (default token `{{event_prompt}}`).

`turn/completed` payloads now include a normalized `result` object when available, making worker handoff and callback parsing easier for agents.

## Design notes

- State is persisted after every state mutation.
- Callbacks are best-effort: callback failures do not fail API mutation calls.
- `codex-foreman` attempts best-effort recovery of agents/projects from persisted state at startup.

See full release/testing instructions:

- Manual: [`docs/manual.md`](docs/manual.md)
- Contributing: [`CONTRIBUTING.md`](CONTRIBUTING.md)
- Testing: [`TESTING.md`](TESTING.md)
- Releases: [`RELEASES.md`](RELEASES.md)
- Changelog: [`CHANGELOG.md`](CHANGELOG.md)
