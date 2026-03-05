use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use codex_api::{
    AppServerClient, EmptyResponse, RawNotification, TextPayload, ThreadStartRequest,
    TurnInterruptRequest, TurnStartRequest, TurnSteerRequest, parse_thread_id, parse_turn_id,
};
use regex::RegexBuilder;
use reqwest::Client as HttpClient;
use serde_json::Value;
use tokio::{
    process::Command,
    sync::{Mutex, RwLock, broadcast},
    time,
};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    config::{
        BudgetOnExceed, CallbackFilter, CallbackProfile, CommandCallbackProfile, ServiceConfig,
        UnixSocketCallbackProfile, WebhookCallbackProfile,
    },
    constants,
    events::events_allowed,
    models::{
        AgentProgressEnvelope, AgentResult, AgentState, CallbackOverrides, CompactProjectRequest,
        CreateProjectJobsRequest, CreateProjectJobsResponse, CreateProjectResponse, FailureClass,
        FailureReport, JobEventEnvelope, JobResult, JobState, OverlapRiskEntry,
        PlanProjectJobsRequest, PlanProjectJobsResponse, ProgressThrottleState,
        ProgressValidationState, ProjectCallbackStatusResponse, ProjectState, RetryAgentRequest,
        RetryAgentResponse, SendAgentInput, SpawnAgentRequest, SpawnAgentResponse,
        SpawnProjectRequest, SpawnProjectWorkerRequest, SpawnProjectWorkerResponse,
        SteerAgentInput, ThrottledWorkerStatus, ToolCallSummary, ToolFilterCallbackPayload,
        WorkerBudgetLimits, WorkerBudgetStatus, WorkerBudgetUsage, WorkerLogEntry,
        WorkerWorktreeSpec,
    },
    project::{
        CallbackSpec, JobDefaultsConfig, ProjectConfig, ProjectRuntimeFiles, ValidationConfig,
    },
    state::{
        PersistedAgentRecord, PersistedGovernorRecord, PersistedJobRecord, PersistedProjectRecord,
        PersistedRuntimeFiles, PersistedState, PersistedWorkerBudget, PersistedWorkerCallback,
        PersistedWorkerCheckpoint,
    },
};

const SILENTLY_IGNORED_NOTIFICATIONS: &[&str] = &["account/rateLimits/updated"];

#[derive(Debug, Clone)]
struct WorkerWebhookCallback {
    url: String,
    secret: Option<String>,
    events: Option<Vec<String>>,
    timeout_ms: Option<u64>,
    vars: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct WorkerUnixSocketCallback {
    socket: PathBuf,
    events: Option<Vec<String>>,
    timeout_ms: Option<u64>,
    vars: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct WorkerProfileCallback {
    profile: String,
    prompt_prefix: Option<String>,
    command_args: Option<Vec<String>>,
    events: Option<Vec<String>>,
    vars: HashMap<String, String>,
}

#[derive(Debug, Clone)]
enum WorkerCallback {
    None,
    Webhook(WorkerWebhookCallback),
    UnixSocket(WorkerUnixSocketCallback),
    Profile(WorkerProfileCallback),
}

#[derive(Debug, Clone, PartialEq)]
enum AgentRole {
    Foreman,
    Worker,
    Standalone,
}

impl AgentRole {
    fn as_str(&self) -> &'static str {
        match self {
            AgentRole::Foreman => constants::AGENT_ROLE_FOREMAN,
            AgentRole::Worker => constants::AGENT_ROLE_WORKER,
            AgentRole::Standalone => constants::AGENT_ROLE_STANDALONE,
        }
    }
}

#[derive(Debug, Clone)]
struct CallbackResolutionParams {
    callback_profile: Option<String>,
    callback_prompt_prefix: Option<String>,
    callback_args: Option<Vec<String>>,
    callback_vars: Option<HashMap<String, String>>,
    callback_events: Option<Vec<String>>,
    use_global_default: bool,
}

#[derive(Debug, Clone)]
struct AgentRecord {
    id: Uuid,
    thread_id: String,
    active_turn_id: Option<String>,
    prompt: Option<String>,
    cwd: Option<String>,
    restart_attempts: u32,
    turns_completed: u32,
    validation_retries: u32,
    last_validation_error: Option<String>,
    worktree_path: Option<String>,
    branch_name: Option<String>,
    model: Option<String>,
    model_provider: Option<String>,
    governor_key: Option<String>,
    governor_inflight: bool,
    throttled: bool,
    retry_at_ms: Option<u64>,
    throttle_reason: Option<String>,
    failure_report: Option<FailureReport>,
    budget: WorkerBudgetState,
    checkpoint: WorkerCheckpoint,
    status: String,
    callback: WorkerCallback,
    role: AgentRole,
    project_id: Option<Uuid>,
    foreman_id: Option<Uuid>,
    error: Option<String>,
    result: Option<AgentResult>,
    job_id: Option<Uuid>,
    created_at: u64,
    updated_at: u64,
    events: VecDeque<crate::models::AgentEventDto>,
}

#[derive(Debug, Clone)]
struct WorkerBudgetState {
    prompt_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    estimated_cost_usd: f64,
    elapsed_ms: u64,
    started_at_ms: u64,
    exceeded: bool,
    exceeded_reason: Option<String>,
    paused_reason: Option<String>,
}

impl WorkerBudgetState {
    fn new() -> Self {
        Self {
            prompt_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            estimated_cost_usd: 0.0,
            elapsed_ms: 0,
            started_at_ms: now_ms(),
            exceeded: false,
            exceeded_reason: None,
            paused_reason: None,
        }
    }

    fn total_tokens(&self) -> u64 {
        self.prompt_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cached_tokens)
    }
}

#[derive(Debug, Clone, Default)]
struct WorkerCheckpoint {
    last_successful_turn_id: Option<String>,
    worktree_path: Option<String>,
    branch_name: Option<String>,
    validation_retries: u32,
    last_validation_error: Option<String>,
    retry_count: u32,
}

#[derive(Debug, Clone, Default)]
struct GovernorState {
    throttled_until_ms: u64,
    backoff_ms: u64,
    recent_errors: u32,
    inflight_count: u32,
}

#[derive(Debug, Clone)]
struct JobRecord {
    id: Uuid,
    project_id: Option<Uuid>,
    status: String,
    worker_ids: Vec<Uuid>,
    worker_labels: HashMap<Uuid, HashMap<String, String>>,
    merged_branches: Vec<String>,
    merge_conflicts: Vec<String>,
    worker_names: HashMap<Uuid, String>,
    dependency_graph: HashMap<Uuid, Vec<Uuid>>,
    blocked_reasons: HashMap<Uuid, String>,
    base_branch: String,
    merge_strategy: String,
    created_at: u64,
    completed_at: Option<u64>,
    updated_at: u64,
}

#[derive(Debug, Clone)]
struct JobWorkerSpecResolved {
    worker_id: Uuid,
    worker_name: String,
    prompt: String,
    labels: HashMap<String, String>,
    callback: WorkerCallback,
    model: Option<String>,
    model_provider: Option<String>,
    sandbox: Option<String>,
    cwd: String,
    worktree_path: Option<String>,
    branch_name: Option<String>,
    depends_on: Vec<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DependencyStatus {
    Satisfied,
    Pending,
    Failed(Uuid),
}

#[derive(Debug, Clone)]
struct CallbackEventContext {
    agent_id: Uuid,
    thread_id: String,
    turn_id: Option<String>,
    role: AgentRole,
    project_id: Option<Uuid>,
    foreman_id: Option<Uuid>,
    job_id: Option<Uuid>,
    method: String,
    ts: u64,
    params: Value,
    event_id: Uuid,
    result_snapshot: Option<AgentResult>,
    callback: WorkerCallback,
}

#[derive(Debug, Clone)]
struct ToolFilterDispatchContext {
    agent_id: Uuid,
    job_id: Option<Uuid>,
    project_id: Uuid,
    params: Value,
}

#[derive(Debug, Clone)]
struct ProjectRecord {
    id: Uuid,
    path: String,
    name: String,
    status: String,
    foreman_agent_id: Option<Uuid>,
    worker_ids: Vec<Uuid>,
    completed_worker_turns: u64,
    config: ProjectConfig,
    runtime: ProjectRuntimeFiles,
    lifecycle_callback_status: HashMap<String, String>,
    created_at: u64,
    updated_at: u64,
}

#[derive(Debug)]
pub struct Foreman {
    client: Arc<AppServerClient>,
    config: Arc<ServiceConfig>,
    http_client: HttpClient,
    pending_events: RwLock<HashMap<String, VecDeque<crate::models::AgentEventDto>>>,
    agents: RwLock<HashMap<Uuid, AgentRecord>>,
    thread_map: RwLock<HashMap<String, Uuid>>,
    projects: RwLock<HashMap<Uuid, ProjectRecord>>,
    jobs: RwLock<HashMap<Uuid, JobRecord>>,
    governors: RwLock<HashMap<String, GovernorState>>,
    callback_filter_debounce: RwLock<HashMap<(Uuid, String), u64>>,
    job_event_tx: broadcast::Sender<JobEventEnvelope>,
    agent_progress_tx: broadcast::Sender<AgentProgressEnvelope>,
    worktree_lock: Mutex<()>,
    started_at: u64,
    state_path: PathBuf,
    recovery_state: RwLock<PersistedState>,
}

impl Foreman {
    pub fn new(
        client: Arc<AppServerClient>,
        event_rx: broadcast::Receiver<RawNotification>,
        config: ServiceConfig,
        persisted_state: PersistedState,
        state_path: PathBuf,
    ) -> Arc<Self> {
        let foreman = Arc::new(Self {
            client: client.clone(),
            config: Arc::new(config),
            http_client: HttpClient::new(),
            pending_events: RwLock::new(HashMap::new()),
            agents: RwLock::new(HashMap::new()),
            thread_map: RwLock::new(HashMap::new()),
            projects: RwLock::new(HashMap::new()),
            jobs: RwLock::new(HashMap::new()),
            governors: RwLock::new(HashMap::new()),
            callback_filter_debounce: RwLock::new(HashMap::new()),
            job_event_tx: broadcast::channel(constants::JOB_EVENT_CHANNEL_CAPACITY).0,
            agent_progress_tx: broadcast::channel(constants::AGENT_PROGRESS_CHANNEL_CAPACITY).0,
            worktree_lock: Mutex::new(()),
            started_at: now_ts(),
            state_path,
            recovery_state: RwLock::new(persisted_state),
        });

        Self::spawn_event_loop(Arc::clone(&foreman), event_rx);
        Self::spawn_worker_monitor(Arc::clone(&foreman));
        foreman
    }

    pub async fn recover_state(self: &Arc<Self>) -> Result<()> {
        let snapshot = {
            let snapshot = self.recovery_state.read().await;
            snapshot.clone()
        };

        let mut loaded_agents = Vec::with_capacity(snapshot.agents.len());
        for record in snapshot.agents {
            let restored = restore_agent_record(record)?;
            loaded_agents.push(restored);
        }

        let mut loaded_projects = Vec::with_capacity(snapshot.projects.len());
        for record in snapshot.projects {
            let restored = restore_project_record(record)?;
            loaded_projects.push(restored);
        }

        let mut loaded_jobs = Vec::with_capacity(snapshot.jobs.len());
        for record in snapshot.jobs {
            let restored =
                restore_job_record(record).context("failed to restore job record from state")?;
            loaded_jobs.push(restored);
        }
        let loaded_governors = snapshot
            .governors
            .into_iter()
            .map(|(key, record)| (key, restore_governor_record(record)))
            .collect::<HashMap<_, _>>();

        {
            let mut projects = self.projects.write().await;
            for project in loaded_projects {
                projects.insert(project.id, project);
            }
        }

        {
            let mut agents = self.agents.write().await;
            let mut thread_map = self.thread_map.write().await;
            for agent in loaded_agents {
                thread_map.insert(agent.thread_id.clone(), agent.id);
                agents.insert(agent.id, agent);
            }
        }

        {
            let mut jobs = self.jobs.write().await;
            for job in loaded_jobs {
                jobs.insert(job.id, job);
            }
        }
        {
            let mut governors = self.governors.write().await;
            governors.extend(loaded_governors);
        }

        self.schedule_state_persist();
        Ok(())
    }

    async fn enforce_worker_monitoring(
        self: &Arc<Self>,
        inactivity_timeout_ms: u64,
        max_restarts: u32,
    ) -> Result<()> {
        let now = now_ts();
        let stale_workers = {
            let agents = self.agents.read().await;
            let mut stale = Vec::new();
            for agent in agents.values() {
                if agent.role != AgentRole::Worker {
                    continue;
                }

                if agent.status != constants::AGENT_STATUS_RUNNING {
                    continue;
                }

                if agent.prompt.is_none() {
                    continue;
                }

                let elapsed_ms = now
                    .saturating_sub(agent.updated_at)
                    .saturating_mul(constants::MILLISECONDS_PER_SECOND);
                if elapsed_ms < inactivity_timeout_ms {
                    continue;
                }

                if agent.active_turn_id.is_none() {
                    continue;
                }

                stale.push(agent.id);
            }
            stale
        };

        for agent_id in stale_workers {
            self.restart_stalled_worker(agent_id, inactivity_timeout_ms, max_restarts)
                .await?;
        }

        Ok(())
    }

    async fn restart_stalled_worker(
        self: &Arc<Self>,
        agent_id: Uuid,
        inactivity_timeout_ms: u64,
        max_restarts: u32,
    ) -> Result<()> {
        #[derive(Debug)]
        struct Candidate {
            project_id: Option<Uuid>,
            job_id: Option<Uuid>,
            thread_id: String,
            prompt: String,
            active_turn_id: Option<String>,
            should_restart: bool,
        }

        let candidate = {
            let mut agents = self.agents.write().await;
            let agent = match agents.get_mut(&agent_id) {
                Some(agent) => agent,
                None => return Ok(()),
            };

            if agent.status != constants::AGENT_STATUS_RUNNING {
                return Ok(());
            }

            let prompt = agent.prompt.clone().filter(|prompt| !prompt.is_empty());
            if prompt.is_none() {
                return Ok(());
            }

            if agent.restart_attempts >= max_restarts {
                let result = Some(AgentResult {
                    agent_id,
                    status: constants::AGENT_STATUS_ABORTED.to_string(),
                    completion_method: Some(constants::EVENT_METHOD_TURN_ABORTED.to_string()),
                    turn_id: agent.active_turn_id.clone(),
                    final_text: None,
                    summary: Some("worker stalled and exceeded restart budget".to_string()),
                    references: None,
                    completed_at: Some(now_ts()),
                    event_id: None,
                    error: Some(format!(
                        "worker stalled for {} ms without progress",
                        inactivity_timeout_ms
                    )),
                });

                agent.status = constants::AGENT_STATUS_FAILED.to_string();
                agent.updated_at = now_ts();
                agent.result = result;
                agent.error = Some("worker monitoring restart budget exceeded".to_string());
                update_failure_report(
                    &mut agent.failure_report,
                    FailureClass::Timeout,
                    "worker monitoring restart budget exceeded".to_string(),
                    Some(format!(
                        "worker stalled for {} ms without progress",
                        inactivity_timeout_ms
                    )),
                    false,
                    now_ts(),
                );
                agent.restart_attempts = agent.restart_attempts.saturating_add(0);

                Some(Candidate {
                    project_id: agent.project_id,
                    job_id: agent.job_id,
                    thread_id: agent.thread_id.clone(),
                    prompt: prompt.unwrap_or_default(),
                    active_turn_id: agent.active_turn_id.clone(),
                    should_restart: false,
                })
            } else {
                agent.status = constants::AGENT_STATUS_RESTARTING.to_string();
                agent.updated_at = now_ts();
                agent.restart_attempts = agent.restart_attempts.saturating_add(1);
                agent.error = None;

                Some(Candidate {
                    project_id: agent.project_id,
                    job_id: agent.job_id,
                    thread_id: agent.thread_id.clone(),
                    prompt: prompt.expect("prompt must exist for restart"),
                    active_turn_id: agent.active_turn_id.clone(),
                    should_restart: true,
                })
            }
        };

        let Some(candidate) = candidate else {
            return Ok(());
        };

        if !candidate.should_restart {
            if let Err(err) = self.cleanup_agent_worktree_if_any(agent_id, false).await {
                warn!(
                    %err,
                    %agent_id,
                    "failed to cleanup worker worktree after restart budget exhaustion"
                );
            }
            if let Some(project_id) = candidate.project_id {
                let _ = self.update_project_after_worker_event(project_id).await;
            }
            if let Some(job_id) = candidate.job_id {
                let result_snapshot = {
                    let agents = self.agents.read().await;
                    agents.get(&agent_id).and_then(|agent| agent.result.clone())
                };
                let _ = self
                    .update_job_after_worker_event(job_id, result_snapshot.as_ref())
                    .await;
            }
            self.schedule_state_persist();
            return Ok(());
        }

        let thread_id = candidate.thread_id;
        let prompt = candidate.prompt;

        if let Some(current_turn_id) = candidate.active_turn_id.as_deref() {
            let _ = self
                .client
                .turn_interrupt(&TurnInterruptRequest {
                    thread_id: thread_id.clone(),
                    turn_id: current_turn_id.to_string(),
                })
                .await;
        }

        let request = TurnStartRequest {
            thread_id: thread_id.clone(),
            input: vec![TextPayload::text(prompt)],
        };
        let response = self.client.turn_start(&request).await;

        let mut agents = self.agents.write().await;
        let Some(agent) = agents.get_mut(&agent_id) else {
            return Ok(());
        };

        match response {
            Ok(response) => {
                let current_turn_id = response.turn_id.clone();
                agent.active_turn_id = Some(current_turn_id.clone());
                let should_reset_result =
                    matches!(agent.status.as_str(), constants::AGENT_STATUS_RESTARTING)
                        || agent
                            .result
                            .as_ref()
                            .filter(
                                |result| {
                                    matches!(
                                        result.completion_method.as_deref(),
                                        Some(constants::EVENT_METHOD_TURN_ABORTED)
                                    )
                                }
                                    && result.turn_id == candidate.active_turn_id
                            )
                            .is_some();

                if should_reset_result {
                    agent.result = None;
                    agent.error = None;
                }
                if matches!(agent.status.as_str(), constants::AGENT_STATUS_RESTARTING) {
                    agent.status = constants::AGENT_STATUS_RUNNING.to_string();
                }
                agent.updated_at = now_ts();
                self.schedule_state_persist();
            }
            Err(err) => {
                agent.status = constants::AGENT_STATUS_FAILED.to_string();
                agent.error = Some(format!("worker restart failed: {err}"));
                update_failure_report(
                    &mut agent.failure_report,
                    FailureClass::Unknown,
                    "worker restart failed".to_string(),
                    Some(err.to_string()),
                    true,
                    now_ts(),
                );
                agent.result = Some(AgentResult {
                    agent_id,
                    status: constants::AGENT_STATUS_ABORTED.to_string(),
                    completion_method: Some(constants::EVENT_METHOD_TURN_ABORTED.to_string()),
                    turn_id: agent.active_turn_id.clone(),
                    final_text: None,
                    summary: Some("worker restart failed".to_string()),
                    references: None,
                    completed_at: Some(now_ts()),
                    event_id: None,
                    error: Some(format!("worker restart failed: {err}")),
                });
                agent.updated_at = now_ts();
                self.schedule_state_persist();
            }
        }

        let result_snapshot = agent.result.clone();
        drop(agents);

        if let Some(project_id) = candidate.project_id {
            let _ = self.update_project_after_worker_event(project_id).await;
        }
        if let Some(job_id) = candidate.job_id {
            let _ = self
                .update_job_after_worker_event(job_id, result_snapshot.as_ref())
                .await;
        }
        Ok(())
    }

    fn spawn_event_loop(this: Arc<Self>, mut event_rx: broadcast::Receiver<RawNotification>) {
        tokio::spawn(async move {
            loop {
                match event_rx.recv().await {
                    Ok(event) => this.route_notification(event).await,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(skipped, "event channel lagged; continuing");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("event channel closed");
                        break;
                    }
                }
            }
        });
    }

    fn spawn_worker_monitor(this: Arc<Self>) {
        let worker_monitoring = this.config.worker_monitoring.clone();
        tokio::spawn(async move {
            let interval = time::Duration::from_millis(
                if worker_monitoring.enabled {
                    worker_monitoring.watch_interval_ms
                } else {
                    constants::DEFAULT_BUDGET_WATCH_INTERVAL_MS
                }
                .max(constants::MIN_WORKER_MONITOR_INTERVAL_MS),
            );
            loop {
                time::sleep(interval).await;
                if let Err(err) = this.enforce_budget_guardrails().await {
                    warn!(%err, "budget guardrail cycle failed");
                }
                if worker_monitoring.enabled
                    && let Err(err) = this
                        .enforce_worker_monitoring(
                            worker_monitoring.inactivity_timeout_ms,
                            worker_monitoring.max_restarts,
                        )
                        .await
                {
                    warn!(%err, "worker monitoring cycle failed");
                }
            }
        });
    }

    async fn enforce_budget_guardrails(self: &Arc<Self>) -> Result<()> {
        let worker_ids = {
            let agents = self.agents.read().await;
            agents
                .values()
                .filter(|agent| agent.role == AgentRole::Worker)
                .map(|agent| agent.id)
                .collect::<Vec<_>>()
        };

        for worker_id in worker_ids {
            self.apply_budget_policy(worker_id, "budget/tick").await?;
        }
        Ok(())
    }

