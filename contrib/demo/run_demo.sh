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
FOREMAN_BIN="${FOREMAN_BIN:-}"
FOREMAN_PID=""
WORKERS_FILE=""
FAILED=false
MODEL_ID="${MODEL_ID:-gpt-5.3-codex-spark}"
MODEL_PROVIDER="${MODEL_PROVIDER:-}"

WORKER_MONITORING_ENABLED="${WORKER_MONITORING_ENABLED:-false}"
WORKER_MONITORING_INACTIVITY_TIMEOUT_MS="${WORKER_MONITORING_INACTIVITY_TIMEOUT_MS:-3000}"
WORKER_MONITORING_MAX_RESTARTS="${WORKER_MONITORING_MAX_RESTARTS:-1}"
WORKER_MONITORING_WATCH_INTERVAL_MS="${WORKER_MONITORING_WATCH_INTERVAL_MS:-750}"

for command in jq curl git; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "required command '$command' is missing" >&2
    exit 1
  fi
done

if [[ -z "${FOREMAN_BIN:-}" ]]; then
  if [[ -x "$REPO_ROOT/target/debug/foreman" ]]; then
    FOREMAN_BIN="$REPO_ROOT/target/debug/foreman"
  elif [[ -x "/usr/local/bin/foreman" ]]; then
    FOREMAN_BIN="/usr/local/bin/foreman"
  fi
fi

if [[ -z "${FOREMAN_BIN:-}" ]]; then
  echo "foreman binary path is unset" >&2
  exit 1
fi
if [[ ! -x "$FOREMAN_BIN" && -x "/usr/local/bin/foreman" ]]; then
  FOREMAN_BIN="/usr/local/bin/foreman"
fi

if [[ -z "${FOREMAN_BIN:-}" ]]; then
  echo "foreman binary not found; expected target/debug/foreman or /usr/local/bin/foreman" >&2
  exit 1
fi

if [[ ! -x "$FOREMAN_BIN" ]]; then
  echo "foreman binary not executable at: $FOREMAN_BIN" >&2
  echo "Set FOREMAN_BIN to a valid executable foreman binary." >&2
  exit 1
fi

if [[ -z "${PROJECT_PATH:-}" ]]; then
  PROJECT_PATH="$REPO_ROOT/contrib/demo"
fi

FOREMAN_RUNTIME_DIR="${FOREMAN_RUNTIME_DIR:-$PROJECT_PATH/.foreman}"
mkdir -p "$FOREMAN_RUNTIME_DIR"

FOREMAN_SOCKET_PATH="${FOREMAN_SOCKET_PATH:-$FOREMAN_RUNTIME_DIR/cf-foreman-$$.sock}"
FOREMAN_STATE_PATH="${FOREMAN_STATE_PATH:-$FOREMAN_RUNTIME_DIR/cf-state.json}"
SERVICE_CONFIG_PATH="${SERVICE_CONFIG_PATH:-$FOREMAN_RUNTIME_DIR/cf-service.toml}"
FOREMAN_LOG_PATH="${FOREMAN_LOG_PATH:-$FOREMAN_RUNTIME_DIR/cf-foreman.log}"
JOB_TIMEOUT_MS="${JOB_TIMEOUT_MS:-180000}"
JOB_POLL_MS="${JOB_POLL_MS:-250}"
WORKTREE_BASE="${WORKTREE_BASE:-$PROJECT_PATH/.audit-worktrees}"
REPORT_DIR="${REPORT_DIR:-$PROJECT_PATH/.audit/reports}"
WORKER_SANDBOX="${WORKER_SANDBOX:-workspace-write}"
STRICT_EXECUTION_CHECKS="${STRICT_EXECUTION_CHECKS:-false}"
RUN_MOCK_DEMO_MODE="${RUN_MOCK_DEMO_MODE:-worktree}"
WORKTREE_CLEANUP="${WORKTREE_CLEANUP:-true}"
FOREMAN_MANAGED_WORKTREES="${FOREMAN_MANAGED_WORKTREES:-true}"
BASE_BRANCH="$(git -C "$PROJECT_PATH" rev-parse --abbrev-ref HEAD 2>/dev/null || echo HEAD)"
# Use a detached-safe ref for foreman-managed worktree creation.
BASE_REF="$(git -C "$PROJECT_PATH" rev-parse HEAD 2>/dev/null || echo HEAD)"

