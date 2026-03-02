#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
CODEX_BIN="${CODEX_BIN:-/home/cmd/.npm-global/bin/codex}"
FOREMAN_BIN="${FOREMAN_BIN:-/usr/local/bin/codex-foreman}"
FOREMAN_PID=""
WORKERS_FILE=""
WORKER_CATEGORIES=()

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
WORKTREE_OUTPUT_DIR="${WORKTREE_OUTPUT_DIR:-$REPO_ROOT/.audit/generated-reports}"
FOREMAN_STATE_PATH="${FOREMAN_STATE_PATH:-/tmp/cf-mock-state.json}"
SERVICE_CONFIG_PATH="${SERVICE_CONFIG_PATH:-/tmp/cf-mock-service.toml}"
FOREMAN_LOG_PATH="${FOREMAN_LOG_PATH:-/tmp/cf-mock-foreman.log}"
JOB_TIMEOUT_MS="${JOB_TIMEOUT_MS:-180000}"
JOB_POLL_MS="${JOB_POLL_MS:-250}"
WAIT_FOREMAN_SECONDS="${WAIT_FOREMAN_SECONDS:-30}"
WORKTREE_BASE="${WORKTREE_BASE:-$REPO_ROOT/.audit/worker-worktrees}"

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

mkdir -p "$WORKTREE_OUTPUT_DIR" "$WORKTREE_BASE"
BASE_BRANCH="$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo HEAD)"

safe_slug() {
  local value="$1"
  value="$(printf '%s' "$value" | tr '[:upper:]' '[:lower:]' | tr ' ' '-' | tr -cd 'a-z0-9._/-')"
  if [[ -z "$value" || "$value" == "." ]]; then
    echo "worker"
  else
    echo "$value"
  fi
}

fetch_worker_result() {
  local worker_id="$1"
  curl -sS "http://$BIND/agents/$worker_id/result"
}

looks_like_prompt_payload() {
  local text="$1"
  [[ -z "$text" ]] && return 0
  [[ "$text" == "# Project Worker"* ]] && return 0
  [[ "$text" == "You are a codex-foreman worker."* ]] && return 0
  [[ "$text" == *"TASK"* && "$text" == *"Run a task for category"* ]] && return 0
  return 1
}

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
  safe_category="$(safe_slug "$category")"
  if [[ -z "$safe_category" || "$safe_category" == "." ]]; then
    safe_category="worker-mock"
  fi
  worker_worktree_path="$WORKTREE_BASE/${safe_category}-$(date +%s%3N)-$RANDOM"
  worker_worktree_branch="foreman-${safe_category}-$(date +%s%3N)-$RANDOM"
  report_text="$(cat "$report")"
  prompt=$(cat <<EOF
You are a codex-foreman mock worker.

Run a task for category '$category' and produce an actionable report with concrete fixes.

Worktree instruction:
- Use this exact worktree path for all filesystem operations: $worker_worktree_path
- Create it first from repository root:
  git -C "$REPO_ROOT" worktree add -b "$worker_worktree_branch" "$worker_worktree_path" "$BASE_BRANCH"
- cd "$worker_worktree_path"
- Perform all reads/writes from that worktree only.
- You must make at least one concrete file edit in this worktree. If no safe code fix exists, create
  "$worker_worktree_path/.audit-generated/$safe_category-notes.md" with concrete follow-up items.
- Return the final report in markdown with:
  - Findings ranked by severity
  - File targets and line references
  - Concrete fixes and exact commands used for verification
  - Risk and effort notes

Audit instructions (use as scoring criteria):
---
${report_text}
---
EOF
)
  worker=$(jq -cn \
    --arg p "$prompt" \
    --arg category "$category" \
    --arg safe_category "$safe_category" \
    --arg worktree_path "$worker_worktree_path" \
    --arg worktree_branch "$worker_worktree_branch" \
    '{prompt:$p, labels:{"category":$category,"safe_category":$safe_category,"worktree_path":$worktree_path,"worktree_branch":$worktree_branch}, callback_overrides:{callback_events:["turn/completed"]}}')
  workers_file_tmp="$(mktemp)"
  jq -c --argjson w "$worker" '. += [$w]' "$WORKERS_FILE" > "$workers_file_tmp"
  mv "$workers_file_tmp" "$WORKERS_FILE"
  WORKER_CATEGORIES+=("$category")
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
deadline_ms=$(( $(date +%s%3N) + JOB_TIMEOUT_MS ))
job_wait_url="http://$BIND/jobs/$job_id/wait"
job_wait_response=""
job_status="running"
job_timed_out="false"

