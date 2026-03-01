use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    fs,
    future::Future,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use codex_api::{
    AppServerClient, EmptyResponse, RawNotification, TextPayload, ThreadStartRequest,
    TurnInterruptRequest, TurnStartRequest, TurnSteerRequest, parse_thread_id, parse_turn_id,
};
use reqwest::Client as HttpClient;
use serde_json::Value;
use tokio::{
    process::Command,
    sync::{RwLock, broadcast},
    time,
};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    config::{CallbackProfile, CommandCallbackProfile, ServiceConfig, WebhookCallbackProfile},
    events::events_allowed,
    models::{
        AgentResult, AgentState, CallbackOverrides, CompactProjectRequest,
        CreateProjectJobsRequest, CreateProjectJobsResponse, CreateProjectResponse, JobResult,
        JobState, ProjectState, SendAgentInput, SpawnAgentRequest, SpawnAgentResponse,
        SpawnProjectRequest, SpawnProjectWorkerRequest, SpawnProjectWorkerResponse,
        SteerAgentInput,
    },
    project::{CallbackSpec, ProjectConfig, ProjectRuntimeFiles},
    state::{
        PersistedAgentRecord, PersistedJobRecord, PersistedProjectRecord, PersistedRuntimeFiles,
        PersistedState, PersistedWorkerCallback,
    },
};

const MAX_EVENTS: usize = 50;
const DEFAULT_CALLBACK_TIMEOUT_MS: u64 = 5_000;
const TURN_COMPLETED_EVENT: &str = "turn/completed";
const TURN_ABORTED_EVENT: &str = "turn/aborted";

#[derive(Debug, Clone)]
struct WorkerWebhookCallback {
    url: String,
    secret: Option<String>,
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
    Profile(WorkerProfileCallback),
}

#[derive(Debug, Clone)]
enum AgentRole {
    Foreman,
    Worker,
    Standalone,
}

