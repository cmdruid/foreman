# foreman vNext DX plan

This plan scopes the next DX-focused release train with concrete tasks, file touch points, and test gates.

## Goals

- Make configuration failures fast and self-explanatory.
- Make worker behavior observable without digging through logs.
- Make callback setup and validation deterministic.
- Reduce onboarding friction with simpler project/template flows.
- Add release gates that catch orchestration regressions early.

## Non-goals

- No compatibility shims for legacy HTTP workflows.
- No broad refactor of `codex-api` in this phase.
- No redesign of core orchestration semantics unless required by DX items below.

## Milestone 1: config safety and diagnostics

### 1. `project.toml` schema validation and lint

#### Deliverables

- New command: `foreman project lint --project <path-to-project.toml> [--fix]`.
- Strict unknown-key rejection for project config.
- Actionable diagnostics with field path and line/column when available.
- `--fix` for safe normalizations only (no semantic rewrites).

#### Implementation

- Add project lint command plumbing in `src/main.rs`.
- Add validator module in `src/project.rs` (or `src/project_lint.rs` if split is cleaner).
- Extend config parse errors to include stable code + contextual field paths in `src/foreman.rs` and parse helpers.
- Centralize constants in `src/constants.rs` for lint codes/messages.

#### Tests

- Add integration tests in `test/integration.rs`:
  - invalid key rejected
  - missing prompt file reported
  - missing callback profile referenced by lifecycle block reported
  - `--fix` rewrites only safe fields
- Add fixture set under `test/fixtures/` for lint valid/invalid cases.

### 2. `foreman doctor`

#### Deliverables

- New command: `foreman doctor --project <path>`.
- Checks:
  - codex binary existence/invocation
  - socket/state directory writability
  - callback command executability
  - project root and `.git` access for worktree flows
  - required prompt files exist
- Exit codes:
  - `0` all pass
  - `1` warnings only (optional)
  - `2` failures

#### Implementation

- Add doctor command in `src/main.rs`.
- Add `doctor` execution path in `src/foreman.rs` with structured result model.
- Constants for check IDs, default probe timeouts in `src/constants.rs`.

#### Tests

- e2e coverage in `test/e2e.rs` for pass/fail modes.
- Integration tests in `test/integration.rs` for command probe and permission failure reporting.

### 3. resolved config introspection

#### Deliverables

- New command: `foreman config show --project <path> --resolved`.
- Output merged effective config with explicit precedence annotations:
  - `project.toml`
  - `~/.foreman/config.toml`
  - CLI flags

#### Implementation

- Config merge reporter in `src/foreman.rs`.
- CLI surface in `src/main.rs`.
- Constants for precedence labels in `src/constants.rs`.

#### Tests

- e2e snapshot-style assertions in `test/e2e.rs` for merged output ordering.

### 4. error taxonomy

#### Deliverables

- Stable error codes for key failures:
  - `CFG_*` config/lint
  - `WK_*` worker lifecycle and monitoring
  - `CB_*` callback resolution/execution
- Consistent surface in API and CLI error payloads/messages.

#### Implementation

- Add error code constants in `src/constants.rs`.
- Thread codes through existing error constructors in `src/foreman.rs`, `src/project.rs`, and handlers.

#### Tests

- Integration assertions for representative code paths in `test/integration.rs`.

## Milestone 2: callback reliability and worker observability

### 5. callback dry-run tester

#### Deliverables

- New command: `foreman callbacks test --project <path> --event <method> [--profile <name>]`.
- Prints rendered env/args/payload and executes callback in dry-run mode.
- Reports event filter match/mismatch reasons.

#### Implementation

- Callback resolution/execution reuse in `src/foreman.rs`.
- CLI plumbing in `src/main.rs`.
- Shared render helpers extracted if needed.

#### Tests

- Integration coverage for command and webhook profiles in `test/integration.rs`.
- Failure cases for unresolved template variables and denied execution.

### 6. worker observability API + CLI

#### Deliverables

- Add worker status fields:
  - phase (`starting`, `running`, `stalled`, `restarting`, `completed`, `failed`)
  - last activity timestamp
  - restart count / max
  - inactivity timeout snapshot
- New CLI command: `foreman watch --project <id|path>` with live updates.

#### Implementation

