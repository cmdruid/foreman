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

if [[ -z "${FOREMAN_BIN:-}" ]]; then
  echo "codex-foreman binary path is unset" >&2
  exit 1
fi
if [[ ! -x "$FOREMAN_BIN" && -x "/usr/local/bin/codex-foreman" ]]; then
  FOREMAN_BIN="/usr/local/bin/codex-foreman"
fi

if [[ -z "${FOREMAN_BIN:-}" ]]; then
  echo "codex-foreman binary not found; expected target/debug/codex-foreman or /usr/local/bin/codex-foreman" >&2
  exit 1
fi

if [[ ! -x "$FOREMAN_BIN" ]]; then
  echo "codex-foreman binary not executable at: $FOREMAN_BIN" >&2
  echo "Set FOREMAN_BIN to a valid executable codex-foreman." >&2
  exit 1
fi

if [[ -z "${PORT:-}" ]]; then
  PORT="8899"
fi
if [[ -z "${PROJECT_PATH:-}" ]]; then
  PROJECT_PATH="$REPO_ROOT/contrib/mock"
fi

BIND="${BIND:-127.0.0.1:$PORT}"
FOREMAN_STATE_PATH="${FOREMAN_STATE_PATH:-/tmp/cf-mock-state.json}"
SERVICE_CONFIG_PATH="${SERVICE_CONFIG_PATH:-/tmp/cf-mock-service.toml}"
FOREMAN_LOG_PATH="${FOREMAN_LOG_PATH:-/tmp/cf-mock-foreman.log}"
JOB_TIMEOUT_MS="${JOB_TIMEOUT_MS:-180000}"
JOB_POLL_MS="${JOB_POLL_MS:-250}"
WORKTREE_BASE="${WORKTREE_BASE:-/tmp/cf-mock/worktrees}"
REPORT_DIR="${REPORT_DIR:-/tmp/cf-mock/reports}"
WORKER_SANDBOX="${WORKER_SANDBOX:-workspace-write}"
FOREMAN_MANAGED_WORKTREES="${FOREMAN_MANAGED_WORKTREES:-true}"
BASE_BRANCH="$(git -C "$PROJECT_PATH" rev-parse --abbrev-ref HEAD 2>/dev/null || echo HEAD)"

prepare_worktree() {
  local worktree_path="$1"

  if [[ -e "$worktree_path" ]]; then
    rm -rf "$worktree_path"
  fi

  if ! git -C "$PROJECT_PATH" worktree add --detach "$worktree_path" "$BASE_BRANCH"; then
    return 1
  fi

  mkdir -p "$worktree_path/.audit-generated"
}

if [[ ! -x "$CODEX_BIN" ]]; then
  echo "codex binary not executable at: $CODEX_BIN" >&2
  echo "Set CODEX_BIN to a valid executable codex binary path." >&2
  exit 1
fi

if [[ ! -f "$PROJECT_PATH/project.toml" ]]; then
  echo "missing project file: $PROJECT_PATH/project.toml" >&2
  exit 1
fi

mkdir -p "$WORKTREE_BASE" "$REPORT_DIR"

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

extract_label_value() {
  local labels_json="$1"
  local key="$2"
  local index="$3"
  jq -r --arg key "$key" --argjson index "$index" '
    (.[$key] // empty)
    | if type == "array" then .[$index] // empty else . end
  ' <<<"$labels_json"
}

extract_prompt_field() {
  local body="$1"
  local field="$2"
  case "$field" in
    category)
      grep -m1 '^You are a codex-foreman mock worker for category: ' <<<"$body" \
        | sed 's/^You are a codex-foreman mock worker for category: //'
      ;;
    worktree_path)
      grep -m1 'Use this exact worktree path for all filesystem operations:' <<<"$body" \
        | sed 's/.*operations: //'
      ;;
  esac
}

cat >"$SERVICE_CONFIG_PATH" <<EOF_CFG
[app_server]
initialize_timeout_ms = 10000
request_timeout_ms = 60000
EOF_CFG

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
declare -a WORKER_CATEGORIES=()

for category in security robustness code-quality; do
  safe_category="$(safe_slug "$category")"
  worker_worktree_path="$WORKTREE_BASE/${safe_category}-$(date +%s%3N)-$RANDOM"
  worker_worktree_branch="foreman-${safe_category}-$(date +%s%3N)-$RANDOM"
  task_file="$PROJECT_PATH/tasks/$category.md"
  deliverable_path=".audit-generated/${safe_category}-worker-deliverable.md"
  worker_cwd="$worker_worktree_path"
  if [[ "$FOREMAN_MANAGED_WORKTREES" == "true" ]]; then
    mkdir -p "$(dirname "$worker_worktree_path")"
  else
    if ! prepare_worktree "$worker_worktree_path"; then
      echo "warning: failed to prepare worktree for category $category at $worker_worktree_path" >&2
      FAILED=true
      WORKER_CATEGORIES+=("$category")
      continue
    fi
  fi
  prompt="$(cat <<EOF
You are a codex-foreman mock worker for category: $category.
Worktree instruction:
- Use this exact worktree path for all filesystem operations: $worker_worktree_path
- Execute all filesystem commands from this current working directory: $worker_worktree_path
- Read the assignment source: $task_file
- Create and write "$worker_worktree_path/$deliverable_path" with actual content.
- Execute this shell command to guarantee the deliverable exists:
  mkdir -p "$worker_worktree_path/.audit-generated"
  cat > "$worker_worktree_path/$deliverable_path" <<'EOF_DELIVERABLE'