while :; do
  now_ms="$(date +%s%3N)"
  remaining_ms=$(( deadline_ms - now_ms ))
  if (( remaining_ms <= 0 )); then
    break
  fi
  poll_ms=$(( remaining_ms < JOB_POLL_MS ? remaining_ms : JOB_POLL_MS ))
  poll_url="${job_wait_url}?timeout_ms=${poll_ms}&poll_ms=${JOB_POLL_MS}&include_workers=true"
  job_wait_response="$(curl -sS "$poll_url")"
  job_status="$(jq -r '.status // "unknown"' <<<"$job_wait_response")"
  job_timed_out="$(jq -r '.timed_out // false' <<<"$job_wait_response")"
  echo "job poll: status=${job_status}, timed_out=${job_timed_out}, remaining_ms=${remaining_ms}"
  if [[ "$job_status" == "completed" || "$job_status" == "failed" || "$job_status" == "cancelled" || "$job_status" == "stopped" ]]; then
    job_timed_out="false"
    break
  fi
done

if [[ -z "$job_wait_response" ]]; then
  job_wait_response='{"status":"unknown","timed_out":true}'
fi

if [[ "$job_status" == "completed" || "$job_status" == "failed" || "$job_status" == "cancelled" || "$job_status" == "stopped" ]]; then
  job_timed_out="false"
else
  job_timed_out="$(jq -r '.timed_out // true' <<<"$job_wait_response")"
fi

# Fetch result only if we did not receive worker payload from wait.
if (( "$(jq -r '( .workers // [] | length )' <<<"$job_wait_response")" == 0 )); then
  job_wait_response=$(curl -sS "http://$BIND/jobs/$job_id/result")
fi

job_status=$(jq -r '.status // "unknown"' <<<"$job_wait_response")
if [[ "$job_status" == "completed" || "$job_status" == "failed" || "$job_status" == "cancelled" || "$job_status" == "stopped" ]]; then
  job_timed_out="false"
else
  job_timed_out="$(jq -r '.timed_out // true' <<<"$job_wait_response")"
fi
echo "job status: $job_status, timed_out=$job_timed_out"

echo "$job_wait_response" | jq '.'

echo "writing mock worker reports to: $WORKTREE_OUTPUT_DIR"
rm -f "$WORKTREE_OUTPUT_DIR/worker-mock-report.md"

