pub const APP_NAME: &str = "foreman";
pub const APP_DESCRIPTION: &str = "foreman";

pub const DEFAULT_CODEX_BINARY: &str = "codex";
pub const DEFAULT_SERVICE_CONFIG_PATH: &str = "/etc/foreman/config.toml";
pub const DEFAULT_LOG_FILTER: &str = "foreman=debug";

pub const FOREMAN_RUNTIME_DIR: &str = ".foreman";
pub const FOREMAN_STATUS_READY: &str = "ready";
pub const FOREMAN_SOCKET_FILENAME: &str = "foreman.sock";
pub const DEFAULT_STATE_FILENAME: &str = "foreman-state.json";
pub const FOREMAN_SOCKET_PERMISSIONS: u32 = 0o600;
pub const EVENT_BUFFER_MAX: usize = 50;
pub const BROADCAST_CHANNEL_CAPACITY: usize = 256;
pub const STATUS_OK: &str = "ok";
pub const DEFAULT_WORKING_DIRECTORY: &str = ".";
pub const HEALTH_ROUTE: &str = "/health";
pub const STATUS_ROUTE: &str = "/status";
pub const ROUTE_AGENTS: &str = "/agents";
pub const ROUTE_AGENT_ID: &str = "/agents/{id}";
pub const ROUTE_AGENT_RESULT: &str = "/agents/{id}/result";
pub const ROUTE_AGENT_WAIT: &str = "/agents/{id}/wait";
pub const ROUTE_AGENT_EVENTS: &str = "/agents/{id}/events";
pub const ROUTE_AGENT_SEND: &str = "/agents/{id}/send";
pub const ROUTE_AGENT_STEER: &str = "/agents/{id}/steer";
pub const ROUTE_AGENT_INTERRUPT: &str = "/agents/{id}/interrupt";
pub const ROUTE_PROJECTS: &str = "/projects";
pub const ROUTE_PROJECT_ID: &str = "/projects/{id}";
pub const ROUTE_PROJECT_WORKERS: &str = "/projects/{id}/workers";
pub const ROUTE_PROJECT_FOREMAN_SEND: &str = "/projects/{id}/foreman/send";
pub const ROUTE_PROJECT_FOREMAN_STEER: &str = "/projects/{id}/foreman/steer";
pub const ROUTE_PROJECT_COMPACT: &str = "/projects/{id}/compact";
pub const ROUTE_PROJECT_CALLBACK_STATUS: &str = "/projects/{id}/callback-status";
pub const ROUTE_PROJECT_JOBS: &str = "/projects/{id}/jobs";
pub const ROUTE_JOBS: &str = "/jobs";
pub const ROUTE_JOB_ID: &str = "/jobs/{id}";
pub const ROUTE_JOB_RESULT: &str = "/jobs/{id}/result";
pub const ROUTE_JOB_WAIT: &str = "/jobs/{id}/wait";

pub const TEMPLATE_DIR_ENV: &str = "CODEX_FOREMAN_TEMPLATE_DIR";
pub const TEMPLATE_DIR_SHARE: &str = "/usr/share/foreman/templates";
pub const TEMPLATE_DIR_ETC: &str = "/etc/foreman/templates";

pub const PROJECT_CONFIG_FILE: &str = "project.toml";
pub const PROJECT_STATUS_READY: &str = "running";
pub const DEFAULT_FOREMAN_PROMPT_FILE: &str = "FOREMAN.md";
pub const DEFAULT_WORKER_PROMPT_FILE: &str = "WORKER.md";
pub const DEFAULT_RUNBOOK_FILE: &str = "RUNBOOK.md";
pub const DEFAULT_HANDOFF_FILE: &str = "HANDOFF.md";
pub const DEFAULT_MANUAL_FILE: &str = "MANUAL.md";
pub const DEFAULT_BUBBLE_UP_EVENTS: [&str; 2] = ["turn/completed", "turn/aborted"];
pub const DEFAULT_PROJECT_NAME: &str = "project";

pub const DEFAULT_APP_SERVER_INITIALIZE_TIMEOUT_MS: u64 = 5_000;
pub const DEFAULT_APP_SERVER_REQUEST_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_WORKER_MONITORING_ENABLED: bool = false;
pub const DEFAULT_WORKER_MONITORING_INACTIVITY_TIMEOUT_MS: u64 = 3_000;
pub const DEFAULT_WORKER_MONITORING_MAX_RESTARTS: u32 = 1;
pub const DEFAULT_WORKER_MONITORING_WATCH_INTERVAL_MS: u64 = 750;