    fn schedule_state_persist(self: &Arc<Self>) {
        let foreman = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = foreman.persist_state().await {
                warn!(%err, "failed to persist foreman state");
            }
        });
    }

    pub async fn reconcile_agent_statuses(&self) {
        let mut agents = self.agents.write().await;
        for agent in agents.values_mut() {
            if let Some(result) = &agent.result
                && matches!(agent.status.as_str(), "running" | "restarting")
            {
                agent.status = result.status.clone();
                agent.updated_at = now_ts();
            }
        }
    }

    pub async fn persist_state_now(self: &Arc<Self>) -> Result<()> {
        self.persist_state().await
    }

    async fn persist_state(self: &Arc<Self>) -> Result<()> {
        let agents = {
            let agents = self.agents.read().await;
            let mut snapshot = Vec::with_capacity(agents.len());
            for agent in agents.values() {
                snapshot.push(persisted_agent_record(agent));
            }
            snapshot
        };

        let projects = {
            let projects = self.projects.read().await;
            let mut snapshot = Vec::with_capacity(projects.len());
            for project in projects.values() {
                snapshot.push(persisted_project_record(project));
            }
            snapshot
        };

        let jobs = {
            let jobs = self.jobs.read().await;
            let mut snapshot = Vec::with_capacity(jobs.len());
            for job in jobs.values() {
                snapshot.push(persisted_job_record(job));
            }
            snapshot
        };
        let governors = {
            let governors = self.governors.read().await;
            governors
                .iter()
                .map(|(key, value)| (key.clone(), persisted_governor_record(value)))
                .collect::<HashMap<_, _>>()
        };

        let state = PersistedState {
            version: constants::PERSISTED_STATE_VERSION,
            generated_at: now_ts(),
            agents,
            projects,
            jobs,
            governors,
        };

        state.save(&self.state_path).await?;
        Ok(())
    }

    async fn route_notification(self: &Arc<Self>, event: RawNotification) {
        let normalized_method = Self::normalize_event_method(event.method.as_str());
        let thread_id = parse_thread_id(&event.params);
        if let Some(thread_id) = thread_id {
            let agent_id = {
                let thread_map = self.thread_map.read().await;
                thread_map.get(&thread_id).cloned()
            };

            if let Some(agent_id) = agent_id {
                self.dispatch_event(agent_id, normalized_method.to_string(), event.params)
                    .await;
            } else {
                self.buffer_pending_event(&thread_id, normalized_method.as_ref(), event.params)
                    .await;
            }
        } else {
            if !SILENTLY_IGNORED_NOTIFICATIONS
                .iter()
                .any(|method| normalized_method.as_ref() == *method)
            {
                debug!(
                    method = %event.method,
                    normalized_method = %normalized_method,
                    "dropping notification without thread id"
                );
            }
        }
    }

    async fn buffer_pending_event(&self, thread_id: &str, method: &str, params: Value) {
        let mut pending_events = self.pending_events.write().await;
        let queue = pending_events
            .entry(thread_id.to_string())
            .or_insert_with(VecDeque::new);
        queue.push_back(crate::models::AgentEventDto {
            ts: now_ts(),
            method: method.to_string(),
            params,
        });
        while queue.len() > constants::EVENT_BUFFER_MAX {
            queue.pop_front();
        }

        debug!(
            method = method,
            thread_id = thread_id,
            queued = queue.len(),
            "buffered notification for unresolved thread"
        );
    }

    async fn drain_pending_events(
        self: &Arc<Self>,
        thread_id: &str,
    ) -> Vec<crate::models::AgentEventDto> {
        let mut pending_events = self.pending_events.write().await;
        pending_events
            .remove(thread_id)
            .map(|events| events.into_iter().collect())
            .unwrap_or_default()
    }

    fn normalize_event_method(method: &str) -> Cow<'_, str> {
        if let Some(rest) = method.strip_prefix(constants::CODEX_EVENT_METHOD_PREFIX) {
            return Cow::Borrowed(rest);
        }

        Cow::Borrowed(method)
    }

    async fn dispatch_event(self: &Arc<Self>, agent_id: Uuid, method: String, params: Value) {
        let ts = now_ts();
        let event_id = Uuid::new_v4();
        let mut callback_context: Option<CallbackEventContext> = None;
        let mut tool_filter_context: Option<ToolFilterDispatchContext> = None;
        let mut should_update_job = false;
        let mut should_release_inflight = false;
        let mut progress_event: Option<AgentProgressEnvelope> = None;
        {
            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&agent_id) {
                agent.updated_at = ts;
                agent.events.push_back(crate::models::AgentEventDto {
                    ts,
                    method: method.clone(),
                    params: params.clone(),
                });
                while agent.events.len() > constants::EVENT_BUFFER_MAX {
                    agent.events.pop_front();
                }
                update_budget_counters(&mut agent.budget, &params);
                agent.budget.elapsed_ms = now_ms().saturating_sub(agent.budget.started_at_ms);

                if method == constants::EVENT_METHOD_ITEM_COMPLETED
                    && is_command_execution_item_completed(&method, &params)
                    && let Some(project_id) = agent.project_id
                {
                    tool_filter_context = Some(ToolFilterDispatchContext {
                        agent_id,
                        job_id: agent.job_id,
                        project_id,
                        params: params.clone(),
                    });
                }

                if matches!(
                    method.as_str(),
                    constants::EVENT_METHOD_TURN_COMPLETED | constants::EVENT_METHOD_TURN_ABORTED
                ) {
                    let completed_turn_id =
                        parse_turn_id(&params).or_else(|| agent.active_turn_id.clone());
                    if method == constants::EVENT_METHOD_TURN_COMPLETED {
                        agent.turns_completed = agent.turns_completed.saturating_add(1);
                        agent.checkpoint.last_successful_turn_id = completed_turn_id.clone();
                        agent.checkpoint.worktree_path = agent.worktree_path.clone();
                        agent.checkpoint.branch_name = agent.branch_name.clone();
                        agent.checkpoint.validation_retries = agent.validation_retries;
                        agent.checkpoint.last_validation_error =
                            agent.last_validation_error.clone();
                    }
                    agent.status = constants::AGENT_STATUS_IDLE.to_string();
                    agent.throttled = false;
                    agent.retry_at_ms = None;
                    agent.throttle_reason = None;
                    agent.result = Some(build_completion_result(
                        agent.id,
                        &method,
                        completed_turn_id,
                        &params,
                        ts,
                        event_id,
                    ));
                    if method == constants::EVENT_METHOD_TURN_ABORTED {
                        let error_text = params
                            .get("error")
                            .and_then(Value::as_str)
                            .map(std::string::ToString::to_string);
                        let class =
                            classify_failure_from_error(error_text.as_deref(), Some(&params));
                        let summary = failure_summary_for_class(&class);
                        let retryable = failure_class_retryable(&class);
                        update_failure_report(
                            &mut agent.failure_report,
                            class,
                            summary.to_string(),
                            error_text,
                            retryable,
                            ts,
                        );
                    }
                    should_update_job = true;
                    should_release_inflight = true;
                }
                if method == constants::THREAD_EVENT_STARTED
                    && let Some(status) = params.get("status").and_then(|value| value.as_str())
                {
                    agent.status = status.to_string();
                    if let Some(completion_method) = completion_method_for_thread_status(status) {
                        let mut result = build_completion_result(
                            agent.id,
                            completion_method,
                            parse_turn_id(&params).or_else(|| agent.active_turn_id.clone()),
                            &params,
                            ts,
                            event_id,
                        );
                        if completion_method == constants::EVENT_METHOD_TURN_ABORTED
                            && status != constants::EVENT_STATUS_ABORTED
                        {
                            result.status = constants::EVENT_STATUS_FAILED.to_string();
                        }
                        if let Some(error) = params.get("error").and_then(Value::as_str) {
                            result.error = Some(error.to_string());
                        }
                        if completion_method == constants::EVENT_METHOD_TURN_ABORTED {
                            let error_text = result.error.clone();
                            let class =
                                classify_failure_from_error(error_text.as_deref(), Some(&params));
                            let summary = failure_summary_for_class(&class);
                            let retryable = failure_class_retryable(&class);
                            update_failure_report(
                                &mut agent.failure_report,
                                class,
                                summary.to_string(),
                                error_text,
                                retryable,
                                ts,
                            );
                        }
                        agent.result = Some(result);
                        should_update_job = true;
                        should_release_inflight = true;
                    }
                }
                if method == constants::EVENT_METHOD_TURN_STARTED
                    && let Some(turn_id) = parse_turn_id(&params)
                {
                    agent.status = constants::AGENT_STATUS_RUNNING.to_string();
                    agent.active_turn_id = Some(turn_id);
                    agent.result = None;
                }
                if method == constants::EVENT_METHOD_ITEM_AGENT_MESSAGE
                    || method == constants::EVENT_METHOD_ITEM_AGENT_MESSAGE_DELTA
                    || (method == constants::EVENT_METHOD_ITEM_COMPLETED
                        && is_agent_item_completion_payload(&params))
                {
                    agent.result = Some(update_agent_result_text(
                        agent.result.clone(),
                        agent.id,
                        &method,
                        parse_turn_id(&params).or_else(|| agent.active_turn_id.clone()),
                        &params,
                        event_id,
                        ts,
                    ));
                }

                let turn_id = parse_turn_id(&params).or_else(|| agent.active_turn_id.clone());
                let thread_id = agent.thread_id.clone();
                let result_snapshot = agent.result.clone();
                callback_context = Some(CallbackEventContext {
                    agent_id,
                    thread_id,
                    turn_id,
                    role: agent.role.clone(),
                    project_id: agent.project_id,
                    foreman_id: agent.foreman_id,
                    job_id: agent.job_id,
                    method: method.clone(),
                    ts,
                    params: params.clone(),
                    event_id,
                    result_snapshot,
                    callback: agent.callback.clone(),
                });
                let event = crate::models::AgentEventDto {
                    ts,
                    method: method.clone(),
                    params: truncate_json_value(&params, 240),
                };
                progress_event = Some(progress_from_agent_event(agent, &event));
            }
        }

        if let Err(err) = self.apply_budget_policy(agent_id, "budget/event").await {
            warn!(%err, agent_id = %agent_id, "budget policy enforcement failed");
        }

        if should_release_inflight {
            self.release_governor_inflight(agent_id).await;
        }

        if let Some(context) = tool_filter_context
            && let Err(err) = self.dispatch_tool_filtered_callbacks(context).await
        {
            warn!(%err, agent_id = %agent_id, "tool-filtered callback execution failed");
        }

        if let Some(context) = callback_context {
            if let Err(err) = self.handle_worker_post_turn(&context).await {
                warn!(%err, agent_id = %context.agent_id, "post-turn worker policy failed");
            }
            if let Err(err) = self.handle_worker_worktree_lifecycle(&context).await {
                warn!(
                    %err,
                    agent_id = %context.agent_id,
                    "worker worktree lifecycle handling failed"
                );
            }

            if let Some(job_id) = context.job_id {
                let _ = self.job_event_tx.send(JobEventEnvelope {
                    job_id,
                    agent_id: context.agent_id,
                    ts: context.ts,
                    method: context.method.clone(),
                    params: context.params.clone(),
                });
            }

            if matches!(
                context.method.as_str(),
                constants::EVENT_METHOD_TURN_COMPLETED | constants::EVENT_METHOD_TURN_ABORTED
            ) || should_update_job
            {
                if let Some(job_id) = context.job_id {
                    if let Ok(Some(status)) = self.update_job_after_worker_event(job_id, None).await
                    {
                        let _ = self.job_event_tx.send(JobEventEnvelope {
                            job_id,
                            agent_id: context.agent_id,
                            ts: now_ts(),
                            method: "job.completed".to_string(),
                            params: serde_json::json!({ "status": status }),
                        });
                    }
                }
                if let Some(project_id) = context.project_id {
                    let _ = self.update_project_after_worker_event(project_id).await;
                }
            }
            self.schedule_state_persist();
            if let Err(err) = self.execute_event(context).await {
                let mut agents = self.agents.write().await;
                if let Some(agent) = agents.get_mut(&agent_id) {
                    agent.error = Some(format!("callback execution failed: {err}"));
                    update_failure_report(
                        &mut agent.failure_report,
                        FailureClass::CallbackError,
                        "callback execution failed".to_string(),
                        Some(err.to_string()),
                        true,
                        now_ts(),
                    );
                    agent.updated_at = now_ts();
                }
                warn!(%err, "callback execution failed");
            }
        }

        if let Some(progress) = progress_event {
            let _ = self.agent_progress_tx.send(progress);
        }
    }

    async fn handle_worker_post_turn(
        self: &Arc<Self>,
        context: &CallbackEventContext,
    ) -> Result<()> {
        if context.role != AgentRole::Worker
            || context.method != constants::EVENT_METHOD_TURN_COMPLETED
        {
            return Ok(());
        }
        let Some(project_id) = context.project_id else {
            return Ok(());
        };

        let (validation, job_defaults) = {
            let projects = self.projects.read().await;
            let Some(project) = projects.get(&project_id) else {
                return Ok(());
            };
            (
                project.config.validation.clone(),
                project.config.jobs.defaults.clone(),
            )
        };

        let (cwd, turns_completed) = {
            let agents = self.agents.read().await;
            let Some(agent) = agents.get(&context.agent_id) else {
                return Ok(());
            };
            (agent.cwd.clone(), agent.turns_completed)
        };

        if let Some(validation) = &validation
            && !validation.on_turn.is_empty()
        {
            let cwd = cwd
                .as_deref()
                .map(Path::new)
                .context("worker has no cwd configured for validation.on_turn")?;
            match run_validation_commands(&validation.on_turn, cwd).await {
                Ok(()) => self.clear_validation_error(context.agent_id).await,
                Err(err) => {
                    if self
                        .handle_validation_failure(context.agent_id, validation, err.to_string())
                        .await?
                    {
                        return Ok(());
                    }
                }
            }
        }

        if !self.is_agent_idle_or_completed(context.agent_id).await {
            return Ok(());
        }

        if let Some(prompt) = continuation_prompt_for_turn(turns_completed, &job_defaults) {
            self.send_turn(
                context.agent_id,
                SendAgentInput {
                    prompt: Some(prompt),
                    callback_profile: None,
                    callback_prompt_prefix: None,
                    callback_args: None,
                    callback_vars: None,
                    callback_events: None,
                },
            )
            .await?;
            return Ok(());
        }

        if let Some(validation) = &validation
            && !validation.on_complete.is_empty()
        {
            let cwd = cwd
                .as_deref()
                .map(Path::new)
                .context("worker has no cwd configured for validation.on_complete")?;
            match run_validation_commands(&validation.on_complete, cwd).await {
                Ok(()) => self.clear_validation_error(context.agent_id).await,
                Err(err) => {
                    let _ = self
                        .handle_validation_failure(context.agent_id, validation, err.to_string())
                        .await?;
                }
            }
        }

        Ok(())
    }

    async fn handle_worker_worktree_lifecycle(
        self: &Arc<Self>,
        context: &CallbackEventContext,
    ) -> Result<()> {
        if context.role != AgentRole::Worker {
            return Ok(());
        }

        let Some(project_id) = context.project_id else {
            return Ok(());
        };

        let Some(job_id) = context.job_id else {
            return Ok(());
        };

        let is_completed = context.method == constants::EVENT_METHOD_TURN_COMPLETED;
        let is_aborted = context.method == constants::EVENT_METHOD_TURN_ABORTED
            || context
                .result_snapshot
                .as_ref()
                .is_some_and(is_worker_failure);
        if !is_completed && !is_aborted {
            return Ok(());
        }

        let (worktree_path, branch_name) = {
            let agents = self.agents.read().await;
            let Some(agent) = agents.get(&context.agent_id) else {
                return Ok(());
            };
            (agent.worktree_path.clone(), agent.branch_name.clone())
        };

        let Some(worktree_path) = worktree_path else {
            return Ok(());
        };

        let (project_path, base_branch, merge_strategy) = {
            let projects = self.projects.read().await;
            let Some(project) = projects.get(&project_id) else {
                return Ok(());
            };
            let jobs = self.jobs.read().await;
            let Some(job) = jobs.get(&job_id) else {
                return Ok(());
            };
            (
                project.path.clone(),
                job.base_branch.clone(),
                job.merge_strategy.clone(),
            )
        };

        if is_completed
            && merge_strategy != "manual"
            && let Some(branch_name_ref) = branch_name.as_deref()
        {
            match self
                .merge_worker_branch(
                    project_path.as_str(),
                    base_branch.as_str(),
                    branch_name_ref,
                    merge_strategy.as_str(),
                )
                .await
            {
                Ok(()) => {
                    {
                        let mut jobs = self.jobs.write().await;
                        if let Some(job) = jobs.get_mut(&job_id)
                            && !job
                                .merged_branches
                                .iter()
                                .any(|value| value == branch_name_ref)
                        {
                            job.merged_branches.push(branch_name_ref.to_string());
                            job.updated_at = now_ts();
                        }
                    }
                    self.cleanup_worker_worktree(
                        project_path.as_str(),
                        worktree_path.as_str(),
                        Some(branch_name_ref),
                        true,
                    )
                    .await?;

                    let mut agents = self.agents.write().await;
                    if let Some(agent) = agents.get_mut(&context.agent_id) {
                        agent.worktree_path = None;
                        agent.branch_name = None;
                        agent.updated_at = now_ts();
                    }
                }
                Err(err) => {
                    let mut jobs = self.jobs.write().await;
                    if let Some(job) = jobs.get_mut(&job_id)
                        && !job
                            .merge_conflicts
                            .iter()
                            .any(|value| value == branch_name_ref)
                    {
                        job.merge_conflicts.push(branch_name_ref.to_string());
                        job.updated_at = now_ts();
                    }
                    drop(jobs);
                    let mut agents = self.agents.write().await;
                    if let Some(agent) = agents.get_mut(&context.agent_id) {
                        let details = format!(
                            "merge strategy '{}' failed for branch '{}': {}",
                            merge_strategy, branch_name_ref, err
                        );
                        agent.error =
                            Some("merge conflict while merging worker branch".to_string());
                        update_failure_report(
                            &mut agent.failure_report,
                            FailureClass::MergeConflict,
                            "merge conflict while merging worker branch".to_string(),
                            Some(details),
                            true,
                            now_ts(),
                        );
                        agent.updated_at = now_ts();
                    }
                    return Err(err);
                }
            }
        }

        if is_aborted {
            self.cleanup_worker_worktree(
                project_path.as_str(),
                worktree_path.as_str(),
                branch_name.as_deref(),
                false,
            )
            .await?;

            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&context.agent_id) {
                agent.worktree_path = None;
                agent.updated_at = now_ts();
            }
        }

        Ok(())
    }

    pub async fn cleanup_residual_worktrees(&self) {
        let workers = {
            let agents = self.agents.read().await;
            let projects = self.projects.read().await;
            let mut workers = Vec::new();
            for agent in agents.values() {
                let Some(worktree_path) = agent.worktree_path.clone() else {
                    continue;
                };
                let Some(project_id) = agent.project_id else {
                    continue;
                };
                let Some(project) = projects.get(&project_id) else {
                    continue;
                };
                workers.push((
                    agent.id,
                    project.path.clone(),
                    worktree_path,
                    agent.branch_name.clone(),
                ));
            }
            workers
        };

        for (agent_id, project_path, worktree_path, branch_name) in workers {
            if let Err(err) = self
                .cleanup_worker_worktree(
                    project_path.as_str(),
                    worktree_path.as_str(),
                    branch_name.as_deref(),
                    false,
                )
                .await
            {
                warn!(
                    %err,
                    %agent_id,
                    worktree_path = %worktree_path,
                    "failed to cleanup residual worker worktree during shutdown reconcile"
                );
                continue;
            }

            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&agent_id) {
                agent.worktree_path = None;
                agent.updated_at = now_ts();
            }
        }
    }

    async fn cleanup_agent_worktree_if_any(&self, agent_id: Uuid, was_merged: bool) -> Result<()> {
        let (project_path, worktree_path, branch_name) = {
            let agents = self.agents.read().await;
            let Some(agent) = agents.get(&agent_id) else {
                return Ok(());
            };
            let Some(worktree_path) = agent.worktree_path.clone() else {
                return Ok(());
            };
            let Some(project_id) = agent.project_id else {
                return Ok(());
            };

            let projects = self.projects.read().await;
            let Some(project) = projects.get(&project_id) else {
                return Ok(());
            };

            (
                project.path.clone(),
                worktree_path,
                agent.branch_name.clone(),
            )
        };

        self.cleanup_worker_worktree(
            project_path.as_str(),
            worktree_path.as_str(),
            branch_name.as_deref(),
            was_merged,
        )
        .await?;

        let mut agents = self.agents.write().await;
        if let Some(agent) = agents.get_mut(&agent_id) {
            agent.worktree_path = None;
            if was_merged {
                agent.branch_name = None;
            }
            agent.updated_at = now_ts();
        }

        Ok(())
    }

    async fn is_agent_idle_or_completed(&self, agent_id: Uuid) -> bool {
        let agents = self.agents.read().await;
        let Some(agent) = agents.get(&agent_id) else {
            return false;
        };
        matches!(
            agent.status.as_str(),
            constants::AGENT_STATUS_IDLE | constants::AGENT_STATUS_COMPLETED
        )
    }

    async fn clear_validation_error(&self, agent_id: Uuid) {
        let mut agents = self.agents.write().await;
        if let Some(agent) = agents.get_mut(&agent_id) {
            agent.last_validation_error = None;
            agent.checkpoint.last_validation_error = None;
            agent.checkpoint.validation_retries = agent.validation_retries;
            agent.updated_at = now_ts();
            let _ = self
                .agent_progress_tx
                .send(progress_validation_state(agent, "validation/ok"));
        }
    }

    async fn handle_validation_failure(
        self: &Arc<Self>,
        agent_id: Uuid,
        validation: &ValidationConfig,
        error_text: String,
    ) -> Result<bool> {
        let (validation_retries, last_turn_id) = {
            let mut agents = self.agents.write().await;
            let Some(agent) = agents.get_mut(&agent_id) else {
                return Ok(true);
            };
            agent.last_validation_error = Some(error_text.clone());
            agent.checkpoint.last_validation_error = Some(error_text.clone());
            agent.checkpoint.validation_retries = agent.validation_retries;
            agent.updated_at = now_ts();
            let _ = self
                .agent_progress_tx
                .send(progress_validation_state(agent, "validation/failed"));
            (agent.validation_retries, agent.active_turn_id.clone())
        };

        if validation.fail_action == "warn" {
            warn!(agent_id = %agent_id, error = %error_text, "validation failed; continuing due to warn mode");
            return Ok(false);
        }

        if validation.fail_action == "retry" && validation_retries < validation.max_retries {
            let retry_prompt = format!("Validation failed: {error_text}\nFix and commit.");
            self.send_turn(
                agent_id,
                SendAgentInput {
                    prompt: Some(retry_prompt),
                    callback_profile: None,
                    callback_prompt_prefix: None,
                    callback_args: None,
                    callback_vars: None,
                    callback_events: None,
                },
            )
            .await
            .context("failed to start retry turn after validation failure")?;

            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&agent_id) {
                agent.validation_retries = agent.validation_retries.saturating_add(1);
                agent.checkpoint.validation_retries = agent.validation_retries;
                agent.updated_at = now_ts();
            }
            return Ok(true);
        }

        let exceeded =
            validation.fail_action == "retry" && validation_retries >= validation.max_retries;
        let message = if exceeded {
            format!(
                "{error_text} (retry budget exhausted: {validation_retries}/{} retries)",
                validation.max_retries
            )
        } else {
            error_text
        };

        let cleanup = {
            let mut agents = self.agents.write().await;
            let mut cleanup: Option<(String, Option<String>, Option<Uuid>)> = None;
            if let Some(agent) = agents.get_mut(&agent_id) {
                cleanup = agent
                    .worktree_path
                    .clone()
                    .map(|path| (path, agent.branch_name.clone(), agent.project_id));
                agent.worktree_path = None;
                agent.status = constants::AGENT_STATUS_FAILED.to_string();
                agent.error = Some(message.clone());
                update_failure_report(
                    &mut agent.failure_report,
                    FailureClass::ValidationFailed,
                    "validation failed".to_string(),
                    Some(message.clone()),
                    false,
                    now_ts(),
                );
                if let Some(result) = agent.result.as_mut() {
                    result.status = constants::AGENT_STATUS_FAILED.to_string();
                    result.error = Some(message.clone());
                    result.turn_id = result.turn_id.clone().or(last_turn_id.clone());
                }
                agent.updated_at = now_ts();
            }
            cleanup
        };

        if let Some((worktree_path, branch_name, Some(project_id))) = cleanup {
            let project_path = {
                let projects = self.projects.read().await;
                projects
                    .get(&project_id)
                    .map(|project| project.path.clone())
                    .unwrap_or_default()
            };
            if !project_path.is_empty() {
                let _ = self
                    .cleanup_worker_worktree(
                        project_path.as_str(),
                        worktree_path.as_str(),
                        branch_name.as_deref(),
                        false,
                    )
                    .await;
            }
        }
        Ok(true)
    }

    async fn execute_event(self: &Arc<Self>, context: CallbackEventContext) -> Result<()> {
        self.execute_agent_callback(context.clone()).await?;

        if let AgentRole::Worker = context.role
            && let Some(project_id) = context.project_id
        {
            self.execute_project_worker_callback(project_id, context)
                .await?;
        }
        Ok(())
    }

    async fn dispatch_tool_filtered_callbacks(
        self: &Arc<Self>,
        context: ToolFilterDispatchContext,
    ) -> Result<()> {
        let tool = normalize_tool_name(constants::EVENT_METHOD_ITEM_COMPLETED, &context.params)
            .context("missing tool name for command execution event")?;
        let command = parse_command(&context.params);
        let filters = {
            let projects = self.projects.read().await;
            projects
                .get(&context.project_id)
                .map(|project| project.config.callbacks.filters.clone())
                .unwrap_or_default()
        };
        if filters.is_empty() {
            return Ok(());
        }

        let now_ms = now_ms();
        let mut debounce_map = self.callback_filter_debounce.write().await;
        for filter in filters {
            if !should_fire_tool_filter(
                &mut debounce_map,
                context.agent_id,
                tool.as_str(),
                command.as_deref(),
                &filter,
                now_ms,
            ) {
                continue;
            }

            let payload = ToolFilterCallbackPayload {
                worker_id: context.agent_id,
                job_id: context.job_id,
                project_id: context.project_id,
                filter_name: filter.name.clone(),
                tool: tool.clone(),
                command: command.clone(),
                exit_code: parse_exit_code(&context.params),
                output_tail: parse_output_tail(&context.params, 20),
            };

            let _profile = self
                .resolve_callback_profile(
                    filter.callback_profile.as_str(),
                    Some(context.project_id),
                )
                .await
                .with_context(|| {
                    format!(
                        "tool-filter callback profile '{}' is not configured",
                        filter.callback_profile
                    )
                })?;

            let callback = WorkerCallback::Profile(WorkerProfileCallback {
                profile: filter.callback_profile.clone(),
                prompt_prefix: None,
                command_args: None,
                events: None,
                vars: HashMap::new(),
            });

            let callback_context = CallbackEventContext {
                agent_id: context.agent_id,
                thread_id: String::new(),
                turn_id: parse_turn_id(&context.params),
                role: AgentRole::Worker,
                project_id: Some(context.project_id),
                foreman_id: None,
                job_id: context.job_id,
                method: constants::EVENT_METHOD_ITEM_COMPLETED.to_string(),
                ts: now_ts(),
                params: serde_json::to_value(payload).context("failed to encode filter payload")?,
                event_id: Uuid::new_v4(),
                result_snapshot: None,
                callback: callback.clone(),
            };

            self.dispatch_callback(callback_context, callback, true)
                .await?;
        }

        Ok(())
    }

    async fn update_job_after_worker_event(
        self: &Arc<Self>,
        job_id: Uuid,
        result_snapshot: Option<&AgentResult>,
    ) -> Result<Option<String>> {
        self.launch_runnable_workers(job_id).await?;
        let workers = {
            let jobs = self.jobs.read().await;
            jobs.get(&job_id)
                .context("job not found")?
                .worker_ids
                .clone()
        };

        if workers.is_empty() {
            let mut jobs = self.jobs.write().await;
            if let Some(job) = jobs.get_mut(&job_id) {
                let prior_status = job.status.clone();
                job.status = constants::JOB_STATUS_COMPLETED.to_string();
                job.completed_at = Some(job.completed_at.unwrap_or_else(now_ts));
                job.updated_at = now_ts();
                if !is_job_terminal_status(&prior_status) && is_job_terminal_status(&job.status) {
                    return Ok(Some(job.status.clone()));
                }
            }
            return Ok(None);
        }

        let worker_results = {
            let agents = self.agents.read().await;
            let mut worker_results = Vec::with_capacity(workers.len());
            for worker_id in workers.iter().copied() {
                let snapshot_match = result_snapshot
                    .filter(|snapshot| snapshot.agent_id == worker_id)
                    .cloned();
                let result = if let Some(snapshot) = snapshot_match {
                    Some(snapshot)
                } else {
                    agents
                        .get(&worker_id)
                        .and_then(|agent| agent.result.clone())
                };
                worker_results.push(result);
            }
            worker_results
        };

        let mut has_running = false;
        let mut has_failure = false;
        let mut has_pending = false;
        for result in worker_results.iter() {
            match result {
                Some(result) => {
                    if matches!(
                        result.status.as_str(),
                        constants::AGENT_STATUS_RUNNING | constants::AGENT_STATUS_INTERRUPTED
                    ) {
                        has_running = true;
                    }
                    if is_worker_failure(result) {
                        has_failure = true;
                    }
                    if result.completed_at.is_none() {
                        has_pending = true;
                    }
                }
                None => has_pending = true,
            }
        }

        let has_running = has_running || has_pending;

        {
            let mut jobs = self.jobs.write().await;
            if let Some(job) = jobs.get_mut(&job_id) {
                let prior_status = job.status.clone();
                if has_running {
                    job.status = constants::JOB_STATUS_RUNNING.to_string();
                } else {
                    job.status = if has_failure {
                        constants::JOB_STATUS_FAILED.to_string()
                    } else {
                        constants::JOB_STATUS_COMPLETED.to_string()
                    };
                    job.completed_at = Some(job.completed_at.unwrap_or_else(now_ts));
                }
                job.updated_at = now_ts();
                if !is_job_terminal_status(&prior_status) && is_job_terminal_status(&job.status) {
                    return Ok(Some(job.status.clone()));
                }
            }
        }
        Ok(None)
    }

    async fn update_project_after_worker_event(&self, project_id: Uuid) -> Result<()> {
        let mut projects = self.projects.write().await;
        let project = projects.get_mut(&project_id).context("project not found")?;
        project.updated_at = now_ts();
        Ok(())
    }

    async fn execute_agent_callback(self: &Arc<Self>, context: CallbackEventContext) -> Result<()> {
        let callback = context.callback.clone();
        self.dispatch_callback(context, callback, false).await
    }

    async fn dispatch_callback(
        &self,
        context: CallbackEventContext,
        callback: WorkerCallback,
        skip_event_filter: bool,
    ) -> Result<()> {
        match callback {
            WorkerCallback::None => Ok(()),
            WorkerCallback::Webhook(webhook) => {
                if !skip_event_filter
                    && !events_allowed(context.method.as_str(), webhook.events.as_deref(), None)
                {
                    return Ok(());
                }
                self.dispatch_webhook_callback(&context, webhook).await
            }
            WorkerCallback::UnixSocket(unix_socket) => {
                if !skip_event_filter
                    && !events_allowed(context.method.as_str(), unix_socket.events.as_deref(), None)
                {
                    return Ok(());
                }
                self.dispatch_unix_socket_callback(&context, unix_socket)
                    .await
            }
            WorkerCallback::Profile(profile) => {
                let profile_definition = self
                    .resolve_callback_profile(&profile.profile, context.project_id)
                    .await
                    .with_context(|| {
                        format!("callback profile '{}' is not configured", profile.profile)
                    })?;

                let profile_events = match &profile_definition {
                    CallbackProfile::Webhook(webhook) => webhook.events.as_deref(),
                    CallbackProfile::Command(command) => command.events.as_deref(),
                    CallbackProfile::UnixSocket(unix_socket) => unix_socket.events.as_deref(),
                };

                if !skip_event_filter
                    && !events_allowed(
                        context.method.as_str(),
                        profile.events.as_deref(),
                        profile_events,
                    )
                {
                    return Ok(());
                }

                match profile_definition {
                    CallbackProfile::Command(command) => {
                        self.dispatch_command_callback(context, profile, command)
                            .await
                    }
                    CallbackProfile::Webhook(webhook) => {
                        self.dispatch_webhook_callback_from_profile(&context, &profile, &webhook)
                            .await
                    }
                    CallbackProfile::UnixSocket(unix_socket) => {
                        self.dispatch_unix_socket_callback_from_profile(
                            &context,
                            &profile,
                            &unix_socket,
                        )
                        .await
                    }
                }
            }
        }
    }

    async fn resolve_callback_profile(
        &self,
        profile_name: &str,
        project_id: Option<Uuid>,
    ) -> Option<CallbackProfile> {
        if let Some(project_id) = project_id {
            let local_profile = {
                let projects = self.projects.read().await;
                projects
                    .get(&project_id)
                    .and_then(|project| project.config.callbacks.profiles.get(profile_name))
                    .cloned()
            };

            if local_profile.is_some() {
                return local_profile;
            }
        }

        self.config.get_callback_profile(profile_name).cloned()
    }

    async fn execute_project_worker_callback(
        self: &Arc<Self>,
        project_id: Uuid,
        context: CallbackEventContext,
    ) -> Result<()> {
        let project = {
            let projects = self.projects.read().await;
            projects.get(&project_id).cloned()
        };

        if let Some(project) = project {
            let should_compact = matches!(
                context.method.as_str(),
                constants::EVENT_METHOD_TURN_COMPLETED | constants::EVENT_METHOD_TURN_ABORTED
            );

            if should_compact {
                let mut compact = false;
                {
                    let mut projects = self.projects.write().await;
                    if let Some(project_record) = projects.get_mut(&project_id) {
                        let (completed_turns, should_compact) = update_completed_turn_counter(
                            project_record.completed_worker_turns,
                            project_record.config.policy.compact_after_turns,
                        );
                        project_record.completed_worker_turns = completed_turns;
                        compact = should_compact;
                    }
                }

                if compact {
                    let _ = self
                        .compact_project(
                            project_id,
                            CompactProjectRequest {
                                prompt: None,
                                reason: Some(constants::PROJECT_COMPACT_SUMMARY_PREFIX.to_string()),
                            },
                        )
                        .await;
                }
            }

            let (lifecycle_key, lifecycle_event) = match context.method.as_str() {
                constants::EVENT_METHOD_TURN_COMPLETED => (
                    constants::PROJECT_LIFECYCLE_WORKER_COMPLETED,
                    constants::PROJECT_LIFECYCLE_EVENT_WORKER_COMPLETED,
                ),
                constants::EVENT_METHOD_TURN_ABORTED => (
                    constants::PROJECT_LIFECYCLE_WORKER_ABORTED,
                    constants::PROJECT_LIFECYCLE_EVENT_WORKER_ABORTED,
                ),
                _ => {
                    return Ok(());
                }
            };

            let mut callback_context = context.clone();
            callback_context.method = lifecycle_event.to_string();
            self.dispatch_project_lifecycle_callback(
                &project,
                lifecycle_key,
                &CallbackOverrides::default(),
                callback_context,
            )
            .await?;

            let bubble_events = project.config.policy.bubble_up_events.as_deref();

            if let Ok(callback) =
                self.resolve_project_bubble_callback(&project, &CallbackOverrides::default())
                && events_allowed(
                    context.method.as_str(),
                    project
                        .config
                        .callbacks
                        .bubble_up
                        .callback_events
                        .as_deref(),
                    bubble_events,
                )
            {
                self.dispatch_project_bubble_callback(&context, callback, &project)
                    .await
                    .context("project bubble callback failed")?;
            }
        }

        Ok(())
    }

    async fn dispatch_project_lifecycle_callback(
        self: &Arc<Self>,
        project: &ProjectRecord,
        lifecycle_key: &str,
        callback_overrides: &CallbackOverrides,
        mut context: CallbackEventContext,
    ) -> Result<()> {
        let callback = self.resolve_project_lifecycle_callback_spec(
            project,
            lifecycle_key,
            callback_overrides,
        )?;
        context.callback = callback;

        if matches!(context.callback, WorkerCallback::None) {
            return self
                .set_project_lifecycle_callback_status(
                    project.id,
                    lifecycle_key,
                    constants::PROJECT_CALLBACK_STATUS_DISABLED,
                )
                .await;
        }

        match self
            .dispatch_callback(context.clone(), context.callback.clone(), true)
            .await
        {
            Ok(()) => {
                self.set_project_lifecycle_callback_status(
                    project.id,
                    lifecycle_key,
                    constants::PROJECT_CALLBACK_STATUS_SUCCESS,
                )
                .await
            }
            Err(err) => {
                let _ = self
                    .set_project_lifecycle_callback_status(
                        project.id,
                        lifecycle_key,
                        constants::PROJECT_CALLBACK_STATUS_FAILED,
                    )
                    .await;
                Err(err)
            }
        }
    }

    async fn set_project_lifecycle_callback_status(
        self: &Arc<Self>,
        project_id: Uuid,
        lifecycle_key: &str,
        status: &str,
    ) -> Result<()> {
        let mut projects = self.projects.write().await;
        let project = projects.get_mut(&project_id).context("project not found")?;
        project
            .lifecycle_callback_status
            .insert(lifecycle_key.to_string(), status.to_string());
        project.updated_at = now_ts();
        self.schedule_state_persist();

        Ok(())
    }

    async fn dispatch_project_bubble_callback(
        &self,
        context: &CallbackEventContext,
        callback: WorkerCallback,
        project: &ProjectRecord,
    ) -> Result<()> {
        match callback {
            WorkerCallback::None => Ok(()),
            WorkerCallback::Webhook(webhook) => {
                let vars = self
                    .project_callback_vars(context, project)
                    .context("failed to build bubble callback vars")?;

                let payload = self.build_base_payload(context, Some(vars));

                let request = {
                    let mut req = self.http_client.post(&webhook.url).json(&payload);
                    if let Some(token) = webhook.secret {
                        req = req.header(constants::CALLBACK_SECRET_HEADER, token);
                    }
                    req
                };

                let response = await_callback_future(
                    "project bubble callback request",
                    webhook.timeout_ms,
                    request.send(),
                )
                .await
                .with_context(|| {
                    format!(
                        "project bubble callback to '{}' failed for project {}",
                        webhook.url, project.id
                    )
                })?;
                response.error_for_status().with_context(|| {
                    format!(
                        "project bubble callback to '{}' returned non-2xx for project {}",
                        webhook.url, project.id
                    )
                })?;
                Ok(())
            }
            WorkerCallback::UnixSocket(unix_socket) => {
                let vars = self
                    .project_callback_vars(context, project)
                    .context("failed to build bubble callback vars")?;

                let payload = self.build_base_payload(context, Some(vars));
                self.dispatch_unix_payload(&payload, &unix_socket.socket, unix_socket.timeout_ms)
                    .await?;
                Ok(())
            }
            WorkerCallback::Profile(invocation) => {
                let profile = self
                    .config
                    .get_callback_profile(&invocation.profile)
                    .with_context(|| {
                        format!(
                            "project bubble callback profile '{}' is not configured",
                            invocation.profile
                        )
                    })?;

                match profile {
                    CallbackProfile::Command(profile) => {
                        self.dispatch_command_callback(
                            context.clone(),
                            invocation.clone(),
                            profile.clone(),
                        )
                        .await
                    }
                    CallbackProfile::Webhook(profile) => {
                        let vars = self
                            .project_callback_vars(context, project)
                            .context("failed to build bubble callback vars")?;
                        let payload = self.build_base_payload(context, Some(vars));
                        let secret = profile
                            .secret_env
                            .as_ref()
                            .and_then(|name| std::env::var(name).ok());

                        let request = {
                            let mut req = self.http_client.post(&profile.url).json(&payload);
                            if let Some(token) = secret {
                                req = req.header(constants::CALLBACK_SECRET_HEADER, token);
                            }
                            req
                        };

                        let response = await_callback_future(
                            "project bubble callback request",
                            profile.timeout_ms,
                            request.send(),
                        )
                        .await
                        .with_context(|| {
                            format!("project bubble callback to '{}' failed", profile.url)
                        })?;
                        response.error_for_status().with_context(|| {
                            format!(
                                "project bubble callback to '{}' returned non-2xx",
                                profile.url
                            )
                        })?;
                        Ok(())
                    }
                    CallbackProfile::UnixSocket(profile) => {
                        let vars = self
                            .project_callback_vars(context, project)
                            .context("failed to build bubble callback vars")?;
                        let payload = self.build_base_payload(context, Some(vars));
                        self.dispatch_unix_payload(&payload, &profile.socket, profile.timeout_ms)
                            .await?;
                        Ok(())
                    }
                }
            }
        }
    }

    async fn dispatch_webhook_callback(
        &self,
        context: &CallbackEventContext,
        webhook: WorkerWebhookCallback,
    ) -> Result<()> {
        let vars = webhook
            .vars
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<HashMap<_, _>>();
        let payload = self.build_base_payload(context, Some(vars));

        let request = {
            let mut req = self.http_client.post(&webhook.url).json(&payload);
            if let Some(token) = webhook.secret {
                req = req.header(constants::CALLBACK_SECRET_HEADER, token);
            }
            req
        };

        let response = await_callback_future(
            "webhook callback request",
            webhook.timeout_ms,
            request.send(),
        )
        .await
        .with_context(|| format!("webhook callback to '{}' failed", webhook.url))?;
        response
            .error_for_status()
            .with_context(|| format!("webhook callback to '{}' returned non-2xx", webhook.url))?;
        Ok(())
    }

    async fn dispatch_webhook_callback_from_profile(
        &self,
        context: &CallbackEventContext,
        invocation: &WorkerProfileCallback,
        profile: &WebhookCallbackProfile,
    ) -> Result<()> {
        let secret = profile
            .secret_env
            .as_ref()
            .and_then(|name| std::env::var(name).ok());

        let vars = self
            .callback_vars(context, invocation.vars.clone())
            .context("failed to build callback vars")?;
        let webhook = WorkerWebhookCallback {
            url: profile.url.clone(),
            secret,
            vars,
            events: profile.events.clone(),
            timeout_ms: profile.timeout_ms,
        };

        self.dispatch_webhook_callback(context, webhook).await
    }

    async fn dispatch_unix_socket_callback(
        &self,
        context: &CallbackEventContext,
        unix_socket: WorkerUnixSocketCallback,
    ) -> Result<()> {
        let vars = unix_socket
            .vars
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<HashMap<_, _>>();
        let payload = self.build_base_payload(context, Some(vars));
        self.dispatch_unix_payload(&payload, &unix_socket.socket, unix_socket.timeout_ms)
            .await
    }

    async fn dispatch_unix_socket_callback_from_profile(
        &self,
        context: &CallbackEventContext,
        invocation: &WorkerProfileCallback,
        profile: &UnixSocketCallbackProfile,
    ) -> Result<()> {
        let vars = self
            .callback_vars(context, invocation.vars.clone())
            .context("failed to build callback vars")?;
        let callback = WorkerUnixSocketCallback {
            socket: profile.socket.clone(),
            events: profile.events.clone(),
            timeout_ms: profile.timeout_ms,
            vars,
        };
        self.dispatch_unix_socket_callback(context, callback).await
    }

    #[cfg(unix)]
    async fn dispatch_unix_payload(
        &self,
        payload: &Value,
        socket: &Path,
        timeout_ms: Option<u64>,
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        use tokio::net::UnixStream;

        let connect = UnixStream::connect(socket);
        let mut stream = match callback_timeout_ms(timeout_ms) {
            Some(ms) => match time::timeout(Duration::from_millis(ms), connect).await {
                Ok(result) => match result {
                    Ok(stream) => stream,
                    Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => {
                        warn!(socket = %socket.display(), "unix callback connection refused");
                        return Ok(());
                    }
                    Err(err) => return Err(err).context("unix callback connect failed"),
                },
                Err(_) => {
                    warn!(socket = %socket.display(), "unix callback connect timed out");
                    return Ok(());
                }
            },
            None => connect.await.context("unix callback connect failed")?,
        };

        let mut line = json_compact_value(payload);
        line.push('\n');
        let write = stream.write_all(line.as_bytes());
        match callback_timeout_ms(timeout_ms) {
            Some(ms) => match time::timeout(Duration::from_millis(ms), write).await {
                Ok(result) => result.context("unix callback write failed")?,
                Err(_) => {
                    warn!(socket = %socket.display(), "unix callback write timed out");
                    return Ok(());
                }
            },
            None => write.await.context("unix callback write failed")?,
        };
        Ok(())
    }

    #[cfg(not(unix))]
    async fn dispatch_unix_payload(
        &self,
        _payload: &Value,
        _socket: &Path,
        _timeout_ms: Option<u64>,
    ) -> Result<()> {
        Err(anyhow!(
            "unix socket callbacks are unsupported on this platform"
        ))
    }

    async fn dispatch_command_callback(
        &self,
        context: CallbackEventContext,
        invocation: WorkerProfileCallback,
        profile: CommandCallbackProfile,
    ) -> Result<()> {
        let mut vars = self
            .callback_vars(&context, invocation.vars.clone())
            .context("failed to build callback vars")?;

        let payload = self.build_base_payload(&context, Some(vars.clone()));
        let compact = json_compact_value(&payload);
        let pretty = json_pretty_value(&payload);
        let base_prefix = profile.prompt_prefix.unwrap_or_default();
        let prefix = invocation.prompt_prefix.unwrap_or(base_prefix);
        let event_prompt_variable = profile
            .event_prompt_variable
            .unwrap_or_else(|| constants::CALLBACK_PROMPT_DEFAULT.to_string());
        let event_prompt = if prefix.trim().is_empty() {
            compact.clone()
        } else {
            format!("{}\n\n{}", prefix, compact)
        };

        vars.insert(
            constants::CALLBACK_EVENT_JSON_KEY.to_string(),
            compact.clone(),
        );
        vars.insert(constants::CALLBACK_EVENT_PRETTY_KEY.to_string(), pretty);
        vars.insert(
            constants::CALLBACK_EVENT_PAYLOAD_KEY.to_string(),
            compact.clone(),
        );
        vars.insert(
            constants::CALLBACK_VAR_EVENT_PAYLOAD.to_string(),
            compact.clone(),
        );
        vars.insert(
            constants::CALLBACK_EVENT_PROMPT_KEY.to_string(),
            event_prompt.clone(),
        );
        vars.insert(event_prompt_variable.to_string(), event_prompt);

        let rendered_command = render_template_strict(&profile.program, &vars)
            .context("failed to render callback command program")?;
        let args = if let Some(custom_args) = invocation.command_args {
            custom_args
        } else {
            profile.args.clone()
        };
        let rendered_args = args
            .iter()
            .map(|arg| {
                render_template_strict(arg, &vars)
                    .with_context(|| format!("failed to render callback command arg '{arg}'"))
            })
            .collect::<Vec<_>>();
        let rendered_args = rendered_args
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .context("failed to render callback command args")?;

        let project_callback_env = if let Some(project_id) = context.project_id {
            self.projects
                .read()
                .await
                .get(&project_id)
                .map(|project| project.config.callbacks.env.clone())
                .unwrap_or_default()
        } else {
            HashMap::new()
        };
        let use_shell =
            profile.shell.unwrap_or(false) || !Path::new(&rendered_command).is_absolute();

        // PATH behavior:
        // - shell=true (or a non-absolute program) runs through `bash -lc`, loading login shell
        //   profile state so PATH-managed tools (e.g. nvm shims) are available.
        // - shell=false with an absolute program uses direct execution.
        // - callbacks.env can provide extra environment variables without shell mode.
        let mut command = if use_shell {
            let shell_command = if rendered_args.is_empty() {
                rendered_command.clone()
            } else {
                format!("{} {}", rendered_command, rendered_args.join(" "))
            };
            let mut command = Command::new("bash");
            command.args(["-lc", &shell_command]);
            command
        } else {
            let mut command = Command::new(&rendered_command);
            command.args(&rendered_args);
            command
        };

        let mut merged_env = project_callback_env;
        for (key, value) in profile.env.iter() {
            let rendered_value = render_template_strict(value, &vars)
                .with_context(|| format!("failed to render callback env value for '{key}'"))?;
            merged_env.insert(key.clone(), rendered_value);
        }

        for (key, value) in merged_env {
            if std::env::var_os(&key).is_none() {
                command.env(key, value);
            }
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to run callback command '{}'", rendered_command))?;

        let status = if let Some(timeout_ms) = callback_timeout_ms(profile.timeout_ms) {
            match time::timeout(Duration::from_millis(timeout_ms), child.wait()).await {
                Ok(wait) => wait?,
                Err(_) => {
                    let _ = child.kill().await;
                    return Err(anyhow!("callback command timed out"));
                }
            }
        } else {
            child.wait().await?
        };

        if !status.success() {
            warn!(
                command = %rendered_command,
                status = status.code().unwrap_or_default(),
                "callback command exited with non-zero status"
            );
        }

        debug!(command = %profile.program, event_id = %context.event_id, "callback command executed");
        Ok(())
    }

    fn build_base_payload(
        &self,
        context: &CallbackEventContext,
        extra_vars: Option<HashMap<String, String>>,
    ) -> Value {
        let mut payload = serde_json::json!({
            constants::CALLBACK_VAR_AGENT_ID: context.agent_id.to_string(),
            constants::CALLBACK_VAR_THREAD_ID: context.thread_id,
            constants::CALLBACK_VAR_TURN_ID: context.turn_id,
            constants::CALLBACK_VAR_EVENT_ID: context.event_id.to_string(),
            constants::CALLBACK_VAR_TS: context.ts,
            constants::CALLBACK_VAR_METHOD: context.method,
            "params": context.params,
            constants::CALLBACK_VAR_ROLE: context.role.as_str(),
        });

        if let Some(project_id) = context.project_id {
            payload[constants::PROJECT_VAR_ID] = serde_json::Value::String(project_id.to_string());
            payload[constants::CALLBACK_VAR_PROJECT_ID] =
                serde_json::Value::String(project_id.to_string());
        }

        if let Some(foreman_id) = context.foreman_id {
            payload[constants::CALLBACK_VAR_FOREMAN_ID] =
                serde_json::Value::String(foreman_id.to_string());
        }

        if let Some(result) = &context.result_snapshot {
            payload[constants::CALLBACK_VAR_RESULT] = serde_json::to_value(result)
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
        }

        if let Some(vars) = extra_vars {
            payload[constants::CALLBACK_VAR_CALLBACK_VARS] = serde_json::to_value(vars)
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
        }

        payload
    }

    fn callback_vars(
        &self,
        context: &CallbackEventContext,
        mut inherited: HashMap<String, String>,
    ) -> Result<HashMap<String, String>> {
        inherited.insert(
            constants::CALLBACK_VAR_AGENT_ID.to_string(),
            context.agent_id.to_string(),
        );
        inherited.insert(
            constants::CALLBACK_VAR_WORKER_ID.to_string(),
            context.agent_id.to_string(),
        );
        inherited.insert(
            constants::CALLBACK_VAR_THREAD_ID.to_string(),
            context.thread_id.clone(),
        );
        inherited.insert(
            constants::CALLBACK_VAR_METHOD.to_string(),
            context.method.clone(),
        );
        inherited.insert(
            constants::CALLBACK_VAR_EVENT_ID.to_string(),
            context.event_id.to_string(),
        );
        inherited.insert(
            constants::CALLBACK_VAR_ROLE.to_string(),
            context.role.as_str().to_string(),
        );

        if let Some(turn_id) = context.turn_id.clone() {
            inherited.insert(constants::CALLBACK_VAR_TURN_ID.to_string(), turn_id);
        }

        if let Some(project_id) = context.project_id {
            inherited.insert(
                constants::PROJECT_VAR_ID.to_string(),
                project_id.to_string(),
            );
            inherited.insert(
                constants::CALLBACK_VAR_PROJECT_ID.to_string(),
                project_id.to_string(),
            );
        }

        if let Some(foreman_id) = context.foreman_id {
            inherited.insert(
                constants::CALLBACK_VAR_FOREMAN_ID.to_string(),
                foreman_id.to_string(),
            );
        }

        inherited.insert(
            constants::CALLBACK_VAR_TS.to_string(),
            context.ts.to_string(),
        );
        inherited.insert(
            constants::CALLBACK_EVENT_TYPE_KEY.to_string(),
            constants::CALLBACK_EVENT_TYPE_AGENT_EVENT.to_string(),
        );

        Ok(inherited)
    }

    fn project_event_vars(
        &self,
        project: &ProjectRecord,
        context: &CallbackEventContext,
    ) -> Result<HashMap<String, String>> {
        let mut vars = self
            .callback_vars(context, HashMap::new())
            .context("failed to build base callback vars")?;

        vars.insert(
            constants::PROJECT_VAR_ID.to_string(),
            project.id.to_string(),
        );
        vars.insert(
            constants::PROJECT_VAR_PATH.to_string(),
            project.path.clone(),
        );
        vars.insert(
            constants::PROJECT_VAR_NAME.to_string(),
            project.name.clone(),
        );
        vars.insert(
            constants::PROJECT_VAR_STATUS.to_string(),
            project.status.clone(),
        );
        if let Some(foreman_id) = project.foreman_agent_id {
            vars.insert(
                constants::PROJECT_VAR_FOREMAN_ID.to_string(),
                foreman_id.to_string(),
            );
        }
        Ok(vars)
    }

    fn project_callback_vars(
        &self,
        context: &CallbackEventContext,
        project: &ProjectRecord,
    ) -> Result<HashMap<String, String>> {
        self.project_event_vars(project, context)
    }

    pub async fn create_project(
        self: &Arc<Self>,
        request: SpawnProjectRequest,
    ) -> Result<CreateProjectResponse> {
        let path = PathBuf::from(&request.path);
        let path = if path.is_absolute() {
            path
        } else if let Some(cwd) = request.cwd.as_ref() {
            PathBuf::from(cwd).join(path)
        } else {
            path
        };

        if !path.exists() {
            return Err(anyhow!("project path does not exist"));
        }

        let config = ProjectConfig::load(&path)?;
        config.validate()?;

        // Validate project callback configuration in advance.
        let _ = self
            .resolve_project_callback_spec(
                &config.callbacks.foreman,
                &config.callbacks.profiles,
                &request.callback_overrides,
                false,
            )
            .context("invalid foreman callback config")?;
        let _ = self
            .resolve_project_callback_spec(
                &config.callbacks.worker,
                &config.callbacks.profiles,
                &CallbackOverrides::default(),
                false,
            )
            .context("invalid worker callback config")?;
        let _ = self
            .resolve_project_callback_spec(
                &config.callbacks.bubble_up,
                &config.callbacks.profiles,
                &CallbackOverrides::default(),
                false,
            )
            .context("invalid bubble-up callback config")?;

        let runtime = config.load_runtime_files(&path)?;
        let project_id = Uuid::new_v4();

        let initial_prompt = if let Some(prompt) = request.start_prompt.as_ref() {
            format!(
                "{}\n\n{}{}{}\n\nSTARTUP\n{}\n",
                runtime.foreman_prompt,
                constants::PROJECT_HOMEDOWN_TEXT,
                constants::PROJECT_RUNBOOK_LABEL,
                runtime.runbook,
                prompt
            )
        } else {
            format!(
                "{}\n\n{}{}{}\n",
                runtime.foreman_prompt,
                constants::PROJECT_HOMEDOWN_TEXT,
                constants::PROJECT_RUNBOOK_LABEL,
                runtime.runbook
            )
        };

        let callback = self
            .resolve_project_callback_spec(
                &config.callbacks.foreman,
                &config.callbacks.profiles,
                &request.callback_overrides,
                false,
            )
            .context("failed to resolve foreman callback")?;

        let spawn_request = SpawnAgentRequest {
            prompt: initial_prompt,
            model: request.model,
            model_provider: request.model_provider,
            cwd: Some(path.to_string_lossy().to_string()),
            sandbox: request.sandbox,
            callback_profile: None,
            callback_prompt_prefix: None,
            callback_args: None,
            callback_vars: None,
            callback_events: None,
        };

        let response = self
            .spawn_agent_record(
                None,
                spawn_request,
                AgentRole::Foreman,
                Some(project_id),
                None,
                None,
                callback,
            )
            .await
            .context("failed to spawn project foreman")?;

        let name = config.name.clone().unwrap_or_else(|| {
            path.file_name().map_or_else(
                || constants::DEFAULT_PROJECT_NAME.to_string(),
                |name| name.to_string_lossy().to_string(),
            )
        });

        let ts = now_ts();
        let project = ProjectRecord {
            id: project_id,
            path: path.to_string_lossy().to_string(),
            name,
            status: constants::PROJECT_STATUS_READY.to_string(),
            foreman_agent_id: Some(response.id),
            worker_ids: Vec::new(),
            completed_worker_turns: 0,
            config,
            runtime,
            lifecycle_callback_status: default_project_lifecycle_callback_status(),
            created_at: ts,
            updated_at: ts,
        };

        {
            let mut projects = self.projects.write().await;
            projects.insert(project_id, project);
        }
        self.schedule_state_persist();

        let project = {
            let projects = self.projects.read().await;
            projects
                .get(&project_id)
                .cloned()
                .context("project vanished")?
        };

        let start_context = CallbackEventContext {
            agent_id: response.id,
            thread_id: String::new(),
            turn_id: None,
            role: AgentRole::Foreman,
            project_id: Some(project_id),
            foreman_id: Some(response.id),
            job_id: None,
            method: constants::PROJECT_LIFECYCLE_EVENT_START.to_string(),
            ts: now_ts(),
            params: serde_json::json!({
                constants::PROJECT_VAR_ID: project_id,
                constants::PROJECT_VAR_NAME: project.name,
            }),
            event_id: Uuid::new_v4(),
            result_snapshot: None,
            callback: WorkerCallback::None,
        };
        let _ = self
            .dispatch_project_lifecycle_callback(
                &project,
                constants::PROJECT_LIFECYCLE_START,
                &request.callback_overrides,
                start_context,
            )
            .await;

        Ok(CreateProjectResponse {
            project_id,
            path: path.to_string_lossy().to_string(),
            foreman_agent_id: Some(response.id),
            status: response.status,
        })
    }

    pub async fn list_projects(&self) -> Vec<ProjectState> {
        let projects = self.projects.read().await;
        projects
            .values()
            .map(|project| ProjectState {
                id: project.id,
                path: project.path.clone(),
                name: project.name.clone(),
                status: project.status.clone(),
                foreman_agent_id: project.foreman_agent_id,
                worker_ids: project.worker_ids.clone(),
                worker_count: project.worker_ids.len(),
                created_at: project.created_at,
                updated_at: project.updated_at,
            })
            .collect()
    }

    pub async fn get_project(&self, project_id: Uuid) -> Result<ProjectState> {
        let project = self
            .projects
            .read()
            .await
            .get(&project_id)
            .context("project not found")?
            .clone();

        Ok(ProjectState {
            id: project.id,
            path: project.path,
            name: project.name,
            status: project.status,
            foreman_agent_id: project.foreman_agent_id,
            worker_count: project.worker_ids.len(),
            worker_ids: project.worker_ids,
            created_at: project.created_at,
            updated_at: project.updated_at,
        })
    }

    pub async fn get_project_callback_status(
        &self,
        project_id: Uuid,
    ) -> Result<ProjectCallbackStatusResponse> {
        let project = self
            .projects
            .read()
            .await
            .get(&project_id)
            .context("project not found")?
            .clone();

        let mut callbacks = default_project_lifecycle_callback_status();
        callbacks.extend(project.lifecycle_callback_status.into_iter());

        Ok(ProjectCallbackStatusResponse {
            project_id,
            callbacks,
        })
    }

    pub async fn reload_project_config(self: &Arc<Self>, project_id: Uuid) -> Result<()> {
        let project_path = {
            let projects = self.projects.read().await;
            let project = projects.get(&project_id).context("project not found")?;
            PathBuf::from(&project.path)
        };

        let config = ProjectConfig::load(&project_path)?;
        config.validate()?;

        let _ = self
            .resolve_project_callback_spec(
                &config.callbacks.foreman,
                &config.callbacks.profiles,
                &CallbackOverrides::default(),
                false,
            )
            .context("invalid foreman callback config")?;
        let _ = self
            .resolve_project_callback_spec(
                &config.callbacks.worker,
                &config.callbacks.profiles,
                &CallbackOverrides::default(),
                false,
            )
            .context("invalid worker callback config")?;
        let _ = self
            .resolve_project_callback_spec(
                &config.callbacks.bubble_up,
                &config.callbacks.profiles,
                &CallbackOverrides::default(),
                false,
            )
            .context("invalid bubble-up callback config")?;

        {
            let mut projects = self.projects.write().await;
            let project = projects.get_mut(&project_id).context("project not found")?;
            // Safe to hot-reload: callback config (profiles/filters/lifecycle/worker), validation,
            // policy, and jobs.defaults.{min_turns,strategy}. Not reloaded: project path, runtime
            // prompts (including worker_prompt), or model choices already baked into spawned agents.
            project.config.callbacks = config.callbacks;
            project.config.validation = config.validation;
            project.config.policy = config.policy;
            project.config.jobs.defaults.min_turns = config.jobs.defaults.min_turns;
            project.config.jobs.defaults.strategy = config.jobs.defaults.strategy;
            project.updated_at = now_ts();
        }
        self.schedule_state_persist();
        Ok(())
    }

    pub async fn spawn_project_worker(
        self: &Arc<Self>,
        project_id: Uuid,
        request: SpawnProjectWorkerRequest,
    ) -> Result<SpawnProjectWorkerResponse> {
        let (project, foreman_id) = {
            let projects = self.projects.read().await;
            let project = projects.get(&project_id).context("project not found")?;
            let foreman_id = project.foreman_agent_id;

            (project.clone(), foreman_id)
        };

        let foreman_id = foreman_id.context("project has no foreman")?;

        let callback = self
            .resolve_project_callback_spec(
                &project.config.callbacks.worker,
                &project.config.callbacks.profiles,
                &request.callback_overrides,
                false,
            )
            .context("failed to resolve worker callback")?;

        let worker_prompt = format!(
            "{}\n\n{}{}\n",
            project.runtime.worker_prompt,
            constants::PROJECT_TASK_LABEL,
            request.prompt
        );
        let project_path = project.runtime.path.to_string_lossy().to_string();
        let worker_id = Uuid::new_v4();
        let mut worker_worktree: Option<(String, String)> = None;
        let worker_cwd = if request.worktree.is_some() {
            self.resolve_worker_cwd(&project_path, request.cwd, request.worktree.as_ref())
                .await?
        } else if should_use_native_worktree(&project.config.jobs.defaults, 1) {
            let worktree = self
                .create_worker_worktree(project_path.as_str(), project_id, worker_id)
                .await?;
            let cwd = worktree.0.clone();
            worker_worktree = Some(worktree);
            cwd
        } else {
            request.cwd.unwrap_or(project_path.clone())
        };

        let spawn_request = SpawnAgentRequest {
            prompt: worker_prompt,
            model: request.model,
            model_provider: request.model_provider,
            cwd: Some(worker_cwd),
            sandbox: request.sandbox,
            callback_profile: None,
            callback_prompt_prefix: None,
            callback_args: None,
            callback_vars: None,
            callback_events: None,
        };

        let response = self
            .spawn_agent_record(
                Some(worker_id),
                spawn_request,
                AgentRole::Worker,
                Some(project_id),
                Some(foreman_id),
                None,
                callback,
            )
            .await
            .context("failed to spawn project worker")?;

        if let Some((worktree_path, branch_name)) = worker_worktree {
            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&response.id) {
                agent.worktree_path = Some(worktree_path);
                agent.branch_name = Some(branch_name);
            }
        }

        {
            let mut projects = self.projects.write().await;
            if let Some(project) = projects.get_mut(&project_id) {
                project.worker_ids.push(response.id);
                project.updated_at = now_ts();
            }
        }
        self.schedule_state_persist();

        Ok(SpawnProjectWorkerResponse {
            id: response.id,
            thread_id: response.thread_id,
            turn_id: response.turn_id,
            status: response.status,
            project_id,
            foreman_id,
            role: response.role,
        })
    }

    pub async fn create_project_jobs(
        self: &Arc<Self>,
        project_id: Uuid,
        request: CreateProjectJobsRequest,
    ) -> Result<CreateProjectJobsResponse> {
        if request.workers.is_empty() {
            return Err(anyhow!("at least one worker spec is required"));
        }

        let approve_overlap = request.approve_overlap;
        let workers = request.workers;
        let plan = self
            .plan_project_jobs(
                project_id,
                PlanProjectJobsRequest {
                    workers: workers.clone(),
                    declared_scope_paths: HashMap::new(),
                },
            )
            .await?;
        if plan.blocked {
            return Err(anyhow!(
                "job blocked by overlap policy '{}' due to high-risk overlaps",
                plan.overlap_policy
            ));
        }
        if plan.requires_approval && !approve_overlap {
            return Err(anyhow!(
                "job requires overlap approval; retry with approve_overlap=true"
            ));
        }

        let (project, foreman_id) = {
            let projects = self.projects.read().await;
            let project = projects.get(&project_id).context("project not found")?;
            let foreman_id = project.foreman_agent_id;
            (project.clone(), foreman_id)
        };

        let foreman_id = foreman_id.context("project has no foreman")?;
        let ts = now_ts();
        let job_id = Uuid::new_v4();
        let worker_specs = self
            .resolve_job_worker_specs(job_id, &project, workers)
            .await?;
        let worker_count = worker_specs.len();
        let project_path = project.runtime.path.to_string_lossy().to_string();
        let merge_strategy = project.config.jobs.defaults.merge_strategy.clone();
        let base_branch = if let Some(branch) = project.config.jobs.defaults.base_branch.clone() {
            branch
        } else {
            detect_base_branch(project_path.as_str()).await?
        };
        let mut worker_ids = Vec::with_capacity(worker_count);
        let mut labels = HashMap::new();
        let mut worker_names = HashMap::new();
        let mut dependency_graph = HashMap::new();

        for spec in worker_specs {
            let spawn_request = SpawnAgentRequest {
                prompt: String::new(),
                model: spec.model.clone(),
                model_provider: spec.model_provider.clone(),
                cwd: Some(spec.cwd.clone()),
                sandbox: spec.sandbox.clone(),
                callback_profile: None,
                callback_prompt_prefix: None,
                callback_args: None,
                callback_vars: None,
                callback_events: None,
            };

            let response = self
                .spawn_agent_record(
                    Some(spec.worker_id),
                    spawn_request,
                    AgentRole::Worker,
                    Some(project_id),
                    Some(foreman_id),
                    Some(job_id),
                    spec.callback.clone(),
                )
                .await
                .context("failed to spawn project worker")?;

            {
                let mut agents = self.agents.write().await;
                if let Some(agent) = agents.get_mut(&response.id) {
                    agent.prompt = Some(spec.prompt.clone());
                    if let Some(worktree_path) = spec.worktree_path.clone() {
                        agent.worktree_path = Some(worktree_path.clone());
                        agent.checkpoint.worktree_path = Some(worktree_path);
                    }
                    if let Some(branch_name) = spec.branch_name.clone() {
                        agent.branch_name = Some(branch_name.clone());
                        agent.checkpoint.branch_name = Some(branch_name);
                    }
                    agent.updated_at = now_ts();
                }
            }

            worker_names.insert(response.id, spec.worker_name);
            dependency_graph.insert(response.id, spec.depends_on.clone());
            labels.insert(response.id, spec.labels);
            worker_ids.push(response.id);
        }

        let job_labels = aggregate_worker_labels(&labels);
        {
            let mut project_record = self.projects.write().await;
            if let Some(project_record) = project_record.get_mut(&project_id) {
                project_record.worker_ids.extend(worker_ids.iter().copied());
                project_record.updated_at = now_ts();
            }
        }

        {
            let mut jobs = self.jobs.write().await;
            jobs.insert(
                job_id,
                JobRecord {
                    id: job_id,
                    project_id: Some(project_id),
                    status: constants::JOB_STATUS_RUNNING.to_string(),
                    worker_ids: worker_ids.clone(),
                    worker_labels: labels,
                    merged_branches: Vec::new(),
                    merge_conflicts: Vec::new(),
                    worker_names,
                    dependency_graph,
                    blocked_reasons: HashMap::new(),
                    base_branch,
                    merge_strategy,
                    created_at: ts,
                    completed_at: None,
                    updated_at: ts,
                },
            );
        }

        self.launch_runnable_workers(job_id).await?;
        let _ = self.update_job_after_worker_event(job_id, None).await;
        self.schedule_state_persist();
        let status = self
            .jobs
            .read()
            .await
            .get(&job_id)
            .map(|job| job.status.clone())
            .unwrap_or_else(|| constants::JOB_STATUS_RUNNING.to_string());
        Ok(CreateProjectJobsResponse {
            job_id,
            project_id,
            status,
            worker_ids: worker_ids.clone(),
            worker_count: worker_ids.len(),
            labels: job_labels,
        })
    }

    pub async fn plan_project_jobs(
        &self,
        project_id: Uuid,
        request: PlanProjectJobsRequest,
    ) -> Result<PlanProjectJobsResponse> {
        if request.workers.is_empty() {
            return Err(anyhow!("at least one worker spec is required"));
        }

        let project = {
            let projects = self.projects.read().await;
            projects
                .get(&project_id)
                .cloned()
                .context("project not found")?
        };
        let names = assign_worker_names(&request.workers)?;
        let matrix = build_overlap_matrix(&names, &request.workers, &request.declared_scope_paths);
        let high_risk_pairs = matrix.iter().filter(|entry| entry.risk == "high").count();
        let overlap_policy = project.config.policy.overlap_policy.clone();
        let blocked = overlap_policy == "block" && high_risk_pairs > 0;
        let requires_approval = overlap_policy == "require_approve" && high_risk_pairs > 0;

        Ok(PlanProjectJobsResponse {
            project_id,
            overlap_policy,
            blocked,
            requires_approval,
            high_risk_pairs,
            matrix,
        })
    }

    async fn resolve_job_worker_specs(
        &self,
        job_id: Uuid,
        project: &ProjectRecord,
        workers: Vec<crate::models::ProjectJobWorkerSpec>,
    ) -> Result<Vec<JobWorkerSpecResolved>> {
        let names = assign_worker_names(&workers)?;
        let name_to_id = names
            .iter()
            .map(|name| (name.clone(), Uuid::new_v4()))
            .collect::<HashMap<_, _>>();
        let project_path = project.runtime.path.to_string_lossy().to_string();
        let worker_count = workers.len();
        let mut resolved = Vec::with_capacity(workers.len());

        for (index, spec) in workers.into_iter().enumerate() {
            let worker_name = names
                .get(index)
                .cloned()
                .context("missing worker name")?;
            let worker_id = *name_to_id
                .get(worker_name.as_str())
                .context("missing worker id mapping")?;
            let callback = self
                .resolve_project_callback_spec(
                    &project.config.callbacks.worker,
                    &project.config.callbacks.profiles,
                    &spec.callback_overrides,
                    false,
                )
                .context("failed to resolve worker callback")?;

            let worker_prompt = format!(
                "{}\n\n{}{}\n",
                project.runtime.worker_prompt,
                constants::PROJECT_TASK_LABEL,
                spec.prompt
            );
            let mut worker_worktree: Option<(String, String)> = None;
            let worker_cwd = if spec.worktree.is_some() {
                self.resolve_worker_cwd(&project_path, spec.cwd.clone(), spec.worktree.as_ref())
                    .await?
            } else if should_use_native_worktree(&project.config.jobs.defaults, worker_count) {
                let worktree = self
                    .create_worker_worktree(project_path.as_str(), job_id, worker_id)
                    .await?;
                let cwd = worktree.0.clone();
                worker_worktree = Some(worktree);
                cwd
            } else {
                spec.cwd.clone().unwrap_or(project_path.clone())
            };

            let depends_on = spec
                .depends_on
                .iter()
                .map(|dep_name| {
                    if dep_name == &worker_name {
                        return Err(anyhow!("worker '{worker_name}' cannot depend on itself"));
                    }
                    name_to_id
                        .get(dep_name)
                        .copied()
                        .ok_or_else(|| anyhow!("worker '{worker_name}' depends on unknown worker '{dep_name}'"))
                })
                .collect::<Result<Vec<_>>>()?;

            resolved.push(JobWorkerSpecResolved {
                worker_id,
                worker_name,
                prompt: worker_prompt,
                labels: spec.labels,
                callback,
                model: spec.model,
                model_provider: spec.model_provider,
                sandbox: spec.sandbox,
                cwd: worker_cwd,
                worktree_path: worker_worktree.as_ref().map(|value| value.0.clone()),
                branch_name: worker_worktree.as_ref().map(|value| value.1.clone()),
                depends_on,
            });
        }

        assert_dependency_graph_acyclic(
            &resolved
                .iter()
                .map(|spec| (spec.worker_id, spec.depends_on.clone()))
                .collect::<HashMap<_, _>>(),
        )?;

        Ok(resolved)
    }

    async fn launch_runnable_workers(self: &Arc<Self>, job_id: Uuid) -> Result<()> {
        let (worker_ids, dependency_graph, worker_names, mut blocked_reasons) = {
            let jobs = self.jobs.read().await;
            let job = jobs.get(&job_id).context("job not found")?;
            (
                job.worker_ids.clone(),
                job.dependency_graph.clone(),
                job.worker_names.clone(),
                job.blocked_reasons.clone(),
            )
        };

        let mut to_start = Vec::new();
        for worker_id in worker_ids {
            let dependencies = dependency_graph
                .get(&worker_id)
                .cloned()
                .unwrap_or_default();
            let status = self
                .dependency_status_for_worker(worker_id, dependencies.clone())
                .await?;
            match status {
                DependencyStatus::Failed(failed_dep_id) => {
                    let failed_name = worker_names
                        .get(&failed_dep_id)
                        .cloned()
                        .unwrap_or_else(|| failed_dep_id.to_string());
                    let reason = format!("blocked: dependency '{failed_name}' failed");
                    blocked_reasons.insert(worker_id, reason.clone());
                    let mut agents = self.agents.write().await;
                    if let Some(agent) = agents.get_mut(&worker_id) {
                        if agent.active_turn_id.is_none() && agent.result.is_none() {
                            agent.status = constants::AGENT_STATUS_BLOCKED.to_string();
                            agent.error = Some(reason.clone());
                            agent.result = Some(AgentResult {
                                agent_id: worker_id,
                                status: constants::AGENT_STATUS_BLOCKED.to_string(),
                                completion_method: Some("dependency_blocked".to_string()),
                                turn_id: None,
                                final_text: None,
                                summary: Some(reason),
                                references: None,
                                completed_at: Some(now_ts()),
                                event_id: None,
                                error: None,
                            });
                            agent.updated_at = now_ts();
                        }
                    }
                }
                DependencyStatus::Satisfied => {
                    blocked_reasons.remove(&worker_id);
                    let should_start = {
                        let agents = self.agents.read().await;
                        match agents.get(&worker_id) {
                            Some(agent) => {
                                agent.active_turn_id.is_none()
                                    && agent.result.is_none()
                                    && !agent.prompt.as_deref().unwrap_or("").trim().is_empty()
                            }
                            None => false,
                        }
                    };
                    if should_start {
                        to_start.push(worker_id);
                    }
                }
                DependencyStatus::Pending => {}
            }
        }

        if !to_start.is_empty() || !blocked_reasons.is_empty() {
            let mut jobs = self.jobs.write().await;
            if let Some(job) = jobs.get_mut(&job_id) {
                job.blocked_reasons = blocked_reasons;
                job.updated_at = now_ts();
            }
        }

        for worker_id in to_start {
            let prompt = {
                let agents = self.agents.read().await;
                agents
                    .get(&worker_id)
                    .and_then(|agent| agent.prompt.clone())
                    .unwrap_or_default()
            };
            if prompt.trim().is_empty() {
                continue;
            }
            let _ = self.start_agent_turn(worker_id, prompt).await?;
        }

        Ok(())
    }

    async fn dependency_status_for_worker(
        &self,
        worker_id: Uuid,
        dependencies: Vec<Uuid>,
    ) -> Result<DependencyStatus> {
        if dependencies.is_empty() {
            return Ok(DependencyStatus::Satisfied);
        }
        let agents = self.agents.read().await;
        let _ = worker_id;
        let mut all_satisfied = true;
        for dependency in dependencies {
            let Some(dep_agent) = agents.get(&dependency) else {
                all_satisfied = false;
                continue;
            };
            if let Some(result) = dep_agent.result.as_ref() {
                if is_worker_failure(result) {
                    return Ok(DependencyStatus::Failed(dependency));
                }
                if !is_worker_success(result) {
                    all_satisfied = false;
                }
            } else {
                all_satisfied = false;
            }
        }

        if all_satisfied {
            Ok(DependencyStatus::Satisfied)
        } else {
            Ok(DependencyStatus::Pending)
        }
    }

    async fn resolve_worker_cwd(
        &self,
        project_path: &str,
        requested_cwd: Option<String>,
        worktree: Option<&WorkerWorktreeSpec>,
    ) -> Result<String> {
        let Some(worktree) = worktree else {
            return Ok(requested_cwd.unwrap_or_else(|| project_path.to_string()));
        };

        let worktree_path = {
            let configured = Path::new(&worktree.path);
            if configured.is_absolute() {
                configured.to_path_buf()
            } else {
                Path::new(project_path).join(configured)
            }
        };

        let worktree_path = worktree_path
            .to_str()
            .context("invalid worktree path")?
            .to_string();

        if worktree.create {
            self.create_worktree(project_path, &worktree_path, worktree.base_ref.as_deref())
                .await?;
            return Ok(worktree_path);
        }

        if !Path::new(&worktree_path).exists() {
            return Err(anyhow!(
                "requested worktree path does not exist: {worktree_path}"
            ));
        }
        if !Path::new(&worktree_path).is_dir() {
            return Err(anyhow!(
                "requested worktree path is not a directory: {worktree_path}"
            ));
        }

        Ok(worktree_path)
    }

    async fn create_worktree(
        &self,
        project_path: &str,
        worktree_path: &str,
        base_ref: Option<&str>,
    ) -> Result<()> {
        let _lock = self.worktree_lock.lock().await;

        if Path::new(worktree_path).exists() {
            if Path::new(worktree_path).is_dir() {
                let verify = Command::new(constants::WORKTREE_GIT_COMMAND)
                    .arg(constants::WORKTREE_OPTION_WORKDIR)
                    .arg(worktree_path)
                    .arg(constants::WORKTREE_VERIFY_COMMAND)
                    .arg(constants::WORKTREE_VERIFY_ARG)
                    .output()
                    .await
                    .with_context(|| {
                        format!("failed to check existing worktree at {worktree_path}")
                    })?;

                if verify.status.success()
                    && String::from_utf8_lossy(&verify.stdout).trim() == "true"
                {
                    return Ok(());
                }
            }

            return Err(anyhow!(
                "worktree path already exists but is not a git worktree: {worktree_path}"
            ));
        }

        let base_ref = base_ref.unwrap_or(constants::WORKTREE_BASE_REF_DEFAULT);
        let output = Command::new(constants::WORKTREE_GIT_COMMAND)
            .arg(constants::WORKTREE_OPTION_WORKDIR)
            .arg(project_path)
            .arg(constants::WORKTREE_COMMAND)
            .arg(constants::WORKTREE_ARG_ADD)
            .arg(worktree_path)
            .arg(base_ref)
            .output()
            .await
            .with_context(|| format!("failed to run git worktree add for {worktree_path}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return Err(anyhow!(
                "failed to create worker worktree at {worktree_path}: {stderr} {stdout}"
            ));
        }

        Ok(())
    }

    async fn create_worker_worktree(
        &self,
        project_path: &str,
        job_id: Uuid,
        worker_id: Uuid,
    ) -> Result<(String, String)> {
        let _lock = self.worktree_lock.lock().await;
        let branch_name = build_worker_branch_name(job_id, worker_id);
        let worktree_path = build_worker_worktree_path(project_path, worker_id);

        if let Some(parent) = Path::new(&worktree_path).parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!(
                    "failed to create worktree parent directory '{}'",
                    parent.display()
                )
            })?;
        }

        run_git_command(
            project_path,
            &[
                "worktree".to_string(),
                "add".to_string(),
                worktree_path.clone(),
                "-b".to_string(),
                branch_name.clone(),
            ],
            "create worker worktree",
        )
        .await?;

        Ok((worktree_path, branch_name))
    }

    async fn merge_worker_branch(
        &self,
        project_path: &str,
        base_branch: &str,
        branch_name: &str,
        strategy: &str,
    ) -> Result<()> {
        let _lock = self.worktree_lock.lock().await;
        run_git_command(
            project_path,
            &["checkout".to_string(), base_branch.to_string()],
            "checkout base branch before merge",
        )
        .await?;

        match strategy {
            "manual" => Ok(()),
            "sequential" => {
                run_git_command(
                    project_path,
                    &[
                        "merge".to_string(),
                        "--no-ff".to_string(),
                        branch_name.to_string(),
                    ],
                    "merge worker branch",
                )
                .await?;
                Ok(())
            }
            "rebase" => {
                run_git_command(
                    project_path,
                    &[
                        "rebase".to_string(),
                        base_branch.to_string(),
                        branch_name.to_string(),
                    ],
                    "rebase worker branch",
                )
                .await?;
                run_git_command(
                    project_path,
                    &[
                        "merge".to_string(),
                        "--no-ff".to_string(),
                        branch_name.to_string(),
                    ],
                    "merge rebased worker branch",
                )
                .await?;
                Ok(())
            }
            _ => Err(anyhow!("unsupported merge strategy '{strategy}'")),
        }
    }

    async fn cleanup_worker_worktree(
        &self,
        project_path: &str,
        worktree_path: &str,
        branch_name: Option<&str>,
        was_merged: bool,
    ) -> Result<()> {
        let _lock = self.worktree_lock.lock().await;
        run_git_command(
            project_path,
            &[
                "worktree".to_string(),
                "remove".to_string(),
                "--force".to_string(),
                worktree_path.to_string(),
            ],
            "remove worker worktree",
        )
        .await?;

        if was_merged && let Some(branch_name) = branch_name {
            run_git_command(
                project_path,
                &[
                    "branch".to_string(),
                    "-d".to_string(),
                    branch_name.to_string(),
                ],
                "delete merged worker branch",
            )
            .await?;
        }

        Ok(())
    }

    pub async fn list_jobs(&self) -> Vec<JobState> {
        let jobs = self.jobs.read().await;
        let agents = self.agents.read().await;
        jobs.values()
            .map(|job| JobState {
                id: job.id,
                project_id: job.project_id,
                status: job.status.clone(),
                worker_ids: job.worker_ids.clone(),
                labels: aggregate_worker_labels(&job.worker_labels),
                created_at: job.created_at,
                completed_at: job.completed_at,
                updated_at: job.updated_at,
                throttled_workers: job
                    .worker_ids
                    .iter()
                    .filter_map(|worker_id| agents.get(worker_id))
                    .filter(|agent| agent.throttled)
                    .map(|agent| ThrottledWorkerStatus {
                        agent_id: agent.id,
                        retry_at_ms: agent.retry_at_ms,
                        reason: agent.throttle_reason.clone(),
                    })
                    .collect(),
                dependency_graph: job.dependency_graph.clone(),
                blocked_reasons: job.blocked_reasons.clone(),
            })
            .collect()
    }

    pub async fn get_job(&self, job_id: Uuid) -> Result<JobState> {
        let job = self
            .jobs
            .read()
            .await
            .get(&job_id)
            .context("job not found")?
            .clone();
        let agents = self.agents.read().await;

        Ok(JobState {
            id: job.id,
            project_id: job.project_id,
            status: job.status.clone(),
            worker_ids: job.worker_ids.clone(),
            labels: aggregate_worker_labels(&job.worker_labels),
            created_at: job.created_at,
            completed_at: job.completed_at,
            updated_at: job.updated_at,
            throttled_workers: job
                .worker_ids
                .iter()
                .filter_map(|worker_id| agents.get(worker_id))
                .filter(|agent| agent.throttled)
                .map(|agent| ThrottledWorkerStatus {
                    agent_id: agent.id,
                    retry_at_ms: agent.retry_at_ms,
                    reason: agent.throttle_reason.clone(),
                })
                .collect(),
            dependency_graph: job.dependency_graph.clone(),
            blocked_reasons: job.blocked_reasons.clone(),
        })
    }

    pub async fn get_job_result(&self, job_id: Uuid) -> Result<JobResult> {
        let job = {
            let jobs = self.jobs.read().await;
            jobs.get(&job_id).cloned().context("job not found")?
        };

        let total_workers = job.worker_ids.len();
        let mut workers = Vec::with_capacity(total_workers);
        let mut completed_workers = 0usize;
        let mut running_workers = 0usize;
        let mut failed_workers = 0usize;

        for worker_id in &job.worker_ids {
            let response = self.get_agent_result(*worker_id).await.unwrap_or_else(|_| {
                crate::models::AgentResultResponse {
                    agent_id: *worker_id,
                    status: constants::JOB_STATUS_UNKNOWN.to_string(),
                    completion_method: None,
                    turn_id: None,
                    final_text: None,
                    summary: None,
                    references: None,
                    completed_at: None,
                    event_id: None,
                    event_count: 0,
                    error: Some("worker disappeared".to_string()),
                    throttled: false,
                    retry_at_ms: None,
                    throttle_reason: None,
                    failure_report: None,
                    budget: None,
                }
            });

            if response.status == constants::AGENT_STATUS_RUNNING {
                running_workers += 1;
            }

            if response.completed_at.is_some() {
                completed_workers += 1;
            }

            if response.status == constants::AGENT_STATUS_FAILED
                || response.status == constants::AGENT_STATUS_BLOCKED
                || matches!(
                    response.completion_method.as_deref(),
                    Some(constants::EVENT_METHOD_TURN_ABORTED)
                )
            {
                failed_workers += 1;
            }

            workers.push(response);
        }

        let status = if total_workers == 0 {
            constants::JOB_STATUS_EMPTY.to_string()
        } else if running_workers > 0 {
            constants::JOB_STATUS_RUNNING.to_string()
        } else if completed_workers == total_workers {
            if failed_workers == total_workers {
                constants::JOB_STATUS_FAILED.to_string()
            } else if failed_workers > 0 {
                constants::JOB_STATUS_PARTIAL.to_string()
            } else {
                constants::JOB_STATUS_COMPLETED.to_string()
            }
        } else if completed_workers > 0 {
            constants::JOB_STATUS_PARTIAL.to_string()
        } else {
            constants::JOB_STATUS_QUEUED.to_string()
        };

        Ok(JobResult {
            id: job.id,
            project_id: job.project_id,
            status,
            total_workers,
            completed_workers,
            running_workers,
            failed_workers,
            worker_count: total_workers,
            workers,
            labels: aggregate_worker_labels(&job.worker_labels),
            merged_branches: job.merged_branches.clone(),
            merge_conflicts: job.merge_conflicts.clone(),
            dependency_graph: job.dependency_graph.clone(),
            blocked_reasons: job.blocked_reasons.clone(),
        })
    }

    pub async fn wait_for_job_result(
        self: &Arc<Self>,
        job_id: Uuid,
        timeout_ms: Option<u64>,
        poll_ms: Option<u64>,
        include_workers: bool,
    ) -> Result<crate::models::JobWaitResponse> {
        let poll_interval =
            std::time::Duration::from_millis(poll_ms.unwrap_or(constants::DEFAULT_WAIT_POLL_MS));
        let has_timeout = timeout_ms.unwrap_or(0) > 0;
        let timeout = std::time::Duration::from_millis(timeout_ms.unwrap_or(0));
        let deadline = if has_timeout {
            Some(tokio::time::Instant::now() + timeout)
        } else {
            None
        };

        loop {
            let mut result = self.get_job_result(job_id).await;

            if let Ok(result) = result.as_mut()
                && (is_job_terminal_status(&result.status) || !include_workers)
            {
                if !include_workers {
                    result.workers = Vec::new();
                }
                if is_job_terminal_status(&result.status) {
                    return Ok(crate::models::JobWaitResponse {
                        result: result.clone(),
                        timed_out: false,
                    });
                }
            }

            if let Some(deadline) = deadline {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    if let Ok(mut result) = result {
                        if is_job_terminal_status(&result.status) {
                            return Ok(crate::models::JobWaitResponse {
                                result,
                                timed_out: false,
                            });
                        }
                        if !include_workers {
                            result.workers = Vec::new();
                        }
                        return Ok(crate::models::JobWaitResponse {
                            result,
                            timed_out: true,
                        });
                    }

                    return Ok(crate::models::JobWaitResponse {
                        result: JobResult {
                            id: job_id,
                            project_id: None,
                            status: constants::JOB_STATUS_UNKNOWN.to_string(),
                            total_workers: 0,
                            completed_workers: 0,
                            running_workers: 0,
                            failed_workers: 0,
                            worker_count: 0,
                            workers: Vec::new(),
                            labels: HashMap::new(),
                            merged_branches: Vec::new(),
                            merge_conflicts: Vec::new(),
                            dependency_graph: HashMap::new(),
                            blocked_reasons: HashMap::new(),
                        },
                        timed_out: true,
                    });
                }

                let remaining = (deadline - now).min(poll_interval);
                if !remaining.is_zero() {
                    time::sleep(remaining).await;
                }
            } else {
                time::sleep(poll_interval).await;
            }
        }
    }

    pub async fn send_to_project_foreman(
        self: &Arc<Self>,
        project_id: Uuid,
        input: SendAgentInput,
    ) -> Result<SpawnProjectWorkerResponse> {
        let foreman_id = {
            let projects = self.projects.read().await;
            projects
                .get(&project_id)
                .and_then(|project| project.foreman_agent_id)
                .context("project not found")?
        };

        let response = self.send_turn(foreman_id, input).await?;

        Ok(SpawnProjectWorkerResponse {
            id: response.id,
            thread_id: response.thread_id,
            turn_id: response.turn_id,
            status: response.status,
            project_id,
            foreman_id,
            role: constants::AGENT_ROLE_FOREMAN.to_string(),
        })
    }

    pub async fn steer_project_foreman(
        self: &Arc<Self>,
        project_id: Uuid,
        req: SteerAgentInput,
    ) -> Result<SpawnProjectWorkerResponse> {
        let foreman_id = {
            let projects = self.projects.read().await;
            projects
                .get(&project_id)
                .and_then(|project| project.foreman_agent_id)
                .context("project not found")?
        };

        let response = self.steer(foreman_id, req.prompt).await?;

        Ok(SpawnProjectWorkerResponse {
            id: response.id,
            thread_id: response.thread_id,
            turn_id: response.turn_id,
            status: response.status,
            project_id,
            foreman_id,
            role: constants::AGENT_ROLE_FOREMAN.to_string(),
        })
    }

    pub async fn compact_project(
        self: &Arc<Self>,
        project_id: Uuid,
        request: CompactProjectRequest,
    ) -> Result<SpawnProjectWorkerResponse> {
        let CompactProjectRequest {
            prompt: request_prompt,
            reason: request_reason,
        } = request;

        let (project, foreman_id) = {
            let projects = self.projects.read().await;
            let project = projects.get(&project_id).context("project not found")?;
            (project.clone(), project.foreman_agent_id)
        };

        let foreman_id = foreman_id.context("project has no foreman")?;
        let mut prompt = project
            .runtime
            .handoff
            .unwrap_or_else(|| constants::PROJECT_LIFECYCLE_COMPACT_PROMPT.to_string());

        if let Some(reason) = request_reason.as_deref() {
            prompt = format!(
                "{}\n\n{} {}\n",
                prompt,
                constants::PROJECT_LIFECYCLE_COMPACT_PROMPT_PREFIX,
                reason
            );
        }

        if let Some(extra_prompt) = request_prompt {
            prompt = format!(
                "{}\n\n{}{}\n",
                prompt,
                constants::PROJECT_LIFECYCLE_PROMPT_PREFIX,
                extra_prompt
            );
        }

        let response = self
            .send_turn(
                foreman_id,
                SendAgentInput {
                    prompt: Some(prompt),
                    callback_profile: None,
                    callback_prompt_prefix: None,
                    callback_args: None,
                    callback_vars: None,
                    callback_events: None,
                },
            )
            .await?;

        let project = {
            let projects = self.projects.read().await;
            projects
                .get(&project_id)
                .cloned()
                .context("project vanished")?
        };

        let compact_context = CallbackEventContext {
            agent_id: response.id,
            thread_id: String::new(),
            turn_id: response.turn_id.clone(),
            role: AgentRole::Foreman,
            project_id: Some(project_id),
            foreman_id: Some(foreman_id),
            job_id: None,
            method: constants::PROJECT_LIFECYCLE_EVENT_COMPACT.to_string(),
            ts: now_ts(),
            params: serde_json::json!({
                constants::CALLBACK_VAR_REASON: request_reason.unwrap_or_else(
                    || constants::PROJECT_LIFECYCLE_COMPACTION_REASON.to_string()
                ),
            }),
            event_id: Uuid::new_v4(),
            result_snapshot: None,
            callback: WorkerCallback::None,
        };
        let _ = self
            .dispatch_project_lifecycle_callback(
                &project,
                constants::PROJECT_LIFECYCLE_COMPACT,
                &CallbackOverrides::default(),
                compact_context,
            )
            .await;

        Ok(SpawnProjectWorkerResponse {
            id: response.id,
            thread_id: response.thread_id,
            turn_id: response.turn_id,
            status: response.status,
            project_id,
            foreman_id,
            role: constants::AGENT_ROLE_FOREMAN.to_string(),
        })
    }

    pub async fn close_project(self: &Arc<Self>, project_id: Uuid) -> Result<()> {
        let (project, worker_ids, foreman_id) = {
            let mut projects = self.projects.write().await;
            let project = projects.remove(&project_id).context("project not found")?;
            let worker_ids = project.worker_ids.clone();
            let foreman_id = project.foreman_agent_id;
            (project, worker_ids, foreman_id)
        };

        let foreman_id_for_callback = foreman_id.unwrap_or_else(Uuid::new_v4);
        let role = if project.foreman_agent_id.is_some() {
            AgentRole::Foreman
        } else {
            AgentRole::Standalone
        };

        let stop_context = CallbackEventContext {
            agent_id: foreman_id_for_callback,
            thread_id: String::new(),
            turn_id: None,
            role,
            project_id: Some(project_id),
            foreman_id: Some(foreman_id_for_callback),
            job_id: None,
            method: constants::PROJECT_LIFECYCLE_EVENT_STOP.to_string(),
            ts: now_ts(),
            params: serde_json::json!({
                constants::PROJECT_VAR_ID: project_id,
                constants::PROJECT_VAR_NAME: project.name,
            }),
            event_id: Uuid::new_v4(),
            result_snapshot: None,
            callback: WorkerCallback::None,
        };

        let _ = self
            .dispatch_project_lifecycle_callback(
                &project,
                constants::PROJECT_LIFECYCLE_STOP,
                &CallbackOverrides::default(),
                stop_context,
            )
            .await;

        for worker_id in worker_ids {
            let _ = self.close_agent(worker_id).await;
        }
        if let Some(close_foreman_id) = foreman_id {
            let _ = self.close_agent(close_foreman_id).await;
        }
        {
            let mut jobs = self.jobs.write().await;
            jobs.retain(|_, job| job.project_id != Some(project_id));
        }
        self.schedule_state_persist();

        Ok(())
    }

    pub async fn spawn_agent(
        self: &Arc<Self>,
        request: SpawnAgentRequest,
    ) -> Result<SpawnAgentResponse> {
        let callback = self
            .resolve_callback(CallbackResolutionParams {
                callback_profile: request.callback_profile.clone(),
                callback_prompt_prefix: request.callback_prompt_prefix.clone(),
                callback_args: request.callback_args.clone(),
                callback_vars: request.callback_vars.clone(),
                callback_events: request.callback_events.clone(),
                use_global_default: true,
            })
            .context("failed to configure worker callback")?;

        self.spawn_agent_record(
            None,
            request,
            AgentRole::Standalone,
            None,
            None,
            None,
            callback,
        )
        .await
    }

    pub async fn send_turn(
        self: &Arc<Self>,
        agent_id: Uuid,
        input: SendAgentInput,
    ) -> Result<SpawnAgentResponse> {
        let has_turn_prompt = input
            .prompt
            .as_ref()
            .is_some_and(|prompt| !prompt.trim().is_empty());
        let callback_update_requested = has_callback_update_fields(&input);

        if !has_turn_prompt && !callback_update_requested {
            return Err(anyhow!(
                "send requires either a non-empty prompt or at least one callback override"
            ));
        }

        let preconfigured_callback = if callback_update_requested {
            let agents = self.agents.read().await;
            let agent = agents.get(&agent_id).context("agent not found")?;
            Some(
                self.resolve_callback_for_send(&input, &agent.callback)
                    .context("failed to configure callback update")?,
            )
        } else {
            None
        };

        if let Some(callback) = preconfigured_callback.clone() {
            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&agent_id) {
                agent.callback = callback;
                agent.updated_at = now_ts();
            }
        }

        let turn_id = if has_turn_prompt {
            self.start_agent_turn(agent_id, input.prompt.clone().unwrap_or_default())
                .await
                .with_context(|| "turn/start failed")?
        } else {
            None
        };

        {
            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&agent_id) {
                agent.prompt = input.prompt.clone().filter(|value| !value.is_empty());
                if has_turn_prompt {
                    agent.restart_attempts = 0;
                }
                agent.updated_at = now_ts();
                if let Some(turn_id) = turn_id.clone() {
                    agent.active_turn_id = Some(turn_id);
                    agent.status = constants::AGENT_STATUS_RUNNING.to_string();
                }
            }
        }
        self.schedule_state_persist();

        let (updated_thread_id, updated_agent) = {
            let agents = self.agents.read().await;
            let agent = agents.get(&agent_id).context("agent missing")?;
            (agent.thread_id.clone(), agent.clone())
        };

        Ok(SpawnAgentResponse {
            id: updated_agent.id,
            thread_id: updated_thread_id,
            turn_id,
            status: {
                if has_turn_prompt {
                    updated_agent.status.clone()
                } else {
                    updated_agent.status
                }
            },
            project_id: updated_agent.project_id,
            role: updated_agent.role.as_str().to_string(),
            foreman_id: updated_agent.foreman_id,
        })
    }

    pub async fn retry_agent(
        self: &Arc<Self>,
        agent_id: Uuid,
        request: RetryAgentRequest,
    ) -> Result<RetryAgentResponse> {
        let requested_mode = request.mode.trim().to_lowercase();
        if requested_mode != "last-turn" && requested_mode != "checkpoint" {
            return Err(anyhow!("retry mode must be 'last-turn' or 'checkpoint'"));
        }

        let snapshot = {
            let agents = self.agents.read().await;
            let agent = agents.get(&agent_id).context("agent not found")?;
            (agent.prompt.clone(), agent.checkpoint.clone())
        };
        let (last_prompt, checkpoint) = snapshot;
        let checkpoint_available = checkpoint.last_successful_turn_id.is_some();
        let selected_mode = if requested_mode == "checkpoint" && !checkpoint_available {
            "last-turn".to_string()
        } else {
            requested_mode
        };

        let retry_prompt = if selected_mode == "checkpoint" {
            format!(
                "Resume from checkpoint. Last successful turn id: {}. Continue from current branch/worktree and finish pending work.",
                checkpoint
                    .last_successful_turn_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string())
            )
        } else {
            last_prompt.unwrap_or_default()
        };

        if retry_prompt.trim().is_empty() {
            return Err(anyhow!(
                "retry is unavailable because there is no prior prompt for this agent"
            ));
        }

        {
            let mut agents = self.agents.write().await;
            let agent = agents.get_mut(&agent_id).context("agent not found")?;
            agent.prompt = Some(retry_prompt.clone());
            if selected_mode == "checkpoint" {
                if agent.worktree_path.is_none() {
                    agent.worktree_path = checkpoint.worktree_path.clone();
                }
                if agent.branch_name.is_none() {
                    agent.branch_name = checkpoint.branch_name.clone();
                }
                agent.validation_retries = checkpoint.validation_retries;
                agent.last_validation_error = checkpoint.last_validation_error.clone();
            }
            agent.checkpoint.retry_count = agent.checkpoint.retry_count.saturating_add(1);
            agent.updated_at = now_ts();
        }

        let restarted_turn_id = self.start_agent_turn(agent_id, retry_prompt).await?;

        {
            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&agent_id) {
                agent.active_turn_id = restarted_turn_id.clone();
                if restarted_turn_id.is_some() {
                    agent.status = constants::AGENT_STATUS_RUNNING.to_string();
                }
                agent.updated_at = now_ts();
            }
        }
        self.schedule_state_persist();

        Ok(RetryAgentResponse {
            agent_id,
            selected_mode,
            restarted_turn_id,
            checkpoint_available,
        })
    }

    pub async fn steer(
        self: &Arc<Self>,
        agent_id: Uuid,
        prompt: String,
    ) -> Result<SpawnAgentResponse> {
        let (thread_id, expected_turn_id, agent_meta) = {
            let agents = self.agents.read().await;
            let agent = agents.get(&agent_id).context("agent not found")?;
            (
                agent.thread_id.clone(),
                agent
                    .active_turn_id
                    .clone()
                    .ok_or_else(|| anyhow!("no active turn to steer")),
                agent.clone(),
            )
        };

        let expected_turn_id = expected_turn_id?;
        let request = TurnSteerRequest {
            thread_id: thread_id.clone(),
            input: vec![TextPayload::text(prompt)],
            expected_turn_id,
        };
        let response = self
            .client
            .turn_steer(&request)
            .await
            .with_context(|| "turn/steer failed")?;

        let turn_id = Some(response.turn_id);

        {
            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&agent_id) {
                agent.status = constants::AGENT_STATUS_RUNNING.to_string();
                agent.updated_at = now_ts();
                if let Some(turn_id) = turn_id.clone() {
                    agent.active_turn_id = Some(turn_id);
                }
            }
        }
        self.schedule_state_persist();

        Ok(SpawnAgentResponse {
            id: agent_meta.id,
            thread_id: request.thread_id,
            turn_id,
            status: constants::AGENT_STATUS_RUNNING.to_string(),
            project_id: agent_meta.project_id,
            role: agent_meta.role.as_str().to_string(),
            foreman_id: agent_meta.foreman_id,
        })
    }

    pub async fn interrupt(
        self: &Arc<Self>,
        agent_id: Uuid,
        override_turn_id: Option<String>,
    ) -> Result<()> {
        let (thread_id, turn_id) = {
            let agents = self.agents.read().await;
            let agent = agents.get(&agent_id).context("agent not found")?;
            let turn_id = override_turn_id.or_else(|| agent.active_turn_id.clone());
            (agent.thread_id.clone(), turn_id)
        };

        let turn_id = turn_id.context("no active turn to interrupt")?;
        let request = TurnInterruptRequest { thread_id, turn_id };
        let _: EmptyResponse = self.client.turn_interrupt(&request).await?;

        {
            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&agent_id) {
                agent.status = constants::AGENT_STATUS_INTERRUPTED.into();
                agent.updated_at = now_ts();
            }
        }
        self.schedule_state_persist();

        Ok(())
    }

    async fn start_agent_turn(
        self: &Arc<Self>,
        agent_id: Uuid,
        prompt: String,
    ) -> Result<Option<String>> {
        let (thread_id, role, governor_key, paused_reason) = {
            let agents = self.agents.read().await;
            let agent = agents.get(&agent_id).context("agent not found")?;
            (
                agent.thread_id.clone(),
                agent.role.clone(),
                agent.governor_key.clone(),
                agent.budget.paused_reason.clone(),
            )
        };

        if role == AgentRole::Worker
            && let Some(reason) = paused_reason
        {
            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&agent_id) {
                agent.status = constants::AGENT_STATUS_PAUSED.to_string();
                agent.error = Some(reason);
                agent.updated_at = now_ts();
            }
            return Ok(None);
        }

        if role == AgentRole::Worker
            && let Some(key) = governor_key.clone()
            && let Some(retry_at_ms) = self.acquire_governor_permit(&key).await
        {
            self.mark_agent_throttled(agent_id, retry_at_ms, "provider rate limit")
                .await;
            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&agent_id) {
                update_failure_report(
                    &mut agent.failure_report,
                    FailureClass::RateLimit,
                    "provider rate limit".to_string(),
                    agent.throttle_reason.clone(),
                    true,
                    now_ts(),
                );
            }
            self.schedule_throttled_retry(agent_id, retry_at_ms);
            return Ok(None);
        }

        let request = TurnStartRequest {
            thread_id: thread_id.clone(),
            input: vec![TextPayload::text(prompt)],
        };
        match self.client.turn_start(&request).await {
            Ok(response) => {
                let turn_id = response.turn_id;
                {
                    let mut agents = self.agents.write().await;
                    if let Some(agent) = agents.get_mut(&agent_id) {
                        agent.active_turn_id = Some(turn_id.clone());
                        agent.status = constants::AGENT_STATUS_RUNNING.to_string();
                        agent.throttled = false;
                        agent.retry_at_ms = None;
                        agent.throttle_reason = None;
                        if role == AgentRole::Worker && governor_key.is_some() {
                            agent.governor_inflight = true;
                        }
                        agent.updated_at = now_ts();
                    }
                }
                if role == AgentRole::Worker
                    && let Some(key) = governor_key
                {
                    self.governor_mark_success(&key).await;
                }
                Ok(Some(turn_id))
            }
            Err(err) => {
                if role == AgentRole::Worker
                    && let Some(key) = governor_key
                {
                    if is_rate_limit_error(err.to_string().as_str()) {
                        let retry_at_ms = self.governor_mark_rate_limited(&key).await;
                        self.mark_agent_throttled(agent_id, retry_at_ms, err.to_string().as_str())
                            .await;
                        let mut agents = self.agents.write().await;
                        if let Some(agent) = agents.get_mut(&agent_id) {
                            update_failure_report(
                                &mut agent.failure_report,
                                FailureClass::RateLimit,
                                "provider rate limit".to_string(),
                                Some(err.to_string()),
                                true,
                                now_ts(),
                            );
                        }
                        self.schedule_throttled_retry(agent_id, retry_at_ms);
                        return Ok(None);
                    }
                    self.governor_release_permit(&key).await;
                }
                Err(err).context("turn/start failed")
            }
        }
    }

    fn schedule_throttled_retry(self: &Arc<Self>, agent_id: Uuid, retry_at_ms: u64) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let now = now_ms();
            let delay_ms = retry_at_ms.saturating_sub(now);
            if delay_ms > 0 {
                time::sleep(Duration::from_millis(delay_ms)).await;
            }
            let _ = this.retry_throttled_agent_turn(agent_id).await;
        });
    }

    async fn retry_throttled_agent_turn(self: &Arc<Self>, agent_id: Uuid) -> Result<()> {
        let prompt = {
            let agents = self.agents.read().await;
            let Some(agent) = agents.get(&agent_id) else {
                return Ok(());
            };
            if !agent.throttled {
                return Ok(());
            }
            if agent.active_turn_id.is_some() {
                return Ok(());
            }
            agent.prompt.clone().unwrap_or_default()
        };
        if prompt.trim().is_empty() {
            return Ok(());
        }
        let _ = self.start_agent_turn(agent_id, prompt).await?;
        self.schedule_state_persist();
        Ok(())
    }

    async fn apply_budget_policy(self: &Arc<Self>, agent_id: Uuid, event_name: &str) -> Result<()> {
        let defaults = self.config.budgets.defaults.clone();
        let now_ms_value = now_ms();
        let mut kill_snapshot: Option<(String, String)> = None;
        let should_emit_event;

        {
            let mut agents = self.agents.write().await;
            let Some(agent) = agents.get_mut(&agent_id) else {
                return Ok(());
            };
            if agent.role != AgentRole::Worker {
                return Ok(());
            }

            agent.budget.elapsed_ms = now_ms_value.saturating_sub(agent.budget.started_at_ms);
            let evaluation = evaluate_budget(
                &agent.budget,
                defaults.max_tokens,
                defaults.max_cost_usd,
                defaults.max_duration_ms,
            );

            if !evaluation.exceeded {
                return Ok(());
            }

            let reason = evaluation
                .reason
                .unwrap_or_else(|| "budget exceeded".to_string());
            if agent.budget.exceeded
                && agent.budget.exceeded_reason.as_deref() == Some(reason.as_str())
            {
                return Ok(());
            }
            agent.budget.exceeded = true;
            agent.budget.exceeded_reason = Some(reason.clone());
            should_emit_event = true;

            if let Some(kill) =
                apply_budget_transition(agent, &defaults.on_exceed, reason.as_str(), now_ts())
            {
                kill_snapshot = Some(kill);
            }
            agent.updated_at = now_ts();
        }

        if let Some((thread_id, turn_id)) = kill_snapshot {
            let _ = self
                .client
                .turn_interrupt(&TurnInterruptRequest { thread_id, turn_id })
                .await;
        }

        if should_emit_event {
            let agents = self.agents.read().await;
            if let Some(agent) = agents.get(&agent_id) {
                let _ = self
                    .agent_progress_tx
                    .send(progress_budget_state(agent, event_name));
            }
            self.schedule_state_persist();
        }
        Ok(())
    }

    async fn acquire_governor_permit(&self, key: &str) -> Option<u64> {
        let mut governors = self.governors.write().await;
        let state = governors.entry(key.to_string()).or_default();
        let now = now_ms();
        if state.throttled_until_ms > now {
            return Some(state.throttled_until_ms);
        }
        state.inflight_count = state.inflight_count.saturating_add(1);
        None
    }

    async fn governor_release_permit(&self, key: &str) {
        let mut governors = self.governors.write().await;
        if let Some(state) = governors.get_mut(key) {
            state.inflight_count = state.inflight_count.saturating_sub(1);
        }
    }

    async fn release_governor_inflight(&self, agent_id: Uuid) {
        let key = {
            let mut agents = self.agents.write().await;
            let Some(agent) = agents.get_mut(&agent_id) else {
                return;
            };
            if !agent.governor_inflight {
                return;
            }
            agent.governor_inflight = false;
            agent.governor_key.clone()
        };

        if let Some(key) = key {
            self.governor_release_permit(&key).await;
        }
    }

    async fn governor_mark_rate_limited(&self, key: &str) -> u64 {
        let mut governors = self.governors.write().await;
        let state = governors.entry(key.to_string()).or_default();
        governor_apply_rate_limit(state, now_ms())
    }

    async fn governor_mark_success(&self, key: &str) {
        let mut governors = self.governors.write().await;
        if let Some(state) = governors.get_mut(key) {
            governor_apply_success(state, now_ms());
        }
    }

    async fn mark_agent_throttled(&self, agent_id: Uuid, retry_at_ms: u64, reason: &str) {
        let mut agents = self.agents.write().await;
        if let Some(agent) = agents.get_mut(&agent_id) {
            agent.status = constants::AGENT_STATUS_THROTTLED.to_string();
            agent.throttled = true;
            agent.retry_at_ms = Some(retry_at_ms);
            agent.throttle_reason = Some(truncate_text(reason, 240));
            agent.active_turn_id = None;
            agent.governor_inflight = false;
            agent.updated_at = now_ts();
            let _ = self
                .agent_progress_tx
                .send(progress_throttle_state(agent, "throttle/wait"));
        }
    }

    pub async fn close_agent(self: &Arc<Self>, agent_id: Uuid) -> Result<()> {
        let (thread_id, project_id, role, _foreman_id, job_id, governor_key, governor_inflight) = {
            let mut agents = self.agents.write().await;
            let agent = agents.remove(&agent_id).context("agent not found")?;
            (
                agent.thread_id,
                agent.project_id,
                agent.role,
                agent.foreman_id,
                agent.job_id,
                agent.governor_key,
                agent.governor_inflight,
            )
        };

        {
            let mut thread_map = self.thread_map.write().await;
            thread_map.remove(&thread_id);
        }

        if let Some(project_id) = project_id {
            let mut projects = self.projects.write().await;
            if let Some(project) = projects.get_mut(&project_id) {
                project.updated_at = now_ts();
                project.worker_ids.retain(|existing| *existing != agent_id);

                if matches!(role, AgentRole::Foreman) && project.foreman_agent_id == Some(agent_id)
                {
                    project.foreman_agent_id = None;
                    project.status = constants::AGENT_STATUS_ORPHANED.to_string();
                }
            }
        }

        if let Some(job_id) = job_id {
            let mut jobs = self.jobs.write().await;
            if let Some(job) = jobs.get_mut(&job_id) {
                job.updated_at = now_ts();
                job.worker_ids.retain(|existing| *existing != agent_id);
                if job.worker_ids.is_empty() {
                    job.completed_at = Some(now_ts());
                }
            }
        }
        if governor_inflight && let Some(key) = governor_key {
            self.governor_release_permit(&key).await;
        }
        self.schedule_state_persist();

        Ok(())
    }

    pub async fn list_agents(&self) -> Vec<crate::models::AgentState> {
        let agents = self.agents.read().await;
        let defaults = self.config.budgets.defaults.clone();
        agents
            .values()
            .map(|agent| build_agent_state(agent, &defaults))
            .collect()
    }

    pub async fn app_server_pid(&self) -> Option<u32> {
        self.client.app_server_pid()
    }

    pub fn configured_callback_profile_count(&self) -> usize {
        self.config.callbacks.profiles.len()
    }

    pub fn default_callback_profile(&self) -> Option<String> {
        self.config.callbacks.default_profile.clone()
    }

    pub async fn status_summary(&self) -> (usize, usize, u64, u64) {
        let agents = self.agents.read().await;
        let projects = self.projects.read().await;
        (
            agents.len(),
            projects.len(),
            self.started_at,
            now_ts().saturating_sub(self.started_at),
        )
    }

    pub async fn list_throttled_workers(&self) -> Vec<ThrottledWorkerStatus> {
        let agents = self.agents.read().await;
        agents
            .values()
            .filter(|agent| agent.throttled || agent.status == constants::AGENT_STATUS_THROTTLED)
            .map(|agent| ThrottledWorkerStatus {
                agent_id: agent.id,
                retry_at_ms: agent.retry_at_ms,
                reason: agent.throttle_reason.clone(),
            })
            .collect()
    }

    pub fn subscribe_job_events(&self) -> broadcast::Receiver<JobEventEnvelope> {
        self.job_event_tx.subscribe()
    }

    pub fn subscribe_agent_progress(&self) -> broadcast::Receiver<AgentProgressEnvelope> {
        self.agent_progress_tx.subscribe()
    }

    pub async fn get_agent_progress_backlog(
        &self,
        agent_id: Uuid,
        since_ts: Option<u64>,
    ) -> Result<Vec<AgentProgressEnvelope>> {
        let agent = self
            .agents
            .read()
            .await
            .get(&agent_id)
            .cloned()
            .context("agent not found")?;
        let min_ts = since_ts.unwrap_or(0);
        let mut envelopes = agent
            .events
            .iter()
            .filter(|event| event.ts >= min_ts)
            .map(|event| progress_from_agent_event(&agent, event))
            .collect::<Vec<_>>();
        envelopes.sort_by_key(|event| event.ts);
        Ok(envelopes)
    }

    pub async fn get_job_event_backlog(
        &self,
        job_id: Uuid,
        since_ts: Option<u64>,
    ) -> Result<Vec<JobEventEnvelope>> {
        let worker_ids = {
            let jobs = self.jobs.read().await;
            jobs.get(&job_id)
                .context("job not found")?
                .worker_ids
                .clone()
        };

        let min_ts = since_ts.unwrap_or(0);
        let mut events = Vec::new();
        {
            let agents = self.agents.read().await;
            for worker_id in worker_ids {
                let Some(agent) = agents.get(&worker_id) else {
                    continue;
                };
                for event in &agent.events {
                    if event.ts < min_ts {
                        continue;
                    }
                    events.push(JobEventEnvelope {
                        job_id,
                        agent_id: worker_id,
                        ts: event.ts,
                        method: event.method.clone(),
                        params: event.params.clone(),
                    });
                }
            }
        }

        events.sort_by_key(|event| event.ts);
        Ok(events)
    }

    pub async fn get_agent(&self, agent_id: Uuid) -> Result<crate::models::AgentState> {
        let agent = self
            .agents
            .read()
            .await
            .get(&agent_id)
            .context("agent not found")?
            .clone();

        Ok(build_agent_state(&agent, &self.config.budgets.defaults))
    }

    pub async fn get_agent_events(
        &self,
        agent_id: Uuid,
        tail: Option<usize>,
    ) -> Result<Vec<crate::models::AgentEventDto>> {
        let mut events = self
            .agents
            .read()
            .await
            .get(&agent_id)
            .map(|agent| agent.events.iter().cloned().collect::<Vec<_>>())
            .context("agent not found")?;

        if let Some(limit) = tail {
            let len = events.len();
            if len > limit {
                events.drain(0..(len - limit));
            }
        }

        Ok(events)
    }

    pub async fn get_agent_logs(
        &self,
        agent_id: Uuid,
        tail: Option<usize>,
        turn_filter: Option<u32>,
    ) -> Result<Vec<WorkerLogEntry>> {
        let events = self
            .agents
            .read()
            .await
            .get(&agent_id)
            .map(|agent| agent.events.iter().cloned().collect::<Vec<_>>())
            .context("agent not found")?;

        let mut turn = 0_u32;
        let mut logs = Vec::new();
        for event in events {
            if event.method == constants::EVENT_METHOD_TURN_STARTED {
                turn = turn.saturating_add(1);
            }

            if let Some(entry) = parse_worker_log_entry(&event, turn) {
                logs.push(entry);
            }
        }

        if let Some(turn) = turn_filter {
            logs.retain(|entry| entry.turn == turn);
        }

        if let Some(limit) = tail {
            let len = logs.len();
            if len > limit {
                logs.drain(0..(len - limit));
            }
        }

        Ok(logs)
    }

    pub async fn get_agent_result(
        &self,
        agent_id: Uuid,
    ) -> Result<crate::models::AgentResultResponse> {
        let agent = self
            .agents
            .read()
            .await
            .get(&agent_id)
            .cloned()
            .context("agent not found")?;
        let status = resolve_agent_status(&agent);

        Ok(crate::models::AgentResultResponse {
            agent_id: agent.id,
            status,
            completion_method: agent
                .result
                .as_ref()
                .and_then(|result| result.completion_method.clone()),
            references: agent
                .result
                .as_ref()
                .and_then(|result| result.references.clone()),
            turn_id: agent.active_turn_id.clone().or_else(|| {
                agent
                    .result
                    .as_ref()
                    .and_then(|result| result.turn_id.clone())
            }),
            final_text: agent
                .result
                .as_ref()
                .and_then(|result| result.final_text.clone()),
            summary: agent
                .result
                .as_ref()
                .and_then(|result| result.summary.clone()),
            completed_at: agent.result.as_ref().and_then(|result| result.completed_at),
            event_id: agent.result.as_ref().and_then(|result| result.event_id),
            event_count: agent.events.len(),
            error: agent
                .result
                .as_ref()
                .and_then(|result| result.error.clone())
                .or(agent.error.clone()),
            throttled: agent.throttled,
            retry_at_ms: agent.retry_at_ms,
            throttle_reason: agent.throttle_reason.clone(),
            failure_report: agent.failure_report.clone(),
            budget: Some(worker_budget_status(&agent, &self.config.budgets.defaults)),
        })
    }

    pub async fn wait_for_agent_result(
        self: &Arc<Self>,
        agent_id: Uuid,
        timeout_ms: Option<u64>,
        poll_ms: Option<u64>,
        include_events: bool,
    ) -> Result<crate::models::AgentWaitResponse> {
        let poll_interval =
            std::time::Duration::from_millis(poll_ms.unwrap_or(constants::DEFAULT_WAIT_POLL_MS));
        let has_timeout = timeout_ms.unwrap_or(0) > 0;
        let timeout = std::time::Duration::from_millis(timeout_ms.unwrap_or(0));
        let deadline = if has_timeout {
            Some(tokio::time::Instant::now() + timeout)
        } else {
            None
        };

        loop {
            let agent_result = self.get_agent_result(agent_id).await;
            if let Ok(result) = &agent_result {
                let complete = result.completed_at.is_some()
                    || (result.status != constants::AGENT_STATUS_RUNNING
                        && result.turn_id.is_none())
                    || result.status == constants::AGENT_STATUS_ORPHANED;
                if complete {
                    return Ok(crate::models::AgentWaitResponse {
                        result: result.clone(),
                        timed_out: false,
                        events: if include_events {
                            self.get_agent_events(agent_id, None).await.ok()
                        } else {
                            None
                        },
                    });
                }
            }

            if let Some(deadline) = deadline {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    return Ok(crate::models::AgentWaitResponse {
                        result: if let Ok(result) = agent_result {
                            result
                        } else {
                            self.get_agent_result(agent_id).await.unwrap_or_else(|_| {
                                crate::models::AgentResultResponse {
                                    agent_id,
                                    status: constants::AGENT_STATUS_UNKNOWN.to_string(),
                                    completion_method: None,
                                    turn_id: None,
                                    final_text: None,
                                    summary: None,
                                    references: None,
                                    completed_at: None,
                                    event_id: None,
                                    event_count: 0,
                                    error: Some("wait timeout".to_string()),
                                    throttled: false,
                                    retry_at_ms: None,
                                    throttle_reason: None,
                                    failure_report: None,
                                    budget: None,
                                }
                            })
                        },
                        timed_out: true,
                        events: if include_events {
                            self.get_agent_events(agent_id, None).await.ok()
                        } else {
                            None
                        },
                    });
                }
                let remaining = (deadline - now).min(poll_interval);
                if !remaining.is_zero() {
                    time::sleep(remaining).await;
                }
            } else {
                time::sleep(poll_interval).await;
            }
        }
    }

    async fn spawn_agent_record(
        self: &Arc<Self>,
        agent_id: Option<Uuid>,
        request: SpawnAgentRequest,
        role: AgentRole,
        project_id: Option<Uuid>,
        foreman_id: Option<Uuid>,
        job_id: Option<Uuid>,
        callback: WorkerCallback,
    ) -> Result<SpawnAgentResponse> {
        let thread_id = {
            let request = ThreadStartRequest {
                model: request.model.clone(),
                model_provider: request.model_provider.clone(),
                cwd: request.cwd.clone(),
                sandbox: request.sandbox,
            };
            let response = self
                .client
                .thread_start(&request)
                .await
                .with_context(|| "thread/start failed")?;
            response.thread_id
        };

        let id = agent_id.unwrap_or_else(Uuid::new_v4);
        let ts = now_ts();
        let role_name = role.as_str().to_string();
        let prompt = if request.prompt.trim().is_empty() {
            None
        } else {
            Some(request.prompt.clone())
        };
        let cwd = request.cwd.clone();
        let governor_key =
            governor_key(request.model_provider.as_deref(), request.model.as_deref());
        {
            let agent = AgentRecord {
                id,
                thread_id: thread_id.clone(),
                active_turn_id: None,
                prompt,
                cwd,
                restart_attempts: 0,
                turns_completed: 0,
                validation_retries: 0,
                last_validation_error: None,
                worktree_path: None,
                branch_name: None,
                model: request.model.clone(),
                model_provider: request.model_provider.clone(),
                governor_key,
                governor_inflight: false,
                throttled: false,
                retry_at_ms: None,
                throttle_reason: None,
                failure_report: None,
                budget: WorkerBudgetState::new(),
                checkpoint: WorkerCheckpoint::default(),
                status: constants::AGENT_STATUS_IDLE.into(),
                callback,
                role: role.clone(),
                project_id,
                foreman_id,
                job_id,
                error: None,
                result: None,
                created_at: ts,
                updated_at: ts,
                events: VecDeque::new(),
            };
            let mut agents = self.agents.write().await;
            let mut thread_map = self.thread_map.write().await;
            agents.insert(id, agent);
            thread_map.insert(thread_id.clone(), id);
        }

        for event in self.drain_pending_events(&thread_id).await {
            self.dispatch_event(id, event.method, event.params).await;
        }

        let mut turn_id: Option<String> = None;
        if !request.prompt.trim().is_empty() {
            turn_id = self
                .start_agent_turn(id, request.prompt.clone())
                .await
                .with_context(|| "turn/start failed")?;
        }

        let status = {
            let agents = self.agents.read().await;
            agents
                .get(&id)
                .map(resolve_agent_status)
                .unwrap_or_else(|| constants::AGENT_STATUS_UNKNOWN.to_string())
        };

        {
            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&id) {
                agent.status = status.clone();
                agent.updated_at = now_ts();
            }
        }

        self.schedule_state_persist();
        Ok(SpawnAgentResponse {
            id,
            thread_id: thread_id.clone(),
            turn_id,
            status,
            project_id,
            role: role_name,
            foreman_id,
        })
    }

    fn resolve_callback(&self, params: CallbackResolutionParams) -> Result<WorkerCallback> {
        self.resolve_callback_with_project_profiles(params, None)
    }

    fn resolve_callback_with_project_profiles(
        &self,
        params: CallbackResolutionParams,
        project_callback_profiles: Option<&HashMap<String, CallbackProfile>>,
    ) -> Result<WorkerCallback> {
        let profile_name = if params.use_global_default {
            params
                .callback_profile
                .or_else(|| self.config.default_callback_profile())
                .unwrap_or_default()
        } else {
            params.callback_profile.unwrap_or_default()
        };

        if profile_name.is_empty() {
            return Ok(WorkerCallback::None);
        }

        let profile = project_callback_profiles
            .and_then(|profiles| profiles.get(&profile_name).cloned())
            .or_else(|| self.config.get_callback_profile(&profile_name).cloned())
            .with_context(|| format!("callback profile '{profile_name}' is not configured"))?;

        match &profile {
            CallbackProfile::Webhook(profile) => {
                let secret = profile
                    .secret_env
                    .as_ref()
                    .and_then(|secret_name| std::env::var(secret_name).ok());
                let events = params.callback_events.or_else(|| profile.events.clone());
                Ok(WorkerCallback::Webhook(WorkerWebhookCallback {
                    url: profile.url.clone(),
                    secret,
                    events,
                    timeout_ms: profile.timeout_ms,
                    vars: params.callback_vars.unwrap_or_default(),
                }))
            }
            CallbackProfile::UnixSocket(profile) => {
                let events = params.callback_events.or_else(|| profile.events.clone());
                Ok(WorkerCallback::UnixSocket(WorkerUnixSocketCallback {
                    socket: profile.socket.clone(),
                    events,
                    timeout_ms: profile.timeout_ms,
                    vars: params.callback_vars.unwrap_or_default(),
                }))
            }
            CallbackProfile::Command(_) => Ok(WorkerCallback::Profile(WorkerProfileCallback {
                profile: profile_name,
                prompt_prefix: params.callback_prompt_prefix,
                command_args: params.callback_args,
                events: params.callback_events,
                vars: params.callback_vars.unwrap_or_default(),
            })),
        }
    }

    fn resolve_project_callback_spec(
        &self,
        spec: &CallbackSpec,
        project_callback_profiles: &HashMap<String, CallbackProfile>,
        overrides: &CallbackOverrides,
        use_global_default: bool,
    ) -> Result<WorkerCallback> {
        let callback_profile = overrides
            .callback_profile
            .clone()
            .or_else(|| spec.callback_profile.clone());
        let callback_prompt_prefix = overrides
            .callback_prompt_prefix
            .clone()
            .or_else(|| spec.callback_prompt_prefix.clone());
        let callback_args = overrides
            .callback_args
            .clone()
            .or_else(|| spec.callback_args.clone());
        let callback_events = overrides
            .callback_events
            .clone()
            .or_else(|| spec.callback_events.clone())
            .or_else(|| spec.events.clone());

        let callback_vars = match (&spec.callback_vars, &overrides.callback_vars) {
            (Some(base), Some(override_vars)) => {
                let mut merged = base.clone();
                for (key, value) in override_vars {
                    merged.insert(key.clone(), value.clone());
                }
                Some(merged)
            }
            (Some(base), None) => Some(base.clone()),
            (None, Some(override_vars)) => Some(override_vars.clone()),
            (None, None) => None,
        };

        if callback_profile.is_none()
            && let Some(socket) = spec.socket.clone()
        {
            return Ok(WorkerCallback::UnixSocket(WorkerUnixSocketCallback {
                socket,
                events: callback_events,
                timeout_ms: spec.timeout_ms,
                vars: callback_vars.unwrap_or_default(),
            }));
        }

        self.resolve_callback_with_project_profiles(
            CallbackResolutionParams {
                callback_profile,
                callback_prompt_prefix,
                callback_args,
                callback_vars,
                callback_events,
                use_global_default,
            },
            Some(project_callback_profiles),
        )
    }

    fn resolve_project_bubble_callback(
        &self,
        project: &ProjectRecord,
        overrides: &CallbackOverrides,
    ) -> Result<WorkerCallback> {
        self.resolve_project_callback_spec(
            &project.config.callbacks.bubble_up,
            &project.config.callbacks.profiles,
            overrides,
            false,
        )
    }

    fn resolve_project_lifecycle_callback_spec(
        &self,
        project: &ProjectRecord,
        lifecycle_key: &str,
        overrides: &CallbackOverrides,
    ) -> Result<WorkerCallback> {
        let spec = match lifecycle_key {
            constants::PROJECT_LIFECYCLE_START => &project.config.callbacks.lifecycle.start,
            constants::PROJECT_LIFECYCLE_COMPACT => &project.config.callbacks.lifecycle.compact,
            constants::PROJECT_LIFECYCLE_STOP => &project.config.callbacks.lifecycle.stop,
            constants::PROJECT_LIFECYCLE_WORKER_COMPLETED => {
                &project.config.callbacks.lifecycle.worker_completed
            }
            constants::PROJECT_LIFECYCLE_WORKER_ABORTED => {
                &project.config.callbacks.lifecycle.worker_aborted
            }
            _ => return Ok(WorkerCallback::None),
        };

        self.resolve_project_callback_spec(
            spec,
            &project.config.callbacks.profiles,
            overrides,
            false,
        )
    }

    fn resolve_callback_for_send(
        &self,
        input: &SendAgentInput,
        existing: &WorkerCallback,
    ) -> Result<WorkerCallback> {
        if !has_callback_update_fields(input) {
            return Ok(existing.clone());
        }

        if input.callback_profile.is_none() {
            return Ok(match existing {
                WorkerCallback::Webhook(existing_webhook) => {
                    WorkerCallback::Webhook(WorkerWebhookCallback {
                        url: existing_webhook.url.clone(),
                        secret: existing_webhook.secret.clone(),
                        events: input
                            .callback_events
                            .clone()
                            .or_else(|| existing_webhook.events.clone()),
                        timeout_ms: existing_webhook.timeout_ms,
                        vars: merge_vars(
                            existing_webhook.vars.clone(),
                            input.callback_vars.clone(),
                        ),
                    })
                }
                WorkerCallback::Profile(existing_profile) => {
                    WorkerCallback::Profile(WorkerProfileCallback {
                        profile: existing_profile.profile.clone(),
                        prompt_prefix: input
                            .callback_prompt_prefix
                            .clone()
                            .or_else(|| existing_profile.prompt_prefix.clone()),
                        command_args: input
                            .callback_args
                            .clone()
                            .or_else(|| existing_profile.command_args.clone()),
                        events: input
                            .callback_events
                            .clone()
                            .or_else(|| existing_profile.events.clone()),
                        vars: merge_vars(
                            existing_profile.vars.clone(),
                            input.callback_vars.clone(),
                        ),
                    })
                }
                WorkerCallback::UnixSocket(existing_unix_socket) => {
                    WorkerCallback::UnixSocket(WorkerUnixSocketCallback {
                        socket: existing_unix_socket.socket.clone(),
                        events: input
                            .callback_events
                            .clone()
                            .or_else(|| existing_unix_socket.events.clone()),
                        timeout_ms: existing_unix_socket.timeout_ms,
                        vars: merge_vars(
                            existing_unix_socket.vars.clone(),
                            input.callback_vars.clone(),
                        ),
                    })
                }
                WorkerCallback::None => {
                    let profile_name = input
                        .callback_profile
                        .clone()
                        .or_else(|| self.config.default_callback_profile())
                        .context("no callback profile configured")?;
                    self.resolve_callback(CallbackResolutionParams {
                        callback_profile: Some(profile_name),
                        callback_prompt_prefix: input.callback_prompt_prefix.clone(),
                        callback_args: input.callback_args.clone(),
                        callback_vars: input.callback_vars.clone(),
                        callback_events: input.callback_events.clone(),
                        use_global_default: true,
                    })?
                }
            });
        }

        let profile_name = input
            .callback_profile
            .as_deref()
            .context("callback_profile is required")?;

        self.resolve_callback(CallbackResolutionParams {
            callback_profile: Some(profile_name.to_string()),
            callback_prompt_prefix: input.callback_prompt_prefix.clone(),
            callback_args: input.callback_args.clone(),
            callback_vars: input.callback_vars.clone(),
            callback_events: input.callback_events.clone(),
            use_global_default: true,
        })
    }
}

