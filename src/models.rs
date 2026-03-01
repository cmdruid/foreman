use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
pub struct SpawnAgentRequest {
    pub prompt: String,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub sandbox: Option<String>,
    pub callback_url: Option<String>,
    pub callback_secret: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SendAgentInput {
    pub prompt: String,
    pub callback_url: Option<String>,
    pub callback_secret: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SteerAgentInput {
    pub prompt: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InterruptInput {
    pub turn_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentEventDto {
    pub ts: u64,
    pub method: String,
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentState {
    pub id: Uuid,
    pub thread_id: String,
    pub active_turn_id: Option<String>,
    pub status: String,
    pub callback_url: Option<String>,
    pub error: Option<String>,
    pub updated_at: u64,
    pub events: Vec<AgentEventDto>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SpawnAgentResponse {
    pub id: Uuid,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub status: String,
}