pub const DEFAULT_WORKER_CALLBACK_TIMEOUT_MS: u64 = 5_000;
pub const MIN_WORKER_MONITOR_INTERVAL_MS: u64 = 50;
pub const DEFAULT_WAIT_POLL_MS: u64 = 250;
pub const DEFAULT_RESTART_MONITORING_DELAY_MS: u64 = 50;
pub const MILLISECONDS_PER_SECOND: u64 = 1000;

pub const CODEX_EVENT_METHOD_PREFIX: &str = "codex/event/";

pub const EVENT_METHOD_TURN_COMPLETED: &str = "turn/completed";
pub const EVENT_METHOD_TURN_ABORTED: &str = "turn/aborted";
pub const EVENT_METHOD_TURN_STARTED: &str = "turn/started";
pub const THREAD_EVENT_STARTED: &str = "thread/status/changed";
pub const EVENT_METHOD_ITEM_AGENT_MESSAGE: &str = "item/agentMessage";
pub const EVENT_METHOD_ITEM_AGENT_MESSAGE_DELTA: &str = "item/agentMessage/delta";
pub const EVENT_METHOD_ITEM_COMPLETED: &str = "item/completed";
pub const EVENT_STATUS_COMPLETED: &str = "completed";
pub const EVENT_STATUS_ABORTED: &str = "aborted";
pub const EVENT_STATUS_INTERRUPTED: &str = "interrupted";
pub const EVENT_STATUS_FAILED: &str = "failed";
pub const EVENT_STATUS_ERROR: &str = "error";
pub const EVENT_STATUS_STOPPED: &str = "stopped";
pub const EVENT_STATUS_TIMEOUT: &str = "timeout";
pub const EVENT_STATUS_TIMED_OUT: &str = "timed_out";

pub const AGENT_STATUS_IDLE: &str = "idle";
pub const AGENT_STATUS_RUNNING: &str = "running";
pub const AGENT_STATUS_RESTARTING: &str = "restarting";
pub const AGENT_STATUS_FAILED: &str = "failed";
pub const AGENT_STATUS_ABORTED: &str = "aborted";
pub const AGENT_STATUS_INTERRUPTED: &str = "interrupted";
pub const AGENT_STATUS_COMPLETED: &str = "completed";
pub const AGENT_STATUS_ORPHANED: &str = "orphaned";
pub const AGENT_STATUS_UNKNOWN: &str = "unknown";

pub const JOB_STATUS_EMPTY: &str = "empty";
pub const JOB_STATUS_RUNNING: &str = "running";
pub const JOB_STATUS_PARTIAL: &str = "partial";
pub const JOB_STATUS_QUEUED: &str = "queued";
pub const JOB_STATUS_COMPLETED: &str = "completed";
pub const JOB_STATUS_FAILED: &str = "failed";
pub const JOB_STATUS_UNKNOWN: &str = "unknown";

pub const AGENT_ROLE_FOREMAN: &str = "foreman";
pub const AGENT_ROLE_WORKER: &str = "worker";
pub const AGENT_ROLE_STANDALONE: &str = "standalone";