prepare_worktree() {
  local worktree_path="$1"

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
  curl --unix-socket "$FOREMAN_SOCKET_PATH" -sS "http://localhost/agents/$worker_id/result"
}

looks_like_prompt_payload() {
  local text="$1"
  [[ -z "$text" ]] && return 0
  [[ "$text" == "# Project Worker"* ]] && return 0
  [[ "$text" == "You are a foreman worker."* ]] && return 0
  return 1
}

wait_for_job_completion() {
  local socket_path="$1"
  local job_id="$2"
  local timeout_ms="$3"
  local poll_ms="$4"
  local start_ms
  local elapsed_ms
  local remaining_ms
  local response
  local status
  local timed_out
  local running_workers
  local completed_workers
  local job_snapshot
  local response_json

  start_ms=$(date +%s%3N)

  while true; do
    elapsed_ms=$(( $(date +%s%3N) - start_ms ))
    remaining_ms=$(( timeout_ms - elapsed_ms ))
    if (( remaining_ms <= 0 )); then
      echo "job timed out while waiting" >&2
      return 1
    fi

    response=$(curl --unix-socket "$socket_path" -sS "http://localhost/jobs/$job_id/wait?timeout_ms=$poll_ms&poll_ms=$poll_ms&include_workers=true") || {
      echo "job wait request failed for $job_id" >&2
      return 1
    }

    response_json=$(printf '%s' "$response" | jq -ce '.' 2>/dev/null | sed -n '1p')
    if [[ -z "$response_json" ]]; then
      echo "non-json wait response for job $job_id: $response" >&2
      sleep 0
      continue
    else
      :
    fi

    status=$(jq -r '.status // "unknown"' <<<"$response_json")
    case "$status" in
      completed|partial|failed)
        echo "$response" > /tmp/cf-job-${job_id}-result.json
        echo "$response"
        return 0
        ;;
      *)
        ;;
    esac

    timed_out=$(jq -r '.timed_out // false' <<<"$response_json")
    if [[ "$timed_out" == "true" ]]; then
      # keep polling, but provide a compact status summary each timeout slice
      running_workers=$(jq -r '.running_workers // 0' <<<"$response_json")
      completed_workers=$(jq -r '.completed_workers // 0' <<<"$response_json")
      echo "status=$status complete=$completed_workers running=$running_workers timed_out=true remaining_ms=$remaining_ms" >&2
    fi

    # Poll progress details without requiring completion.
    job_snapshot="$(curl --unix-socket "$socket_path" -sS "http://localhost/jobs/$job_id" || true)"
    if [[ -n "$job_snapshot" ]]; then
      snapshot_json=$(printf '%s' "$job_snapshot" | jq -ce '{status,id: .id,completed_workers,worker_count,running_workers,failed_workers}' 2>/dev/null | sed -n '1p')
      if [[ -n "$snapshot_json" ]]; then
        echo "snapshot: $snapshot_json" >&2
      else
        echo "snapshot: <non-json response from /jobs/$job_id> $job_snapshot" >&2
      fi
    fi

  done
}