# Remediation Deliverable: $category

- Source: $task_file
  - Summary: replace with concrete issue and fix details.
- Impacted files:
  - TODO: add modified files
- Verification:
  - TODO: add a concrete command that proves completion
EOF_DELIVERABLE
  - Include actionable fix tasks and explicit verification command.
- Return a concise completion summary.
EOF
)"
  if [[ "$FOREMAN_MANAGED_WORKTREES" == "true" ]]; then
    worker=$(jq -cn \
      --arg p "$prompt" \
      --arg category "$category" \
      --arg safe_category "$safe_category" \
      --arg worktree_path "$worker_worktree_path" \
      --arg worktree_branch "$worker_worktree_branch" \
      --arg cwd "$worker_cwd" \
      --arg sandbox "$WORKER_SANDBOX" \
      --arg base_ref "$BASE_BRANCH" \
      '{prompt:$p, cwd:$cwd, sandbox:$sandbox, worktree:{path:$worktree_path, create:true, base_ref:$base_ref}, labels:{"category":$category,"safe_category":$safe_category,"worktree_path":$worktree_path,"worktree_branch":$worktree_branch}, callback_overrides:{callback_events:["turn/completed","item/assistantMessage","item/assistantMessage/delta","item/agentMessage","item/agentMessage/delta","codex/event/exec_command_begin","codex/event/exec_command_end"]}}')
  else
    worker=$(jq -cn \
      --arg p "$prompt" \
      --arg category "$category" \
      --arg safe_category "$safe_category" \
      --arg worktree_path "$worker_worktree_path" \
      --arg worktree_branch "$worker_worktree_branch" \
      --arg cwd "$worker_cwd" \
      --arg sandbox "$WORKER_SANDBOX" \
      '{prompt:$p, cwd:$cwd, sandbox:$sandbox, labels:{"category":$category,"safe_category":$safe_category,"worktree_path":$worktree_path,"worktree_branch":$worktree_branch}, callback_overrides:{callback_events:["turn/completed","item/assistantMessage","item/assistantMessage/delta","item/agentMessage","item/agentMessage/delta","codex/event/exec_command_begin","codex/event/exec_command_end"]}}')
  fi
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
    if [[ "$category" == "null" || "$category" == "" ]]; then
      category="${WORKER_CATEGORIES[$i]:-worker-$((i+1))}"
    fi
    safe_category="$(safe_slug "$category")"
    report_body="$(jq -r '.final_text // .summary // ""' <<<"$worker")"
    if looks_like_prompt_payload "$report_body" && [[ -n "$worker_id" && "$worker_id" != "null" ]]; then
      report_body="$(jq -r '.final_text // .summary // ""' <<<"$(fetch_worker_result "$worker_id")")"
    fi
    if [[ "$category" == "null" || "$category" == "" ]]; then
      category="$(extract_prompt_field "$report_body" "category")"
    fi
    if [[ "$category" == "null" || "$category" == "" ]]; then
      category="worker-$((i+1))"
    fi
    safe_category="$(safe_slug "$category")"
    worker_worktree_path="$(extract_prompt_field "$report_body" "worktree_path")"
    if [[ "$worker_worktree_path" == "null" || "$worker_worktree_path" == "" ]]; then
      worker_worktree_path="${WORKTREE_BASE}/${safe_category}-fallback-$(date +%s%3N)-$RANDOM"
    fi
    deliverable_path="$worker_worktree_path/.audit-generated/${safe_category}-worker-deliverable.md"

    if [[ -z "$worker_id" || "$worker_id" == "null" ]]; then
      echo "warning: worker $i had missing agent_id"
      FAILED=true
    fi
    if [[ -z "$report_body" || "$report_body" == "null" ]]; then
      echo "warning: worker $worker_id returned empty completion text"
      FAILED=true
    fi
    if [[ -n "$worker_id" && "$worker_id" != "null" ]]; then
      agent_events=$(curl -sS "http://$BIND/agents/$worker_id/events?tail=200")
      if ! jq -e 'any(.[]; .method == "item/assistantMessage" or .method == "item/assistantMessage/delta" or .method == "item/agentMessage" or .method == "item/agentMessage/delta")' <<<"$agent_events" >/dev/null; then
        echo "warning: worker $worker_id had no assistant events; likely prompt-echo completion"
        FAILED=true
      fi
    fi
    if [[ ! -f "$deliverable_path" ]]; then
      echo "warning: deliverable missing for worker $worker_id at $deliverable_path"
      FAILED=true
    elif [[ -z "$(cat "$deliverable_path")" ]]; then
      echo "warning: deliverable empty for worker $worker_id at $deliverable_path"
      FAILED=true
    else
      echo "worker $worker_id deliverable: $deliverable_path"
      echo "-------"
      cat "$deliverable_path"
      echo "-------"
      worker_report_path="$REPORT_DIR/${safe_category}-worker-deliverable.md"
      cp "$deliverable_path" "$worker_report_path"
      echo "snapshot saved: $worker_report_path"
    fi
  done
fi

curl -sS -X DELETE "http://$BIND/projects/$project_id" >/dev/null
echo "project deleted"
echo "run complete"

if [[ "$FAILED" == "true" ]]; then
  echo "result: failure (deliverables incomplete)"
  exit 1
fi

echo "result: success"
