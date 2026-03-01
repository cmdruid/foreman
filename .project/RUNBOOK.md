# Project Runbook

- Start by summarizing current Git state and open changes.
- Confirm any active `.project` callbacks and callback vars are in place.
- Review `project.toml`, `FOREMAN.md`, `WORKER.md`, and `HANDOFF.md` for current instructions.
- Execute the assigned task with minimal scope.
- Run targeted tests after changes:
  - `cargo test --test integration -- --nocapture` for callback/lifecycle paths
  - `cargo test --test e2e -- --nocapture` for endpoint scaffolding
  - `cargo test -- --nocapture` for unit checks when needed
- Capture failures with clear reproduction steps before making additional changes.
- On completion, post compact status including:
  - modified files
  - verification commands/results
  - open risks
