# codex-foreman API Quick Reference

## Agent endpoints

- `POST /agents`
- `GET /agents`
- `GET /agents/:id`
- `POST /agents/:id/send`
- `POST /agents/:id/steer`
- `POST /agents/:id/interrupt`
- `DELETE /agents/:id`

## Project endpoints

- `POST /projects`
- `GET /projects`
- `GET /projects/:id`
- `DELETE /projects/:id`
- `POST /projects/:id/workers`
- `POST /projects/:id/foreman/send`
- `POST /projects/:id/foreman/steer`
- `POST /projects/:id/compact`

## Common request examples

Create project:

```bash
curl -s -X POST http://127.0.0.1:8787/projects \
  -H "Content-Type: application/json" \
  -d '{
    "path": "/tmp/example-project",
    "start_prompt": "Continue from handoff."
  }'
```

Spawn worker:

```bash
curl -s -X POST http://127.0.0.1:8787/projects/PROJECT_ID/workers \
  -H "Content-Type: application/json" \
  -d '{"prompt":"Investigate open issue and summarize."}'
```

Foreman send:

```bash
curl -s -X POST http://127.0.0.1:8787/projects/PROJECT_ID/foreman/send \
  -H "Content-Type: application/json" \
  -d '{"prompt":"Draft a new handoff and risk register."}'
```

Update compaction policy:

```bash
curl -s -X POST http://127.0.0.1:8787/projects/PROJECT_ID/compact \
  -H "Content-Type: application/json" \
  -d '{"reason":"Routine handoff","prompt":"Summarize next actions."}'
```

## project.toml callback hints

- `project.toml` should include:
  - `policy.compact_after_turns`
  - `policy.bubble_up_events`
  - `callbacks.foreman` events:
    - `on_project_start`
    - `on_project_stop`
    - `on_project_compaction`
    - `on_worker_completed`
    - `on_worker_aborted`

## Callback profile behavior

- `project.toml` and service callbacks support:
  - callback-only send
  - optional prompt prefix injection (`callback_prompt_prefix`)
  - per-request callback overrides in agent/project send/steer inputs

