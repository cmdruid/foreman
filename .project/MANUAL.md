# codex-foreman Project Manual

This project has an optional `MANUAL.md` for instructing a codex agent on how to use
`codex-foreman` as an execution tool.

Runbook principles:

- Prefer scoped commands (`/projects/:id/workers`) over broad local terminal actions.
- Treat callback events as state transitions and routing signals.
- Keep responses compact: state, action, and next owner.
- Escalate if a callback is missing or malformed and work cannot continue safely.

Core operating rules:

- Spawn project workers for sub-tasks.
- Keep the project foreman focused on planning and task distribution.
- Use handoff points and keep unresolved work explicit for continuity.
- Document callback wiring and completion criteria for every handoff.

## Development commands

For local validation in this repository, use workspace-scoped Rust test commands:

- `cargo test --workspace --tests -- --nocapture`
- `cargo test -p codex-api --tests -- --nocapture`

### Running in restricted environments

- If the environment blocks socket binding or process operations, the service may not start.
- Before project execution, request one-time elevated execution for the launch command when needed for:
  - TCP bind/listen (`--bind`)
  - spawning child `codex app-server`
  - writing recovery state
  - callback command/webhook execution
- Prefer running integration/e2e tests from code paths that do not require a full networked foreman when sandbox limits are active.
