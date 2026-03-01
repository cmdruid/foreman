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

/// Parse an ID-bearing JSON value from a protocol field.
fn extract_id(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(num) => Some(num.to_string()),
        _ => None,
    }
}

pub fn parse_thread_id(params: &serde_json::Value) -> Option<String> {
    params.get("thread_id").and_then(extract_id)
}

pub fn parse_turn_id(params: &serde_json::Value) -> Option<String> {
    params.get("turn_id").and_then(extract_id)
}

#[cfg(test)]
mod tests {
    use super::{parse_thread_id, parse_turn_id};
    use serde_json::json;

    #[test]
    fn parse_thread_id_supports_current_thread_id_field() {
        assert_eq!(
            parse_thread_id(&json!({"thread_id": "0192"})),
            Some("0192".into())
        );
        assert_eq!(
            parse_thread_id(&json!({"thread_id": 42})),
            Some("42".into())
        );
        assert!(parse_thread_id(&json!({"thread": {"id": "legacy"}})).is_none());
        assert!(parse_thread_id(&json!({"thread-id": "legacy"})).is_none());
        assert!(parse_thread_id(&json!({"msg": {"thread_id": "legacy"}})).is_none());
    }

    #[test]
    fn parse_turn_id_supports_current_turn_id_field() {
        assert_eq!(
            parse_turn_id(&json!({"turn_id": "turn-1"})),
            Some("turn-1".into())
        );
        assert_eq!(parse_turn_id(&json!({"turn_id": 99})), Some("99".into()));
        assert!(parse_turn_id(&json!({"turn": {"id": "legacy"}})).is_none());
        assert!(parse_turn_id(&json!({"turn-id": "legacy"})).is_none());
        assert!(parse_turn_id(&json!({"msg": {"turn_id": "legacy"}})).is_none());
    }
}
