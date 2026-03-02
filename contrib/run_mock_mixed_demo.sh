#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_AS_USER="${RUN_AS_USER:-$(id -un)}"
CURRENT_USER="$(id -un)"

if [[ "$CURRENT_USER" != "$RUN_AS_USER" ]]; then
  if command -v sudo >/dev/null 2>&1; then
    echo "restarting as user $RUN_AS_USER (requested by RUN_AS_USER) from $CURRENT_USER"
    exec sudo -u "$RUN_AS_USER" -H "$0" "$@"
  elif command -v su >/dev/null 2>&1; then
    echo "restarting as user $RUN_AS_USER (requested by RUN_AS_USER) from $CURRENT_USER"
    exec su -s /bin/bash "$RUN_AS_USER" -c "$(printf '%q ' "$0" "$@")"
  else
    echo "cannot switch user (no sudo or su available) when current user is $CURRENT_USER and RUN_AS_USER is $RUN_AS_USER" >&2
    exit 1
  fi
fi

CODEX_BIN="${CODEX_BIN:-/home/cmd/.npm-global/bin/codex}"
FOREMAN_BIN="${FOREMAN_BIN:-$REPO_ROOT/target/debug/codex-foreman}"
FOREMAN_PID=""
WORKERS_FILE=""
FAILED=false

for command in jq curl git; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "required command '$command' is missing" >&2
    exit 1
  fi
done

if [[ ! -x "$FOREMAN_BIN" && -x "$REPO_ROOT/target/debug/codex-foreman" ]]; then
  FOREMAN_BIN="$REPO_ROOT/target/debug/codex-foreman"
fi

if [[ ! -x "$FOREMAN_BIN" ]]; then
  echo "codex-foreman binary not executable at: $FOREMAN_BIN" >&2
  exit 1
fi

if [[ ! -x "$CODEX_BIN" ]]; then
  echo "codex binary not executable at: $CODEX_BIN" >&2
  echo "Set CODEX_BIN to a valid executable codex binary path." >&2
  exit 1
fi

PORT="${PORT:-8897}"
PROJECT_PATH="${PROJECT_PATH:-$REPO_ROOT/contrib/mock}"
REPORT_DIR="${REPORT_DIR:-/tmp/cf-mock-mixed/reports}"
WORKTREE_BASE="${WORKTREE_BASE:-/tmp/cf-mock-mixed/worktrees}"
BIND="${BIND:-127.0.0.1:$PORT}"
FOREMAN_STATE_PATH="${FOREMAN_STATE_PATH:-/tmp/cf-mock-mixed-state.json}"
SERVICE_CONFIG_PATH="${SERVICE_CONFIG_PATH:-/tmp/cf-mock-mixed-service.toml}"
FOREMAN_LOG_PATH="${FOREMAN_LOG_PATH:-/tmp/cf-mock-mixed-foreman.log}"
JOB_TIMEOUT_MS="${JOB_TIMEOUT_MS:-180000}"
JOB_POLL_MS="${JOB_POLL_MS:-250}"
WORKER_SANDBOX="${WORKER_SANDBOX:-workspace-write}"
BASE_BRANCH="$(git -C "$PROJECT_PATH" rev-parse --abbrev-ref HEAD 2>/dev/null || echo HEAD)"
mkdir -p "$WORKTREE_BASE" "$REPORT_DIR" "$PROJECT_PATH/.audit-generated"

cat >"$SERVICE_CONFIG_PATH" <<EOF_CFG
[app_server]
initialize_timeout_ms = 10000
request_timeout_ms = 60000
EOF_CFG

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
  return 1
}

echo "using foreman : $FOREMAN_BIN"
echo "using codex  : $CODEX_BIN"
echo "running as   : $CURRENT_USER"
echo "binding at   : $BIND"
echo "project path : $PROJECT_PATH"

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

echo "starting codex-foreman..."
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

if [[ ! -f "$PROJECT_PATH/project.toml" ]]; then
  echo "missing project config: $PROJECT_PATH/project.toml" >&2
  exit 1
fi

project_response=$(curl -sS -X POST "http://$BIND/projects" \
  -H 'Content-Type: application/json' \
  -d "$(jq -cn --arg p "$PROJECT_PATH" '{path:$p}')")
