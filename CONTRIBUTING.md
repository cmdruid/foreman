# Contributing to foreman

Thank you for contributing.

## Repo layout

- `src/main.rs` — CLI and API handlers served via Unix socket
- `src/foreman.rs` — worker/callback orchestration logic
- `crates/codex-api/` — JSON-RPC bridge and protocol boundary to `codex app-server`
- `src/config.rs` — service configuration model + validation
- `src/project.rs` — project config model and prompt loading
- `src/models.rs` — request/response payloads
- `src/events.rs` — event normalization and routing
- `docs/` — operational manual and references

## Development setup

```bash
cd ~/repos/foreman
cargo build
```

## Workflow

1. Create a branch for your change.
2. Implement changes with focused, small commits.
3. Run formatting:

```bash
cargo fmt --all
```

4. Run tests (or at least manual verification) listed in `TESTING.md`:

```bash
cargo test -p codex-api --tests
```

5. Run automated suite:
   - `cargo test --workspace --tests -- --nocapture`

6. Validate fixture-based negative coverage:
- `test/fixtures/project-missing-worker`
- `test/fixtures/project-invalid-config`

## Coding style

- Keep changes minimal and explicit.
- Favor existing naming and error style (`anyhow::Context`).
- Prefer small, pure helpers over large closures.
- Add new config fields in `src/config.rs` and extend validation there.
- Update models and docs in the same patch for API-related work.

## Adding callback behavior

When changing callback behavior:

- update configuration schema in `src/config.rs`
- update per-request override model in `src/models.rs` if needed
- add/adjust execution tests or at least behavior notes in `TESTING.md`
- document template variables and event behavior in `docs/manual.md`

## Reviews

Please include:

- command used to exercise the change
- sample payloads for agent and project flows
- expected outcome and logs/screenshots where useful

## Security posture

Auth is optional and configured via `security.auth` in the service config.
Any contribution touching auth, TLS, credentials handling, or durable queueing
must document threat model and failure behavior clearly.
