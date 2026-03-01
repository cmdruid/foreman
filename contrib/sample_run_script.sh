#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
CODEX_BIN="${CODEX_BIN:-/home/cmd/.npm-global/bin/codex}"
FOREMAN_BIN="${FOREMAN_BIN:-/usr/local/bin/codex-foreman}"
FOREMAN_PID=""
WORKERS_FILE=""

for command in jq curl; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "required command '$command' is missing" >&2
    exit 1
  fi
done

if [[ ! -x "$FOREMAN_BIN" && -x "$REPO_ROOT/target/debug/codex-foreman" ]]; then
  FOREMAN_BIN="$REPO_ROOT/target/debug/codex-foreman"
fi

PORT="${PORT:-8898}"
BIND="${BIND:-127.0.0.1:$PORT}"
PROJECT_PATH="${PROJECT_PATH:-$REPO_ROOT/.project}"
REPORT_DIR="${REPORT_DIR:-$REPO_ROOT/.audit}"
AUDIT_OUTPUT_DIR="${AUDIT_OUTPUT_DIR:-$REPO_ROOT/.audit/generated-reports}"
FOREMAN_STATE_PATH="${FOREMAN_STATE_PATH:-/tmp/cf-audit-remediation-state.json}"
SERVICE_CONFIG_PATH="${SERVICE_CONFIG_PATH:-/tmp/cf-audit-remediation-service.toml}"
FOREMAN_LOG_PATH="${FOREMAN_LOG_PATH:-/tmp/cf-audit-remediation-foreman.log}"
JOB_TIMEOUT_MS="${JOB_TIMEOUT_MS:-180000}"
JOB_POLL_MS="${JOB_POLL_MS:-250}"
WAIT_FOREMAN_SECONDS="${WAIT_FOREMAN_SECONDS:-30}"

if [[ ! -x "$CODEX_BIN" ]]; then
  echo "codex binary not executable at: $CODEX_BIN" >&2
  exit 1
fi

if [[ ! -x "$FOREMAN_BIN" ]]; then
  echo "codex-foreman binary not executable at: $FOREMAN_BIN" >&2
  echo "Build it first (cargo build --release) or set FOREMAN_BIN explicitly." >&2
  exit 1
fi

if [[ ! -f "$PROJECT_PATH/project.toml" ]]; then
  echo "missing project config: $PROJECT_PATH/project.toml" >&2
  exit 1
fi

if [[ ! -d "$REPORT_DIR" ]]; then
  echo "missing reports directory: $REPORT_DIR" >&2
  exit 1
fi

mkdir -p "$AUDIT_OUTPUT_DIR"

cat >"$SERVICE_CONFIG_PATH" <<EOF_CFG
[app_server]
initialize_timeout_ms = 10000
request_timeout_ms = 60000
EOF_CFG

echo "using foreman : $FOREMAN_BIN"
echo "using codex  : $CODEX_BIN"
echo "binding at   : $BIND"

echo "state path  : $FOREMAN_STATE_PATH"
echo "log file   : $FOREMAN_LOG_PATH"

echo "starting codex-foreman..."

cleanup() {
  set +e
  if [[ -n "${FOREMAN_PID:-}" ]] && kill -0 "$FOREMAN_PID" 2>/dev/null; then
    echo "stopping codex-foreman (pid $FOREMAN_PID)"
    kill "$FOREMAN_PID"
  fi

  if [[ -n "${WORKERS_FILE:-}" && -f "$WORKERS_FILE" ]]; then
    rm -f "$WORKERS_FILE"
  fi
}
trap cleanup EXIT INT TERM

"$FOREMAN_BIN" \
  --bind "$BIND" \
  --codex-binary "$CODEX_BIN" \
  --service-config "$SERVICE_CONFIG_PATH" \
  --state-path "$FOREMAN_STATE_PATH" \
  >"$FOREMAN_LOG_PATH" 2>&1 &
FOREMAN_PID=$!

for i in $(seq 1 60); do
  if curl -sS "http://$BIND/health" >/dev/null 2>&1; then
    break
  fi
  sleep 0.5
done

if ! curl -sS "http://$BIND/health" >/dev/null 2>&1; then
  echo "foreman healthcheck failed; inspect $FOREMAN_LOG_PATH" >&2
  exit 1
fi

echo "foreman ready"

create_project_payload=$(jq -cn --arg p "$PROJECT_PATH" '{path:$p}')
project_response=$(curl -sS -X POST "http://$BIND/projects" \
  -H 'Content-Type: application/json' \
  -d "$create_project_payload")
project_id=$(jq -r '.project_id // empty' <<<"$project_response")

if [[ -z "$project_id" || "$project_id" == "null" ]]; then
  echo "project creation failed: $project_response" >&2
  exit 1
fi

echo "created project: $project_id"

WORKERS_FILE="$(mktemp)"
echo "[]" >"$WORKERS_FILE"