project_id=$(jq -r '.project_id // empty' <<<"$project_response")
if [[ -z "$project_id" || "$project_id" == "null" ]]; then
  echo "project creation failed: $project_response" >&2
  exit 1
fi
echo "created project: $project_id"

WORKERS_FILE="$(mktemp)"
echo "[]" >"$WORKERS_FILE"
declare -a WORKER_CATEGORIES=()

append_worker() {
  local category="$1"
  local worktree="$2"
  local worktree_path="$3"
  local task_file="$4"
  local artifact_path="$5"

  safe_category="$(safe_slug "$category")"
  prompt="$(cat <<EOF
You are a codex-foreman mock worker for category: $category.
Worktree instruction:
- Use the current working directory for all filesystem operations: $worktree_path
- Read the assignment source: $task_file
- Create and write "$artifact_path" with actual content.
- Return a concise completion summary with what changed.
EOF
)"

  if [[ "$worktree" == "true" ]]; then
    worker=$(jq -cn \
      --arg p "$prompt" \
      --arg category "$category" \
      --arg safe_category "$safe_category" \
      --arg worktree_path "$worktree_path" \
      --arg artifact_path "$artifact_path" \
      --arg base_ref "$BASE_BRANCH" \
      --arg sandbox "$WORKER_SANDBOX" \
      '{prompt:$p, cwd:$worktree_path, sandbox:$sandbox, worktree:{path:$worktree_path, create:true, base_ref:$base_ref}, labels:{"category":$category,"safe_category":$safe_category,"artifact_path":$artifact_path,"worktree_path":$worktree_path,"worktree":$worktree_path,"requires_worktree":"true"}, callback_overrides:{callback_events:["turn/completed","item/assistantMessage","item/assistantMessage/delta","item/agentMessage","item/agentMessage/delta"]}}')
  else
    worker=$(jq -cn \
      --arg p "$prompt" \
      --arg category "$category" \
      --arg safe_category "$safe_category" \
      --arg artifact_path "$artifact_path" \
      --arg worktree_path "$worktree_path" \
      --arg sandbox "$WORKER_SANDBOX" \
      '{prompt:$p, cwd:$worktree_path, sandbox:$sandbox, labels:{"category":$category,"safe_category":$safe_category,"artifact_path":$artifact_path,"worktree_path":$worktree_path,"requires_worktree":"false"}, callback_overrides:{callback_events:["turn/completed","item/assistantMessage","item/assistantMessage/delta","item/agentMessage","item/agentMessage/delta"]}}')
  fi

  workers_file_tmp="$(mktemp)"
  jq -c --argjson w "$worker" '. += [$w]' "$WORKERS_FILE" >"$workers_file_tmp"
  mv "$workers_file_tmp" "$WORKERS_FILE"
  WORKER_CATEGORIES+=("$category")
}

security_category="security"
security_worktree_path="$WORKTREE_BASE/security-mixed-$(date +%s%3N)-$RANDOM"
security_task_file="$PROJECT_PATH/tasks/security.md"
security_artifact="$security_worktree_path/.audit-generated/security-worker-deliverable.md"
mkdir -p "$WORKTREE_BASE"
append_worker "$security_category" "true" "$security_worktree_path" "$security_task_file" "$security_artifact"

notes_category="notes"
notes_artifact="$PROJECT_PATH/.audit-generated/mock-notes-deliverable.md"
append_worker "$notes_category" "false" "$PROJECT_PATH" "$PROJECT_PATH/README.md" "$notes_artifact"

jobs_payload=$(jq -cn --argjson workers "$(cat "$WORKERS_FILE")" '{workers:$workers}')
job_response=$(curl -sS -X POST "http://$BIND/projects/$project_id/jobs" \
  -H 'Content-Type: application/json' \
  -d "$jobs_payload")
job_id=$(jq -r '.job_id // empty' <<<"$job_response")
if [[ -z "$job_id" || "$job_id" == "null" ]]; then
  echo "job creation failed: $job_response" >&2
  exit 1
fi

echo "job started: $job_id"

job_wait_response=$(curl -sS "http://$BIND/jobs/$job_id/wait?timeout_ms=$JOB_TIMEOUT_MS&poll_ms=$JOB_POLL_MS&include_workers=true")
echo "$job_wait_response" | jq '.'