fn resolve_agent_status(agent: &AgentRecord) -> String {
    if let Some(result) = &agent.result
        && (result.completion_method.is_some() || result.completed_at.is_some())
    {
        return result.status.clone();
    }
    agent.status.clone()
}

fn build_agent_state(
    agent: &AgentRecord,
    defaults: &crate::config::BudgetDefaultsConfig,
) -> AgentState {
    let mut files_modified = Vec::new();
    let mut seen_paths = std::collections::HashSet::new();
    let mut last_tool_call = None;
    for event in &agent.events {
        if let Some(tool_call) = parse_tool_call_summary(event) {
            if is_file_modifying_tool(tool_call.tool.as_str()) {
                for path in parse_modified_paths(&event.params, tool_call.tool.as_str()) {
                    if seen_paths.insert(path.clone()) {
                        files_modified.push(path);
                    }
                }
            }
            last_tool_call = Some(tool_call);
        }
    }

    AgentState {
        id: agent.id,
        thread_id: agent.thread_id.clone(),
        active_turn_id: agent.active_turn_id.clone(),
        status: agent.status.clone(),
        callback_profile: agent_callback_profile(&agent.callback),
        role: agent.role.as_str().to_string(),
        project_id: agent.project_id,
        foreman_id: agent.foreman_id,
        error: agent.error.clone(),
        updated_at: agent.updated_at,
        events: agent.events.iter().cloned().collect(),
        turns_completed: agent.turns_completed,
        validation_retries: agent.validation_retries,
        last_validation_error: agent.last_validation_error.clone(),
        worktree_path: agent.worktree_path.clone(),
        branch: agent.branch_name.clone(),
        last_tool_call,
        files_modified,
        elapsed_ms: Some(
            now_ts()
                .saturating_sub(agent.created_at)
                .saturating_mul(constants::MILLISECONDS_PER_SECOND),
        ),
        throttled: agent.throttled,
        retry_at_ms: agent.retry_at_ms,
        throttle_reason: agent.throttle_reason.clone(),
        failure_report: agent.failure_report.clone(),
        budget: Some(worker_budget_status(agent, defaults)),
    }
}

