# Test Coverage Audit (Fresh) — codex-foreman

## Scope
- docs/manual feature matrix + implemented routes in `src/main.rs`
- Unit + integration + e2e existing test coverage
- integration/e2e/contract gaps
- negative/error path coverage (timeouts, malformed input, invalid config, process failures)
- CI/release regression protection and guardrails

Sources reviewed:
- `docs/manual.md`
- `src/main.rs`, `src/foreman.rs`, `src/config.rs`, `src/project.rs`, `src/state.rs`, `src/events.rs`
- `tests/common/mod.rs`, `tests/integration.rs`, `tests/e2e.rs`, `tests/release.rs`
- `.github/workflows/ci.yml`, `.github/workflows/release.yml`

## 1) Coverage heatmap by feature

| Feature | Coverage | Evidence | Gaps |
|---|---|---|---|
| `GET /health`, `GET /status` | Covered | `src/main.rs:193-216`, `tests/integration.rs` | Missing explicit error-path assertions for unauthorized on status under auth. |
| Agent CRUD + lifecycle (`/agents*`) | Mostly covered | `src/main.rs:193-215`, `src/foreman.rs`, `tests/integration.rs` | No malformed UUID/body tests, no explicit missing/invalid callback-profile path, weak auth-invalid header cases. |
| Agent turn/steer/interrupt | Partially covered | `src/main.rs:487-506`, `tests/integration.rs:89-104` | Missing no-op/error boundary tests for invalid `turn_id`, non-existent agent, completed-turn interruption contract. |
| Agent events + wait + result | Covered | `src/main.rs:382-651`, `tests/integration.rs:160-239` | Limited malformed query coverage (`timeout_ms`, `poll_ms`, `include_events`, `tail` not validated). |
| Callback dispatch (command/webhook) | Partially covered | `src/foreman.rs:2410+`, `tests/integration.rs` + command helper tests in `src/foreman.rs` | Missing failure semantics tests: webhook non-2xx, command non-zero exit, callback runtime errors should be non-fatal and visible. |
| Auth middleware and skip paths | Partially covered | `src/config.rs`, `src/main.rs:238-356`, `tests/integration.rs:362-391` | Missing header-scheme variants, malformed/missing token env, invalid header names/values, skip-path precedence with auth path collisions. |
| Project CRUD + worker spawn + compact | Partially covered | `src/main.rs:202-210`, `tests/integration.rs:564-703` | Missing project foreman routes (`/foreman/send`, `/foreman/steer`), missing contract tests for `POST /projects/{id}/compact` with invalid policy/labels. |
| Jobs (`/jobs*`, include-workers wait) | Partially covered | `src/main.rs:211-216`, `tests/integration.rs:823-1013` | Missing job-not-found, malformed job UUID, wait-timeout status semantics, repeated wait + terminal-state transitions. |
| Project/job fixture handling + config validation | Covered/partially covered | `src/project.rs`, `src/config.rs`, `tests/e2e.rs`, `tests/integration.rs:1020-1060` | Missing coverage for invalid config resolution precedence, partial fixture fallback, invalid toml syntax error shape. |
| Service startup lifecycle + version guard | Partially covered (unit/CLI only) | `src/main.rs:243-255` (no tests yet) | Missing explicit startup-fail tests for missing `codex` binary/version mismatch + recovery of broken state on boot. |
| Persistence and recovery | Partially covered | `src/foreman.rs:2986+`, `src/state.rs`, `tests/integration.rs:505-706` | Missing corruption/cross-version persisted-state resilience tests; missing targeted coverage for `PersistedWorkerCallback` restore errors. |
| CLI (`--init-project`, `--validate-config`) | Partially covered | `tests/e2e.rs`, `src/main.rs` | Missing invalid path failure, template resolution precedence failure, non-overwrite guard (and overwrite semantics with permission failures). |
| CI and regression protection | Weakly covered | `.github/workflows/ci.yml`, `scripts/verify_action_shas.sh` | No automated test asserts that CI/security assumptions remain true after workflow edits (pinned SHAs + required jobs not directly tested). |

## 2) Missing / high-risk scenarios

- Startup:
  - `--codex-binary` mismatch/absent/cannot-exec and startup abort behavior.
  - Recovery from unreadable or corrupted `.cf-live-state.json` should degrade gracefully.
- Input contracts:
  - malformed UUIDs in path params for `/agents/{id}`, `/projects/{id}`, `/jobs/{id}`.
  - malformed JSON and empty objects in `POST /agents/{id}/send`, especially callback-only updates.
  - invalid query values for `wait` and `events` endpoints (`timeout_ms`, `poll_ms`, `tail`, `include_events`, `include_workers`).
- Error pathways:
  - webhook callbacks returning `4xx/5xx`.
  - command callback command failures, invalid commands, timeout + cancellation behavior.
  - `POST /projects/{id}/foreman/send`/`/foreman/steer` with invalid IDs, missing prompts, and completed project edge cases.