job_status=$(jq -r '.status // "unknown"' <<<"$job_wait_response")
job_timed_out=$(jq -r '.timed_out // false' <<<"$job_wait_response")
if [[ "$job_timed_out" == "true" ]]; then
  echo "job timed out while waiting: status=$job_status" >&2
  FAILED=true
fi

mapfile -t job_workers < <(jq -c '.workers // [] | .[]' <<<"$job_wait_response")
if (( ${#job_workers[@]} == 0 )); then
  echo "no worker results were returned"
  FAILED=true
else
  for i in "${!job_workers[@]}"; do
    worker="${job_workers[$i]}"
    worker_id="$(jq -r '.agent_id // empty' <<<"$worker")"
    category="$(jq -r '(.labels.category // empty) | if type == "array" then .[0] else . end' <<<"$worker")"
    safe_category="$(safe_slug "$category")"

    if [[ "$category" == "null" || "$category" == "" ]]; then
      category="${WORKER_CATEGORIES[$i]:-worker-$((i+1))}"
      safe_category="$(safe_slug "$category")"
    fi

    report_body="$(jq -r '.final_text // .summary // ""' <<<"$worker")"
    if looks_like_prompt_payload "$report_body" && [[ -n "$worker_id" && "$worker_id" != "null" ]]; then
      report_body="$(jq -r '.final_text // .summary // ""' <<<"$(fetch_worker_result "$worker_id")")"
    fi

    artifact_path="$(jq -r '(.labels.artifact_path // empty) | if type == "array" then .[0] else . end' <<<"$worker")"
    if [[ "$artifact_path" == "null" || "$artifact_path" == "" ]]; then
      requires_worktree="$(jq -r '(.labels.requires_worktree // empty) | if type == "array" then .[0] else . end' <<<"$worker")"
      if [[ "$requires_worktree" == "true" ]]; then
        artifact_path="$WORKTREE_BASE/${safe_category}-fallback-$(date +%s%3N)-$RANDOM/.audit-generated/${safe_category}-worker-deliverable.md"
      else
        artifact_path="$PROJECT_PATH/.audit-generated/${safe_category}-worker-deliverable.md"
      fi
    fi

    requires_worktree="$(jq -r '(.labels.requires_worktree // empty) | if type == "array" then .[0] else . end' <<<"$worker")"
    if [[ -z "$requires_worktree" || "$requires_worktree" == "null" ]]; then
      requires_worktree="unknown"
    fi
    echo "worker $worker_id ($category) requires_worktree=$requires_worktree artifact=$artifact_path"

    if [[ -z "$worker_id" || "$worker_id" == "null" ]]; then
      echo "warning: worker $i had missing agent_id"
      FAILED=true
    fi
    if [[ -z "$report_body" || "$report_body" == "null" ]]; then
      echo "warning: worker $worker_id returned empty completion text"
      FAILED=true
    fi
    if ! jq -e 'any(.[]; .method == "item/assistantMessage" or .method == "item/assistantMessage/delta" or .method == "item/agentMessage" or .method == "item/agentMessage/delta")' <<<"$(curl -sS "http://$BIND/agents/$worker_id/events?tail=200")" >/dev/null; then
      echo "warning: worker $worker_id had no assistant events; likely prompt-echo completion"
      FAILED=true
    fi
    if [[ ! -f "$artifact_path" ]]; then
      echo "warning: deliverable missing for worker $worker_id at $artifact_path"
      FAILED=true
    elif [[ -z "$(cat "$artifact_path")" ]]; then
      echo "warning: deliverable empty for worker $worker_id at $artifact_path"
      FAILED=true
    else
      echo "worker $worker_id deliverable: $artifact_path"
      echo "-------"
      cat "$artifact_path"
      echo "-------"
      cp "$artifact_path" "$REPORT_DIR/$(basename "$artifact_path")"
    fi
  done
fi

curl -sS -X DELETE "http://$BIND/projects/$project_id" >/dev/null
echo "project deleted"
echo "run complete"

if [[ "$FAILED" == "true" ]]; then
  echo "result: failure (mixed contracts incomplete)"
  exit 1
fi

echo "result: success"
You are a codex-foreman mock worker for category: $category.