for report in \
  "$REPORT_DIR/security.md" \
  "$REPORT_DIR/robustness.md" \
  "$REPORT_DIR/code-quality.md" \
  "$REPORT_DIR/technical-debt.md" \
  "$REPORT_DIR/test-coverage.md"; do
  if [[ ! -f "$report" ]]; then
    echo "warn: missing report template $report"
    continue
  fi
  category="$(basename "$report" .md)"
  report_text="$(cat "$report")"
  prompt="You are a codex-foreman audit worker.\\n\\nRun a fresh audit for category '$category' against the repository and generate an actionable audit report.\\n\\nAudit instructions (use as scoring criteria):\\n---\\n${report_text}\\n---\\n\\nReturn in markdown with:\\n- Findings ranked by severity\\n- File targets and line references\\n- Recommended remediations per finding\\n- Validation commands or checks to verify fixes\\n- Risk and effort notes\\n"
  worker=$(jq -cn \
    --arg p "$prompt" \
    --arg category "$category" \
    '{prompt:$p, labels:{"category":$category}, callback_overrides:{callback_events:["turn/completed"]}}')
  workers_file_tmp="$(mktemp)"
  jq -c --argjson w "$worker" '. += [$w]' "$WORKERS_FILE" > "$workers_file_tmp"
  mv "$workers_file_tmp" "$WORKERS_FILE"
done

jobs_payload=$(jq -cn --argjson workers "$(cat "$WORKERS_FILE")" '{workers:$workers}')
job_response=$(curl -sS -X POST "http://$BIND/projects/$project_id/jobs" \
  -H 'Content-Type: application/json' \
  -d "$jobs_payload")
job_id=$(jq -r '.job_id // empty' <<<"$job_response")

if [[ -z "$job_id" || "$job_id" == "null" ]]; then
  echo "job creation failed: $job_response" >&2
  exit 1
fi

job_count=$(jq '.workers | length' <<<"$jobs_payload")
echo "job started: $job_id (workers: $job_count)"

echo "waiting for jobs..."
job_wait_url="http://$BIND/jobs/$job_id/wait?timeout_ms=$JOB_TIMEOUT_MS&poll_ms=$JOB_POLL_MS&include_workers=true"
job_wait_response=$(curl -sS "$job_wait_url")
job_status=$(jq -r '.status // "unknown"' <<<"$job_wait_response")
job_timed_out=$(jq -r '.timed_out // true' <<<"$job_wait_response")
echo "job status: $job_status, timed_out=$job_timed_out"

echo "$job_wait_response" | jq '.'

echo "writing audit reports to: $AUDIT_OUTPUT_DIR"

echo "$job_wait_response" | jq -c '.workers[]' | while read -r worker; do
  worker_category="$(jq -r '.labels.category // "worker"' <<<"$worker")"
  report_body="$(jq -r '.final_text // .summary // ""' <<<"$worker")"
  safe_category="$(printf '%s' "$worker_category" | tr '[:upper:]' '[:lower:]' | tr ' ' '-' | tr -cd 'a-z0-9._/-')"
  if [[ -z "$safe_category" || "$safe_category" == "." ]]; then
    safe_category="worker"
  fi
  report_path="$AUDIT_OUTPUT_DIR/${safe_category}-audit-report.md"
  {
    echo "# Audit report: $worker_category"
    echo
    echo "$report_body"
  } > "$report_path"
  echo "  - $worker_category => $report_path"
done

project_state=$(curl -sS "http://$BIND/projects/$project_id")
foreman_id=$(jq -r '.foreman_agent_id // empty' <<<"$project_state")

if [[ -n "$foreman_id" && "$foreman_id" != "null" ]]; then
  foreman_prompt="Consolidate the worker audit reports into one prioritized remediation plan. Provide an actionable sequencing plan: quick fixes, risk-reducing fixes, and optional hardening follow-ups. Include explicit code path references and confidence levels."
  foreman_send=$(jq -cn --arg p "$foreman_prompt" '{prompt:$p}')
  foreman_wait_url="http://$BIND/agents/$foreman_id/wait?timeout_ms=$((WAIT_FOREMAN_SECONDS*1000))&poll_ms=500&include_events=true"
  foreman_wait_response=$(curl -sS "$foreman_wait_url")
  curl -sS -X POST "http://$BIND/projects/$project_id/foreman/send" \
    -H 'Content-Type: application/json' \
    -d "$foreman_send" >/dev/null
  echo "foreman status:"
  echo "$foreman_wait_response" | jq '.'
fi

jobs_result=$(curl -sS "http://$BIND/jobs/$job_id/result")
echo "final job result:"
echo "$jobs_result" | jq '.'

echo "run complete"
if [[ -n "$FOREMAN_LOG_PATH" && -f "$FOREMAN_LOG_PATH" ]]; then
  echo "last log lines:"
  tail -n 25 "$FOREMAN_LOG_PATH"
fi
