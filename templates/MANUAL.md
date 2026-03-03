# foreman Manual

## Runtime constraints for project runs

In restricted environments, project runs may fail before `/health` becomes available if process/network permissions are denied.

For one-time project executions, request elevated execution for the launch command and keep that scope narrow:

- Confirm permissions for:
  - local unix socket creation (`--socket-path`)
  - spawning `codex app-server`
  - writing state and callback artifacts
- Start `foreman` in a privileged run.
- Verify `GET /health`.
- Run project/worker commands while elevated.
- Stop the process when the run finishes.

If escalation is unavailable, use test-based verification (`cargo test`) instead of live orchestrations.

You are operating this project with `foreman`.

Use this directory as a control surface for local Codex agent orchestration:

- Spawn dedicated workers with `POST /projects/:project_id/workers`.
- Spawn standalone tasks with `POST /agents`.
- Keep a compact progress state and send status updates through project handoffs.
- Configure callbacks for completion signals and operational events.

Core flow:

1. Start a project:
   `POST /projects` with `{"path":"<project_path>"}`
2. Dispatch work:
   `POST /projects/<project_id>/workers` with `{"prompt":"..."}`
3. Monitor and steer:
   `GET /agents/<agent_id>`
4. On completion, handle callback events and route next actions.

When calling callbacks, treat event payloads as instructions to update state,
not as direct execution prompts unless explicitly documented by your callback path.

Keep handoffs compact and include:
- state
- blockers
- next action

## Development and validation commands

This project runs as part of a Rust workspace. Use workspace commands when validating.

```bash
cargo test --workspace --tests -- --nocapture
cargo test -p codex-api --tests -- --nocapture
cargo test --tests
```