fn aggregate_worker_labels(
    labels: &HashMap<Uuid, HashMap<String, String>>,
) -> HashMap<String, Vec<String>> {
    let mut aggregate: HashMap<String, Vec<String>> = HashMap::new();
    for worker_labels in labels.values() {
        for (key, value) in worker_labels {
            aggregate
                .entry(key.clone())
                .or_default()
                .push(value.clone());
        }
    }

    for values in aggregate.values_mut() {
        values.sort_unstable();
    }

    aggregate
}

fn has_callback_update_fields(input: &SendAgentInput) -> bool {
    input.callback_profile.is_some()
        || input.callback_prompt_prefix.is_some()
        || input.callback_args.is_some()
        || input.callback_vars.is_some()
        || input.callback_events.is_some()
}

fn merge_vars(
    base: HashMap<String, String>,
    override_vars: Option<HashMap<String, String>>,
) -> HashMap<String, String> {
    let mut merged = base;
    if let Some(extra) = override_vars {
        merged.extend(extra);
    }
    merged
}

fn default_project_lifecycle_callback_status() -> HashMap<String, String> {
    let mut status = HashMap::new();
    status.insert(
        constants::PROJECT_LIFECYCLE_START.to_string(),
        constants::PROJECT_CALLBACK_STATUS_NEVER_RUN.to_string(),
    );
    status.insert(
        constants::PROJECT_LIFECYCLE_COMPACT.to_string(),
        constants::PROJECT_CALLBACK_STATUS_NEVER_RUN.to_string(),
    );
    status.insert(
        constants::PROJECT_LIFECYCLE_STOP.to_string(),
        constants::PROJECT_CALLBACK_STATUS_NEVER_RUN.to_string(),
    );
    status.insert(
        constants::PROJECT_LIFECYCLE_WORKER_COMPLETED.to_string(),
        constants::PROJECT_CALLBACK_STATUS_NEVER_RUN.to_string(),
    );
    status.insert(
        constants::PROJECT_LIFECYCLE_WORKER_ABORTED.to_string(),
        constants::PROJECT_CALLBACK_STATUS_NEVER_RUN.to_string(),
    );
    status
}

