use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use reqwest::Client as HttpClient;
use tokio::sync::{RwLock, broadcast};
use tracing::warn;
use uuid::Uuid;

use crate::{
    app_server_client::{AppServerClient, RawNotification},
    models::{AgentEventDto, AgentState},
    protocol::{parse_thread_id, parse_turn_id},
};

const MAX_EVENTS: usize = 50;

#[derive(Debug)]
struct CallbackConfig {
    url: String,
    secret: Option<String>,
}

#[derive(Debug)]
struct AgentRecord {
    id: Uuid,
    thread_id: String,
    active_turn_id: Option<String>,
    status: String,
    callback: Option<CallbackConfig>,
    error: Option<String>,
    updated_at: u64,
    events: VecDeque<AgentEventDto>,
}

pub struct Foreman {
    client: Arc<AppServerClient>,
    http_client: HttpClient,
    agents: RwLock<HashMap<Uuid, AgentRecord>>,
    thread_map: RwLock<HashMap<String, Uuid>>,
    event_rx: RwLock<broadcast::Receiver<RawNotification>>,
}

impl Foreman {
    pub async fn new(
        client: Arc<AppServerClient>,
        event_rx: broadcast::Receiver<RawNotification>,
    ) -> Arc<Self> {
        let foreman = Arc::new(Self {
            client: client.clone(),
            http_client: HttpClient::new(),
            agents: RwLock::new(HashMap::new()),
            thread_map: RwLock::new(HashMap::new()),
            event_rx: RwLock::new(event_rx),
        });

        Self::spawn_event_loop(Arc::clone(&foreman));
        foreman
    }

