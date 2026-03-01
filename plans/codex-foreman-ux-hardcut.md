# Implementation Plan: Agent Experience UX Hardening (Alpha)

Date: 2026-03-01  
Repository: `codex-foreman`  
Goal: Make `codex-foreman` easier to use for autonomous agents by replacing ad-hoc event parsing with stable result/wait semantics and batch orchestration primitives.

## Principles

- Hard-cut changes only; no backward compatibility constraints.
- Preserve existing behavior while making workflows deterministic.
- Prefer API/CLI contracts that reduce custom agent glue code.
- Prioritize reliability and observability over micro-optimizations.

## Current state of pain (from live audit)

- No canonical result endpoint; final text is only in event history.
- Completion detection is indirect (status/event polling only).
- Callback/event payloads are not normalized for machine consumption.
- Multi-worker audit mode requires external bookkeeping for IDs/categories.
- Service lifecycle visibility is manual.

---

## Phase 0 — Foundation (before changes)

### 0.1 Define data model changes

- [ ] Extend core models to expose stable worker completion state.
- Files to update:
  - `src/models.rs`
  - `src/state.rs` (if persisted result is stored)
- Add new types:
  - `AgentResult` (status/state/final_text/summary/references/files/error/event_id/turn_id/completed_at)
  - `WaitResult` (terminal enum + context fields)
  - `ResultView` response DTO for `GET /agents/{id}/result`

### 0.2 Event/result contract baseline

- [ ] Define canonical completion payload shape and attach to completion handling.
- [ ] Decide where final output is persisted:
  - in-memory agent record + persisted state (`state.rs`) for crash recovery.

---

## Phase 1 — P0: Predictable completion and retrieval (highest priority)

### 1.1 Canonical worker result endpoint

- [ ] Add endpoint:
  - `GET /agents/{id}/result`
- [ ] Add handler in `src/main.rs`:
  - `get_agent_result`
- [ ] Add foreman method in `src/foreman.rs`:
  - `get_agent_result(agent_id: Uuid) -> Result<AgentResult>`
- [ ] Ensure result includes:
  - `agent_id`, `status`, `state`, `thread_id`, `turn_id`, `completed_at`, `summary`, `final_text`,
    `findings` (if any), `references`, `error`.
- [ ] Return 404 when missing; return terminal summary for completed/aborted/error states.

### 1.2 Deterministic wait endpoint/behavior

- [ ] Add endpoint:
  - `GET /agents/{id}/wait`
- [ ] Query params:
  - `timeout_ms` (default 0 = no timeout),
  - `poll_ms` (default 250),
  - `include_events` (default false).
- [ ] Handler in `src/main.rs`:
  - `wait_agent`
- [ ] Foreman helper in `src/foreman.rs`:
  - wait loop via existing state snapshots / event notifier channel.
- [ ] Return structured terminal state:
  - `completed | aborted | errored | timeout`

### 1.3 Store parsed final result at turn completion

- [ ] Parse and capture:
  - final assistant text
  - optional summary/finding blocks
  - file refs (best-effort extraction, fallback to raw text)
- [ ] Populate agent record field:
  - `result: Option<AgentResult>`
- [ ] On completion and error events, persist result into state.

### 1.4 Standardize completion payload in event stream

- [ ] Ensure `turn/completed` path includes a compact machine-readable payload:
  - `result: { summary, tags, severity?, references, completion_reason }`
- [ ] Backward-compatible: keep existing free-form output.

### 1.5 Minimal tests for P0

- [ ] Add integration tests in `tests/integration.rs`:
  - `test_agent_result_endpoint_for_completed_turn`
  - `test_agent_wait_returns_completed`
  - `test_agent_wait_timeout`
  - `test_result_persists_after_restart`
- [ ] Update/fix fixtures if needed in `tests/fixtures`.

---

## Phase 2 — P1: Batch orchestration UX

### 2.1 Add CLI run/wait workflows

- [ ] Extend CLI (likely `src/main.rs` CLI parser/subcommands):
  - `codex-foreman run ... --wait --emit-json`
  - `codex-foreman run-project --path ... --template ... --model ... --wait`
  - `codex-foreman wait <agent-id> [--timeout] [--json]`
- [ ] Commands should print normalized result JSON/NDJSON by default.