fn normalize_project_lifecycle_callback_status(
    provided: std::collections::HashMap<String, String>,
) -> HashMap<String, String> {
    let mut status = default_project_lifecycle_callback_status();
    status.extend(provided);
    status
}

fn should_use_native_worktree(defaults: &JobDefaultsConfig, worker_count: usize) -> bool {
    match defaults.worktree_mode.as_str() {
        "always" => true,
        "auto" => worker_count > 1,
        _ => false,
    }
}

fn short_uuid(id: Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

fn build_worker_branch_name(job_id: Uuid, worker_id: Uuid) -> String {
    format!("foreman/{}/{}", short_uuid(job_id), short_uuid(worker_id))
}

fn build_worker_worktree_path(project_path: &str, worker_id: Uuid) -> String {
    Path::new(project_path)
        .join(".foreman")
        .join("worktrees")
        .join(short_uuid(worker_id))
        .to_string_lossy()
        .to_string()
}

async fn run_git_command(project_path: &str, args: &[String], action: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_path)
        .args(args)
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to execute git command for {action}: git -C {project_path} {}",
                args.join(" ")
            )
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        return Err(anyhow!(
            "{action} failed (exit {code}) for git -C {project_path} {}. stdout: {} stderr: {}",
            args.join(" "),
            if stdout.is_empty() {
                "<empty>"
            } else {
                &stdout
            },
            if stderr.is_empty() {
                "<empty>"
            } else {
                &stderr
            },
        ));
    }

    Ok(stdout)
}

