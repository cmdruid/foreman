---
name: codex-foreman-orchestrator
description: Teaches agents how to use codex-foreman for worker orchestration, callback handling, and project workflows.
metadata:
  short-description: Operate codex-foreman foreman/worker model and callbacks.
---

# Codex-Foreman Orchestrator

Use this skill when you need to start, monitor, steer, and complete workflows through `codex-foreman`.

## Core model

- `codex-foreman` runs a long-lived foreman service.
- A **foreman** agent orchestrates one project and can spawn **worker** agents.
- Workers and the foreman are controlled via HTTP endpoints and callback hooks.
- Delegation is explicit: foreman executes worker tasks only when specified in request payloads.
- Use `jobs` for explicit multi-worker batches and `workers` for single-worker dispatch.
- Projects are filesystem-based and use files in the project folder:
  - `project.toml`
  - `FOREMAN.md`
  - `WORKER.md`
  - `RUNBOOK.md`
  - optional `HANDOFF.md`

## Quick install into Codex

Install this skill path into your Codex skill directory (`$CODEX_HOME/skills/codex-foreman`).

```bash
mkdir -p "$CODEX_HOME/skills"
cp -r contrib/codex-foreman-skill "$CODEX_HOME/skills/codex-foreman"
```

For OpenClaw-compatible runtimes, install as the equivalent local skill bundle folder; keep the folder name `codex-foreman` to make intent explicit.

## Runtime baseline

Before issuing agent-facing instructions, verify the service target:

- Foreman endpoint reachable (example: `http://127.0.0.1:8787`)
- API token if auth is enabled in config:
  - Header: `Authorization: Bearer $CODEX_FOREMAN_API_TOKEN`
- `POST /health` returns `ok`

## Minimal project bootstrap

```bash
codex-foreman --init-project /path/to/project --init-project-manual
```

Generated:
- `project.toml`
- `FOREMAN.md`
- `WORKER.md`
- `RUNBOOK.md`
- optional `HANDOFF.md`
- optional `MANUAL.md` (if `--init-project-manual` set)

Start the service and open the project via API:

```bash
codex-foreman --service-config /etc/codex-foreman/config.toml --state-path /var/lib/codex-foreman/state.json
```

```bash
curl -s -X POST http://127.0.0.1:8787/projects \
  -H "Content-Type: application/json" \
  -d '{"path":"/path/to/project","start_prompt":"Resume with current handoff context"}'
```

## Worker operations

- `POST /projects/:id/workers` to spawn a worker.
- `POST /projects/:id/jobs` for explicit multi-worker delegation (`workers` array).
- `POST /projects/:id/jobs` is not a planner endpoint; you provide each worker prompt explicitly.
- `POST /projects/:id/foreman/send` to add a foreman prompt.
- `POST /projects/:id/foreman/steer` to steer foreman direction.
- `POST /projects/:id/compact` to compact foreman context.
- `DELETE /projects/:id` to close foreman/worker context.

For non-project scope, use:
- `POST /agents` to spawn one-off agents.
- `/agents/:id/send`, `/agents/:id/steer`, `/agents/:id/interrupt`, `/agents/:id` (delete).

For exact payloads, response fields, and full schema, use [`CODEX_FOREMAN_API.md`](references/CODEX_FOREMAN_API.md).

## Callback protocol essentials

- Prefer callback profiles in `project.toml` or service config.
- Configure a callback command/webhook to emit `event payload + event_prompt`.
- Include a short, explicit prefix in prompts so the agent can interpret event payloads:

Example profile:

```toml
[callbacks.profiles.openclaw]
program = "/usr/local/bin/openclaw-callback"
args = ["--event-prompt", "{{event_prompt}}", "--payload", "{{event_payload}}"]
prompt_prefix = "OpenClaw event received. Handle the following completion payload:\n"
```

- For command callbacks, set `event_payload` JSON safely escaped for shell.
- For webhook callbacks, set `x-foreman-secret` and validate origin in target handler.

## Typical behavior policies

- Use `policy.bubble_up_events = true` only for high-signal worker completions.
- Keep worker callback callbacks concise; send foreman-level summaries unless action is needed.
- Use `policy.compact_after_turns` to reduce context bloat in long sessions.
- Track and resume using persistent `state-path` on service restart.

## Safety and operational notes

- Do not embed secrets in prompt files; pass via environment variables in templates/config.
- Prefer local stdio `codex app-server` supervision; avoid external remote transports unless required.
- On timeout or app-server process exits, the service should recover by restarting and re-hydrating state.

## When to use this skill

- Any prompt requiring foreman orchestration, multi-agent delegation, callback handling, or project-specific handoff/compaction loops.
- Any task that requires reliable guidance for project file layout and event-driven callback behavior.