- Security:
  - auth header malformed or wrong scheme.
  - invalid token from env variable and precedence of configured token vs token_env.
- Integration/regression:
  - `GET /jobs/{id}` for failed/partial jobs before completion.
  - callback-hook execution (`on_project_start`, `on_project_stop`, `on_project_compaction`) not directly asserted.
  - compact policy behavior (`compact_after_turns`) not covered by integration path.

## 3) Priority backlog (concrete, file-targeted)

### P0: Startup and API contract hardening
- `tests/integration.rs` and/or `tests/common/mod.rs`  
  Acceptance test: `startup_rejects_missing_or_mismatched_codex_binary`
  - Start foreman with fake codex that exits invalid version or missing binary path and assert process exits with non-zero.
  - Assert error includes expected version guard message.
- `tests/integration.rs`  
  Acceptance test: `malformed_ids_return_400_or_404`
  - Hit agent/project/job endpoints with invalid UUIDs and malformed paths.
  - Verify explicit error codes remain stable.
- `tests/integration.rs`  
  Acceptance test: `malformed_query_and_body_validation_errors`
  - Send invalid `timeout_ms/poll_ms/tail/include_events` values and malformed send payloads.
  - Verify 400 responses and descriptive error payload shape.

Effort: S (3 focused tests, no harness changes expected).

### P1: Callback/process failure visibility
- `tests/integration.rs`  
  Acceptance test: `callback_dispatch_errors_are_non_fatal`
  - Use invalid webhook URL and non-zero exit command callbacks.
  - Verify turn completes and agent/job result is available; callback failure does not block completion.
- `tests/integration.rs`  
  Acceptance test: `command_callback_timeout_isolation_with_error`
  - Extend existing timeout case: command blocked past timeout should not drop completion metadata.
  - Verify explicit timeout/error marker is emitted in webhook/command log if captured.
- `src/foreman.rs` (if needed for hook helpers) and `tests/integration.rs`  
  Acceptance test: `project_hooks_execute_and_record_errors`
  - Trigger project start/compact/worker completion with failing & succeeding hook scripts.
  - Assert behavior and non-fatal handling are deterministic.

Effort: M (2–3 integration tests + minimal fixture additions).

### P2: Project/job API and control-plane closure
- `tests/integration.rs`  
  Acceptance test: `project_foreman_send_steer_contract`
  - Exercise both `/projects/{id}/foreman/send` and `/foreman/steer` with valid payloads and closed-project error cases.
- `tests/integration.rs`  
  Acceptance test: `jobs_terminal_state_and_repeated_wait`
  - Validate `/jobs/{id}` and `/jobs/{id}/wait` transitions from running -> partial/failed/completed and repeated polling idempotence.
- `src/foreman.rs` + `tests/integration.rs`  
  Acceptance test: `corrupt_persisted_state_is_tolerated_and_recovered`
  - Corrupt persisted state JSON and assert startup continues with logged warning while retaining available valid entities.

Effort: M-L (3 integration tests, add 1–2 helper fixtures).

### P3: CI and regression guardrails
- `scripts/` + `.github/workflows/ci.yml`  
  Add a lightweight audit-safety test in repo CI:
  - `scripts/run_audit_remediation_demo.sh` smoke + pinned action SHA check remains green.
  - Add a minimal assertion test that required jobs exist (`ci` check+test+fmt+clippy+build) and failure aborts fast.
- `tests/release.rs` or new regression test under `tests`  
  Validate that CI-visible invariants (changelog/release and lockfile expectations) remain consistent.

Effort: M (small workflow + script guard additions).

## 4) Acceptance criteria by priority

1. P0 complete when startup guards and malformed-path/body contracts are proven with deterministic 4xx/ non-zero exits, and no regressions in happy paths.
2. P1 complete when callback and hook failures are explicitly classified as non-fatal with completion data preserved.
3. P2 complete when project foreman APIs, job terminal transitions, and corrupted-state recovery have explicit contract tests.
4. P3 complete when CI checks are self-validating and cannot silently drift (especially action pinning and required stages).

## Command validation (for the new report verification)

- `cargo test --test integration -- --nocapture`
- `cargo test --test e2e -- --nocapture`
- `cargo test --test release -- --nocapture`
- `cargo test --all`
- `./scripts/run_audit_remediation_demo.sh`
- `cargo fmt --all --check`
- `cargo clippy --all-targets --all-features -- -D warnings`

## Risk notes

- `tests/integration.rs` already has long-running async flows; adding many contract-failure cases may increase runtime unless tests are tightly scoped with short timeouts.
- Some error semantics are implementation-defined in `main.rs`/`foreman.rs`; acceptance criteria should match current status-code behavior to avoid false-negative test churn.
- CI-level validation improvements require approval because workflow edits can alter local developer-only CI assumptions.