async fn detect_base_branch(project_path: &str) -> Result<String> {
    match run_git_command(
        project_path,
        &[
            "symbolic-ref".to_string(),
            "--short".to_string(),
            "HEAD".to_string(),
        ],
        "detect base branch",
    )
    .await
    {
        Ok(branch) if !branch.trim().is_empty() => Ok(branch),
        Ok(_) => Ok("main".to_string()),
        Err(_) => Ok("main".to_string()),
    }
}

async fn run_validation_commands(commands: &[String], cwd: &Path) -> Result<()> {
    for command in commands {
        let output = Command::new("bash")
            .arg("-lc")
            .arg(command)
            .current_dir(cwd)
            .output()
            .await
            .with_context(|| format!("failed to run validation command '{command}'"))?;

        if output.status.success() {
            continue;
        }

        let code = output.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let details = if stderr.is_empty() { stdout } else { stderr };
        let details = if details.is_empty() {
            "no stderr output".to_string()
        } else {
            details
        };

        return Err(anyhow!(
            "command '{command}' exited {code}. stderr: {details}"
        ));
    }
    Ok(())
}

fn effective_min_turns(defaults: &JobDefaultsConfig) -> u32 {
    match defaults.strategy.as_deref() {
        Some("explore-plan-execute") => 3,
        _ => defaults.min_turns.unwrap_or(0),
    }
}

fn continuation_prompt_for_turn(
    turns_completed: u32,
    defaults: &JobDefaultsConfig,
) -> Option<String> {
    let min_turns = effective_min_turns(defaults);
    if min_turns == 0 || turns_completed >= min_turns {
        return None;
    }

    if defaults.strategy.as_deref() == Some("explore-plan-execute") {
        let prompt = match turns_completed {
            1 => {
                "Continue your work. Phase 1 complete. Now create a detailed plan: list every file you will change and what changes you will make. Do NOT make code changes yet."
            }
            2 => {
                "Continue your work. Phase 2 complete. Execute your plan now: make the changes, verify they compile/run correctly, then commit."
            }
            _ => {
                "Continue your work. Review your changes, ensure they are complete and correct, then commit if not already done."
            }
        };
        return Some(prompt.to_string());
    }

    Some(format!(
        "Continue your work. You have completed turn {turns_completed} of at least {min_turns} required."
    ))
}

fn update_completed_turn_counter(
    completed_turns: u64,
    compact_after_turns: Option<u64>,
) -> (u64, bool) {
    let compact_after_turns = compact_after_turns.unwrap_or(0);
    if compact_after_turns == 0 {
        return (completed_turns, false);
    }

    let next = completed_turns.saturating_add(1);
    if next >= compact_after_turns {
        (0, true)
    } else {
        (next, false)
    }
}

fn callback_timeout_ms(custom_ms: Option<u64>) -> Option<u64> {
    let timeout_ms = custom_ms.unwrap_or(constants::DEFAULT_WORKER_CALLBACK_TIMEOUT_MS);
    if timeout_ms == 0 {
        None
    } else {
        Some(timeout_ms)
    }
}

async fn await_callback_future<T, F, E>(
    operation: &str,
    timeout_ms: Option<u64>,
    future: F,
) -> Result<T>
where
    F: Future<Output = std::result::Result<T, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    if let Some(timeout_ms) = callback_timeout_ms(timeout_ms) {
        let status = time::timeout(Duration::from_millis(timeout_ms), future)
            .await
            .with_context(|| format!("{operation} timed out after {timeout_ms}ms"))?;
        status.map_err(Into::into)
    } else {
        future.await.map_err(Into::into)
    }
}

fn json_compact_value(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
}

fn json_pretty_value(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
}

fn extract_item_payload(params: &Value) -> Option<&Value> {
    params
        .get("item")
        .or_else(|| params.get("msg").and_then(|msg| msg.get("item")))
}

fn is_agent_item_completion(item: &Value) -> bool {
    let role = item.get("role").and_then(Value::as_str);
    let item_type = item.get("type").and_then(Value::as_str);

    let Some(role) = role else {
        return match item_type {
            Some(item_type) if item_type.eq_ignore_ascii_case("usermessage") => false,
            Some("user_message") => false,
            _ => true,
        };
    };

    if role.eq_ignore_ascii_case("user") {
        return false;
    }

    match item_type {
        Some(item_type) if item_type.eq_ignore_ascii_case("usermessage") => false,
        Some("user_message") => false,
        _ => true,
    }
}

fn is_agent_item_completion_payload(params: &Value) -> bool {
    extract_item_payload(params).is_some_and(is_agent_item_completion)
}

fn extract_agent_result_text(method: &str, params: &Value) -> Option<String> {
    match method {
        constants::EVENT_METHOD_TURN_COMPLETED => extract_text_from_turn(
            params
                .get("turn")
                .or_else(|| params.get("msg").and_then(|msg| msg.get("turn")))
                .unwrap_or(params),
        )
        .or_else(|| extract_text_from_params(params, &["last_agent_message", "text", "result"]))
        .or_else(|| {
            params
                .get("item")
                .or_else(|| params.get("msg").and_then(|msg| msg.get("item")))
                .and_then(extract_text_from_item)
        }),
        constants::EVENT_METHOD_ITEM_AGENT_MESSAGE
        | constants::EVENT_METHOD_ITEM_AGENT_MESSAGE_DELTA => extract_text_from_params(
            params,
            &["text", "message", "delta", "snippet"],
        )
        .or_else(|| {
            extract_text_from_params(params.get("msg")?, &["text", "message", "delta", "snippet"])
        }),
        constants::EVENT_METHOD_ITEM_COMPLETED => extract_item_payload(params)
            .and_then(extract_text_from_item)
            .or_else(|| {
                params
                    .get("result")
                    .and_then(|result| result.get("aggregatedOutput"))
                    .and_then(Value::as_str)
                    .map(std::string::ToString::to_string)
            }),
        _ => None,
    }
}

fn extract_text_from_turn(value: &Value) -> Option<String> {
    if let Some(text) =
        extract_text_from_params(value, &["last_agent_message", "message", "text", "result"])
    {
        return Some(text);
    }

    let items = value.get("items")?;
    let items = items.as_array()?;
    for item in items.iter().rev() {
        if let Some(text) = extract_text_from_item(item) {
            return Some(text);
        }
    }

    None
}

fn extract_text_from_params(params: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| params.get(*key))
        .and_then(|value| {
            let value = match value {
                Value::String(text) => Some(text.as_str().to_string()),
                _ => value.as_str().map(std::string::ToString::to_string),
            };
            value.filter(|text| !text.trim().is_empty())
        })
}

fn extract_text_from_item(item: &Value) -> Option<String> {
    if let Some(text) = item
        .get("text")
        .and_then(Value::as_str)
        .or_else(|| item.get("message").and_then(Value::as_str))
        .or_else(|| item.get("aggregatedOutput").and_then(Value::as_str))
        .or_else(|| {
            item.get("result")
                .and_then(|result| result.get("text"))
                .and_then(Value::as_str)
        })
    {
        if text.trim().is_empty() {
            return None;
        }
        return Some(text.to_string());
    }

    let item_contents = item.get("content")?;
    let Value::Array(contents) = item_contents else {
        return None;
    };
    let mut message = String::new();
    for content in contents {
        if let Some(text) = content
            .get("text")
            .or_else(|| content.get("message"))
            .and_then(Value::as_str)
        {
            message.push_str(text);
        }
    }

    if message.trim().is_empty() {
        None
    } else {
        Some(message)
    }
}

fn completion_method_for_thread_status(status: &str) -> Option<&'static str> {
    match status {
        constants::EVENT_STATUS_COMPLETED => Some(constants::EVENT_METHOD_TURN_COMPLETED),
        constants::EVENT_STATUS_ABORTED
        | constants::EVENT_STATUS_INTERRUPTED
        | constants::EVENT_STATUS_FAILED
        | constants::EVENT_STATUS_ERROR
        | constants::EVENT_STATUS_STOPPED
        | constants::EVENT_STATUS_TIMEOUT
        | constants::EVENT_STATUS_TIMED_OUT => Some(constants::EVENT_METHOD_TURN_ABORTED),
        _ => None,
    }
}

fn is_worker_failure(result: &AgentResult) -> bool {
    result.completion_method.as_deref() == Some(constants::EVENT_METHOD_TURN_ABORTED)
        || matches!(
            result.status.as_str(),
            constants::AGENT_STATUS_ABORTED
                | constants::AGENT_STATUS_FAILED
                | constants::AGENT_STATUS_BLOCKED
        )
}

fn is_worker_success(result: &AgentResult) -> bool {
    result.completed_at.is_some() && !is_worker_failure(result)
}

fn assign_worker_names(workers: &[crate::models::ProjectJobWorkerSpec]) -> Result<Vec<String>> {
    let mut names = Vec::with_capacity(workers.len());
    let mut seen = std::collections::HashSet::new();
    for (index, worker) in workers.iter().enumerate() {
        let default_name = format!("worker-{}", index + 1);
        let name = worker
            .name
            .clone()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or(default_name);
        if !seen.insert(name.clone()) {
            return Err(anyhow!("duplicate worker name '{name}'"));
        }
        names.push(name);
    }
    Ok(names)
}

fn build_overlap_matrix(
    names: &[String],
    workers: &[crate::models::ProjectJobWorkerSpec],
    declared_scope_paths: &HashMap<String, Vec<String>>,
) -> Vec<OverlapRiskEntry> {
    let mut matrix = Vec::new();
    for i in 0..workers.len() {
        for j in (i + 1)..workers.len() {
            let left_name = names
                .get(i)
                .cloned()
                .unwrap_or_else(|| format!("worker-{}", i + 1));
            let right_name = names
                .get(j)
                .cloned()
                .unwrap_or_else(|| format!("worker-{}", j + 1));
            let mut reasons = Vec::new();
            let mut score = 0u8;

            let left_scope = combined_scope_paths(
                workers.get(i).cloned(),
                declared_scope_paths.get(left_name.as_str()),
            );
            let right_scope = combined_scope_paths(
                workers.get(j).cloned(),
                declared_scope_paths.get(right_name.as_str()),
            );
            let exact_overlap = left_scope
                .iter()
                .filter(|path| right_scope.contains(*path))
                .cloned()
                .collect::<Vec<_>>();
            if !exact_overlap.is_empty() {
                score = score.saturating_add(5);
                reasons.push(format!(
                    "declared path overlap: {}",
                    exact_overlap.join(", ")
                ));
            } else if left_scope.iter().any(|left| {
                right_scope
                    .iter()
                    .any(|right| left.starts_with(right) || right.starts_with(left))
            }) {
                score = score.saturating_add(3);
                reasons.push("scope path prefix overlap".to_string());
            }

            let left_hints = extract_path_hints(&workers[i].prompt);
            let right_hints = extract_path_hints(&workers[j].prompt);
            let shared_hints = left_hints
                .iter()
                .filter(|hint| right_hints.contains(*hint))
                .cloned()
                .collect::<Vec<_>>();
            if !shared_hints.is_empty() {
                score = score.saturating_add(2);
                reasons.push(format!("shared prompt path hints: {}", shared_hints.join(", ")));
            }

            let risk = if score >= 5 {
                "high".to_string()
            } else if score >= 3 {
                "medium".to_string()
            } else {
                "low".to_string()
            };
            matrix.push(OverlapRiskEntry {
                worker_a: left_name,
                worker_b: right_name,
                risk,
                score,
                reasons,
            });
        }
    }
    matrix
}

fn combined_scope_paths(
    worker: Option<crate::models::ProjectJobWorkerSpec>,
    declared_scope_paths: Option<&Vec<String>>,
) -> std::collections::HashSet<String> {
    let mut paths = std::collections::HashSet::new();
    if let Some(worker) = worker {
        for path in worker.scope_paths {
            let normalized = normalize_scope_path(path.as_str());
            if !normalized.is_empty() {
                paths.insert(normalized);
            }
        }
    }
    if let Some(extra) = declared_scope_paths {
        for path in extra {
            let normalized = normalize_scope_path(path.as_str());
            if !normalized.is_empty() {
                paths.insert(normalized);
            }
        }
    }
    paths
}

fn normalize_scope_path(path: &str) -> String {
    path.trim_matches(|ch: char| ch.is_whitespace() || ch == '"' || ch == '\'')
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
}

fn extract_path_hints(prompt: &str) -> std::collections::HashSet<String> {
    prompt
        .split_whitespace()
        .map(|token| token.trim_matches(|ch: char| ",:;()[]{}\"'`".contains(ch)))
        .filter(|token| token.contains('/') || token.ends_with(".rs") || token.ends_with(".md"))
        .map(normalize_scope_path)
        .filter(|token| !token.is_empty())
        .collect()
}

fn assert_dependency_graph_acyclic(dependency_graph: &HashMap<Uuid, Vec<Uuid>>) -> Result<()> {
    let mut marks = HashMap::<Uuid, u8>::new();
    for worker in dependency_graph.keys().copied() {
        if detect_cycle(worker, dependency_graph, &mut marks) {
            return Err(anyhow!("dependency graph contains a cycle"));
        }
    }
    Ok(())
}

fn detect_cycle(
    worker: Uuid,
    dependency_graph: &HashMap<Uuid, Vec<Uuid>>,
    marks: &mut HashMap<Uuid, u8>,
) -> bool {
    if matches!(marks.get(&worker), Some(1)) {
        return true;
    }
    if matches!(marks.get(&worker), Some(2)) {
        return false;
    }
    marks.insert(worker, 1);
    if let Some(deps) = dependency_graph.get(&worker) {
        for dep in deps {
            if detect_cycle(*dep, dependency_graph, marks) {
                return true;
            }
        }
    }
    marks.insert(worker, 2);
    false
}

fn build_completion_result(
    agent_id: Uuid,
    method: &str,
    turn_id: Option<String>,
    params: &Value,
    completed_at: u64,
    event_id: Uuid,
) -> AgentResult {
    AgentResult {
        agent_id,
        status: match method {
            constants::EVENT_METHOD_TURN_ABORTED => constants::AGENT_STATUS_ABORTED.to_string(),
            _ => constants::AGENT_STATUS_COMPLETED.to_string(),
        },
        completion_method: Some(method.to_string()),
        turn_id,
        final_text: extract_agent_result_text(method, params),
        summary: extract_result_summary(params),
        references: extract_result_references(params),
        completed_at: Some(completed_at),
        event_id: Some(event_id),
        error: None,
    }
}

fn update_agent_result_text(
    existing: Option<AgentResult>,
    agent_id: Uuid,
    method: &str,
    turn_id: Option<String>,
    params: &Value,
    event_id: Uuid,
    completed_at: u64,
) -> AgentResult {
    let mut result = existing.unwrap_or_else(|| AgentResult {
        agent_id,
        status: if method == constants::EVENT_METHOD_TURN_ABORTED {
            constants::AGENT_STATUS_ABORTED.to_string()
        } else {
            constants::AGENT_STATUS_COMPLETED.to_string()
        },
        completion_method: Some(method.to_string()),
        turn_id: turn_id.clone(),
        final_text: None,
        summary: None,
        references: None,
        completed_at: None,
        event_id: None,
        error: None,
    });

    result.turn_id = turn_id.or(result.turn_id);
    let snippet = extract_agent_result_text(method, params);
    match snippet {
        Some(text) if !text.trim().is_empty() => {
            let merged = match result.final_text {
                Some(existing_text) => format!("{existing_text}{text}"),
                None => text,
            };
            result.final_text = Some(merged);
        }
        _ => {}
    }

    if result.completed_at.is_none() {
        result.completed_at = Some(completed_at);
    }
    if result.summary.is_none() {
        result.summary = extract_result_summary(params);
    }
    if result.references.is_none() {
        result.references = extract_result_references(params);
    }
    result.event_id = Some(event_id);
    result
}

fn extract_result_summary(params: &Value) -> Option<String> {
    params
        .get("summary")
        .or_else(|| params.get("item").and_then(|item| item.get("summary")))
        .or_else(|| extract_item_payload(params).and_then(|item| item.get("summary")))
        .or_else(|| {
            params
                .get("result")
                .and_then(|result| result.get("summary"))
        })
        .and_then(Value::as_str)
        .map(std::string::ToString::to_string)
}

fn extract_result_references(params: &Value) -> Option<Vec<String>> {
    parse_references_value(params.get("references"))
        .or_else(|| {
            parse_references_value(params.get("item").and_then(|item| item.get("references")))
        })
        .or_else(|| {
            parse_references_value(
                extract_item_payload(params).and_then(|item| item.get("references")),
            )
        })
        .or_else(|| {
            parse_references_value(
                params
                    .get("result")
                    .and_then(|result| result.get("references")),
            )
        })
}

fn parse_references_value(value: Option<&Value>) -> Option<Vec<String>> {
    let value = value?;
    match value {
        Value::Array(values) => {
            let references = values
                .iter()
                .filter_map(Value::as_str)
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>();
            if references.is_empty() {
                None
            } else {
                Some(references)
            }
        }
        Value::String(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(vec![trimmed.to_string()])
            }
        }
        _ => None,
    }
}

fn parse_tool_call_summary(event: &crate::models::AgentEventDto) -> Option<ToolCallSummary> {
    if event.method != constants::EVENT_METHOD_ITEM_COMPLETED {
        return None;
    }

    let tool = normalize_tool_name(event.method.as_str(), &event.params)?;
    Some(ToolCallSummary {
        tool,
        command: parse_command(&event.params),
        path: parse_path(&event.params),
        at: event.ts,
    })
}

fn parse_worker_log_entry(
    event: &crate::models::AgentEventDto,
    turn: u32,
) -> Option<WorkerLogEntry> {
    let tool_call = parse_tool_call_summary(event)?;
    Some(WorkerLogEntry {
        turn,
        tool: tool_call.tool,
        command: tool_call.command,
        path: tool_call.path,
        exit_code: parse_exit_code(&event.params),
        stderr: parse_stderr(&event.params),
        bytes: parse_bytes(&event.params),
        at: event.ts,
    })
}

fn normalize_tool_name(method: &str, params: &Value) -> Option<String> {
    if !matches!(
        method,
        constants::EVENT_METHOD_ITEM_STARTED | constants::EVENT_METHOD_ITEM_COMPLETED
    ) {
        return None;
    }

    let item = extract_item_payload(params)?;
    let item_type = item.get("type").and_then(Value::as_str)?;

    match item_type {
        "commandExecution" => Some("exec_command".to_string()),
        "fileChange" => Some("apply_patch".to_string()),
        "dynamicToolCall" => item
            .get("tool")
            .and_then(Value::as_str)
            .map(std::string::ToString::to_string),
        "agentMessage" | "contextCompaction" => None,
        _ => None,
    }
}

fn is_command_execution_item_completed(method: &str, params: &Value) -> bool {
    if method != constants::EVENT_METHOD_ITEM_COMPLETED {
        return false;
    }

    extract_item_payload(params)
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        == Some("commandExecution")
}

fn tool_matches_filter(normalized_tool: &str, filter: &CallbackFilter) -> bool {
    normalized_tool.eq_ignore_ascii_case(filter.tool.as_str())
}

fn should_fire_tool_filter(
    debounce_map: &mut HashMap<(Uuid, String), u64>,
    agent_id: Uuid,
    normalized_tool: &str,
    command: Option<&str>,
    filter: &CallbackFilter,
    now_ms: u64,
) -> bool {
    if !tool_matches_filter(normalized_tool, filter) {
        return false;
    }
    if !command_matches_pattern(command, filter.command_pattern.as_str()) {
        return false;
    }

    let key = (agent_id, filter.name.clone());
    if let Some(window_ms) = filter.debounce_ms.filter(|window| *window > 0)
        && let Some(last_fired_ms) = debounce_map.get(&key)
        && now_ms.saturating_sub(*last_fired_ms) < window_ms
    {
        return false;
    }
    debounce_map.insert(key, now_ms);
    true
}

fn command_matches_pattern(command: Option<&str>, pattern: &str) -> bool {
    let Some(command) = command else {
        return false;
    };
    if pattern.trim().is_empty() {
        return true;
    }

    if pattern_looks_like_regex(pattern) {
        return RegexBuilder::new(strip_regex_delimiters(pattern))
            .case_insensitive(true)
            .build()
            .map(|regex| regex.is_match(command))
            .unwrap_or(false);
    }

    command
        .to_ascii_lowercase()
        .contains(&pattern.to_ascii_lowercase())
}

fn pattern_looks_like_regex(pattern: &str) -> bool {
    let trimmed = pattern.trim();
    if trimmed.starts_with('^') {
        return true;
    }
    if trimmed.starts_with('/') && trimmed.ends_with('/') && trimmed.len() > 1 {
        return true;
    }
    trimmed.chars().any(|ch| {
        matches!(
            ch,
            '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
        )
    })
}