### 2.2 Multi-worker batch/job handle

- [ ] Add job abstraction for grouped workers:
  - `JobRecord` (id, project_id optional, category map, worker_ids, status, created_at/completed_at)
- [ ] Add state storage for jobs in `src/state.rs`.
- [ ] Add routes:
  - `POST /projects/{id}/jobs` (creates labeled batch with worker specs),
  - `GET /jobs/{job_id}`,
  - `GET /jobs/{job_id}/results`.
- [ ] CLI option:
  - `codex-foreman run ... --batch-label "security" --batch-label "robustness" ...`

### 2.3 Batch-level result aggregation

- [ ] Provide job result endpoint returning:
  - per-worker result links
  - aggregate severity/status summary
  - optional short merged summary

### 2.4 Tests for batch orchestration

- [ ] Add tests:
  - `test_job_creation_and_worker_tracking`
  - `test_job_wait_all`
  - `test_job_result_aggregation`

---

## Phase 3 — P1/P2: Observability and diagnostics

### 3.1 Error summary on agent state

- [ ] Add compact `last_error`, `error_count`, `last_error_at` fields.
- [ ] Ensure completion/abort/errors update these fields.

### 3.2 Tail events endpoint

- [ ] Add:
  - `GET /agents/{id}/events?tail=100`
- [ ] Keep filters for:
  - `methods=turn/completed,item/agentMessage,thread/status/changed` (optional)
- [ ] Use as fallback diagnostics without full event log dumps.

### 3.3 Service status/diagnostics

- [ ] Add endpoint:
  - `GET /status`
- [ ] Expose:
  - pid, bind, codex path, app-server pid if managed,
    startup time, ready state, state file path, version, uptime.

### 3.4 CLI status/start-stop

- [ ] Add:
  - `codex-foreman status`
  - `codex-foreman stop`
  - `codex-foreman restart` (hard-cut: managed process wrapper where feasible)

### 3.5 Error and timeout observability tests

- [ ] Add tests for:
  - error summary fields on failed turns
  - event tail filtering
  - status endpoint metadata shape

---

## Phase 4 — P2: Callback/event contract stability

### 4.1 Shared callback dispatch path

- [ ] Refactor callback handling in `src/foreman.rs`:
  - one unified path for webhook/command/profile/legacy URL with shared timeout/error behavior.
- [ ] Ensure explicit and consistent timeout handling.
- [ ] Make callback errors surfaced in result summary where relevant.

### 4.2 Standardized callback payload fields

- [ ] Document/implement guaranteed fields for callback events:
  - completion result reference + minimal context IDs.
- [ ] Remove ambiguity between callback payload and event payload.

### 4.3 Validation tests

- [ ] Add integration test for callback timeout path not blocking.
- [ ] Add callback payload contract test.

---

## Phase 5 — Documentation and migration notes

- [ ] Update:
  - `README.md`
  - `docs/manual.md`
  - `RELEASES.md` (new “UX hardening” release entry)
- [ ] Add usage snippets for:
  - result endpoint
  - wait endpoint
  - job/batch workflows
  - run + wait CLI
- [ ] Add troubleshooting section for stuck/empty outputs.

---

## Verification scope (target commands)

- [ ] `cargo test`
- [ ] `cargo test --test integration` (or targeted integration cases above)
- [ ] `cargo test --test e2e`
- [ ] Manual smoke script:
  - create project,
  - dispatch 2-3 workers,
  - poll via `wait`,
  - read results via `/agents/{id}/result`.

---

## Risks and mitigation

- **State shape changes**: requires migration to include result/job state.
  - Mitigate by handling missing fields with defaults.
- **Longer handler changes**: more API surface to test.
  - Mitigate with focused test additions above.
- **Long-running wait endpoint**: resource pressure under heavy usage.
  - Mitigate by capped wait loops and bounded polling.

---

## Delivery order (recommended)

1. **Week 1 (P0):** result endpoint + wait endpoint + completion persistence.
2. **Week 2 (P1):** result-first CLI (`run/wait`) + job grouping basics.
3. **Week 3 (P1/P2):** events tail + status + callback contract alignment + docs/tests.

This sequencing yields immediate UX value first, then richer workflow ergonomics, then callback hardening.
