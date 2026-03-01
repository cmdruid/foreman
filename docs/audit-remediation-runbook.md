# Audit Remediation Runbook

This runbook documents a repeatable end-to-end exercise for testing
`codex-foreman` against existing audit artifacts.

The goal is to:
1. start `codex-foreman`,
2. spawn a project from the local `.project` directory,
3. define explicit parallel audit-remediation worker prompts (security, robustness, code quality, technical debt, test coverage),
4. capture worker outputs in one wait/result call, and
5. cleanly tear everything down.

## Prerequisites

- `curl`
- `jq`
- Built `codex-foreman` binary at `target/debug/codex-foreman` (or use `--foreman-bin` override)
- `codex` CLI available for app-server bootstrap
- A writable temp area for state/logs

## Recommended One-Command Run

```bash
cd ~/repos/codex-foreman
./scripts/run_audit_remediation_demo.sh
```

The script performs:
- temporary service config write,
- foreman startup,
- health check,
- project bootstrap from `.project`,
- job dispatch from `.audit/security.md`, `.audit/robustness.md`, `.audit/code-quality.md`, `.audit/technical-debt.md`, `.audit/test-coverage.md`,
- job wait with worker results,
- optional project deletion and foreman shutdown.

## Manual Execution (Equivalent Steps)

1. Start a local service
   - `codex-foreman` on a fixed local port.
   - Configure service callbacks only if you need command/webhook testing.
2. Create a project:
   - `POST /projects` with `{"path":"<repo>/.project"}`.
3. Start one job with one worker per audit category:
   - `POST /projects/{project_id}/jobs` with a `workers` array.
   - Foreman only executes the explicit worker prompts you include in this array.
4. Wait for completion:
   - `GET /jobs/{job_id}/wait?include_workers=true&timeout_ms=120000`.
5. Validate:
   - Job result has one worker entry per category.
   - Worker texts are present in `workers[].final_text`/`summary`.
6. Ask foreman for consolidation (optional):
   - `POST /projects/{project_id}/foreman/send` with a consolidation prompt.
7. Delete the project to exercise teardown:
   - `DELETE /projects/{project_id}`.
8. Stop `codex-foreman`.

## Scripted Workflow

- File: [`scripts/run_audit_remediation_demo.sh`](/home/cmd/repos/codex-foreman/scripts/run_audit_remediation_demo.sh)
- It is designed to be idempotent for repeated reruns and prints each important artifact.

Key behavior:
- Builds workers from audit templates in `.audit/*`.
- Adds a per-worker label `{ "category": "<name>" }`.
- Uses the job API so workers execute in parallel.
- Foreman is the executor of explicit worker prompts, not a planner that invents worker tasks.
- Supports configurable timeout/poll values via env vars.
- Captures full job result and saves it to `stdout` for copy/paste review.

## Environment Knobs

- `PORT` (default: `8898`)
- `CODEX_BIN` (default: `/home/cmd/.npm-global/bin/codex`)
- `FOREMAN_BIN` (default: `/usr/local/bin/codex-foreman`, with repo debug fallback)
- `FOREMAN_STATE_PATH` (default: `/tmp/cf-audit-remediation-state.json`)
- `SERVICE_CONFIG_PATH` (default: `/tmp/cf-audit-remediation-service.toml`)
- `PROJECT_PATH` (default: `$(pwd)/.project`)
- `REPORT_DIR` (default: `$(pwd)/.audit`)
- `JOB_TIMEOUT_MS` (default: `180000`)
- `JOB_POLL_MS` (default: `250`)
- `WAIT_FOREMAN_SECONDS` (default: `30`)

## Interpretation of Outputs

- `project_id`: identifies the running project scope.
- `job_id`: identifies the audit-remediation job.
- `job.wait` response:
  - `status`: terminal status (`completed`/`partial`/`failed`).
  - `workers`: one result per audit category.
  - `workers[].summary` and/or `workers[].final_text`: your remediation recommendations.
- If `timed_out` is `true`, increase timeout and rerun.

## Failure Recovery

- If healthcheck fails, read `foreman.log` and check:
  - codex binary path,
  - app-server startup timeout,
  - port conflicts,
  - file permission to state path.
- If the codex server exits early, the script stops and prints a hint to inspect logs.
- If a single worker stalls, rerun with longer `JOB_TIMEOUT_MS` and keep prompts concise.