fn strip_regex_delimiters(pattern: &str) -> &str {
    let trimmed = pattern.trim();
    if trimmed.starts_with('/') && trimmed.ends_with('/') && trimmed.len() > 1 {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

fn parse_command(params: &Value) -> Option<String> {
    if let Some(item) = extract_item_payload(params) {
        let command = item.get("command").and_then(Value::as_str);
        let arguments = parse_string_array(item.get("arguments"));

        if let Some(command) = command {
            if let Some(arguments) = arguments
                && !arguments.is_empty()
            {
                return Some(format!("{command} {}", arguments.join(" ")));
            }
            return Some(command.to_string());
        }

        if let Some(arguments) = arguments
            && !arguments.is_empty()
        {
            return Some(arguments.join(" "));
        }
    }

    params
        .get("command")
        .or_else(|| params.get("cmd"))
        .or_else(|| params.get("program"))
        .and_then(Value::as_str)
        .map(std::string::ToString::to_string)
}

fn parse_path(params: &Value) -> Option<String> {
    if let Some(path) = parse_change_paths(params).into_iter().next() {
        return Some(path);
    }

    params
        .get("path")
        .or_else(|| params.get("file"))
        .or_else(|| params.get("target"))
        .or_else(|| {
            extract_item_payload(params)
                .and_then(|item| item.get("path"))
                .or_else(|| extract_item_payload(params).and_then(|item| item.get("file")))
                .or_else(|| extract_item_payload(params).and_then(|item| item.get("target")))
        })
        .and_then(Value::as_str)
        .map(std::string::ToString::to_string)
}

fn parse_exit_code(params: &Value) -> Option<i32> {
    if let Some(exit_code) = extract_item_payload(params)
        .and_then(|item| item.get("exit_code").or_else(|| item.get("exitCode")))
        .or_else(|| {
            extract_item_payload(params).and_then(|item| {
                item.get("result")
                    .and_then(|result| result.get("exit_code").or_else(|| result.get("exitCode")))
            })
        })
        .and_then(Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
    {
        return Some(exit_code);
    }

    params
        .get("exit_code")
        .or_else(|| params.get("exitCode"))
        .or_else(|| params.get("code"))
        .and_then(Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
}

fn parse_stderr(params: &Value) -> Option<String> {
    if let Some(stderr) = extract_item_payload(params)
        .and_then(|item| item.get("stderr").or_else(|| item.get("error")))
        .or_else(|| {
            extract_item_payload(params)
                .and_then(|item| item.get("result").and_then(|result| result.get("stderr")))
        })
        .and_then(Value::as_str)
    {
        return Some(stderr.to_string());
    }

    params
        .get("stderr")
        .and_then(Value::as_str)
        .map(std::string::ToString::to_string)
}

fn parse_bytes(params: &Value) -> Option<usize> {
    if let Some(bytes) = extract_item_payload(params)
        .and_then(|item| {
            item.get("bytes")
                .or_else(|| item.get("size"))
                .or_else(|| item.get("output_bytes"))
        })
        .or_else(|| {
            extract_item_payload(params).and_then(|item| {
                item.get("result").and_then(|result| {
                    result
                        .get("bytes")
                        .or_else(|| result.get("size"))
                        .or_else(|| result.get("output_bytes"))
                })
            })
        })
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
    {
        return Some(bytes);
    }

    params
        .get("bytes")
        .or_else(|| params.get("size"))
        .or_else(|| params.get("output_bytes"))
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn parse_output_tail(params: &Value, max_lines: usize) -> Option<String> {
    let output = extract_item_payload(params)
        .and_then(|item| item.get("output"))
        .and_then(Value::as_str)?;

    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let start = lines.len().saturating_sub(max_lines);
    let tail = lines[start..].join("\n");
    if tail.trim().is_empty() {
        None
    } else {
        Some(tail)
    }
}

fn is_file_modifying_tool(tool: &str) -> bool {
    matches!(tool, "apply_patch" | "write_file")
}

fn parse_modified_paths(params: &Value, tool: &str) -> Vec<String> {
    if tool == "apply_patch" {
        let paths = parse_change_paths(params);
        if !paths.is_empty() {
            return paths;
        }
    }

    parse_path(params).into_iter().collect()
}

fn parse_change_paths(params: &Value) -> Vec<String> {
    extract_item_payload(params)
        .and_then(|item| item.get("changes"))
        .and_then(Value::as_array)
        .map(|changes| {
            changes
                .iter()
                .filter_map(|change| change.get("path").and_then(Value::as_str))
                .map(std::string::ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_string_array(value: Option<&Value>) -> Option<Vec<String>> {
    let values = value?.as_array()?;
    let collected: Vec<String> = values
        .iter()
        .filter_map(Value::as_str)
        .map(std::string::ToString::to_string)
        .collect();
    if collected.is_empty() {
        None
    } else {
        Some(collected)
    }
}

fn render_template(template: &str, vars: &HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (key, value) in vars {
        let token = ["{{", key.as_str(), "}}"].concat();
        result = result.replace(&token, value);
    }
    result
}

fn render_template_strict(template: &str, vars: &HashMap<String, String>) -> Result<String> {
    let rendered = render_template(template, vars);
    let unresolved = unresolved_template_tokens(&rendered);
    if unresolved.is_empty() {
        return Ok(rendered);
    }

    Err(anyhow!(
        "callback template contains unresolved tokens: {}",
        unresolved.join(", ")
    ))
}

fn unresolved_template_tokens(template: &str) -> Vec<String> {
    let mut unresolved = Vec::new();
    let mut cursor = 0;
    while let Some(open) = template[cursor..].find("{{") {
        let open = cursor + open;
        let search = open + 2;
        if let Some(close) = template[search..].find("}}") {
            let close = search + close;
            let token = template[open + 2..close].trim().to_string();
            if !token.is_empty() && !unresolved.contains(&token) {
                unresolved.push(token);
            }
            cursor = close + 2;
        } else {
            break;
        }
    }

    unresolved
}

fn agent_callback_profile(callback: &WorkerCallback) -> Option<String> {
    match callback {
        WorkerCallback::Profile(profile) => Some(profile.profile.clone()),
        _ => None,
    }
}

fn restore_agent_record(record: PersistedAgentRecord) -> Result<AgentRecord> {
    let role = match record.role.as_str() {
        constants::AGENT_ROLE_FOREMAN => AgentRole::Foreman,
        constants::AGENT_ROLE_WORKER => AgentRole::Worker,
        constants::AGENT_ROLE_STANDALONE => AgentRole::Standalone,
        _ => AgentRole::Standalone,
    };

    let id = Uuid::parse_str(&record.id).context("invalid persisted agent id")?;
    let thread_id = record.thread_id;
    let active_turn_id = record.active_turn_id;
    let status = record.status;
    let prompt = record.prompt;
    let cwd = record.cwd;
    let restart_attempts = record.restart_attempts;
    let project_id = if let Some(project_id) = record.project_id {
        Some(Uuid::parse_str(&project_id).context("invalid persisted project id")?)
    } else {
        None
    };
    let foreman_id = if let Some(foreman_id) = record.foreman_id {
        Some(Uuid::parse_str(&foreman_id).context("invalid persisted foreman id")?)
    } else {
        None
    };

    let callback =
        restore_persisted_callback(record.callback).context("invalid persisted callback")?;
    let job_id = if let Some(job_id) = record.job_id {
        Some(Uuid::parse_str(&job_id).context("invalid persisted agent job id")?)
    } else {
        None
    };
    let turns_completed = if record.turns_completed == 0 {
        record
            .events
            .iter()
            .filter(|event| event.method == constants::EVENT_METHOD_TURN_COMPLETED)
            .count() as u32
    } else {
        record.turns_completed
    };

    let model = record.model;
    let model_provider = record.model_provider;
    let governor_key = record
        .governor_key
        .or_else(|| governor_key(model_provider.as_deref(), model.as_deref()));
    let checkpoint = record
        .checkpoint
        .map(restore_worker_checkpoint)
        .unwrap_or(WorkerCheckpoint {
            last_successful_turn_id: record
                .events
                .iter()
                .rev()
                .find(|event| event.method == constants::EVENT_METHOD_TURN_COMPLETED)
                .and_then(|event| parse_turn_id(&event.params)),
            worktree_path: record.worktree_path.clone(),
            branch_name: record.branch_name.clone(),
            validation_retries: record.validation_retries,
            last_validation_error: record.last_validation_error.clone(),
            retry_count: 0,
        });

    Ok(AgentRecord {
        id,
        thread_id,
        active_turn_id,
        prompt,
        cwd,
        restart_attempts,
        turns_completed,
        validation_retries: record.validation_retries,
        last_validation_error: record.last_validation_error,
        worktree_path: record.worktree_path,
        branch_name: record.branch_name,
        model,
        model_provider,
        governor_key,
        governor_inflight: record.governor_inflight,
        throttled: record.throttled,
        retry_at_ms: record.retry_at_ms,
        throttle_reason: record.throttle_reason,
        failure_report: record.failure_report,
        budget: record
            .budget
            .map(restore_worker_budget)
            .unwrap_or_else(WorkerBudgetState::new),
        checkpoint,
        status,
        callback,
        role,
        project_id,
        foreman_id,
        error: record.error,
        result: record.result,
        job_id,
        created_at: if record.created_at == 0 {
            record.updated_at
        } else {
            record.created_at
        },
        updated_at: record.updated_at,
        events: VecDeque::from(record.events),
    })
}

fn restore_job_record(record: PersistedJobRecord) -> Result<JobRecord> {
    let id = Uuid::parse_str(&record.id).context("invalid persisted job id")?;
    let project_id = if let Some(project_id) = record.project_id {
        Some(Uuid::parse_str(&project_id).context("invalid persisted job project id")?)
    } else {
        None
    };

    let worker_ids = record
        .worker_ids
        .into_iter()
        .map(|value| Uuid::parse_str(&value))
        .collect::<Result<Vec<_>, _>>()
        .context("invalid persisted job worker id")?;

    let mut worker_labels = HashMap::new();
    for (worker_id, labels) in record.worker_labels {
        let worker_id =
            Uuid::parse_str(&worker_id).context("invalid persisted job worker id in labels")?;
        worker_labels.insert(worker_id, labels);
    }

    let mut worker_names = HashMap::new();
    for (worker_id, name) in record.worker_names {
        let worker_id = Uuid::parse_str(&worker_id)
            .context("invalid persisted job worker id in worker_names")?;
        worker_names.insert(worker_id, name);
    }

    let mut dependency_graph = HashMap::new();
    for (worker_id, deps) in record.dependency_graph {
        let worker_id =
            Uuid::parse_str(&worker_id).context("invalid persisted job worker id in deps")?;
        let parsed_deps = deps
            .into_iter()
            .map(|dep| Uuid::parse_str(&dep))
            .collect::<Result<Vec<_>, _>>()
            .context("invalid persisted job dependency worker id")?;
        dependency_graph.insert(worker_id, parsed_deps);
    }

    let mut blocked_reasons = HashMap::new();
    for (worker_id, reason) in record.blocked_reasons {
        let worker_id = Uuid::parse_str(&worker_id)
            .context("invalid persisted job worker id in blocked_reasons")?;
        blocked_reasons.insert(worker_id, reason);
    }

    Ok(JobRecord {
        id,
        project_id,
        status: record.status,
        worker_ids,
        worker_labels,
        merged_branches: record.merged_branches,
        merge_conflicts: record.merge_conflicts,
        worker_names,
        dependency_graph,
        blocked_reasons,
        base_branch: record.base_branch.unwrap_or_else(|| "main".to_string()),
        merge_strategy: record
            .merge_strategy
            .unwrap_or_else(|| "manual".to_string()),
        created_at: record.created_at,
        completed_at: record.completed_at,
        updated_at: record.updated_at,
    })
}

fn restore_persisted_callback(value: PersistedWorkerCallback) -> Result<WorkerCallback> {
    match value {
        PersistedWorkerCallback::None => Ok(WorkerCallback::None),
        PersistedWorkerCallback::Webhook {
            url,
            secret,
            timeout_ms,
            events,
            vars,
        } => Ok(WorkerCallback::Webhook(WorkerWebhookCallback {
            url,
            secret,
            timeout_ms,
            events,
            vars,
        })),
        PersistedWorkerCallback::UnixSocket {
            socket,
            timeout_ms,
            events,
            vars,
        } => Ok(WorkerCallback::UnixSocket(WorkerUnixSocketCallback {
            socket,
            events,
            timeout_ms,
            vars,
        })),
        PersistedWorkerCallback::Profile {
            profile,
            prompt_prefix,
            command_args,
            events,
            vars,
        } => Ok(WorkerCallback::Profile(WorkerProfileCallback {
            profile,
            prompt_prefix,
            command_args,
            events,
            vars,
        })),
    }
}

fn persisted_agent_record(agent: &AgentRecord) -> PersistedAgentRecord {
    let role = agent.role.as_str().to_string();
    let callback = persist_worker_callback(&agent.callback);
    PersistedAgentRecord {
        id: agent.id.to_string(),
        thread_id: agent.thread_id.clone(),
        active_turn_id: agent.active_turn_id.clone(),
        prompt: agent.prompt.clone(),
        cwd: agent.cwd.clone(),
        restart_attempts: agent.restart_attempts,
        turns_completed: agent.turns_completed,
        validation_retries: agent.validation_retries,
        last_validation_error: agent.last_validation_error.clone(),
        worktree_path: agent.worktree_path.clone(),
        branch_name: agent.branch_name.clone(),
        model: agent.model.clone(),
        model_provider: agent.model_provider.clone(),
        governor_key: agent.governor_key.clone(),
        governor_inflight: agent.governor_inflight,
        throttled: agent.throttled,
        retry_at_ms: agent.retry_at_ms,
        throttle_reason: agent.throttle_reason.clone(),
        failure_report: agent.failure_report.clone(),
        budget: Some(persist_worker_budget(&agent.budget)),
        checkpoint: Some(persist_worker_checkpoint(&agent.checkpoint)),
        status: agent.status.clone(),
        callback,
        role,
        project_id: agent.project_id.map(|id| id.to_string()),
        foreman_id: agent.foreman_id.map(|id| id.to_string()),
        error: agent.error.clone(),
        result: agent.result.clone(),
        job_id: agent.job_id.map(|id| id.to_string()),
        created_at: agent.created_at,
        updated_at: agent.updated_at,
        events: agent.events.iter().cloned().collect(),
    }
}

fn persisted_job_record(job: &JobRecord) -> PersistedJobRecord {
    PersistedJobRecord {
        id: job.id.to_string(),
        project_id: job.project_id.map(|id| id.to_string()),
        status: job.status.clone(),
        worker_ids: job.worker_ids.iter().map(|id| id.to_string()).collect(),
        worker_labels: job
            .worker_labels
            .iter()
            .map(|(id, labels)| (id.to_string(), labels.clone()))
            .collect(),
        merged_branches: job.merged_branches.clone(),
        merge_conflicts: job.merge_conflicts.clone(),
        base_branch: Some(job.base_branch.clone()),
        merge_strategy: Some(job.merge_strategy.clone()),
        worker_names: job
            .worker_names
            .iter()
            .map(|(id, name)| (id.to_string(), name.clone()))
            .collect(),
        dependency_graph: job
            .dependency_graph
            .iter()
            .map(|(id, deps)| {
                (
                    id.to_string(),
                    deps.iter().map(|dep| dep.to_string()).collect::<Vec<_>>(),
                )
            })
            .collect(),
        blocked_reasons: job
            .blocked_reasons
            .iter()
            .map(|(id, reason)| (id.to_string(), reason.clone()))
            .collect(),
        created_at: job.created_at,
        completed_at: job.completed_at,
        updated_at: job.updated_at,
    }
}

fn restore_governor_record(record: PersistedGovernorRecord) -> GovernorState {
    GovernorState {
        throttled_until_ms: record.throttled_until_ms,
        backoff_ms: record.backoff_ms,
        recent_errors: record.recent_errors,
        inflight_count: record.inflight_count,
    }
}

fn persisted_governor_record(state: &GovernorState) -> PersistedGovernorRecord {
    PersistedGovernorRecord {
        throttled_until_ms: state.throttled_until_ms,
        backoff_ms: state.backoff_ms,
        recent_errors: state.recent_errors,
        inflight_count: state.inflight_count,
    }
}

fn persist_worker_callback(callback: &WorkerCallback) -> PersistedWorkerCallback {
    match callback {
        WorkerCallback::None => PersistedWorkerCallback::None,
        WorkerCallback::Webhook(webhook) => PersistedWorkerCallback::Webhook {
            url: webhook.url.clone(),
            secret: webhook.secret.clone(),
            timeout_ms: webhook.timeout_ms,
            events: webhook.events.clone(),
            vars: webhook.vars.clone(),
        },
        WorkerCallback::UnixSocket(unix_socket) => PersistedWorkerCallback::UnixSocket {
            socket: unix_socket.socket.clone(),
            timeout_ms: unix_socket.timeout_ms,
            events: unix_socket.events.clone(),
            vars: unix_socket.vars.clone(),
        },
        WorkerCallback::Profile(profile) => PersistedWorkerCallback::Profile {
            profile: profile.profile.clone(),
            prompt_prefix: profile.prompt_prefix.clone(),
            command_args: profile.command_args.clone(),
            events: profile.events.clone(),
            vars: profile.vars.clone(),
        },
    }
}

fn restore_worker_budget(value: PersistedWorkerBudget) -> WorkerBudgetState {
    WorkerBudgetState {
        prompt_tokens: value.prompt_tokens,
        output_tokens: value.output_tokens,
        cached_tokens: value.cached_tokens,
        estimated_cost_usd: value.estimated_cost_usd,
        elapsed_ms: value.elapsed_ms,
        started_at_ms: now_ms().saturating_sub(value.elapsed_ms),
        exceeded: value.exceeded,
        exceeded_reason: value.exceeded_reason,
        paused_reason: value.paused_reason,
    }
}

fn persist_worker_budget(value: &WorkerBudgetState) -> PersistedWorkerBudget {
    PersistedWorkerBudget {
        prompt_tokens: value.prompt_tokens,
        output_tokens: value.output_tokens,
        cached_tokens: value.cached_tokens,
        estimated_cost_usd: value.estimated_cost_usd,
        elapsed_ms: value.elapsed_ms,
        exceeded: value.exceeded,
        exceeded_reason: value.exceeded_reason.clone(),
        paused_reason: value.paused_reason.clone(),
    }
}

fn restore_worker_checkpoint(value: PersistedWorkerCheckpoint) -> WorkerCheckpoint {
    WorkerCheckpoint {
        last_successful_turn_id: value.last_successful_turn_id,
        worktree_path: value.worktree_path,
        branch_name: value.branch_name,
        validation_retries: value.validation_retries,
        last_validation_error: value.last_validation_error,
        retry_count: value.retry_count,
    }
}

fn persist_worker_checkpoint(value: &WorkerCheckpoint) -> PersistedWorkerCheckpoint {
    PersistedWorkerCheckpoint {
        last_successful_turn_id: value.last_successful_turn_id.clone(),
        worktree_path: value.worktree_path.clone(),
        branch_name: value.branch_name.clone(),
        validation_retries: value.validation_retries,
        last_validation_error: value.last_validation_error.clone(),
        retry_count: value.retry_count,
    }
}

fn restore_project_record(record: PersistedProjectRecord) -> Result<ProjectRecord> {
    let id = Uuid::parse_str(&record.id).context("invalid persisted project id")?;
    let foreman_agent_id = record
        .foreman_agent_id
        .map(|value| Uuid::parse_str(&value))
        .transpose()
        .context("invalid persisted project foreman id")?;
    let worker_ids = record
        .worker_ids
        .into_iter()
        .map(|value| Uuid::parse_str(&value))
        .collect::<Result<Vec<_>, _>>()
        .context("invalid persisted project worker id")?;

    let path = PathBuf::from(&record.path);
    let config = ProjectConfig::load(&path).unwrap_or_else(|_| record.config.clone());

    let runtime = match config.load_runtime_files(&path) {
        Ok(live) => live,
        Err(_) => ProjectRuntimeFiles {
            path,
            foreman_prompt: record.runtime.foreman_prompt,
            worker_prompt: record.runtime.worker_prompt,
            runbook: record.runtime.runbook,
            handoff: record.runtime.handoff,
        },
    };

    Ok(ProjectRecord {
        id,
        path: record.path,
        name: record.name,
        status: record.status,
        foreman_agent_id,
        worker_ids,
        completed_worker_turns: record.completed_worker_turns,
        lifecycle_callback_status: normalize_project_lifecycle_callback_status(
            record.lifecycle_callback_status,
        ),
        config,
        runtime,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

fn persisted_project_record(project: &ProjectRecord) -> PersistedProjectRecord {
    PersistedProjectRecord {
        id: project.id.to_string(),
        path: project.path.clone(),
        name: project.name.clone(),
        status: project.status.clone(),
        foreman_agent_id: project.foreman_agent_id.map(|id| id.to_string()),
        worker_ids: project.worker_ids.iter().map(|id| id.to_string()).collect(),
        completed_worker_turns: project.completed_worker_turns,
        lifecycle_callback_status: project.lifecycle_callback_status.clone(),
        config: project.config.clone(),
        runtime: PersistedRuntimeFiles {
            foreman_prompt: project.runtime.foreman_prompt.clone(),
            worker_prompt: project.runtime.worker_prompt.clone(),
            runbook: project.runtime.runbook.clone(),
            handoff: project.runtime.handoff.clone(),
        },
        created_at: project.created_at,
        updated_at: project.updated_at,
    }
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn governor_key(provider: Option<&str>, model: Option<&str>) -> Option<String> {
    let provider = provider.map(str::trim).filter(|value| !value.is_empty())?;
    let model = model.map(str::trim).filter(|value| !value.is_empty());
    Some(match model {
        Some(model) => format!("{provider}/{model}"),
        None => provider.to_string(),
    })
}

fn is_rate_limit_error(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("rate limit")
        || normalized.contains("429")
        || normalized.contains("too many requests")
        || normalized.contains("throttle")
        || normalized.contains("quota")
}

fn governor_apply_rate_limit(state: &mut GovernorState, now_ms: u64) -> u64 {
    state.inflight_count = state.inflight_count.saturating_sub(1);
    state.recent_errors = state.recent_errors.saturating_add(1);
    let base = if state.backoff_ms == 0 {
        constants::GOVERNOR_BACKOFF_INITIAL_MS
    } else {
        state.backoff_ms.saturating_mul(2)
    };
    let capped = base.min(constants::GOVERNOR_BACKOFF_MAX_MS);
    let jitter_window = (capped / 5).max(25);
    let jitter = if jitter_window == 0 {
        0
    } else {
        now_ms % (jitter_window + 1)
    };
    state.backoff_ms = capped;
    state.throttled_until_ms = now_ms.saturating_add(capped.saturating_add(jitter));
    state.throttled_until_ms
}

fn governor_apply_success(state: &mut GovernorState, now_ms: u64) {
    state.recent_errors = state.recent_errors.saturating_sub(1);
    state.backoff_ms /= 2;
    if state.backoff_ms == 0 || state.recent_errors == 0 {
        state.backoff_ms = 0;
        if state.throttled_until_ms <= now_ms {
            state.throttled_until_ms = 0;
        }
    }
}

fn truncate_text(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        return text.to_string();
    }
    let mut out = text.chars().take(max_len).collect::<String>();
    out.push_str("...");
    out
}

fn truncate_json_value(value: &Value, max_len: usize) -> Value {
    match value {
        Value::Object(map) => {
            let mut truncated = serde_json::Map::new();
            for (key, value) in map {
                truncated.insert(key.clone(), truncate_json_value(value, max_len));
            }
            Value::Object(truncated)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .take(20)
                .map(|value| truncate_json_value(value, max_len))
                .collect(),
        ),
        Value::String(text) => Value::String(truncate_text(text, max_len)),
        _ => value.clone(),
    }
}

#[derive(Debug)]
struct BudgetEvaluation {
    exceeded: bool,
    reason: Option<String>,
}

fn evaluate_budget(
    budget: &WorkerBudgetState,
    max_tokens: u64,
    max_cost_usd: f64,
    max_duration_ms: u64,
) -> BudgetEvaluation {
    if budget.total_tokens() > max_tokens {
        return BudgetEvaluation {
            exceeded: true,
            reason: Some(format!(
                "token budget exceeded ({}/{})",
                budget.total_tokens(),
                max_tokens
            )),
        };
    }
    if budget.estimated_cost_usd > max_cost_usd {
        return BudgetEvaluation {
            exceeded: true,
            reason: Some(format!(
                "cost budget exceeded ({:.6}/{:.6})",
                budget.estimated_cost_usd, max_cost_usd
            )),
        };
    }
    if budget.elapsed_ms > max_duration_ms {
        return BudgetEvaluation {
            exceeded: true,
            reason: Some(format!(
                "duration budget exceeded ({}/{})",
                budget.elapsed_ms, max_duration_ms
            )),
        };
    }
    BudgetEvaluation {
        exceeded: false,
        reason: None,
    }
}

fn update_budget_counters(budget: &mut WorkerBudgetState, params: &Value) {
    if let Some(value) = find_numeric_key(params, &["prompt_tokens", "input_tokens"]) {
        budget.prompt_tokens = budget.prompt_tokens.saturating_add(value);
    }
    if let Some(value) = find_numeric_key(params, &["output_tokens", "completion_tokens"]) {
        budget.output_tokens = budget.output_tokens.saturating_add(value);
    }
    if let Some(value) = find_numeric_key(params, &["cached_tokens", "cache_read_tokens"]) {
        budget.cached_tokens = budget.cached_tokens.saturating_add(value);
    }

    if let Some(cost_usd) =
        find_float_key(params, &["cost_usd", "estimated_cost_usd", "usd", "cost"])
    {
        budget.estimated_cost_usd += cost_usd.max(0.0);
    } else {
        budget.estimated_cost_usd =
            budget.total_tokens() as f64 * constants::DEFAULT_ESTIMATED_COST_PER_TOKEN_USD;
    }
}

fn find_numeric_key(value: &Value, keys: &[&str]) -> Option<u64> {
    match value {
        Value::Object(map) => {
            for key in keys {
                if let Some(item) = map.get(*key) {
                    if let Some(v) = item.as_u64() {
                        return Some(v);
                    }
                    if let Some(v) = item.as_f64()
                        && v >= 0.0
                    {
                        return Some(v as u64);
                    }
                }
            }
            for nested in map.values() {
                if let Some(v) = find_numeric_key(nested, keys) {
                    return Some(v);
                }
            }
            None
        }
        Value::Array(items) => {
            for item in items {
                if let Some(v) = find_numeric_key(item, keys) {
                    return Some(v);
                }
            }
            None
        }
        _ => None,
    }
}

fn find_float_key(value: &Value, keys: &[&str]) -> Option<f64> {
    match value {
        Value::Object(map) => {
            for key in keys {
                if let Some(item) = map.get(*key) {
                    if let Some(v) = item.as_f64() {
                        return Some(v);
                    }
                    if let Some(v) = item.as_u64() {
                        return Some(v as f64);
                    }
                }
            }
            for nested in map.values() {
                if let Some(v) = find_float_key(nested, keys) {
                    return Some(v);
                }
            }
            None
        }
        Value::Array(items) => {
            for item in items {
                if let Some(v) = find_float_key(item, keys) {
                    return Some(v);
                }
            }
            None
        }
        _ => None,
    }
}

fn worker_budget_status(
    agent: &AgentRecord,
    defaults: &crate::config::BudgetDefaultsConfig,
) -> WorkerBudgetStatus {
    let on_exceed = match defaults.on_exceed {
        BudgetOnExceed::Warn => "warn",
        BudgetOnExceed::Pause => "pause",
        BudgetOnExceed::Kill => "kill",
    };
    WorkerBudgetStatus {
        usage: WorkerBudgetUsage {
            prompt_tokens: agent.budget.prompt_tokens,
            output_tokens: agent.budget.output_tokens,
            cached_tokens: agent.budget.cached_tokens,
            estimated_cost_usd: agent.budget.estimated_cost_usd,
            elapsed_ms: agent.budget.elapsed_ms,
        },
        limits: WorkerBudgetLimits {
            max_tokens: defaults.max_tokens,
            max_cost_usd: defaults.max_cost_usd,
            max_duration_ms: defaults.max_duration_ms,
            on_exceed: on_exceed.to_string(),
        },
        exceeded: agent.budget.exceeded,
        exceeded_reason: agent.budget.exceeded_reason.clone(),
    }
}

fn worker_budget_json(agent: &AgentRecord) -> Value {
    serde_json::json!({
        "prompt_tokens": agent.budget.prompt_tokens,
        "output_tokens": agent.budget.output_tokens,
        "cached_tokens": agent.budget.cached_tokens,
        "estimated_cost_usd": agent.budget.estimated_cost_usd,
        "elapsed_ms": agent.budget.elapsed_ms,
        "exceeded": agent.budget.exceeded,
        "exceeded_reason": agent.budget.exceeded_reason.clone(),
    })
}

fn classify_failure_from_error(error: Option<&str>, params: Option<&Value>) -> FailureClass {
    let status = params
        .and_then(|value| value.get("status").and_then(Value::as_str))
        .unwrap_or_default()
        .to_ascii_lowercase();
    if status == constants::EVENT_STATUS_TIMEOUT || status == constants::EVENT_STATUS_TIMED_OUT {
        return FailureClass::Timeout;
    }

    let normalized = error.unwrap_or_default().to_ascii_lowercase();
    if normalized.contains("rate limit")
        || normalized.contains("429")
        || normalized.contains("too many requests")
    {
        return FailureClass::RateLimit;
    }
    if normalized.contains("timeout") || normalized.contains("timed out") {
        return FailureClass::Timeout;
    }
    if normalized.contains("validation") {
        return FailureClass::ValidationFailed;
    }
    if normalized.contains("merge conflict") || normalized.contains("conflict") {
        return FailureClass::MergeConflict;
    }
    if normalized.contains("callback") {
        return FailureClass::CallbackError;
    }
    if normalized.contains("tool") || normalized.contains("command") {
        return FailureClass::ToolError;
    }
    FailureClass::Unknown
}

fn failure_summary_for_class(class: &FailureClass) -> &'static str {
    match class {
        FailureClass::RateLimit => "rate limit reached",
        FailureClass::ValidationFailed => "validation failed",
        FailureClass::MergeConflict => "merge conflict detected",
        FailureClass::Timeout => "operation timed out",
        FailureClass::ToolError => "tool execution failed",
        FailureClass::CallbackError => "callback execution failed",
        FailureClass::Unknown => "unknown worker failure",
    }
}

fn failure_class_retryable(class: &FailureClass) -> bool {
    matches!(
        class,
        FailureClass::RateLimit
            | FailureClass::Timeout
            | FailureClass::ToolError
            | FailureClass::CallbackError
    )
}

fn update_failure_report(
    slot: &mut Option<FailureReport>,
    class: FailureClass,
    summary: String,
    details: Option<String>,
    retryable: bool,
    ts: u64,
) {
    match slot {
        Some(existing) if existing.class == class && existing.summary == summary => {
            existing.last_seen = ts;
            existing.details = details.or_else(|| existing.details.clone());
            existing.retryable = retryable;
        }
        Some(existing) => {
            *existing = FailureReport {
                class,
                summary,
                details,
                first_seen: existing.first_seen,
                last_seen: ts,
                retryable,
            };
        }
        None => {
            *slot = Some(FailureReport {
                class,
                summary,
                details,
                first_seen: ts,
                last_seen: ts,
                retryable,
            });
        }
    }
}

fn apply_budget_transition(
    agent: &mut AgentRecord,
    on_exceed: &BudgetOnExceed,
    reason: &str,
    ts: u64,
) -> Option<(String, String)> {
    match on_exceed {
        BudgetOnExceed::Warn => {
            agent.error = Some(format!("budget warning: {reason}"));
            None
        }
        BudgetOnExceed::Pause => {
            agent.budget.paused_reason = Some(reason.to_string());
            if agent.active_turn_id.is_none() {
                agent.status = constants::AGENT_STATUS_PAUSED.to_string();
            }
            agent.error = Some(format!("budget paused: {reason}"));
            None
        }
        BudgetOnExceed::Kill => {
            let summary = format!("budget exceeded: {reason}");
            let class = if reason.contains("duration") {
                FailureClass::Timeout
            } else {
                FailureClass::Unknown
            };
            update_failure_report(
                &mut agent.failure_report,
                class,
                summary.clone(),
                Some(reason.to_string()),
                false,
                ts,
            );
            agent.status = constants::AGENT_STATUS_FAILED.to_string();
            agent.error = Some(summary.clone());
            let turn_id = agent.active_turn_id.clone();
            if let Some(result) = agent.result.as_mut() {
                result.status = constants::AGENT_STATUS_FAILED.to_string();
                result.error = Some(summary.clone());
            } else {
                agent.result = Some(AgentResult {
                    agent_id: agent.id,
                    status: constants::AGENT_STATUS_FAILED.to_string(),
                    completion_method: Some(constants::EVENT_METHOD_TURN_ABORTED.to_string()),
                    turn_id: turn_id.clone(),
                    final_text: None,
                    summary: Some("worker stopped by budget guardrail".to_string()),
                    references: None,
                    completed_at: Some(ts),
                    event_id: None,
                    error: Some(summary),
                });
            }

            turn_id.map(|turn_id| (agent.thread_id.clone(), turn_id))
        }
    }
}

fn progress_from_agent_event(
    agent: &AgentRecord,
    event: &crate::models::AgentEventDto,
) -> AgentProgressEnvelope {
    let validation = if agent.last_validation_error.is_some() || agent.validation_retries > 0 {
        Some(ProgressValidationState {
            retries: agent.validation_retries,
            last_error: agent.last_validation_error.clone(),
        })
    } else {
        None
    };
    let throttle = Some(ProgressThrottleState {
        throttled: agent.throttled,
        retry_at_ms: agent.retry_at_ms,
        reason: agent.throttle_reason.clone(),
    });
    AgentProgressEnvelope {
        agent_id: agent.id,
        ts: event.ts,
        event: event.method.clone(),
        status: agent.status.clone(),
        turn_id: parse_turn_id(&event.params).or_else(|| agent.active_turn_id.clone()),
        tool_call: parse_tool_call_summary(event),
        validation,
        throttle,
        budget: Some(worker_budget_json(agent)),
        payload: truncate_json_value(&event.params, 240),
    }
}

fn progress_validation_state(agent: &AgentRecord, event: &str) -> AgentProgressEnvelope {
    AgentProgressEnvelope {
        agent_id: agent.id,
        ts: now_ts(),
        event: event.to_string(),
        status: agent.status.clone(),
        turn_id: agent.active_turn_id.clone(),
        tool_call: None,
        validation: Some(ProgressValidationState {
            retries: agent.validation_retries,
            last_error: agent.last_validation_error.clone(),
        }),
        throttle: Some(ProgressThrottleState {
            throttled: agent.throttled,
            retry_at_ms: agent.retry_at_ms,
            reason: agent.throttle_reason.clone(),
        }),
        budget: Some(worker_budget_json(agent)),
        payload: serde_json::json!({}),
    }
}

fn progress_throttle_state(agent: &AgentRecord, event: &str) -> AgentProgressEnvelope {
    AgentProgressEnvelope {
        agent_id: agent.id,
        ts: now_ts(),
        event: event.to_string(),
        status: agent.status.clone(),
        turn_id: agent.active_turn_id.clone(),
        tool_call: None,
        validation: None,
        throttle: Some(ProgressThrottleState {
            throttled: agent.throttled,
            retry_at_ms: agent.retry_at_ms,
            reason: agent.throttle_reason.clone(),
        }),
        budget: Some(worker_budget_json(agent)),
        payload: serde_json::json!({}),
    }
}

fn progress_budget_state(agent: &AgentRecord, event: &str) -> AgentProgressEnvelope {
    AgentProgressEnvelope {
        agent_id: agent.id,
        ts: now_ts(),
        event: event.to_string(),
        status: agent.status.clone(),
        turn_id: agent.active_turn_id.clone(),
        tool_call: None,
        validation: None,
        throttle: Some(ProgressThrottleState {
            throttled: agent.throttled,
            retry_at_ms: agent.retry_at_ms,
            reason: agent.throttle_reason.clone(),
        }),
        budget: Some(worker_budget_json(agent)),
        payload: serde_json::json!({}),
    }
}

fn is_job_terminal_status(status: &str) -> bool {
    matches!(
        status,
        constants::JOB_STATUS_COMPLETED
            | constants::JOB_STATUS_PARTIAL
            | constants::JOB_STATUS_FAILED
    )
}

#[cfg(test)]
mod tests {
    use super::{
        AgentRecord, AgentRole, Foreman, GovernorState, JobRecord, WorkerBudgetState,
        WorkerCallback, WorkerCheckpoint, assign_worker_names, build_overlap_matrix, now_ts,
        apply_budget_transition, build_worker_branch_name, build_worker_worktree_path,
        callback_timeout_ms, classify_failure_from_error, continuation_prompt_for_turn,
        evaluate_budget, governor_apply_rate_limit, governor_apply_success, render_template,
        render_template_strict, resolve_agent_status, run_validation_commands,
        should_fire_tool_filter, unresolved_template_tokens, update_completed_turn_counter,
    };
    use crate::config::{BudgetOnExceed, CallbackFilter};
    use crate::constants;
    use crate::events::events_allowed;
    use crate::project::JobDefaultsConfig;
    use crate::state::{PersistedAgentRecord, PersistedState, PersistedWorkerCallback};
    use crate::{
        config::ServiceConfig,
        models::{
            AgentResult, FailureClass, ProjectJobWorkerSpec, RetryAgentRequest,
        },
    };
    use codex_api::AppServerClient;
    use serde_json::json;
    use std::collections::{HashMap, VecDeque};
    use std::path::PathBuf;
    use tempfile::tempdir;
    use tokio::sync::broadcast;
    use uuid::Uuid;

    #[test]
    fn compact_turn_counter_resets_on_threshold() {
        let (completed_turns, should_compact) = update_completed_turn_counter(0, Some(3));
        assert_eq!(completed_turns, 1);
        assert!(!should_compact);

        let (completed_turns, should_compact) = update_completed_turn_counter(3, Some(3));
        assert_eq!(completed_turns, 0);
        assert!(should_compact);
    }

    #[test]
    fn worker_branch_name_uses_short_job_and_worker_ids() {
        let job_id =
            Uuid::parse_str("12345678-90ab-cdef-1234-567890abcdef").expect("parse job uuid");
        let worker_id =
            Uuid::parse_str("abcdef12-3456-7890-abcd-ef1234567890").expect("parse worker uuid");
        let branch = build_worker_branch_name(job_id, worker_id);
        assert_eq!(branch, "foreman/12345678/abcdef12");
    }

    #[test]
    fn worker_worktree_path_uses_foreman_worktree_directory() {
        let worker_id =
            Uuid::parse_str("abcdef12-3456-7890-abcd-ef1234567890").expect("parse worker uuid");
        let path = build_worker_worktree_path("/tmp/example-project", worker_id);
        assert_eq!(path, "/tmp/example-project/.foreman/worktrees/abcdef12");
    }

    #[test]
    fn compact_turn_counter_ignores_zero_and_absent_threshold() {
        let (completed_turns, should_compact) = update_completed_turn_counter(10, None);
        assert_eq!(completed_turns, 10);
        assert!(!should_compact);

        let (completed_turns, should_compact) = update_completed_turn_counter(10, Some(0));
        assert_eq!(completed_turns, 10);
        assert!(!should_compact);
    }

    #[test]
    fn callback_timeout_respects_zero_as_no_timeout() {
        assert_eq!(
            callback_timeout_ms(None),
            Some(crate::constants::DEFAULT_WORKER_CALLBACK_TIMEOUT_MS)
        );
        assert_eq!(callback_timeout_ms(Some(12_000)), Some(12_000));
        assert_eq!(callback_timeout_ms(Some(0)), None);
    }

    #[test]
    fn events_allowed_respects_override_or_wildcard() {
        assert!(events_allowed("turn/completed", None, None));

        let callback_events = Some(vec!["turn/aborted".to_string()]);
        assert!(events_allowed(
            "turn/aborted",
            callback_events.as_deref(),
            None
        ));
        assert!(!events_allowed(
            "turn/completed",
            callback_events.as_deref(),
            None
        ));

        let wildcard_events = Some(vec!["*".to_string()]);
        assert!(events_allowed(
            "thread/status/changed",
            wildcard_events.as_deref(),
            None
        ));
    }

    #[test]
    fn normalize_event_method_strips_codex_prefix() {
        assert_eq!(
            Foreman::normalize_event_method("codex/event/turn/started").to_string(),
            "turn/started"
        );
        assert_eq!(
            Foreman::normalize_event_method("codex/event/item/agentMessage").to_string(),
            "item/agentMessage"
        );
        assert_eq!(
            Foreman::normalize_event_method("codex/event/turn/completed").to_string(),
            "turn/completed"
        );
        assert_eq!(
            Foreman::normalize_event_method("turn/completed").to_string(),
            "turn/completed"
        );
        assert_eq!(
            Foreman::normalize_event_method("codex/event/custom.method").to_string(),
            "custom.method"
        );
        assert_eq!(
            Foreman::normalize_event_method("item_agentMessage/delta").to_string(),
            "item_agentMessage/delta"
        );
    }

    #[test]
    fn normalize_event_method_leaves_native_methods_unchanged() {
        assert_eq!(
            Foreman::normalize_event_method("turn/completed").to_string(),
            "turn/completed"
        );
        assert_eq!(
            Foreman::normalize_event_method("item/agentMessage").to_string(),
            "item/agentMessage"
        );
    }

    #[test]
    fn extract_agent_result_text_turn_completed_uses_turn_items() {
        let payload = json!({
            "thread_id": "thread-1",
            "turn": {
                "id": "turn-1",
                "items": [
                    {
                        "id": "x",
                        "type": "commandExecution",
                        "aggregatedOutput": "ignored"
                    },
                    {
                        "id": "agent-message-1",
                        "type": "agentMessage",
                        "text": "final answer",
                    }
                ]
            }
        });

        let text = super::extract_agent_result_text("turn/completed", &payload);
        assert_eq!(text, Some("final answer".to_string()));
    }

    #[test]
    fn extract_agent_result_text_turn_completed_requires_turn_payload() {
        let payload = json!({
            "thread_id": "thread-1",
            "turn": {
                "id": "turn-1",
                "items": [
                    {
                        "id": "x",
                        "type": "agentMessage",
                        "text": "",
                    },
                    {
                        "id": "y",
                        "type": "agentMessage",
                        "message": "agent message",
                    }
                ]
            }
        });

        let text = super::extract_agent_result_text("turn/completed", &payload);
        assert_eq!(text, Some("agent message".to_string()));
    }

    #[test]
    fn extract_agent_result_text_item_completed_falls_back_to_text_fields() {
        let payload = json!({
            "thread_id": "thread-1",
            "item": {
                "id": "result-item",
                "type": "agentMessage",
                "content": [
                    {
                        "type": "text",
                        "message": "from content"
                    }
                ],
            }
        });

        let text = super::extract_agent_result_text("item/completed", &payload);
        assert_eq!(text, Some("from content".to_string()));
    }

    #[test]
    fn render_template_replaces_all_known_tokens() {
        let vars = [
            ("agent_id".to_string(), "agent-123".to_string()),
            ("event_id".to_string(), "event-456".to_string()),
            ("thread_id".to_string(), "thread-789".to_string()),
        ]
        .into_iter()
        .collect();
        let rendered = render_template(
            "agent={{agent_id}} event={{event_id}} thread={{thread_id}}",
            &vars,
        );
        assert_eq!(
            rendered,
            "agent=agent-123 event=event-456 thread=thread-789"
        );
    }

    #[test]
    fn render_template_ignores_unknown_tokens() {
        let vars = std::collections::HashMap::from([("known".to_string(), "x".to_string())]);
        let rendered = render_template("prefix {{known}} {{missing}} suffix", &vars);
        assert_eq!(rendered, "prefix x {{missing}} suffix");
    }

    #[test]
    fn render_template_strict_rejects_unresolved_tokens() {
        let vars = std::collections::HashMap::from([("known".to_string(), "x".to_string())]);
        let result = render_template_strict("prefix {{known}} {{missing}} suffix", &vars);
        assert!(result.is_err());
        let error = result.expect_err("render error");
        let message = error.to_string();
        assert!(message.contains("missing"));
    }

    #[test]
    fn unresolved_template_tokens_returns_unique_tokens() {
        let unresolved = unresolved_template_tokens("a {{first}} {{second}} {{first}} {{ third }}");
        assert_eq!(unresolved, vec!["first", "second", "third"]);
    }

    #[tokio::test]
    async fn validation_commands_fail_on_first_non_zero_exit() {
        let temp = tempdir().expect("tempdir");
        let commands = vec![
            "echo ok >/dev/null".to_string(),
            "printf 'bad news' >&2; exit 7".to_string(),
            "exit 0".to_string(),
        ];
        let err = run_validation_commands(&commands, temp.path())
            .await
            .expect_err("expected validation failure");
        let message = err.to_string();
        assert!(message.contains("exit"));
        assert!(message.contains("bad news"));
    }

    #[test]
    fn continuation_prompt_uses_strategy_and_min_turns() {
        let explore_defaults = JobDefaultsConfig {
            min_turns: None,
            strategy: Some("explore-plan-execute".to_string()),
            ..JobDefaultsConfig::default()
        };
        let first = continuation_prompt_for_turn(1, &explore_defaults).expect("turn 1 prompt");
        assert!(first.contains("Phase 1 complete"));
        let second = continuation_prompt_for_turn(2, &explore_defaults).expect("turn 2 prompt");
        assert!(second.contains("Phase 2 complete"));
        assert!(
            continuation_prompt_for_turn(3, &explore_defaults).is_none(),
            "strategy preset should allow completion at turn 3"
        );

        let generic_defaults = JobDefaultsConfig {
            min_turns: Some(4),
            strategy: Some("single".to_string()),
            ..JobDefaultsConfig::default()
        };
        let generic = continuation_prompt_for_turn(2, &generic_defaults).expect("generic prompt");
        assert!(generic.contains("turn 2 of at least 4"));
        assert!(continuation_prompt_for_turn(4, &generic_defaults).is_none());
    }

    #[test]
    fn governor_rate_limit_backoff_is_shared_and_increases() {
        let mut shared = GovernorState {
            inflight_count: 2,
            ..GovernorState::default()
        };
        let first_retry = governor_apply_rate_limit(&mut shared, 10_000);
        let first_backoff = shared.backoff_ms;
        assert!(
            first_backoff >= constants::GOVERNOR_BACKOFF_INITIAL_MS,
            "initial shared backoff should be set"
        );
        assert!(first_retry > 10_000);
        assert_eq!(shared.inflight_count, 1, "inflight decremented on throttle");

        let second_retry = governor_apply_rate_limit(&mut shared, 11_000);
        assert!(
            shared.backoff_ms >= first_backoff,
            "shared backoff should grow across worker errors"
        );
        assert!(
            second_retry > first_retry.saturating_sub(200),
            "retry window should move forward"
        );
    }

    #[test]
    fn governor_success_decays_backoff() {
        let mut shared = GovernorState {
            throttled_until_ms: 50_000,
            backoff_ms: 4_000,
            recent_errors: 2,
            inflight_count: 0,
        };
        governor_apply_success(&mut shared, 60_000);
        assert_eq!(shared.backoff_ms, 2_000);
        assert_eq!(shared.recent_errors, 1);
        governor_apply_success(&mut shared, 60_000);
        assert_eq!(shared.backoff_ms, 0);
        assert_eq!(shared.recent_errors, 0);
        assert_eq!(shared.throttled_until_ms, 0);
    }

    #[test]
    fn failure_classification_detects_common_error_types() {
        assert_eq!(
            classify_failure_from_error(Some("429 rate limit exceeded"), None),
            FailureClass::RateLimit
        );
        assert_eq!(
            classify_failure_from_error(Some("operation timed out"), None),
            FailureClass::Timeout
        );
        assert_eq!(
            classify_failure_from_error(Some("callback command failed"), None),
            FailureClass::CallbackError
        );
        assert_eq!(
            classify_failure_from_error(Some("mystery issue"), None),
            FailureClass::Unknown
        );
    }

    #[test]
    fn budget_enforcement_transitions_warn_pause_kill() {
        let mut warn_agent = sample_worker_agent();
        warn_agent.budget.prompt_tokens = 12;
        let warn_eval = evaluate_budget(&warn_agent.budget, 10, 1.0, 100_000);
        assert!(warn_eval.exceeded);
        let _ = apply_budget_transition(
            &mut warn_agent,
            &BudgetOnExceed::Warn,
            warn_eval.reason.as_deref().expect("warn reason"),
            1_700_000_100,
        );
        assert!(
            warn_agent
                .error
                .as_deref()
                .is_some_and(|text| text.contains("budget warning"))
        );
        assert_ne!(warn_agent.status, constants::AGENT_STATUS_PAUSED);
        assert_ne!(warn_agent.status, constants::AGENT_STATUS_FAILED);

        let mut pause_agent = sample_worker_agent();
        pause_agent.budget.prompt_tokens = 12;
        let pause_eval = evaluate_budget(&pause_agent.budget, 10, 1.0, 100_000);
        let _ = apply_budget_transition(
            &mut pause_agent,
            &BudgetOnExceed::Pause,
            pause_eval.reason.as_deref().expect("pause reason"),
            1_700_000_200,
        );
        assert_eq!(pause_agent.status, constants::AGENT_STATUS_PAUSED);
        assert!(pause_agent.budget.paused_reason.is_some());

        let mut kill_agent = sample_worker_agent();
        kill_agent.active_turn_id = Some("turn-9".to_string());
        kill_agent.budget.elapsed_ms = 999_999;
        let kill_eval = evaluate_budget(&kill_agent.budget, 100, 999.0, 10);
        let kill = apply_budget_transition(
            &mut kill_agent,
            &BudgetOnExceed::Kill,
            kill_eval.reason.as_deref().expect("kill reason"),
            1_700_000_300,
        );
        assert_eq!(kill_agent.status, constants::AGENT_STATUS_FAILED);
        assert!(kill_agent.failure_report.is_some());
        assert!(kill.is_some(), "kill should request turn interrupt");
    }

    #[test]
    fn tool_filter_matching_honors_tool_command_and_debounce() {
        let filter = CallbackFilter {
            name: "on-commit".to_string(),
            callback_profile: "wake-openclaw".to_string(),
            tool: "exec_command".to_string(),
            command_pattern: "git commit".to_string(),
            debounce_ms: Some(5_000),
        };
        let agent_id = Uuid::new_v4();
        let mut debounce_map: HashMap<(Uuid, String), u64> = HashMap::new();

        assert!(should_fire_tool_filter(
            &mut debounce_map,
            agent_id,
            "exec_command",
            Some("git commit -m 'msg'"),
            &filter,
            10_000,
        ));
        assert!(!should_fire_tool_filter(
            &mut debounce_map,
            agent_id,
            "exec_command",
            Some("git commit -m 'msg 2'"),
            &filter,
            12_000,
        ));
        assert!(should_fire_tool_filter(
            &mut debounce_map,
            agent_id,
            "exec_command",
            Some("git commit -m 'msg 3'"),
            &filter,
            16_000,
        ));
        assert!(!should_fire_tool_filter(
            &mut debounce_map,
            agent_id,
            "apply_patch",
            Some("git commit -m 'msg 4'"),
            &filter,
            20_000,
        ));
    }

    #[tokio::test]
    async fn get_agent_result_reflects_completed_status_from_persisted_record() {
        let fake_codex = test_fake_codex_binary();
        let state_dir = tempdir().expect("temp state directory");
        let state_path = state_dir.path().join("foreman-state.json");
        let state: PersistedState = build_recovery_state_for_completed_running_agent();
        let (event_tx, event_rx) = broadcast::channel(16);
        let client = AppServerClient::connect(&fake_codex, &[], event_tx.clone())
            .await
            .expect("connect fake app-server");

        let foreman = Foreman::new(
            client,
            event_rx,
            ServiceConfig::default(),
            state.clone(),
            state_path,
        );
        foreman.recover_state().await.expect("recover state");

        let persisted = state.agents.first().expect("agent present");
        let agent_id = Uuid::parse_str(&persisted.id).expect("agent id");
        let result = foreman
            .get_agent_result(agent_id)
            .await
            .expect("agent result");

        assert_eq!(result.status, "completed");
        assert_eq!(result.completion_method.as_deref(), Some("turn/completed"));
        assert!(result.completed_at.is_some());
    }

    #[tokio::test]
    async fn reload_project_config_returns_error_when_project_not_found() {
        let fake_codex = test_fake_codex_binary();
        let state_dir = tempdir().expect("temp state directory");
        let state_path = state_dir.path().join("foreman-state.json");
        let (event_tx, event_rx) = broadcast::channel(16);
        let client = AppServerClient::connect(&fake_codex, &[], event_tx.clone())
            .await
            .expect("connect fake app-server");
        let foreman = Foreman::new(
            client,
            event_rx,
            ServiceConfig::default(),
            PersistedState::default(),
            state_path,
        );

        let err = foreman
            .reload_project_config(Uuid::new_v4())
            .await
            .expect_err("reload should fail for unknown project");
        assert!(
            err.to_string().contains("project not found"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn retry_mode_selects_last_turn_when_checkpoint_missing_and_checkpoint_when_present() {
        let fake_codex = test_fake_codex_binary();
        let state_dir = tempdir().expect("temp state directory");
        let state_path = state_dir.path().join("foreman-state.json");
        let (event_tx, event_rx) = broadcast::channel(16);
        let client = AppServerClient::connect(&fake_codex, &[], event_tx.clone())
            .await
            .expect("connect fake app-server");
        let foreman = Foreman::new(
            client,
            event_rx,
            ServiceConfig::default(),
            PersistedState::default(),
            state_path,
        );

        let spawned = foreman
            .spawn_agent(crate::models::SpawnAgentRequest {
                prompt: "implement changes".to_string(),
                model: None,
                model_provider: None,
                cwd: None,
                sandbox: None,
                callback_profile: None,
                callback_prompt_prefix: None,
                callback_args: None,
                callback_vars: None,
                callback_events: None,
            })
            .await
            .expect("spawn agent");

        {
            let mut agents = foreman.agents.write().await;
            let agent = agents.get_mut(&spawned.id).expect("agent");
            agent.checkpoint.last_successful_turn_id = None;
            agent.checkpoint.worktree_path = None;
            agent.checkpoint.branch_name = None;
        }

        let without_checkpoint = foreman
            .retry_agent(
                spawned.id,
                RetryAgentRequest {
                    mode: "checkpoint".to_string(),
                },
            )
            .await
            .expect("retry should fallback");
        assert_eq!(without_checkpoint.selected_mode, "last-turn");
        assert!(!without_checkpoint.checkpoint_available);

        {
            let mut agents = foreman.agents.write().await;
            let agent = agents.get_mut(&spawned.id).expect("agent");
            agent.checkpoint.last_successful_turn_id = Some("turn-checkpoint".to_string());
        }

        let with_checkpoint = foreman
            .retry_agent(
                spawned.id,
                RetryAgentRequest {
                    mode: "checkpoint".to_string(),
                },
            )
            .await
            .expect("retry with checkpoint");
        assert_eq!(with_checkpoint.selected_mode, "checkpoint");
        assert!(with_checkpoint.checkpoint_available);
    }

    #[test]
    fn overlap_matrix_marks_scope_overlap_as_high_risk() {
        let workers = vec![
            ProjectJobWorkerSpec {
                name: Some("worker-a".to_string()),
                prompt: "Update src/core/lib.rs and docs/guide.md".to_string(),
                depends_on: Vec::new(),
                scope_paths: vec!["src/core".to_string()],
                labels: HashMap::new(),
                callback_overrides: crate::models::CallbackOverrides::default(),
                model: None,
                model_provider: None,
                cwd: None,
                worktree: None,
                sandbox: None,
            },
            ProjectJobWorkerSpec {
                name: Some("worker-b".to_string()),
                prompt: "Refactor src/core/lib.rs API".to_string(),
                depends_on: Vec::new(),
                scope_paths: vec!["src/core/lib.rs".to_string()],
                labels: HashMap::new(),
                callback_overrides: crate::models::CallbackOverrides::default(),
                model: None,
                model_provider: None,
                cwd: None,
                worktree: None,
                sandbox: None,
            },
        ];
        let names = assign_worker_names(&workers).expect("assign names");
        let matrix = build_overlap_matrix(&names, &workers, &HashMap::new());
        assert_eq!(matrix.len(), 1);
        assert_eq!(matrix[0].risk, "high");
        assert!(
            matrix[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("overlap"))
        );
    }

    #[tokio::test]
    async fn dependency_failure_blocks_downstream_worker() {
        let fake_codex = test_fake_codex_binary();
        let state_dir = tempdir().expect("temp state directory");
        let state_path = state_dir.path().join("foreman-state.json");
        let (event_tx, event_rx) = broadcast::channel(16);
        let client = AppServerClient::connect(&fake_codex, &[], event_tx.clone())
            .await
            .expect("connect fake app-server");
        let foreman = Foreman::new(
            client,
            event_rx,
            ServiceConfig::default(),
            PersistedState::default(),
            state_path,
        );

        let upstream_id = Uuid::new_v4();
        let downstream_id = Uuid::new_v4();
        let job_id = Uuid::new_v4();

        {
            let mut agents = foreman.agents.write().await;
            let mut upstream = sample_worker_agent();
            upstream.id = upstream_id;
            upstream.result = Some(AgentResult {
                agent_id: upstream_id,
                status: constants::AGENT_STATUS_FAILED.to_string(),
                completion_method: Some(constants::EVENT_METHOD_TURN_ABORTED.to_string()),
                turn_id: Some("turn-upstream".to_string()),
                final_text: None,
                summary: None,
                references: None,
                completed_at: Some(now_ts()),
                event_id: None,
                error: Some("failure".to_string()),
            });

            let mut downstream = sample_worker_agent();
            downstream.id = downstream_id;
            downstream.prompt = Some("downstream task".to_string());

            agents.insert(upstream_id, upstream);
            agents.insert(downstream_id, downstream);
        }

        {
            let mut jobs = foreman.jobs.write().await;
            jobs.insert(
                job_id,
                JobRecord {
                    id: job_id,
                    project_id: None,
                    status: constants::JOB_STATUS_RUNNING.to_string(),
                    worker_ids: vec![upstream_id, downstream_id],
                    worker_labels: HashMap::new(),
                    merged_branches: Vec::new(),
                    merge_conflicts: Vec::new(),
                    worker_names: HashMap::from([
                        (upstream_id, "upstream".to_string()),
                        (downstream_id, "downstream".to_string()),
                    ]),
                    dependency_graph: HashMap::from([(downstream_id, vec![upstream_id])]),
                    blocked_reasons: HashMap::new(),
                    base_branch: "main".to_string(),
                    merge_strategy: "manual".to_string(),
                    created_at: now_ts(),
                    completed_at: None,
                    updated_at: now_ts(),
                },
            );
        }

        foreman
            .launch_runnable_workers(job_id)
            .await
            .expect("launch runnable");

        let downstream = foreman
            .agents
            .read()
            .await
            .get(&downstream_id)
            .cloned()
            .expect("downstream");
        assert_eq!(downstream.status, constants::AGENT_STATUS_BLOCKED);

        let job = foreman.jobs.read().await.get(&job_id).cloned().expect("job");
        assert!(job.blocked_reasons.contains_key(&downstream_id));
    }

    #[test]
    fn resolve_agent_status_prefers_result_status_when_completed() {
        let agent_id = Uuid::new_v4();
        let agent = AgentRecord {
            id: agent_id,
            thread_id: "thread-1".to_string(),
            active_turn_id: None,
            prompt: None,
            cwd: None,
            restart_attempts: 0,
            turns_completed: 0,
            validation_retries: 0,
            last_validation_error: None,
            worktree_path: None,
            branch_name: None,
            model: Some("gpt-5".to_string()),
            model_provider: Some("openai".to_string()),
            governor_key: Some("openai/gpt-5".to_string()),
            governor_inflight: false,
            throttled: false,
            retry_at_ms: None,
            throttle_reason: None,
            failure_report: None,
            budget: WorkerBudgetState::new(),
            checkpoint: WorkerCheckpoint::default(),
            status: "running".to_string(),
            callback: WorkerCallback::None,
            role: AgentRole::Worker,
            project_id: None,
            foreman_id: None,
            error: None,
            result: Some(AgentResult {
                agent_id,
                status: "completed".to_string(),
                completion_method: Some("turn/completed".to_string()),
                turn_id: None,
                final_text: None,
                summary: None,
                references: None,
                completed_at: Some(1_700_000_000),
                event_id: None,
                error: None,
            }),
            job_id: None,
            created_at: 0,
            updated_at: 0,
            events: VecDeque::new(),
        };

        assert_eq!(resolve_agent_status(&agent), "completed");
    }

    fn sample_worker_agent() -> AgentRecord {
        AgentRecord {
            id: Uuid::new_v4(),
            thread_id: "thread-budget".to_string(),
            active_turn_id: None,
            prompt: None,
            cwd: None,
            restart_attempts: 0,
            turns_completed: 0,
            validation_retries: 0,
            last_validation_error: None,
            worktree_path: None,
            branch_name: None,
            model: Some("gpt-5".to_string()),
            model_provider: Some("openai".to_string()),
            governor_key: Some("openai/gpt-5".to_string()),
            governor_inflight: false,
            throttled: false,
            retry_at_ms: None,
            throttle_reason: None,
            failure_report: None,
            budget: WorkerBudgetState::new(),
            checkpoint: WorkerCheckpoint::default(),
            status: constants::AGENT_STATUS_IDLE.to_string(),
            callback: WorkerCallback::None,
            role: AgentRole::Worker,
            project_id: None,
            foreman_id: None,
            error: None,
            result: None,
            job_id: None,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            events: VecDeque::new(),
        }
    }

    fn test_fake_codex_binary() -> String {
        let candidate_paths = [
            std::env::var("CARGO_BIN_EXE_fake_codex").ok(),
            std::env::var("CARGO_BIN_EXE_fake-codex").ok(),
        ];
        for candidate in candidate_paths.into_iter().flatten() {
            if !candidate.trim().is_empty() {
                return candidate;
            }
        }

        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let binary = if cfg!(windows) {
            manifest_dir
                .join("target")
                .join("debug")
                .join("fake_codex.exe")
        } else {
            manifest_dir.join("target").join("debug").join("fake_codex")
        };
        assert!(
            binary.exists(),
            "fake_codex binary not found at {}",
            binary.display()
        );
        binary.to_string_lossy().to_string()
    }

    fn build_recovery_state_for_completed_running_agent() -> PersistedState {
        PersistedState {
            version: 1,
            generated_at: 1_700_000_000,
            agents: vec![PersistedAgentRecord {
                id: Uuid::new_v4().to_string(),
                thread_id: "thread-recovery".to_string(),
                active_turn_id: Some("turn-0".to_string()),
                prompt: Some("recovery prompt".to_string()),
                cwd: None,
                restart_attempts: 0,
                turns_completed: 0,
                validation_retries: 0,
                last_validation_error: None,
                worktree_path: None,
                branch_name: None,
                model: Some("gpt-5".to_string()),
                model_provider: Some("openai".to_string()),
                governor_key: Some("openai/gpt-5".to_string()),
                governor_inflight: false,
                throttled: false,
                retry_at_ms: None,
                throttle_reason: None,
                failure_report: None,
                budget: None,
                checkpoint: None,
                status: "running".to_string(),
                callback: PersistedWorkerCallback::None,
                role: "standalone".to_string(),
                project_id: None,
                foreman_id: None,
                error: None,
                result: Some(AgentResult {
                    agent_id: Uuid::new_v4(),
                    status: "completed".to_string(),
                    completion_method: Some("turn/completed".to_string()),
                    turn_id: Some("turn-0".to_string()),
                    final_text: Some("recovered".to_string()),
                    summary: None,
                    references: None,
                    completed_at: Some(1_700_000_001),
                    event_id: Some(Uuid::new_v4()),
                    error: None,
                }),
                job_id: None,
                created_at: 1_700_000_000,
                updated_at: 1_700_000_001,
                events: Vec::new(),
            }],
            projects: Vec::new(),
            jobs: Vec::new(),
            governors: HashMap::new(),
        }
    }
}