- Worker state model extensions in `src/foreman.rs`.
- Route additions/expansions in API handlers in `src/foreman.rs`.
- CLI rendering loop in `src/main.rs`.
- Constants for phase names and watch intervals in `src/constants.rs`.

#### Tests

- Integration tests for stalled/restart/completed transitions in `test/integration.rs`.
- Mock-server based determinism using existing fake codex harnesses under `test/bin/`.

### 7. safer callback defaults

#### Deliverables

- Configurable defaults for callback timeout/retry/backoff/max concurrency.
- Clear per-profile override semantics.

#### Implementation

- Default constants in `src/constants.rs`.
- Config parse/merge logic in `src/foreman.rs` and config structs.

#### Tests

- Integration tests for default application and override precedence in `test/integration.rs`.

## Milestone 3: project and template UX

### 8. template-based project init

#### Deliverables

- `foreman project init --template audit|remediation|generic [--with-manual]`.
- Versioned built-in templates with opinionated, auditable prompt structures.

#### Implementation

- Init command improvements in `src/main.rs`.
- Template assets in repo (likely under `contrib/demo/templates` or a dedicated template dir).
- Template metadata/version constants in `src/constants.rs`.

#### Tests

- e2e creation tests per template in `test/e2e.rs`.
- Verify generated files + expected sections.

### 9. template upgrade workflow

#### Deliverables

- `foreman project upgrade-template --project <path> [--dry-run]`.
- Non-destructive updates with conflict reporting.

#### Implementation

- Upgrade planner/applicator in `src/foreman.rs` or dedicated module.
- Diff/patch summary output in CLI.

#### Tests

- Integration tests for no-op, clean upgrade, and conflict paths.

### 10. prompt contract + completion checklist

#### Deliverables

- Standard contract header in generated `FOREMAN.md`/`WORKER.md`/`RUNBOOK.md`/`HANDOFF.md`.
- Required completion checklist for worker deliverables.

#### Implementation

- Update scaffold/template content in command handlers/assets.
- Document format in `docs/manual.md` and `README.md`.

#### Tests

- e2e assertions that generated templates include contract and checklist sections.

## Milestone 4: golden path CLI

### 11. single-command run path

#### Deliverables

- New command: `foreman run --project <path>` for start + dispatch + wait summary.

#### Implementation

- Compose existing flows in `src/foreman.rs` with structured run result.
- CLI command in `src/main.rs`.

#### Tests

- Integration test for end-to-end run with mock project and deterministic fake codex in `test/integration.rs`.

### 12. command/flag consistency pass

#### Deliverables

- Normalize naming and help text across CLI subcommands.
- Ensure examples default to standalone binary usage.

#### Implementation

- CLI flags/help updates in `src/main.rs`.
- Docs updates in `README.md`, `TESTING.md`, `docs/manual.md`.

#### Tests

- e2e help-output assertions in `test/e2e.rs`.

## Milestone 5: release gating and docs

### 13. CI smoke gate

#### Deliverables

- Deterministic mock-project smoke test in CI:
  - spawn workers
  - generate deliverables
  - verify lifecycle callbacks and job result summary

#### Implementation

- CI workflow update under `.github/workflows/`.
- Reuse `contrib/demo` scenario and `test` fixtures/harness.

#### Tests

- CI-required job with artifacts on failure for debug.

### 14. docs consolidation and release checklist

#### Deliverables

- Ensure docs consistently use `foreman` naming.
- Update release checklist in `RELEASE.md` with:
  - smoke gate pass
  - callback test pass
  - doctor pass
  - template generation sanity checks

#### Implementation

- `README.md`
- `docs/manual.md`
- `TESTING.md`
- `RELEASE.md`

## Sequencing and risk control

1. Milestone 1 first (foundation, low feature risk, high leverage).
2. Milestone 2 second (observability + callback determinism).
3. Milestone 3 third (template UX surface changes).
4. Milestone 4 fourth (compose existing capabilities into simpler command flow).
5. Milestone 5 last (CI/docs lock-in).

## Definition of done (release gate)

- All tests pass:
  - `cargo test --test e2e`
  - `cargo test --test integration -- --nocapture`
- New DX commands have help text and docs examples.
- Error codes appear consistently in CLI/API surfaces for key failures.
- CI smoke workflow is green and required for merge/release.
- Changelog updated with user-facing DX changes.