select_worker_label() {
  local worker_json="$1"
  local label_path="$2"

  jq -r "
    .labels[\"$label_path\"] // \"\" |
    if type == \"array\" then
      .[0]
    else
      .
    end
  " <<<"$worker_json"
}

fetch_worker_events() {
  local worker_id="$1"
  local tail_count="${2:-200}"
  curl --unix-socket "$FOREMAN_SOCKET_PATH" -sS "http://localhost/agents/$worker_id/events?tail=$tail_count"
}

cat >"$SERVICE_CONFIG_PATH" <<EOF_CFG
[app_server]
initialize_timeout_ms = 10000
request_timeout_ms = 60000

[worker_monitoring]
enabled = $WORKER_MONITORING_ENABLED
inactivity_timeout_ms = $WORKER_MONITORING_INACTIVITY_TIMEOUT_MS
max_restarts = $WORKER_MONITORING_MAX_RESTARTS
watch_interval_ms = $WORKER_MONITORING_WATCH_INTERVAL_MS
EOF_CFG

echo "using foreman : $FOREMAN_BIN"
echo "using codex  : $CODEX_BIN"
echo "running as   : $CURRENT_USER"
echo "socket path  : $FOREMAN_SOCKET_PATH"
echo "project path : $PROJECT_PATH"

cleanup() {
  set +e
  if [[ "$WORKTREE_CLEANUP" == "true" ]]; then
    for path in "${WORKTREE_PATHS[@]:-}"; do
      if [[ -n "$path" && -d "$path" ]]; then
        git -C "$PROJECT_PATH" worktree remove --force "$path" >/dev/null 2>&1 || true
      fi
    done
  fi
  if [[ -n "${FOREMAN_PID:-}" ]] && kill -0 "$FOREMAN_PID" 2>/dev/null; then
    echo "stopping foreman (pid $FOREMAN_PID)"
    kill "$FOREMAN_PID"
  fi
  if [[ -S "$FOREMAN_SOCKET_PATH" ]]; then
    rm -f "$FOREMAN_SOCKET_PATH"
  fi
  if [[ -n "${WORKERS_FILE:-}" && -f "$WORKERS_FILE" ]]; then
    rm -f "$WORKERS_FILE"
  fi
}
trap cleanup EXIT INT TERM

echo "starting foreman..."
"$FOREMAN_BIN" \
  --socket-path "$FOREMAN_SOCKET_PATH" \
  --codex-binary "$CODEX_BIN" \
  --service-config "$SERVICE_CONFIG_PATH" \
  --state-path "$FOREMAN_STATE_PATH" \
  --project "$PROJECT_PATH/project.toml" \
  >"$FOREMAN_LOG_PATH" 2>&1 &
FOREMAN_PID=$!

for i in $(seq 1 60); do
  if curl --unix-socket "$FOREMAN_SOCKET_PATH" -sS "http://localhost/health" >/dev/null 2>&1; then
    break
  fi
  sleep 0.5
done

if ! curl --unix-socket "$FOREMAN_SOCKET_PATH" -sS "http://localhost/health" >/dev/null 2>&1; then
  echo "foreman healthcheck failed; inspect $FOREMAN_LOG_PATH" >&2
  exit 1
fi
echo "foreman ready"

create_project_payload=$(jq -cn \
  --arg p "$PROJECT_PATH" \
  --arg model "$MODEL_ID" \
  --arg provider "$MODEL_PROVIDER" \
  '{
    path: $p,
    model: $model,
    model_provider: (if $provider != "" then $provider else null end)
  }')
project_response=$(curl --unix-socket "$FOREMAN_SOCKET_PATH" -sS -X POST "http://localhost/projects" \
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
declare -a WORKTREE_PATHS=()
declare -a WORKER_ARTIFACT_PATHS=()
declare -a WORKER_WORKTREES=()
declare -a WORKER_REQUIRES_WORKTREE=()
declare -a WORKER_REQUIRES_COMMAND=()
declare -A WORKER_CATEGORY_BY_ID=()
declare -A WORKER_ARTIFACT_BY_ID=()
declare -A WORKER_WORKTREE_BY_ID=()
declare -A WORKER_REQUIRES_WORKTREE_BY_ID=()
declare -A WORKER_REQUIRES_COMMAND_BY_ID=()

if [[ "$RUN_MOCK_DEMO_MODE" == "mixed" || "$RUN_MOCK_DEMO_MODE" == "mixed-no-worktree" ]]; then
  WORKER_CATEGORIES_TO_RUN=(security robustness code-quality notes)
else
  WORKER_CATEGORIES_TO_RUN=(security robustness code-quality)
fi

for category in "${WORKER_CATEGORIES_TO_RUN[@]}"; do
  safe_category="$(safe_slug "$category")"
  requires_worktree="false"
  requires_command="true"
  create_worktree="true"

  if [[ "$category" == "notes" ]]; then
    task_file="$PROJECT_PATH/README.md"
    deliverable_path="$PROJECT_PATH/.audit-generated/${safe_category}-worker-deliverable.md"
    worker_cwd="$PROJECT_PATH"
  else
    requires_worktree="true"
    worker_worktree_path="$WORKTREE_BASE/${safe_category}-$(date +%s%3N)-$RANDOM"
    deliverable_path="$worker_worktree_path/.audit-generated/${safe_category}-worker-deliverable.md"
    worker_cwd="$worker_worktree_path"
    WORKTREE_PATHS+=("$worker_worktree_path")

    if [[ "$FOREMAN_MANAGED_WORKTREES" == "true" ]]; then
      mkdir -p "$(dirname "$worker_worktree_path")"
    else
      if ! prepare_worktree "$worker_worktree_path"; then
        echo "warning: failed to prepare worktree for category $category at $worker_worktree_path" >&2
        FAILED=true
        continue
      fi
      create_worktree="false"
    fi
    task_file="$PROJECT_PATH/tasks/$category.md"
  fi

  if [[ "$requires_worktree" == "true" ]]; then
    prompt="$(cat <<EOF
You are a foreman mock worker for category: $category.
Worktree instruction:
- Use this exact worktree path for all filesystem operations: $worker_worktree_path
- Execute all filesystem commands from this current working directory: $worker_worktree_path
- Read the assignment source: $task_file
- Execute this shell command block to guarantee the deliverable exists:
  cd "$worker_worktree_path"
  mkdir -p ".audit-generated"
  cat > "$deliverable_path" <<'EOF_DELIVERABLE'
# Mock Deliverable: $category

- Source: $task_file
- Summary: concrete actions taken for this work tree.
- Impacted files:
  - .audit-generated/${safe_category}-worker-deliverable.md
  - (and any other files modified for the task)
- Verification:
  - `git status --short`
EOF_DELIVERABLE
- Run this verification command before returning:
  if ! test -f "$deliverable_path"; then
    echo "WORKER_DELIVERY_MISSING:$deliverable_path"
    exit 1
  fi
  ls -l "$deliverable_path"
  echo "WORKER_DELIVERY_OK:$deliverable_path"
- Keep modifications scoped to that worktree.
- Include a concise completion summary in your final response.
EOF
)"
  else
    prompt="$(cat <<EOF
You are a foreman mock worker for category: $category.
Work without a dedicated worktree and keep all edits in the shared project.
Read the assignment source: $task_file
- IMPORTANT: execute the shell command block exactly as written and return only after it succeeds.
- Execute this shell command block to guarantee the deliverable exists:
  cd "$PROJECT_PATH"
  mkdir -p ".audit-generated"
  cat > "$deliverable_path" <<'EOF_DELIVERABLE'
# Mock Deliverable: $category

- Source: $task_file
- Notes: complete the assignment and log findings in this report.
EOF_DELIVERABLE
  ls -l "$deliverable_path"
  echo "WORKER_DELIVERY_OK:$deliverable_path"
- Include a concise completion summary.
EOF
)"
  fi

  if [[ "$requires_worktree" == "true" ]]; then
    worker=$(jq -cn \
      --arg p "$prompt" \
      --arg category "$category" \
      --arg safe_category "$safe_category" \
      --arg worktree_path "$worker_worktree_path" \
      --arg cwd "$worker_cwd" \
      --arg sandbox "$WORKER_SANDBOX" \
      --arg base_ref "$BASE_REF" \
      --arg model "$MODEL_ID" \
      --arg provider "$MODEL_PROVIDER" \
      --arg artifact_path "$deliverable_path" \
      --arg requires_worktree "$requires_worktree" \
      --arg requires_command "$requires_command" \
      --arg create_worktree "$create_worktree" \
      '{prompt:$p, model:$model, model_provider:(if $provider != "" then $provider else null end), cwd:$cwd, sandbox:$sandbox, worktree:{path:$worktree_path, create:($create_worktree == "true"), base_ref:$base_ref}, labels:{"category":$category,"safe_category":$safe_category,"worktree_path":$worktree_path,"artifact_path":$artifact_path,"requires_worktree":$requires_worktree,"requires_command":$requires_command}, callback_overrides:{callback_events:["*"]}}')
  else
    worker=$(jq -cn \
      --arg p "$prompt" \
      --arg category "$category" \
      --arg safe_category "$safe_category" \
      --arg cwd "$worker_cwd" \
      --arg sandbox "$WORKER_SANDBOX" \
      --arg model "$MODEL_ID" \
      --arg provider "$MODEL_PROVIDER" \
      --arg artifact_path "$deliverable_path" \
      --arg requires_worktree "$requires_worktree" \
      --arg requires_command "$requires_command" \
      '{prompt:$p, model:$model, model_provider:(if $provider != "" then $provider else null end), cwd:$cwd, sandbox:$sandbox, labels:{"category":$category,"safe_category":$safe_category,"artifact_path":$artifact_path,"requires_worktree":$requires_worktree,"requires_command":$requires_command}, callback_overrides:{callback_events:["*"]}}')
  fi

  workers_file_tmp="$(mktemp)"
  jq -c --argjson w "$worker" '. += [$w]' "$WORKERS_FILE" > "$workers_file_tmp"
  mv "$workers_file_tmp" "$WORKERS_FILE"
  WORKER_CATEGORIES+=("$category")
  WORKER_ARTIFACT_PATHS+=("$deliverable_path")
  WORKER_WORKTREES+=("${worker_worktree_path:-$PROJECT_PATH}")
  WORKER_REQUIRES_WORKTREE+=("$requires_worktree")
  WORKER_REQUIRES_COMMAND+=("$requires_command")