pub const CALLBACK_SECRET_HEADER: &str = "x-foreman-secret";
pub const CALLBACK_EVENT_JSON_KEY: &str = "event_json";
pub const CALLBACK_EVENT_PRETTY_KEY: &str = "event_pretty";
pub const CALLBACK_EVENT_PAYLOAD_KEY: &str = "event_payload";
pub const CALLBACK_EVENT_PROMPT_KEY: &str = "event_prompt";
pub const CALLBACK_EVENT_TYPE_KEY: &str = "callback_type";
pub const CALLBACK_EVENT_TYPE_AGENT_EVENT: &str = "agent-event";
pub const CALLBACK_VAR_AGENT_ID: &str = "agent_id";
pub const CALLBACK_VAR_WORKER_ID: &str = "worker_id";
pub const CALLBACK_VAR_THREAD_ID: &str = "thread_id";
pub const CALLBACK_VAR_METHOD: &str = "method";
pub const CALLBACK_VAR_EVENT_ID: &str = "event_id";
pub const CALLBACK_VAR_ROLE: &str = "agent_role";
pub const CALLBACK_VAR_TURN_ID: &str = "turn_id";
pub const CALLBACK_VAR_TS: &str = "ts";
pub const CALLBACK_VAR_PROJECT_ID: &str = "project_id";
pub const CALLBACK_VAR_FOREMAN_ID: &str = "foreman_id";
pub const CALLBACK_VAR_RESULT: &str = "result";
pub const CALLBACK_VAR_CALLBACK_VARS: &str = "callback_vars";
pub const CALLBACK_VAR_EVENT_PAYLOAD: &str = "event_payload";
pub const CALLBACK_VAR_WORKSPACE_PROMPT: &str = "event_prompt";
pub const CALLBACK_VAR_REASON: &str = "reason";
pub const CALLBACK_PROMPT_DEFAULT: &str = "event_prompt";
pub const PROJECT_VAR_ID: &str = "project_id";
pub const PROJECT_VAR_PATH: &str = "project_path";
pub const PROJECT_VAR_NAME: &str = "project_name";
pub const PROJECT_VAR_STATUS: &str = "project_status";
pub const PROJECT_VAR_FOREMAN_ID: &str = "project_foreman_id";
pub const PROJECT_LIFECYCLE_START: &str = "start";
pub const PROJECT_LIFECYCLE_COMPACT: &str = "compact";
pub const PROJECT_LIFECYCLE_STOP: &str = "stop";
pub const PROJECT_LIFECYCLE_WORKER_COMPLETED: &str = "worker_completed";
pub const PROJECT_LIFECYCLE_WORKER_ABORTED: &str = "worker_aborted";
pub const PROJECT_LIFECYCLE_EVENT_START: &str = "project/lifecycle/start";
pub const PROJECT_LIFECYCLE_EVENT_COMPACT: &str = "project/lifecycle/compact";
pub const PROJECT_LIFECYCLE_EVENT_STOP: &str = "project/lifecycle/stop";
pub const PROJECT_LIFECYCLE_EVENT_WORKER_COMPLETED: &str = "project/lifecycle/worker/completed";
pub const PROJECT_LIFECYCLE_EVENT_WORKER_ABORTED: &str = "project/lifecycle/worker/aborted";
pub const PROJECT_LIFECYCLE_COMPACTION_REASON: &str = "compact";
pub const PROJECT_COMPACT_SUMMARY_PREFIX: &str = "compact_after_turns threshold";
pub const PROJECT_LIFECYCLE_COMPACT_PROMPT: &str =
    "---\nHandoff requested by project coordinator.\n";
pub const PROJECT_LIFECYCLE_COMPACT_PROMPT_PREFIX: &str = "REASON:";
pub const PROJECT_LIFECYCLE_PROMPT_PREFIX: &str = "PROMPT:";
pub const PROJECT_TASK_LABEL: &str = "TASK\n";
pub const PROJECT_RUNBOOK_LABEL: &str = "RUNBOOK\n";
pub const PROJECT_HOMEDOWN_TEXT: &str = "---\n";

pub const PROJECT_CALLBACK_STATUS_NEVER_RUN: &str = "never_run";
pub const PROJECT_CALLBACK_STATUS_DISABLED: &str = "disabled";
pub const PROJECT_CALLBACK_STATUS_SKIPPED: &str = "skipped";
pub const PROJECT_CALLBACK_STATUS_SUCCESS: &str = "success";
pub const PROJECT_CALLBACK_STATUS_FAILED: &str = "failed";

pub const WORKTREE_VERIFY_COMMAND: &str = "rev-parse";
pub const WORKTREE_VERIFY_ARG: &str = "--is-inside-work-tree";
pub const WORKTREE_BASE_REF_DEFAULT: &str = "HEAD";
pub const WORKTREE_GIT_COMMAND: &str = "git";
pub const WORKTREE_COMMAND: &str = "worktree";
pub const WORKTREE_ARG_ADD: &str = "add";
pub const WORKTREE_ARG_DETACH: &str = "--detach";
pub const WORKTREE_OPTION_WORKDIR: &str = "-C";


pub const APP_SERVER_COMMAND: &str = "app-server";
pub const APP_SERVER_METHOD_INITIALIZE: &str = "initialize";
pub const APP_SERVER_METHOD_INITIALIZED: &str = "initialized";
pub const APP_SERVER_UNHANDLED_ERROR_CODE: i64 = -32601;
pub const APP_SERVER_UNHANDLED_MESSAGE: &str = "Request not handled by foreman";

pub const APP_SERVER_METHOD_THREAD_INTERRUPT: &str = "thread/interrupt";
pub const APP_SERVER_METHOD_TURN_INTERRUPT: &str = "turn/interrupt";
pub const APP_SERVER_METHOD_THREAD_START: &str = "thread/start";
pub const APP_SERVER_METHOD_TURN_START: &str = "turn/start";
pub const APP_SERVER_METHOD_TURN_STEER: &str = "turn/steer";

pub const DEFAULT_AUTH_HEADER_NAME: &str = "authorization";
pub const DEFAULT_AUTH_SKIP_PATH: &str = "/health";
pub const PERSISTED_STATE_VERSION: u32 = 1;

pub const APP_SERVER_ID_KEY: &str = "threadId";