mapfile -t job_workers < <(jq -c '.workers // [] | .[]' <<<"$job_wait_response")
if (( ${#job_workers[@]} == 0 )); then
  echo "no worker results were returned"
else
  for i in "${!job_workers[@]}"; do
    worker="${job_workers[$i]}"
    worker_category="$(jq -r '(.labels.category // empty) | if type == "array" then .[0] else . end' <<<"$worker")"
    if [[ "$worker_category" == "null" || "$worker_category" == "" ]]; then
      worker_category="${WORKER_CATEGORIES[$i]:-worker-$((i+1))}"
    fi
    worker_id="$(jq -r '.agent_id // empty' <<<"$worker")"
    report_body="$(jq -r '.final_text // .summary // ""' <<<"$worker")"
    if looks_like_prompt_payload "$report_body" && [[ -n "$worker_id" && "$worker_id" != "null" ]]; then
      echo "worker $worker_id returned request-like text; fetching canonical worker result"
      agent_result="$(fetch_worker_result "$worker_id")"
      report_body="$(jq -r '.final_text // .summary // ""' <<<"$agent_result")"
    fi
    if looks_like_prompt_payload "$report_body"; then
      echo "warning: worker $worker_id still looks like prompt-only output"
    fi

    worker_safe_category="$(jq -r '(.labels.safe_category // empty) | if type == "array" then .[0] else . end' <<<"$worker")"
    if [[ "$worker_safe_category" == "null" || "$worker_safe_category" == "" ]]; then
      worker_safe_category="$(safe_slug "$worker_category")"
    fi
    safe_category="$worker_safe_category"
    if [[ -z "$safe_category" || "$safe_category" == "." ]]; then
      safe_category="worker-$((i+1))"
    fi

    worker_worktree_path="$(jq -r '(.labels.worktree_path // empty) | if type == "array" then .[0] else . end' <<<"$worker")"
    if [[ -n "$worker_worktree_path" && -d "$worker_worktree_path" ]]; then
      worktree_changes="$(git -C "$worker_worktree_path" status --short || true)"
      if [[ -n "$worktree_changes" ]]; then
        echo "  - worker $worker_id changed files in $worker_worktree_path:"
        echo "$worktree_changes"
      else
        echo "  - warning: worker $worker_id had no file modifications in $worker_worktree_path"
      fi
    else
      echo "  - warning: worktree missing for worker $worker_id: $worker_worktree_path"
    fi
    report_path="$WORKTREE_OUTPUT_DIR/${safe_category}-mock-report.md"
    {
      echo "# Mock worker report: $worker_category"
      echo
      echo "$report_body"
    } > "$report_path"
    echo "  - $worker_category => $report_path"
  done
fi

project_state=$(curl -sS "http://$BIND/projects/$project_id")
foreman_id=$(jq -r '.foreman_agent_id // empty' <<<"$project_state")

if [[ -n "$foreman_id" && "$foreman_id" != "null" ]]; then
  foreman_prompt="Consolidate the worker reports into one prioritized action plan. Provide an actionable sequencing plan: quick fixes, risk-reducing fixes, and optional hardening follow-ups. Include explicit code path references and confidence levels."
  foreman_send=$(jq -cn --arg p "$foreman_prompt" '{prompt:$p}')
  foreman_send_url="http://$BIND/projects/$project_id/foreman/send"
  foreman_send_response=$(curl -sS -X POST "$foreman_send_url" \
    -H 'Content-Type: application/json' \
    -d "$foreman_send")
  echo "foreman send response:"
  echo "$foreman_send_response" | jq '.'

  foreman_wait_url="http://$BIND/agents/$foreman_id/wait"
  foreman_wait_response=""
  foreman_status="running"
  foreman_deadline_ms=$(( $(date +%s%3N) + (WAIT_FOREMAN_SECONDS * 1000) ))
  while :; do
    now_ms="$(date +%s%3N)"
    remaining_ms=$(( foreman_deadline_ms - now_ms ))
    if (( remaining_ms <= 0 )); then
      break
    fi
    poll_ms=$(( remaining_ms < JOB_POLL_MS ? remaining_ms : JOB_POLL_MS ))
    foreman_wait_response=$(curl -sS "${foreman_wait_url}?timeout_ms=${poll_ms}&poll_ms=${JOB_POLL_MS}&include_events=true")
    foreman_status="$(jq -r '.status // "unknown"' <<<"$foreman_wait_response")"
    foreman_timed_out="$(jq -r '.timed_out // false' <<<"$foreman_wait_response")"
    echo "foreman poll: status=${foreman_status}, timed_out=${foreman_timed_out}, remaining_ms=${remaining_ms}"
    if [[ "$foreman_status" == "completed" || "$foreman_status" == "failed" || "$foreman_status" == "cancelled" || "$foreman_status" == "stopped" ]]; then
      break
    fi
  done

  echo "foreman final status: $foreman_status"
  if [[ -n "$foreman_wait_response" ]]; then
    echo "$foreman_wait_response" | jq '.'
  fi
fi

jobs_result=$(curl -sS "http://$BIND/jobs/$job_id/result")
echo "final job result:"
echo "$jobs_result" | jq '.'

echo "run complete"
if [[ -n "$FOREMAN_LOG_PATH" && -f "$FOREMAN_LOG_PATH" ]]; then
  echo "last log lines:"
  tail -n 25 "$FOREMAN_LOG_PATH"
fi
