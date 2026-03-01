use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    String(String),
    Integer(i64),
}

impl RequestId {
    pub fn new() -> Self {
        Self::String(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_key(&self) -> String {
        match self {
            Self::String(s) => s.clone(),
            Self::Integer(i) => i.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JSONRPCMessage {
    Request(JSONRPCRequest),
    Notification(JSONRPCNotification),
    Response(JSONRPCResponse),
    Error(JSONRPCError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JSONRPCRequest {
    pub id: RequestId,
    pub method: String,
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JSONRPCNotification {
    pub method: String,
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JSONRPCResponse {
    pub id: RequestId,
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JSONRPCError {
    pub id: RequestId,
    pub error: JSONRPCErrorError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JSONRPCErrorError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

pub fn parse_thread_id(params: &serde_json::Value) -> Option<String> {
    params
        .get("threadId")
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .or_else(|| {
            params
                .get("thread_id")
                .and_then(|value| value.as_str().map(ToOwned::to_owned))
        })
        .or_else(|| {
            params
                .get("thread")
                .and_then(|thread| thread.get("id"))
                .and_then(|value| value.as_str().map(ToOwned::to_owned))
        })
}

pub fn parse_turn_id(params: &serde_json::Value) -> Option<String> {
    params
        .get("turnId")
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .or_else(|| {
            params
                .get("turn_id")
                .and_then(|value| value.as_str().map(ToOwned::to_owned))
        })
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(|value| value.as_str().map(ToOwned::to_owned))
        })
}