impl AgentRole {
    fn as_str(&self) -> &'static str {
        match self {
            AgentRole::Foreman => "foreman",
            AgentRole::Worker => "worker",
            AgentRole::Standalone => "standalone",
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
    status: String,
    callback: WorkerCallback,
    role: AgentRole,
    project_id: Option<Uuid>,
    foreman_id: Option<Uuid>,
    error: Option<String>,
    result: Option<AgentResult>,
    job_id: Option<Uuid>,
    updated_at: u64,
    events: VecDeque<crate::models::AgentEventDto>,
}

#[derive(Debug, Clone)]
struct JobRecord {
    id: Uuid,
    project_id: Option<Uuid>,
    status: String,
    worker_ids: Vec<Uuid>,
    worker_labels: HashMap<Uuid, HashMap<String, String>>,
    created_at: u64,
    completed_at: Option<u64>,
    updated_at: u64,
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
            started_at: now_ts(),
            state_path,
            recovery_state: RwLock::new(persisted_state),
        });

        Self::spawn_event_loop(Arc::clone(&foreman), event_rx);
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

        self.schedule_state_persist();
        Ok(())
    }

    fn spawn_event_loop(this: Arc<Self>, mut event_rx: broadcast::Receiver<RawNotification>) {
        tokio::spawn(async move {
            loop {
                match event_rx.recv().await {
                    Ok(event) => this.route_notification(event).await,
                    Err(err) => {
                        warn!(%err, "event channel closed");
                        break;
                    }
                }
            }
        });
    }

    fn schedule_state_persist(self: &Arc<Self>) {
        let foreman = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = foreman.persist_state().await {
                warn!(%err, "failed to persist foreman state");
            }
        });
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

        let state = PersistedState {
            version: 1,
            generated_at: now_ts(),
            agents,
            projects,
            jobs,
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
            debug!(
                method = %event.method,
                normalized_method = %normalized_method,
                "dropping notification without thread id"
            );
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
        while queue.len() > MAX_EVENTS {
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
        if let Some(rest) = method.strip_prefix("codex/event/") {
            return match rest {
                "task_started" | "turn_started" => Cow::Borrowed("turn/started"),
                "task_complete" | "turn_complete" => Cow::Borrowed("turn/completed"),
                "item_started" => Cow::Borrowed("item/started"),
                "item_completed" => Cow::Borrowed("item/completed"),
                "item_agentMessage" => Cow::Borrowed("item/agentMessage"),
                "item_agentMessage/delta" => Cow::Borrowed("item/agentMessage/delta"),
                _ => Cow::Borrowed(method),
            };
        }

        Cow::Borrowed(method)
    }

    async fn dispatch_event(self: &Arc<Self>, agent_id: Uuid, method: String, params: Value) {
        let ts = now_ts();
        let event_id = Uuid::new_v4();
        let mut callback_context: Option<CallbackEventContext> = None;
        {
            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&agent_id) {
                agent.updated_at = ts;
                agent.events.push_back(crate::models::AgentEventDto {
                    ts,
                    method: method.clone(),
                    params: params.clone(),
                });
                while agent.events.len() > MAX_EVENTS {
                    agent.events.pop_front();
                }

                if matches!(method.as_str(), TURN_COMPLETED_EVENT | TURN_ABORTED_EVENT) {
                    agent.status = "idle".into();
                    agent.result = Some(build_completion_result(
                        agent.id,
                        &method,
                        parse_turn_id(&params).or_else(|| agent.active_turn_id.clone()),
                        &params,
                        ts,
                        event_id,
                    ));
                }
                if method == "thread/status/changed"
                    && let Some(status) = params.get("status").and_then(|value| value.as_str())
                {
                    agent.status = status.to_string();
                }
                if method == "turn/started"
                    && let Some(turn_id) = parse_turn_id(&params)
                {
                    agent.status = "running".into();
                    agent.active_turn_id = Some(turn_id);
                    agent.result = None;
                }
                if method == "item/agentMessage"
                    || method == "item/agentMessage/delta"
                    || (method == "item/completed" && params.get("item").is_some())
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
                    method,
                    ts,
                    params,
                    event_id,
                    result_snapshot,
                    callback: agent.callback.clone(),
                });
            }
        }

        if let Some(context) = callback_context {
            if matches!(
                context.method.as_str(),
                TURN_COMPLETED_EVENT | TURN_ABORTED_EVENT
            ) {
                if let Some(job_id) = context.job_id {
                    let _ = self
                        .update_job_after_worker_event(job_id, context.result_snapshot.as_ref())
                        .await;
                }
                if let Some(project_id) = context.project_id {
                    let _ = self.update_project_after_worker_event(project_id).await;
                }
            }
            self.schedule_state_persist();
            let foreman = Arc::clone(self);
            tokio::spawn(async move {
                if let Err(err) = foreman.execute_event(context).await {
                    warn!(%err, "callback execution failed");
                }
            });
        }
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

    async fn update_job_after_worker_event(
        &self,
        job_id: Uuid,
        result_snapshot: Option<&AgentResult>,
    ) -> Result<()> {
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
                job.status = "completed".to_string();
                job.completed_at = Some(job.completed_at.unwrap_or_else(now_ts));
                job.updated_at = now_ts();
            }
            return Ok(());
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
                    if matches!(result.status.as_str(), "running" | "interrupted") {
                        has_running = true;
                    }
                    if result.completion_method.as_deref() == Some("turn/aborted") {
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
                if has_running {
                    job.status = "running".to_string();
                } else {
                    job.status = if has_failure {
                        "failed".to_string()
                    } else {
                        "completed".to_string()
                    };
                    job.completed_at = Some(job.completed_at.unwrap_or_else(now_ts));
                }
                job.updated_at = now_ts();
            }
        }
        Ok(())
    }

    async fn update_project_after_worker_event(&self, project_id: Uuid) -> Result<()> {
        let mut projects = self.projects.write().await;
        let project = projects.get_mut(&project_id).context("project not found")?;
        project.updated_at = now_ts();
        Ok(())
    }

    async fn execute_agent_callback(self: &Arc<Self>, context: CallbackEventContext) -> Result<()> {
        let callback = context.callback.clone();

        match callback {
            WorkerCallback::None => Ok(()),
            WorkerCallback::Webhook(webhook) => {
                if !events_allowed(context.method.as_str(), webhook.events.as_deref(), None) {
                    return Ok(());
                }
                self.dispatch_webhook_callback(&context, webhook).await
            }
            WorkerCallback::Profile(profile) => {
                let profile_definition = self
                    .config
                    .get_callback_profile(&profile.profile)
                    .with_context(|| {
                        format!("callback profile '{}' is not configured", profile.profile)
                    })?;

                let profile_events = match &profile_definition {
                    CallbackProfile::Webhook(webhook) => webhook.events.as_deref(),
                    CallbackProfile::Command(command) => command.events.as_deref(),
                };

                if !events_allowed(
                    context.method.as_str(),
                    profile.events.as_deref(),
                    profile_events,
                ) {
                    return Ok(());
                }

                match profile_definition {
                    CallbackProfile::Command(command) => {
                        self.dispatch_command_callback(context, profile, command.clone())
                            .await
                    }
                    CallbackProfile::Webhook(webhook) => {
                        self.dispatch_webhook_callback_from_profile(&context, &profile, webhook)
                            .await
                    }
                }
            }
        }
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
            let vars = self
                .project_event_vars(&project, &context)
                .context("failed to build project event vars")?;

            let event_payload = json_compact_value(&context.params);

            let should_compact = matches!(
                context.method.as_str(),
                TURN_COMPLETED_EVENT | TURN_ABORTED_EVENT
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
                                reason: Some("compact_after_turns threshold".to_string()),
                            },
                        )
                        .await;
                }
            }

            match context.method.as_str() {
                TURN_COMPLETED_EVENT => {
                    if let Some(command) = project.config.hooks.on_worker_completed.as_ref() {
                        self.run_hook_command(&project, command, &vars, &event_payload)
                            .await?;
                    }
                }
                TURN_ABORTED_EVENT => {
                    if let Some(command) = project.config.hooks.on_worker_aborted.as_ref() {
                        self.run_hook_command(&project, command, &vars, &event_payload)
                            .await?;
                    }
                }
                _ => {}
            }

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
                        req = req.header("x-foreman-secret", token);
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
                                req = req.header("x-foreman-secret", token);
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
                req = req.header("x-foreman-secret", token);
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
            .unwrap_or_else(|| "event_prompt".to_string());
        let event_prompt = if prefix.trim().is_empty() {
            compact.clone()
        } else {
            format!("{}\n\n{}", prefix, compact)
        };

        vars.insert("event_json".to_string(), compact.clone());
        vars.insert("event_pretty".to_string(), pretty);
        vars.insert("event_payload".to_string(), compact);
        vars.insert("event_prompt".to_string(), event_prompt.clone());
        vars.insert(event_prompt_variable.to_string(), event_prompt);

        let command = render_template_strict(&profile.program, &vars)
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

        let mut command = Command::new(command);
        command.args(&rendered_args);
        for (key, value) in profile.env.iter() {
            let rendered_value = render_template_strict(value, &vars)
                .with_context(|| format!("failed to render callback env value for '{key}'"))?;
            command.env(key, rendered_value);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to run callback command '{}'", profile.program))?;

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
                command = %profile.program,
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
            "agent_id": context.agent_id.to_string(),
            "thread_id": context.thread_id,
            "turn_id": context.turn_id,
            "event_id": context.event_id.to_string(),
            "ts": context.ts,
            "method": context.method,
            "params": context.params,
            "agent_role": context.role.as_str(),
        });

        if let Some(project_id) = context.project_id {
            payload["project_id"] = serde_json::Value::String(project_id.to_string());
        }

        if let Some(foreman_id) = context.foreman_id {
            payload["foreman_id"] = serde_json::Value::String(foreman_id.to_string());
        }

        if let Some(result) = &context.result_snapshot {
            payload["result"] = serde_json::to_value(result)
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
        }

        if let Some(vars) = extra_vars {
            payload["callback_vars"] = serde_json::to_value(vars)
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
        }

        payload
    }

    fn callback_vars(
        &self,
        context: &CallbackEventContext,
        mut inherited: HashMap<String, String>,
    ) -> Result<HashMap<String, String>> {
        inherited.insert("agent_id".to_string(), context.agent_id.to_string());
        inherited.insert("worker_id".to_string(), context.agent_id.to_string());
        inherited.insert("thread_id".to_string(), context.thread_id.clone());
        inherited.insert("method".to_string(), context.method.clone());
        inherited.insert("event_id".to_string(), context.event_id.to_string());
        inherited.insert("agent_role".to_string(), context.role.as_str().to_string());

        if let Some(turn_id) = context.turn_id.clone() {
            inherited.insert("turn_id".to_string(), turn_id);
        }

        if let Some(project_id) = context.project_id {
            inherited.insert("project_id".to_string(), project_id.to_string());
        }

        if let Some(foreman_id) = context.foreman_id {
            inherited.insert("foreman_id".to_string(), foreman_id.to_string());
        }

        inherited.insert("ts".to_string(), context.ts.to_string());
        inherited.insert("callback_type".to_string(), "agent-event".to_string());

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

        vars.insert("project_id".to_string(), project.id.to_string());
        vars.insert("project_path".to_string(), project.path.clone());
        vars.insert("project_name".to_string(), project.name.clone());
        vars.insert("project_status".to_string(), project.status.clone());
        if let Some(foreman_id) = project.foreman_agent_id {
            vars.insert("project_foreman_id".to_string(), foreman_id.to_string());
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

    async fn run_hook_command(
        &self,
        project: &ProjectRecord,
        command: &str,
        vars: &HashMap<String, String>,
        event_payload: &str,
    ) -> Result<()> {
        let mut vars = vars.clone();
        vars.insert("event_payload".to_string(), event_payload.to_string());
        let rendered = render_template_strict(command, &vars)
            .context("hook command template render failed")?;

        fs::create_dir_all(&project.path)
            .with_context(|| format!("failed to prepare project path {}", project.path))?;
        fs::create_dir_all(&project.runtime.path).with_context(|| {
            format!(
                "failed to prepare project runtime path {}",
                project.runtime.path.display()
            )
        })?;

        let mut command = Command::new("sh");
        command.arg("-lc").arg(rendered);
        command.current_dir(&project.runtime.path);
        for (key, value) in vars {
            command.env(key, value);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to run project hook for {}", project.path))?;

        let status = await_callback_future(
            "project hook command",
            Some(DEFAULT_CALLBACK_TIMEOUT_MS),
            child.wait(),
        )
        .await?;
        if !status.success() {
            return Err(anyhow!(
                "project hook exited with status {}",
                status.code().unwrap_or_default()
            ));
        }

        Ok(())
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
                &request.callback_overrides,
                false,
            )
            .context("invalid foreman callback config")?;
        let _ = self
            .resolve_project_callback_spec(
                &config.callbacks.worker,
                &CallbackOverrides::default(),
                false,
            )
            .context("invalid worker callback config")?;
        let _ = self
            .resolve_project_callback_spec(
                &config.callbacks.bubble_up,
                &CallbackOverrides::default(),
                false,
            )
            .context("invalid bubble-up callback config")?;

        let runtime = config.load_runtime_files(&path)?;
        let project_id = Uuid::new_v4();

        let initial_prompt = if let Some(prompt) = request.start_prompt.as_ref() {
            format!(
                "{}\n\n---\nRUNBOOK\n{}\n\nSTARTUP\n{}\n",
                runtime.foreman_prompt, runtime.runbook, prompt
            )
        } else {
            format!(
                "{}\n\n---\nRUNBOOK\n{}\n",
                runtime.foreman_prompt, runtime.runbook
            )
        };

        let callback = self
            .resolve_project_callback_spec(
                &config.callbacks.foreman,
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
            path.file_name().map_or("project".to_string(), |name| {
                name.to_string_lossy().to_string()
            })
        });

        let ts = now_ts();
        let project = ProjectRecord {
            id: project_id,
            path: path.to_string_lossy().to_string(),
            name,
            status: "running".to_string(),
            foreman_agent_id: Some(response.id),
            worker_ids: Vec::new(),
            completed_worker_turns: 0,
            config,
            runtime,
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

        self.dispatch_project_lifecycle_hook(&project, "start", None)
            .await
            .ok();

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
                &request.callback_overrides,
                false,
            )
            .context("failed to resolve worker callback")?;

        let worker_prompt = format!(
            "{}\n\nTASK\n{}\n",
            project.runtime.worker_prompt, request.prompt
        );

        let spawn_request = SpawnAgentRequest {
            prompt: worker_prompt,
            model: request.model,
            model_provider: request.model_provider,
            cwd: Some(
                request
                    .cwd
                    .unwrap_or_else(|| project.runtime.path.to_string_lossy().to_string()),
            ),
            sandbox: request.sandbox,
            callback_profile: None,
            callback_prompt_prefix: None,
            callback_args: None,
            callback_vars: None,
            callback_events: None,
        };

        let response = self
            .spawn_agent_record(
                spawn_request,
                AgentRole::Worker,
                Some(project_id),
                Some(foreman_id),
                None,
                callback,
            )
            .await
            .context("failed to spawn project worker")?;

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

        let (project, foreman_id) = {
            let projects = self.projects.read().await;
            let project = projects.get(&project_id).context("project not found")?;
            let foreman_id = project.foreman_agent_id;
            (project.clone(), foreman_id)
        };

        let foreman_id = foreman_id.context("project has no foreman")?;
        let ts = now_ts();
        let job_id = Uuid::new_v4();
        let mut worker_ids = Vec::new();
        let mut labels = HashMap::new();

        for spec in request.workers {
            let callback = self
                .resolve_project_callback_spec(
                    &project.config.callbacks.worker,
                    &spec.callback_overrides,
                    false,
                )
                .context("failed to resolve worker callback")?;

            let worker_prompt = format!(
                "{}\n\nTASK\n{}\n",
                project.runtime.worker_prompt, spec.prompt
            );

            let spawn_request = SpawnAgentRequest {
                prompt: worker_prompt,
                model: spec.model,
                model_provider: spec.model_provider,
                cwd: Some(
                    spec.cwd
                        .unwrap_or_else(|| project.runtime.path.to_string_lossy().to_string()),
                ),
                sandbox: spec.sandbox,
                callback_profile: None,
                callback_prompt_prefix: None,
                callback_args: None,
                callback_vars: None,
                callback_events: None,
            };

            let response = self
                .spawn_agent_record(
                    spawn_request,
                    AgentRole::Worker,
                    Some(project_id),
                    Some(foreman_id),
                    Some(job_id),
                    callback,
                )
                .await
                .context("failed to spawn project worker")?;

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
                    status: "running".to_string(),
                    worker_ids: worker_ids.clone(),
                    worker_labels: labels,
                    created_at: ts,
                    completed_at: None,
                    updated_at: ts,
                },
            );
        }

        self.schedule_state_persist();
        Ok(CreateProjectJobsResponse {
            job_id,
            project_id,
            status: "running".to_string(),
            worker_ids: worker_ids.clone(),
            worker_count: worker_ids.len(),
            labels: job_labels,
        })
    }

    pub async fn list_jobs(&self) -> Vec<JobState> {
        let jobs = self.jobs.read().await;
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

        Ok(JobState {
            id: job.id,
            project_id: job.project_id,
            status: job.status.clone(),
            worker_ids: job.worker_ids.clone(),
            labels: aggregate_worker_labels(&job.worker_labels),
            created_at: job.created_at,
            completed_at: job.completed_at,
            updated_at: job.updated_at,
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
                    status: "missing".to_string(),
                    completion_method: None,
                    turn_id: None,
                    final_text: None,
                    summary: None,
                    references: None,
                    completed_at: None,
                    event_id: None,
                    event_count: 0,
                    error: Some("worker disappeared".to_string()),
                }
            });

            if response.status == "running" {
                running_workers += 1;
            }

            if response.completed_at.is_some() {
                completed_workers += 1;
            }

            if matches!(response.completion_method.as_deref(), Some("turn/aborted")) {
                failed_workers += 1;
            }

            workers.push(response);
        }

        let status = if total_workers == 0 {
            "empty".to_string()
        } else if running_workers > 0 {
            "running".to_string()
        } else if completed_workers == total_workers {
            if failed_workers == total_workers {
                "failed".to_string()
            } else if failed_workers > 0 {
                "partial".to_string()
            } else {
                "completed".to_string()
            }
        } else if completed_workers > 0 {
            "partial".to_string()
        } else {
            "queued".to_string()
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
        })
    }

    pub async fn wait_for_job_result(
        self: &Arc<Self>,
        job_id: Uuid,
        timeout_ms: Option<u64>,
        poll_ms: Option<u64>,
        include_workers: bool,
    ) -> Result<crate::models::JobWaitResponse> {
        let poll_interval = std::time::Duration::from_millis(poll_ms.unwrap_or(250));
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
                            status: "unknown".to_string(),
                            total_workers: 0,
                            completed_workers: 0,
                            running_workers: 0,
                            failed_workers: 0,
                            worker_count: 0,
                            workers: Vec::new(),
                            labels: HashMap::new(),
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
            role: "foreman".to_string(),
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
            role: "foreman".to_string(),
        })
    }

    pub async fn compact_project(
        self: &Arc<Self>,
        project_id: Uuid,
        request: CompactProjectRequest,
    ) -> Result<SpawnProjectWorkerResponse> {
        let (project, foreman_id) = {
            let projects = self.projects.read().await;
            let project = projects.get(&project_id).context("project not found")?;
            (project.clone(), project.foreman_agent_id)
        };

        let foreman_id = foreman_id.context("project has no foreman")?;
        let mut prompt = project
            .runtime
            .handoff
            .unwrap_or_else(|| "---\nHandoff requested by project coordinator.\n".to_string());

        if let Some(reason) = request.reason {
            prompt = format!("{}\n\nREASON: {}\n", prompt, reason);
        }

        if let Some(extra_prompt) = request.prompt {
            prompt = format!("{}\n\nPROMPT:\n{}\n", prompt, extra_prompt);
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

        self.dispatch_project_lifecycle_hook(&project, "compact", Some("compact".to_string()))
            .await
            .ok();

        Ok(SpawnProjectWorkerResponse {
            id: response.id,
            thread_id: response.thread_id,
            turn_id: response.turn_id,
            status: response.status,
            project_id,
            foreman_id,
            role: "foreman".to_string(),
        })
    }

    async fn dispatch_project_lifecycle_hook(
        &self,
        project: &ProjectRecord,
        kind: &str,
        extra: Option<String>,
    ) -> Result<()> {
        let project = project.clone();

        let (command, command_name) = match kind {
            "start" => (
                project.config.hooks.on_project_start.as_ref(),
                "on_project_start",
            ),
            "compact" => (
                project.config.hooks.on_project_compaction.as_ref(),
                "on_project_compaction",
            ),
            "stop" => (
                project.config.hooks.on_project_stop.as_ref(),
                "on_project_stop",
            ),
            _ => (None, "project_hook"),
        };

        if let Some(command) = command {
            let mut vars = HashMap::new();
            vars.insert("project_id".to_string(), project.id.to_string());
            vars.insert("project_path".to_string(), project.path.clone());
            vars.insert("project_name".to_string(), project.name.clone());
            vars.insert("hook".to_string(), command_name.to_string());
            if let Some(extra) = extra {
                vars.insert("reason".to_string(), extra);
            }

            let payload = serde_json::to_string(&vars)?;
            self.run_hook_command(&project, command, &vars, &payload)
                .await
                .with_context(|| format!("project hook {} failed", command_name))?;
        }

        Ok(())
    }

    pub async fn close_project(self: &Arc<Self>, project_id: Uuid) -> Result<()> {
        let (project, worker_ids, foreman_id) = {
            let mut projects = self.projects.write().await;
            let project = projects.remove(&project_id).context("project not found")?;
            let worker_ids = project.worker_ids.clone();
            let foreman_id = project.foreman_agent_id;
            (project, worker_ids, foreman_id)
        };

        self.dispatch_project_lifecycle_hook(&project, "stop", None)
            .await
            .ok();

        for worker_id in worker_ids {
            let _ = self.close_agent(worker_id).await;
        }
        if let Some(foreman_id) = foreman_id {
            let _ = self.close_agent(foreman_id).await;
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

        self.spawn_agent_record(request, AgentRole::Standalone, None, None, None, callback)
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

        let thread_id = {
            let agents = self.agents.read().await;
            let agent = agents.get(&agent_id).context("agent not found")?;
            agent.thread_id.clone()
        };

        let turn_id = if has_turn_prompt {
            let request = TurnStartRequest {
                thread_id: thread_id.clone(),
                input: vec![TextPayload::text(input.prompt.clone().unwrap_or_default())],
            };
            let response = self
                .client
                .turn_start(&request)
                .await
                .with_context(|| "turn/start failed")?;

            Some(response.turn_id)
        } else {
            None
        };

        {
            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&agent_id) {
                agent.updated_at = now_ts();
                if has_turn_prompt {
                    agent.status = "running".into();
                }

                if let Some(turn_id) = turn_id.clone() {
                    agent.active_turn_id = Some(turn_id);
                    agent.status = "running".into();
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
                    "running".to_string()
                } else {
                    updated_agent.status
                }
            },
            project_id: updated_agent.project_id,
            role: updated_agent.role.as_str().to_string(),
            foreman_id: updated_agent.foreman_id,
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
                agent.status = "running".into();
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
            status: "running".to_string(),
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
                agent.status = "interrupted".into();
                agent.updated_at = now_ts();
            }
        }
        self.schedule_state_persist();

        Ok(())
    }

    pub async fn close_agent(self: &Arc<Self>, agent_id: Uuid) -> Result<()> {
        let (thread_id, project_id, role, _foreman_id, job_id) = {
            let mut agents = self.agents.write().await;
            let agent = agents.remove(&agent_id).context("agent not found")?;
            (
                agent.thread_id,
                agent.project_id,
                agent.role,
                agent.foreman_id,
                agent.job_id,
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
                    project.status = "orphaned".to_string();
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
        self.schedule_state_persist();

        Ok(())
    }

    pub async fn list_agents(&self) -> Vec<crate::models::AgentState> {
        let agents = self.agents.read().await;
        agents
            .values()
            .map(|agent| AgentState {
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
            })
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

    pub async fn get_agent(&self, agent_id: Uuid) -> Result<crate::models::AgentState> {
        let agent = self
            .agents
            .read()
            .await
            .get(&agent_id)
            .context("agent not found")?
            .clone();

        Ok(crate::models::AgentState {
            id: agent.id,
            thread_id: agent.thread_id,
            active_turn_id: agent.active_turn_id,
            status: agent.status,
            callback_profile: agent_callback_profile(&agent.callback),
            role: agent.role.as_str().to_string(),
            project_id: agent.project_id,
            foreman_id: agent.foreman_id,
            error: agent.error,
            updated_at: agent.updated_at,
            events: agent.events.iter().cloned().collect(),
        })
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
                .or(agent.error),
        })
    }

    pub async fn wait_for_agent_result(
        self: &Arc<Self>,
        agent_id: Uuid,
        timeout_ms: Option<u64>,
        poll_ms: Option<u64>,
        include_events: bool,
    ) -> Result<crate::models::AgentWaitResponse> {
        let poll_interval = std::time::Duration::from_millis(poll_ms.unwrap_or(250));
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
                    || (result.status != "running" && result.turn_id.is_none())
                    || result.status == "interrupted"
                    || result.status == "idle"
                    || result.status == "orphaned";
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
                                    status: "unknown".to_string(),
                                    completion_method: None,
                                    turn_id: None,
                                    final_text: None,
                                    summary: None,
                                    references: None,
                                    completed_at: None,
                                    event_id: None,
                                    event_count: 0,
                                    error: Some("wait timeout".to_string()),
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
        request: SpawnAgentRequest,
        role: AgentRole,
        project_id: Option<Uuid>,
        foreman_id: Option<Uuid>,
        job_id: Option<Uuid>,
        callback: WorkerCallback,
    ) -> Result<SpawnAgentResponse> {
        let thread_id = {
            let request = ThreadStartRequest {
                model: request.model,
                model_provider: request.model_provider,
                cwd: request.cwd,
                sandbox: request.sandbox,
            };
            let response = self
                .client
                .thread_start(&request)
                .await
                .with_context(|| "thread/start failed")?;
            response.thread_id
        };

        let id = Uuid::new_v4();
        let ts = now_ts();
        let role_name = role.as_str().to_string();
        {
            let agent = AgentRecord {
                id,
                thread_id: thread_id.clone(),
                active_turn_id: None,
                status: "idle".into(),
                callback,
                role: role.clone(),
                project_id,
                foreman_id,
                job_id,
                error: None,
                result: None,
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
            let turn_request = TurnStartRequest {
                thread_id: thread_id.clone(),
                input: vec![TextPayload::text(request.prompt)],
            };
            let response = self
                .client
                .turn_start(&turn_request)
                .await
                .with_context(|| "turn/start failed")?;
            turn_id = Some(response.turn_id);

            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&id) {
                agent.active_turn_id = turn_id.clone();
                agent.status = "running".into();
                agent.updated_at = now_ts();
            }
        }

        let status = if turn_id.is_some() {
            "running".to_string()
        } else {
            "idle".to_string()
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

        let profile = self
            .config
            .get_callback_profile(&profile_name)
            .with_context(|| format!("callback profile '{profile_name}' is not configured"))?;

        match profile {
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
            .or_else(|| spec.callback_events.clone());

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

        self.resolve_callback(CallbackResolutionParams {
            callback_profile,
            callback_prompt_prefix,
            callback_args,
            callback_vars,
            callback_events,
            use_global_default,
        })
    }

    fn resolve_project_bubble_callback(
        &self,
        project: &ProjectRecord,
        overrides: &CallbackOverrides,
    ) -> Result<WorkerCallback> {
        self.resolve_project_callback_spec(&project.config.callbacks.bubble_up, overrides, false)
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
    let timeout_ms = custom_ms.unwrap_or(DEFAULT_CALLBACK_TIMEOUT_MS);
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

fn extract_agent_result_text(method: &str, params: &Value) -> Option<String> {
    match method {
        "turn/completed" => extract_text_from_turn(params.get("turn")?)
            .or_else(|| extract_text_from_params(params, &["last_agent_message", "text", "result"]))
            .or_else(|| params.get("item").and_then(extract_text_from_item)),
        "item/agentMessage" | "item/agentMessage/delta" => {
            extract_text_from_params(params, &["text", "message", "delta", "snippet"])
        }
        "item/completed" => params
            .get("item")
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
            "turn/aborted" => "aborted".to_string(),
            _ => "completed".to_string(),
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
        status: if method == "turn/aborted" {
            "aborted".to_string()
        } else {
            "completed".to_string()
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
        "foreman" => AgentRole::Foreman,
        "worker" => AgentRole::Worker,
        "standalone" => AgentRole::Standalone,
        _ => AgentRole::Standalone,
    };

    let id = Uuid::parse_str(&record.id).context("invalid persisted agent id")?;
    let thread_id = record.thread_id;
    let active_turn_id = record.active_turn_id;
    let status = record.status;
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

    Ok(AgentRecord {
        id,
        thread_id,
        active_turn_id,
        status,
        callback,
        role,
        project_id,
        foreman_id,
        error: record.error,
        result: record.result,
        job_id,
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

    Ok(JobRecord {
        id,
        project_id,
        status: record.status,
        worker_ids,
        worker_labels,
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
        status: agent.status.clone(),
        callback,
        role,
        project_id: agent.project_id.map(|id| id.to_string()),
        foreman_id: agent.foreman_id.map(|id| id.to_string()),
        error: agent.error.clone(),
        result: agent.result.clone(),
        job_id: agent.job_id.map(|id| id.to_string()),
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
        created_at: job.created_at,
        completed_at: job.completed_at,
        updated_at: job.updated_at,
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
        WorkerCallback::Profile(profile) => PersistedWorkerCallback::Profile {
            profile: profile.profile.clone(),
            prompt_prefix: profile.prompt_prefix.clone(),
            command_args: profile.command_args.clone(),
            events: profile.events.clone(),
            vars: profile.vars.clone(),
        },
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

fn is_job_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "partial" | "failed")
}

#[cfg(test)]
mod tests {
    use super::{
        AgentRecord, AgentRole, Foreman, WorkerCallback, callback_timeout_ms, render_template,
        render_template_strict, resolve_agent_status, unresolved_template_tokens,
        update_completed_turn_counter,
    };
    use crate::events::events_allowed;
    use crate::state::{PersistedAgentRecord, PersistedState, PersistedWorkerCallback};
    use crate::{config::ServiceConfig, models::AgentResult};
    use codex_api::AppServerClient;
    use serde_json::json;
    use std::collections::VecDeque;
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
        assert_eq!(callback_timeout_ms(None), Some(5_000));
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
    fn normalize_event_method_maps_codex_prefix_events() {
        assert_eq!(
            Foreman::normalize_event_method("codex/event/task_started").to_string(),
            "turn/started"
        );
        assert_eq!(
            Foreman::normalize_event_method("codex/event/task_complete").to_string(),
            "turn/completed"
        );
        assert_eq!(
            Foreman::normalize_event_method("codex/event/item_agentMessage/delta").to_string(),
            "item/agentMessage/delta"
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

    #[test]
    fn resolve_agent_status_prefers_result_status_when_completed() {
        let agent_id = Uuid::new_v4();
        let agent = AgentRecord {
            id: agent_id,
            thread_id: "thread-1".to_string(),
            active_turn_id: None,
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
            updated_at: 0,
            events: VecDeque::new(),
        };

        assert_eq!(resolve_agent_status(&agent), "completed");
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
                updated_at: 1_700_000_001,
                events: Vec::new(),
            }],
            projects: Vec::new(),
            jobs: Vec::new(),
        }
    }
}
