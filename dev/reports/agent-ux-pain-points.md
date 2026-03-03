# Live Demonstration UX Report: `foreman`

Date: 2026-03-01  
Scope: Live audit demo of `foreman` with parallel workers, run via `/projects`, `/projects/{id}/workers`, and `GET /agents/{id}` polling.

## Executive Summary

The demonstration completed successfully, but the agent workflow had repeated friction points around:

- orchestration visibility,
- predictable completion and result extraction,
- callback output consistency,
- lifecycle/process ergonomics,
- and command/control consistency across control paths.

These are mostly **operator-facing usability gaps** and **observability gaps**, not hard feature gaps. The tool worked, but the interactive model was not yet tuned for smooth agent-led autonomous usage.

## What we ran during the demo

- Created an audit project under `/home/cmd/repos/foreman/.project`
- Used:
  - `POST /projects` to create a control project
  - `POST /projects/{project_id}/workers` to dispatch 5 workers
  - `GET /agents/{id}` polling loop until each worker became idle
- Used `gpt-5.3-codex-spark` for all workers.
- Extracted final results from persisted state (`/tmp/cf-audit-state-2.json`) rather than direct `GET` result payload.

## Pain points and UX issues observed

### 1) Unclear process/runtime ownership

**Issue**

- The demo required manual process control outside the tool (`kill`, PID tracking, separate `--bind` settings).
- The service required an external supervisory approach, and process health wasn’t surfaced via a single status abstraction.

**Impact**

- Agent-led workflows become fragile when the process is expected to recover/reconnect transparently.
- Human operators need to remember per-run ports, command flags, and cleanup operations.

**UX consequence**

- The tool did what it should, but the “session experience” felt manual and hard to reason about for longer audits.

### 2) Namespace / execution-context fragility

**Issue**

- In this environment, service startup and HTTP calls were sometimes only reliable in escalated execution contexts.
- Non-privileged runs could fail to interact with the loopback service even though the service had been launched.

**Impact**

- Live agent experiences can fail due to platform execution context differences rather than tool logic.

**UX consequence**

- The agent loses confidence in completion and control state because transport behavior appears inconsistent.

### 3) No single canonical “final result” shape

**Issue**

- `GET /agents/{id}` returned agent metadata and a dense event array.
- The actionable final narrative/report text was embedded inside `item/agentMessage/delta` events, not exposed as a direct result field.
- Turn completion metadata (`turn/completed`) did not include the useful audit report payload itself.

**Impact**

- To obtain report outputs, we had to build custom extraction scripts over full event history.
- This is unreliable for agents without bespoke parsing logic and adds latency between completion and consumption.

**UX consequence**

- Polling is not enough; agents needed event-shape knowledge and post-processing heuristics.

### 4) Completion detection is indirect

**Issue**

- We had to infer completion from status transitions and event stream markers (`item/completed`, `turn/completed`, etc.).

**Impact**

- Increased orchestration complexity for multi-agent parallel runs.
- Additional logic needed to distinguish “idle but no useful output” vs “idle but still processing/reporting later”.

**UX consequence**

- Higher cognitive load in orchestrators and higher chance of false positives/false negatives in automation.

### 5) Callback/event payload ergonomics were not aligned with agent consumption

**Issue**

- Callback and event payloads mixed control data and free-form text; the event-driven signal was not normalized to agent-friendly result schema.
- Some paths produced only minimal metadata (`threadId`, `turn`) instead of the substantive report text.

**Impact**

- Harder for agents to consume completion events as action signals.
- More work needed to map which event stream fields contain final findings.

**UX consequence**

- Extra adapter code is required just to integrate the intended feedback loops.

### 6) No dedicated result fetch primitive

**Issue**

- There is no endpoint like `GET /agents/{id}/result` (or equivalent) that normalizes:
  - status,
  - final text,
  - summary,
  - error details,
  - references to files/lines,
  - and completion reason.

**Impact**

- “Result retrieval” became an ad-hoc parsing task and not a core API operation.

**UX consequence**

- Tool output is difficult to serialize into reliable dashboards, notifications, and handoffs.

### 7) CLI/agent orchestration commands were too low-level

**Issue**

- The demo used API endpoints directly; there was no thin one-shot CLI command for “start foreman, dispatch workers, wait, collect structured outputs”.

**Impact**

- Every automated workflow had to re-implement polling + aggregation glue.

### 8) Multi-worker orchestration required manual coordination

**Issue**

- Worker IDs, roles, and prompt/template mapping were tracked manually from temporary files/scripts.
- No built-in “batch job + result group” object in the UX layer.

**Impact**

- Additional bookkeeping was required to associate each result with category and maintain ordering of findings.

### 9) Error-path visibility is weaker than success-path visibility

**Issue**

- Failure behavior existed and worked, but the demo did not expose a compact “why” object at the high-level agent API.
- In long runs, users would need deeper event introspection to explain why an agent failed or stalled.

**Impact**

- Troubleshooting becomes time-consuming, especially in parallel runs where each agent needs separate deep inspection.

## What this means for agent experience

`foreman` is functionally capable but not yet agent-ergonomic for:

- “Fire-and-wait” autonomous execution,
- one-shot parallel delegation,
- and concise handoff of results.

The tool needs better abstractions that separate control-plane mechanics from result consumption.

## Recommended UX improvement set

Priority ranking is from highest immediate impact to lower, given the live demo behavior.

### P0 (make demo feel predictable)

1. Add a stable worker result contract
   - New endpoint: `GET /agents/{id}/result`
   - Include: `status`, `state`, `summary`, `final_text`, `error`, `references`, `event_count`, `completed_at`.

2. Add `wait` primitives
   - `GET /workers/{id}/wait` with optional timeout and poll hints, or CLI `foreman wait <id>`.
   - Return explicit terminal states only.

3. Standardize completion event payloads
   - Ensure `turn/completed` carries a compact, parseable result object for all completion modalities.

### P1 (make agent batching simple)

4. Add batch/project job abstraction
   - `POST /projects/{id}/jobs` (or dedicated orchestration handle) that tracks worker set + category labels + summary.

5. Add run mode for CLI
   - `foreman run ... --model ... --template ... --wait --emit-json`.
   - Supports multi-worker fan-out and final merged result output.

6. Improve callback/result schema docs
   - Clear contract for event payload vs callback payload and where findings are guaranteed.

### P2 (make reliability visible)

7. Surface service/process status in API
   - `foreman status` CLI/endpoint with PID, uptime, last event, restart reason.

8. Improve health and connectivity clarity
   - explicit startup diagnostics for namespace/context mismatch and listener readiness.

## Retrospective outcome

The live demonstration validated core execution and persistence flows, but the operator/agent experience is currently “manual by design” in several steps.  
The most severe issues are not feature absence, but **result ergonomics and orchestration ergonomics**.  
Addressing those first will give immediate gains in autonomous usability with relatively small API and CLI surface changes.

