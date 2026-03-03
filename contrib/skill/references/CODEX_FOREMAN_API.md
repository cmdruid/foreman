# foreman API Quick Reference

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
- `POST /projects/:id/jobs`
- `GET /projects/:id/callback-status`
- `POST /projects/:id/foreman/send`
- `POST /projects/:id/foreman/steer`
- `POST /projects/:id/compact`

Explicit delegation rule:
`POST /projects/:id/jobs` is an explicit batch contract. Foreman does not derive worker tasks on its own.
You must send every intended worker job in the `workers` array, and each item will be executed as-is.

Failure mapping:

- Terminal `thread/status/changed` events are interpreted as worker completion for status accounting.
- Failure terminal statuses (`aborted`, `interrupted`, `failed`, `error`, `stopped`, `timeout`, `timed_out`) are represented as:
  - `completion_method: "turn/aborted"`
  - worker status that may surface as `aborted` or `failed`
  - counted in `failed_workers` in job results

## Common request examples

Create project:

```bash
export FOREMAN_SOCKET_PATH="/tmp/foreman.sock"

curl --unix-socket "$FOREMAN_SOCKET_PATH" -s -X POST "http://localhost/projects" \
  -H "Content-Type: application/json" \
  -d '{
    "path": "/tmp/example-project",
    "start_prompt": "Continue from handoff."
  }'
```

Spawn worker:

```bash
curl --unix-socket "$FOREMAN_SOCKET_PATH" -s -X POST "http://localhost/projects/PROJECT_ID/workers" \
  -H "Content-Type: application/json" \
  -d '{"prompt":"Investigate open issue and summarize."}'
```

Create explicit worker batch:

```bash
curl --unix-socket "$FOREMAN_SOCKET_PATH" -s -X POST "http://localhost/projects/PROJECT_ID/jobs" \
  -H "Content-Type: application/json" \
  -d '{
    "workers":[
      {"prompt":"Check code quality and report high-signal fixes.","labels":{"category":"code-quality"}},
      {"prompt":"Check test coverage gaps and suggest priorities.","labels":{"category":"test-coverage"}}
    ]
  }'
```

Foreman send:

```bash
curl --unix-socket "$FOREMAN_SOCKET_PATH" -s -X POST "http://localhost/projects/PROJECT_ID/foreman/send" \
  -H "Content-Type: application/json" \
  -d '{"prompt":"Draft a new handoff and risk register."}'
```

Update compaction policy:

```bash
curl --unix-socket "$FOREMAN_SOCKET_PATH" -s -X POST "http://localhost/projects/PROJECT_ID/compact" \
  -H "Content-Type: application/json" \
  -d '{"reason":"Routine handoff","prompt":"Summarize next actions."}'
```

## project.toml callback hints

- `project.toml` should include:
  - `policy.compact_after_turns`
  - `policy.bubble_up_events`
  - `callbacks.lifecycle.start`
  - `callbacks.lifecycle.compact`
  - `callbacks.lifecycle.stop`
  - `callbacks.lifecycle.worker_completed`
  - `callbacks.lifecycle.worker_aborted`

## Callback profile behavior

- `project.toml` and service callbacks support:
  - callback-only send
  - optional prompt prefix injection (`callback_prompt_prefix`)
  - per-request callback overrides in agent/project send/steer inputs