    fn spawn_event_loop(this: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                let recv = {
                    let mut receiver = this.event_rx.write().await;
                    receiver.recv().await
                };

                match recv {
                    Ok(event) => this.route_notification(event).await,
                    Err(err) => {
                        warn!(%err, "event channel closed");
                        break;
                    }
                }
            }
        });
    }

    async fn dispatch_event(&self, agent_id: Uuid, method: String, params: serde_json::Value) {
        let ts = now_ts();
        let callback = {
            let agents = self.agents.read().await;
            agents.get(&agent_id).and_then(|agent| {
                agent
                    .callback
                    .as_ref()
                    .map(|callback| (callback.url.clone(), callback.secret.clone()))
            })
        };

        {
            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&agent_id) {
                agent.updated_at = ts;
                agent.events.push_back(AgentEventDto {
                    ts,
                    method: method.clone(),
                    params: params.clone(),
                });
                while agent.events.len() > MAX_EVENTS {
                    agent.events.pop_front();
                }

                if method == "turn/completed" || method == "turn/aborted" {
                    agent.status = "idle".into();
                }
                if method == "thread/status/changed" {
                    if let Some(status) = params.get("status").and_then(|value| value.as_str()) {
                        agent.status = status.to_string();
                    }
                }
                if method == "turn/started" {
                    if let Some(turn_id) = parse_turn_id(&params) {
                        agent.status = "running".into();
                        agent.active_turn_id = Some(turn_id);
                    }
                }
            }
        }

        if let Some((url, secret)) = callback {
            let payload = serde_json::json!({
                "agentId": agent_id.to_string(),
                "ts": ts,
                "method": method,
                "params": params,
            });
            let mut req = self.http_client.post(&url).json(&payload);
            if let Some(token) = secret {
                req = req.header("x-foreman-secret", token);
            }
            tokio::spawn(async move {
                let _ = req.send().await;
            });
        }
    }

    async fn route_notification(self: &Arc<Self>, event: RawNotification) {
        let thread_id = parse_thread_id(&event.params);
        if let Some(thread_id) = thread_id {
            let agent_id = {
                let thread_map = self.thread_map.read().await;
                thread_map.get(&thread_id).cloned()
            };

            if let Some(agent_id) = agent_id {
                self.dispatch_event(agent_id, event.method, event.params)
                    .await;
            }
        }
    }

    pub async fn spawn_agent(
        &self,
        request: crate::models::SpawnAgentRequest,
    ) -> anyhow::Result<crate::models::SpawnAgentResponse> {
        let thread_id = {
            let params = serde_json::json!({
                "model": request.model,
                "modelProvider": request.model_provider,
                "cwd": request.cwd,
                "sandbox": request.sandbox,
            });
            let response: serde_json::Value = self
                .client
                .request("thread/start", &params)
                .await
                .with_context(|| "thread/start failed")?;
            response
                .get("thread")
                .and_then(|thread| thread.get("id"))
                .and_then(|id| id.as_str())
                .context("thread/start response missing thread.id")?
                .to_string()
        };

        let mut turn_id: Option<String> = None;
        if !request.prompt.trim().is_empty() {
            let input = vec![serde_json::json!({
                "type": "text",
                "text": request.prompt,
                "textElements": [],
            })];
            let params = serde_json::json!({"threadId": thread_id.clone(), "input": input});
            let response: serde_json::Value = self
                .client
                .request("turn/start", &params)
                .await
                .with_context(|| "turn/start failed")?;
            turn_id = response
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(|id| id.as_str())
                .map(ToOwned::to_owned);
        }

        let id = Uuid::new_v4();
        let ts = now_ts();
        let agent = AgentRecord {
            id,
            thread_id: thread_id.clone(),
            active_turn_id: turn_id.clone(),
            status: if turn_id.is_some() {
                "running".into()
            } else {
                "idle".into()
            },
            callback: request.callback_url.map(|url| CallbackConfig {
                url,
                secret: request.callback_secret,
            }),
            error: None,
            updated_at: ts,
            events: VecDeque::new(),
        };

        {
            let mut agents = self.agents.write().await;
            let mut thread_map = self.thread_map.write().await;
            agents.insert(id, agent);
            thread_map.insert(thread_id.clone(), id);
        }

        Ok(crate::models::SpawnAgentResponse {
            id,
            thread_id,
            turn_id,
            status: if request.prompt.trim().is_empty() {
                "idle".to_string()
            } else {
                "running".to_string()
            },
        })
    }

    pub async fn send_turn(
        &self,
        agent_id: Uuid,
        input: crate::models::SendAgentInput,
    ) -> anyhow::Result<crate::models::SpawnAgentResponse> {
        let thread_id = {
            let agents = self.agents.read().await;
            let agent = agents.get(&agent_id).context("agent not found")?;
            agent.thread_id.clone()
        };

        let input_json = vec![serde_json::json!({
            "type": "text",
            "text": input.prompt,
            "textElements": [],
        })];
        let params = serde_json::json!({"threadId": thread_id, "input": input_json});
        let response: serde_json::Value = self
            .client
            .request("turn/start", &params)
            .await
            .with_context(|| "turn/start failed")?;

        let turn_id = response
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(|id| id.as_str())
            .map(ToOwned::to_owned);

        {
            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&agent_id) {
                agent.status = "running".into();
                agent.updated_at = now_ts();
                if let Some(url) = input
                    .callback_url
                    .or_else(|| agent.callback.as_ref().map(|cb| cb.url.clone()))
                {
                    let secret = agent
                        .callback
                        .as_ref()
                        .and_then(|c| c.secret.as_ref())
                        .cloned();
                    agent.callback = Some(CallbackConfig { url, secret });
                }
                if let Some(token) = input.callback_secret {
                    if let Some(cb) = agent.callback.as_mut() {
                        cb.secret = Some(token);
                    }
                }
                agent.active_turn_id = turn_id.clone();
            }
        }

        Ok(crate::models::SpawnAgentResponse {
            id: agent_id,
            thread_id: {
                let agents = self.agents.read().await;
                agents
                    .get(&agent_id)
                    .context("agent missing")?
                    .thread_id
                    .clone()
            },
            turn_id,
            status: "running".to_string(),
        })
    }

    pub async fn steer(
        &self,
        agent_id: Uuid,
        prompt: String,
    ) -> anyhow::Result<crate::models::SpawnAgentResponse> {
        let (thread_id, expected_turn_id) = {
            let agents = self.agents.read().await;
            let agent = agents.get(&agent_id).context("agent not found")?;
            (
                agent.thread_id.clone(),
                agent
                    .active_turn_id
                    .clone()
                    .or_else(|| return Err(anyhow::anyhow!("no active turn to steer"))),
            )
        };

        let expected_turn_id = expected_turn_id?;
        let input_json = vec![serde_json::json!({
            "type": "text",
            "text": prompt,
            "textElements": [],
        })];
        let params = serde_json::json!({
            "threadId": thread_id,
            "input": input_json,
            "expectedTurnId": expected_turn_id,
        });

        let response: serde_json::Value = self
            .client
            .request("turn/steer", &params)
            .await
            .with_context(|| "turn/steer failed")?;

        let turn_id = response
            .get("turnId")
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned);

        if let Some(turn_id) = turn_id.clone() {
            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&agent_id) {
                agent.status = "running".into();
                agent.updated_at = now_ts();
                agent.active_turn_id = Some(turn_id);
            }
        }

        Ok(crate::models::SpawnAgentResponse {
            id: agent_id,
            thread_id,
            turn_id,
            status: "running".to_string(),
        })
    }

    pub async fn interrupt(
        &self,
        agent_id: Uuid,
        override_turn_id: Option<String>,
    ) -> anyhow::Result<()> {
        let (thread_id, turn_id) = {
            let agents = self.agents.read().await;
            let agent = agents.get(&agent_id).context("agent not found")?;
            let turn_id = override_turn_id.or_else(|| agent.active_turn_id.clone());
            (agent.thread_id.clone(), turn_id)
        };

        let turn_id = turn_id.context("no active turn to interrupt")?;
        let params = serde_json::json!({"threadId": thread_id.clone(), "turnId": turn_id});
        let _: serde_json::Value = self.client.request("turn/interrupt", &params).await?;

        {
            let mut agents = self.agents.write().await;
            if let Some(agent) = agents.get_mut(&agent_id) {
                agent.status = "interrupted".into();
                agent.updated_at = now_ts();
            }
        }

        Ok(())
    }

    pub async fn close_agent(&self, agent_id: Uuid) -> anyhow::Result<()> {
        let thread_id = {
            let mut agents = self.agents.write().await;
            let agent = agents.remove(&agent_id).context("agent not found")?;
            agent.thread_id
        };

        {
            let mut thread_map = self.thread_map.write().await;
            thread_map.remove(&thread_id);
        }
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
                callback_url: agent.callback.as_ref().map(|cb| cb.url.clone()),
                error: agent.error.clone(),
                updated_at: agent.updated_at,
                events: agent.events.iter().cloned().collect(),
            })
            .collect()
    }

    pub async fn get_agent(&self, agent_id: Uuid) -> anyhow::Result<crate::models::AgentState> {
        let agents = self.agents.read().await;
        let agent = agents.get(&agent_id).context("agent not found")?;
        Ok(crate::models::AgentState {
            id: agent.id,
            thread_id: agent.thread_id.clone(),
            active_turn_id: agent.active_turn_id.clone(),
            status: agent.status.clone(),
            callback_url: agent.callback.as_ref().map(|cb| cb.url.clone()),
            error: agent.error.clone(),
            updated_at: agent.updated_at,
            events: agent.events.iter().cloned().collect(),
        })
    }
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
        .unwrap_or_default()
}