done

jobs_payload=$(jq -cn --argjson workers "$(cat "$WORKERS_FILE")" '{workers:$workers}')
job_response=$(curl --unix-socket "$FOREMAN_SOCKET_PATH" -sS -X POST "http://localhost/projects/$project_id/jobs" \
  -H 'Content-Type: application/json' \
  -d "$jobs_payload")
job_id=$(jq -r '.job_id // empty' <<<"$job_response")
if [[ -z "$job_id" || "$job_id" == "null" ]]; then
  echo "job creation failed: $job_response" >&2
  exit 1
fi
job_count=$(jq '.workers | length' <<<"$jobs_payload")
mapfile -t JOB_WORKER_IDS < <(jq -r '.worker_ids // [] | .[]' <<<"$job_response")
if (( ${#JOB_WORKER_IDS[@]} != ${#WORKER_CATEGORIES[@]} )); then
  echo "warning: expected ${#WORKER_CATEGORIES[@]} worker ids from API, got ${#JOB_WORKER_IDS[@]}"
fi
for idx in "${!JOB_WORKER_IDS[@]}"; do
  worker_id="${JOB_WORKER_IDS[$idx]}"
  WORKER_CATEGORY_BY_ID["$worker_id"]="${WORKER_CATEGORIES[$idx]:-worker-$((idx+1))}"
  WORKER_ARTIFACT_BY_ID["$worker_id"]="${WORKER_ARTIFACT_PATHS[$idx]:-}"
  WORKER_WORKTREE_BY_ID["$worker_id"]="${WORKER_WORKTREES[$idx]:-}"
  WORKER_REQUIRES_WORKTREE_BY_ID["$worker_id"]="${WORKER_REQUIRES_WORKTREE[$idx]:-false}"
  WORKER_REQUIRES_COMMAND_BY_ID["$worker_id"]="${WORKER_REQUIRES_COMMAND[$idx]:-false}"
done
echo "job started: $job_id (workers: $job_count)"

echo "waiting for jobs..."
if ! job_wait_response="$(wait_for_job_completion "$FOREMAN_SOCKET_PATH" "$job_id" "$JOB_TIMEOUT_MS" "$JOB_POLL_MS")"; then
  FAILED=true
  job_wait_response="$(curl --unix-socket "$FOREMAN_SOCKET_PATH" -sS "http://localhost/jobs/$job_id" || jq -cn '{status:"unknown",timed_out:true,workers:[]}')"
fi
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
    if [[ -n "$worker_id" && "$worker_id" != "null" ]]; then
      category="${WORKER_CATEGORY_BY_ID[$worker_id]:-}"
      worktree_path="${WORKER_WORKTREE_BY_ID[$worker_id]:-}"
      deliverable_path="${WORKER_ARTIFACT_BY_ID[$worker_id]:-}"
      requires_worktree="${WORKER_REQUIRES_WORKTREE_BY_ID[$worker_id]:-}"
      requires_command="${WORKER_REQUIRES_COMMAND_BY_ID[$worker_id]:-}"
    else
      category=""
      worktree_path=""
      deliverable_path=""
      requires_worktree=""
      requires_command=""
    fi

    if [[ -z "$category" || "$category" == "null" ]]; then
      category="$(select_worker_label "$worker" "category")"
    fi
    if [[ "$category" == "null" || "$category" == "" ]]; then
      category="${WORKER_CATEGORIES[$i]:-worker-$((i+1))}"
    fi
    safe_category="$(safe_slug "$category")"

    if [[ -z "$worktree_path" || "$worktree_path" == "null" ]]; then
      worktree_path="$(select_worker_label "$worker" "worktree_path")"
      if [[ "$worktree_path" == "null" || "$worktree_path" == "" ]]; then
        worktree_path="${WORKER_WORKTREES[$i]:-}"
      fi
    fi

    if [[ -z "$deliverable_path" || "$deliverable_path" == "null" ]]; then
      deliverable_path="$(select_worker_label "$worker" "artifact_path")"
      if [[ -z "$deliverable_path" || "$deliverable_path" == "null" ]]; then
        deliverable_path="${WORKER_ARTIFACT_PATHS[$i]:-}"
      fi
    fi

    if [[ -z "$requires_worktree" || "$requires_worktree" == "null" ]]; then
      requires_worktree="$(select_worker_label "$worker" "requires_worktree")"
      if [[ "$requires_worktree" == "null" || "$requires_worktree" == "" ]]; then
        requires_worktree="${WORKER_REQUIRES_WORKTREE[$i]:-false}"
      fi
    fi

    if [[ -z "$requires_command" || "$requires_command" == "null" ]]; then
      requires_command="$(select_worker_label "$worker" "requires_command")"
      if [[ "$requires_command" == "null" || "$requires_command" == "" ]]; then
        requires_command="${WORKER_REQUIRES_COMMAND[$i]:-false}"
      fi
    fi

    report_body="$(jq -r '.final_text // .summary // ""' <<<"$worker")"
    if looks_like_prompt_payload "$report_body" && [[ -n "$worker_id" && "$worker_id" != "null" ]]; then
      report_body="$(jq -r '.final_text // .summary // ""' <<<"$(fetch_worker_result "$worker_id")")"
    elif [[ -n "$worker_id" && "$worker_id" != "null" && ( -z "$report_body" || "$report_body" == "null" ) ]]; then
      report_body="$(jq -r '.final_text // .summary // ""' <<<"$(fetch_worker_result "$worker_id")")"
    fi

    if [[ -z "$deliverable_path" || "$deliverable_path" == "null" ]]; then
      if [[ "$requires_worktree" == "true" ]]; then
        deliverable_path="$worktree_path/.audit-generated/${safe_category}-worker-deliverable.md"
      else
        deliverable_path="$PROJECT_PATH/.audit-generated/${safe_category}-worker-deliverable.md"
      fi
    fi

    if [[ "$requires_worktree" == "true" ]]; then
      if [[ -z "$worktree_path" || "$worktree_path" == "null" ]]; then
        worktree_path="${WORKTREE_BASE}/${safe_category}-candidate-$(date +%s%3N)-$RANDOM"
      fi
      worktree_probe_path="$worktree_path"
    else
      worktree_probe_path="$PROJECT_PATH"
    fi

    if [[ -z "$worker_id" || "$worker_id" == "null" ]]; then
      echo "warning: worker $i had missing agent_id"
      FAILED=true
    fi
    if [[ -z "$report_body" || "$report_body" == "null" ]]; then
      echo "warning: worker $worker_id returned empty completion text"
      if [[ "$STRICT_EXECUTION_CHECKS" == "true" ]]; then
        FAILED=true
      fi
    fi
    if [[ -n "$worker_id" && "$worker_id" != "null" ]]; then
      agent_events="$(fetch_worker_events "$worker_id" 200)"
      if ! jq -e 'any(.[]; .method == "item/assistantMessage" or .method == "item/assistantMessage/delta" or .method == "item/agentMessage" or .method == "item/agentMessage/delta" or .method == "agent_message" or .method == "agent_message_delta")' <<<"$agent_events" >/dev/null; then
        echo "warning: worker $worker_id had no assistant events; likely prompt-echo completion"
        if [[ "$STRICT_EXECUTION_CHECKS" == "true" ]]; then
          FAILED=true
        fi
      fi
      if [[ "$requires_command" == "true" ]] && ! jq -e 'any(.[]; .method == "exec_command_begin" or .method == "exec_command_end" or .method == "codex/event/exec_command_begin" or .method == "codex/event/exec_command_end")' <<<"$agent_events" >/dev/null; then
        echo "warning: worker $worker_id did not emit command events for a requires_command=true work item"
        if [[ "$STRICT_EXECUTION_CHECKS" == "true" ]]; then
          FAILED=true
        fi
      fi
    fi

    if [[ "$requires_worktree" == "true" ]]; then
      if [[ ! -d "$worktree_probe_path" ]]; then
        echo "warning: worktree missing for worker $worker_id at $worktree_probe_path"
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

curl --unix-socket "$FOREMAN_SOCKET_PATH" -sS -X DELETE "http://localhost/projects/$project_id" >/dev/null
echo "project deleted"
echo "run complete"

if [[ "$FAILED" == "true" ]]; then
  echo "result: failure (deliverables incomplete)"
  exit 1
fi

echo "result: success"
