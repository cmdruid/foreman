use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkerWorktreeSpec {
    pub path: String,
    #[serde(default)]
    pub create: bool,
    pub base_ref: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SpawnAgentRequest {
    pub prompt: String,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub sandbox: Option<String>,

    // Callback profile selected from service config.
    pub callback_profile: Option<String>,
    // Optional prompt text prepended to event JSON before passing to callback commands.
    pub callback_prompt_prefix: Option<String>,
    // Optional full argument list override for callback command profiles.
    pub callback_args: Option<Vec<String>>,
    // Optional variables injected into callback templates.
    pub callback_vars: Option<HashMap<String, String>>,
    // Optional event method filter overrides (e.g. ["turn/completed"]).
    pub callback_events: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SendAgentInput {
    pub prompt: Option<String>,
    pub callback_profile: Option<String>,
    pub callback_prompt_prefix: Option<String>,
    pub callback_args: Option<Vec<String>>,
    pub callback_vars: Option<HashMap<String, String>>,
    pub callback_events: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SteerAgentInput {
    pub prompt: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InterruptInput {
    pub turn_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct CallbackOverrides {
    pub callback_profile: Option<String>,
    pub callback_prompt_prefix: Option<String>,
    pub callback_args: Option<Vec<String>>,
    pub callback_vars: Option<HashMap<String, String>>,
    pub callback_events: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SpawnProjectRequest {
    pub path: String,
    pub start_prompt: Option<String>,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub sandbox: Option<String>,
    #[serde(default)]
    pub callback_overrides: CallbackOverrides,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SpawnProjectWorkerRequest {
    pub prompt: String,
    #[serde(default)]
    pub callback_overrides: CallbackOverrides,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub cwd: Option<String>,
    pub worktree: Option<WorkerWorktreeSpec>,
    #[serde(default)]
    pub sandbox: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectJobWorkerSpec {
    pub prompt: String,
    #[serde(default)]
    pub labels: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub callback_overrides: CallbackOverrides,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub cwd: Option<String>,
    pub worktree: Option<WorkerWorktreeSpec>,
    #[serde(default)]
    pub sandbox: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateProjectJobsRequest {
    pub workers: Vec<ProjectJobWorkerSpec>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateProjectJobsResponse {
    pub job_id: Uuid,
    pub project_id: Uuid,
    pub status: String,
    pub worker_ids: Vec<Uuid>,
    pub worker_count: usize,
    pub labels: std::collections::HashMap<String, Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CompactProjectRequest {
    pub prompt: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AgentEventDto {
    pub ts: u64,
    pub method: String,
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct JobEventEnvelope {
    pub job_id: Uuid,
    pub agent_id: Uuid,
    pub ts: u64,
    pub method: String,
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ToolCallSummary {
    pub tool: String,
    pub command: Option<String>,
    pub path: Option<String>,
    pub at: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct AgentResult {
    pub agent_id: Uuid,
    pub status: String,
    pub completion_method: Option<String>,
    pub turn_id: Option<String>,
    pub final_text: Option<String>,
    pub summary: Option<String>,
    #[serde(default)]
    pub references: Option<Vec<String>>,
    pub completed_at: Option<u64>,
    pub event_id: Option<Uuid>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentState {
    pub id: Uuid,
    pub thread_id: String,
    pub active_turn_id: Option<String>,
    pub status: String,
    pub callback_profile: Option<String>,
    pub role: String,
    pub project_id: Option<Uuid>,
    pub foreman_id: Option<Uuid>,
    pub error: Option<String>,
    pub updated_at: u64,
    pub events: Vec<AgentEventDto>,
    pub turns_completed: u32,
    pub validation_retries: u32,
    pub last_validation_error: Option<String>,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub last_tool_call: Option<ToolCallSummary>,
    pub files_modified: Vec<String>,
    pub elapsed_ms: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AgentResultResponse {
    pub agent_id: Uuid,
    pub status: String,
    pub completion_method: Option<String>,
    pub turn_id: Option<String>,
    pub final_text: Option<String>,
    pub summary: Option<String>,
    #[serde(default)]
    pub references: Option<Vec<String>>,
    pub completed_at: Option<u64>,
    pub event_id: Option<Uuid>,
    pub error: Option<String>,
    pub event_count: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AgentWaitResponse {
    #[serde(flatten)]
    pub result: AgentResultResponse,
    pub timed_out: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<crate::models::AgentEventDto>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct JobWaitResponse {
    #[serde(flatten)]
    pub result: JobResult,
    pub timed_out: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SpawnAgentResponse {
    pub id: Uuid,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub status: String,
    pub project_id: Option<Uuid>,
    pub role: String,
    pub foreman_id: Option<Uuid>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateProjectResponse {
    pub project_id: Uuid,
    pub path: String,
    pub foreman_agent_id: Option<Uuid>,
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReloadProjectResponse {
    pub reloaded: bool,
    pub project_id: Uuid,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectState {
    pub id: Uuid,
    pub path: String,
    pub name: String,
    pub status: String,
    pub foreman_agent_id: Option<Uuid>,
    pub worker_ids: Vec<Uuid>,
    pub worker_count: usize,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectCallbackStatusResponse {
    pub project_id: Uuid,
    pub callbacks: std::collections::HashMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JobState {
    pub id: Uuid,
    pub project_id: Option<Uuid>,
    pub status: String,
    pub worker_ids: Vec<Uuid>,
    pub labels: std::collections::HashMap<String, Vec<String>>,
    pub created_at: u64,
    pub completed_at: Option<u64>,
    pub updated_at: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct JobResult {
    pub id: Uuid,
    pub project_id: Option<Uuid>,
    pub status: String,
    pub total_workers: usize,
    pub completed_workers: usize,
    pub running_workers: usize,
    pub failed_workers: usize,
    pub worker_count: usize,
    pub workers: Vec<AgentResultResponse>,
    pub labels: std::collections::HashMap<String, Vec<String>>,
    pub merged_branches: Vec<String>,
    pub merge_conflicts: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SpawnProjectWorkerResponse {
    pub id: Uuid,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub status: String,
    pub project_id: Uuid,
    pub foreman_id: Uuid,
    pub role: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkerLogEntry {
    pub turn: u32,
    pub tool: String,
    pub command: Option<String>,
    pub path: Option<String>,
    pub exit_code: Option<i32>,
    pub stderr: Option<String>,
    pub bytes: Option<usize>,
    pub at: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ToolFilterCallbackPayload {
    pub worker_id: Uuid,
    pub job_id: Option<Uuid>,
    pub project_id: Uuid,
    pub filter_name: String,
    pub tool: String,
    pub command: Option<String>,
    pub exit_code: Option<i32>,
    pub output_tail: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spawn_agent_request_supports_callback_fields() {
        let raw = json!({
            "prompt": "hello",
            "model_provider": "openai",
            "callback_profile": "openclaw",
            "callback_prompt_prefix": "prefix:",
            "callback_args": ["--event", "{{method}}"],
            "callback_vars": { "thread_id": "abc" },
            "callback_events": ["turn/completed"]
        });

        let request: SpawnAgentRequest =
            serde_json::from_value(raw).expect("deserialize spawn agent request with snake_case");

        assert_eq!(request.model_provider, Some("openai".to_string()));
        assert_eq!(request.callback_profile, Some("openclaw".to_string()));
        assert_eq!(request.callback_prompt_prefix, Some("prefix:".to_string()));
        assert_eq!(
            request.callback_args,
            Some(vec!["--event".to_string(), "{{method}}".to_string()])
        );
        assert_eq!(
            request
                .callback_vars
                .expect("callback vars")
                .get("thread_id")
                .expect("thread id"),
            "abc"
        );
        assert_eq!(
            request.callback_events,
            Some(vec!["turn/completed".to_string()])
        );
    }

    #[test]
    fn spawn_project_worker_request_supports_optional_worktree_spec() {
        let raw = json!({
            "prompt": "worktree action",
            "worktree": {
                "path": ".cf-test-worktree",
                "create": true,
                "base_ref": "HEAD"
            }
        });

        let request: SpawnProjectWorkerRequest =
            serde_json::from_value(raw).expect("deserialize spawn project worker with worktree");
        let worktree = request.worktree.expect("worktree spec");

        assert_eq!(worktree.path, ".cf-test-worktree");
        assert!(worktree.create);
    }
}
