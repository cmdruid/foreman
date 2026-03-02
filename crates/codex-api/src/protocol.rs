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

fn parse_id_field(value: &serde_json::Value, candidates: &[&str]) -> Option<String> {
    candidates
        .iter()
        .find_map(|candidate| value.get(*candidate).and_then(extract_id))
}

fn parse_thread_id_from_message(value: &serde_json::Value) -> Option<String> {
    parse_id_field(
        value,
        &["threadId", "thread_id"],
    )
    .or_else(|| {
        value
            .get("thread")
            .and_then(|thread| parse_id_field(thread, &["id", "threadId", "thread_id"]))
    })
    .or_else(|| parse_id_field(value, &["conversationId", "conversation_id"]))
    .or_else(|| {
        value.get("conversation").and_then(|conversation| {
            parse_id_field(
                conversation,
                &[
                    "threadId",
                    "thread_id",
                    "id",
                    "conversationId",
                    "conversation_id",
                ],
            )
            .or_else(|| {
                conversation
                    .get("thread")
                    .and_then(|thread| parse_id_field(thread, &["id", "threadId", "thread_id"]))
            })
        })
    })
}

fn parse_turn_id_from_message(value: &serde_json::Value) -> Option<String> {
    parse_id_field(value, &["turnId", "turn_id"])
        .or_else(|| {
            value
                .get("turn")
                .and_then(|turn| parse_id_field(turn, &["id", "turnId", "turn_id"]))
        })
        .or_else(|| {
            value.get("conversation").and_then(|conversation| {
                parse_id_field(conversation, &["turnId", "turn_id"]).or_else(|| {
                    conversation
                        .get("turn")
                        .and_then(|turn| parse_id_field(turn, &["id", "turnId", "turn_id"]))
                })
            })
        })
}

pub fn parse_thread_id(params: &serde_json::Value) -> Option<String> {
    parse_id_field(
        params,
        &["threadId", "thread_id", "conversationId", "conversation_id"],
    )
    .or_else(|| {
        params
            .get("thread")
            .and_then(|thread| parse_id_field(thread, &["id", "threadId", "thread_id"]))
    })
    .or_else(|| {
        params.get("conversation").and_then(|conversation| {
            parse_id_field(
                conversation,
                &[
                    "threadId",
                    "thread_id",
                    "id",
                    "conversationId",
                    "conversation_id",
                ],
            )
            .or_else(|| {
                conversation
                    .get("thread")
                    .and_then(|thread| parse_id_field(thread, &["id", "threadId", "thread_id"]))
            })
        })
    })
    .or_else(|| params.get("msg").and_then(parse_thread_id_from_message))
    .or_else(|| params.get("result").and_then(parse_thread_id_from_message))
}

pub fn parse_turn_id(params: &serde_json::Value) -> Option<String> {
    parse_id_field(params, &["turnId", "turn_id"])
        .or_else(|| params.get("turn").and_then(parse_turn_id_from_message))
        .or_else(|| parse_turn_id_from_message(params))
        .or_else(|| params.get("result").and_then(parse_turn_id_from_message))
        .or_else(|| {
            params
                .get("msg")
                .and_then(parse_turn_id_from_message)
                .or_else(|| {
                    params.get("thread").and_then(|thread| {
                        parse_id_field(thread, &["turnId", "turn_id"]).or_else(|| {
                            parse_turn_id_from_message(
                                thread.get("msg").unwrap_or(&serde_json::json!(null)),
                            )
                        })
                    })
                })
        })
        .or_else(|| {
            params.get("conversation").and_then(|conversation| {
                parse_id_field(conversation, &["turnId", "turn_id"]).or_else(|| {
                    parse_turn_id_from_message(
                        conversation.get("turn").unwrap_or(&serde_json::json!(null)),
                    )
                    .or_else(|| {
                        parse_turn_id_from_message(
                            conversation
                                .get("msg")
                                .unwrap_or(&serde_json::json!(null)),
                        )
                    })
                })
            })
        })
}

#[cfg(test)]
mod tests {
    use super::{parse_thread_id, parse_turn_id};
    use serde_json::json;

    #[test]
    fn parse_thread_id_supports_strict_response_field() {
        assert_eq!(
            parse_thread_id(&json!({"threadId": "thread-0192"})),
            Some("thread-0192".into())
        );
        assert_eq!(
            parse_thread_id(&json!({"thread": {"id": "0192"}})),
            Some("0192".into())
        );
        assert_eq!(
            parse_thread_id(&json!({"thread_id": "0192"})),
            Some("0192".into())
        );
        assert_eq!(
            parse_thread_id(&json!({"thread_id": 42})),
            Some("42".into())
        );
        assert_eq!(
            parse_thread_id(&json!({"conversationId":"conv-1"})),
            Some("conv-1".into())
        );
        assert_eq!(
            parse_thread_id(&json!({"msg":{"threadId":"thread-001"}})),
            Some("thread-001".into())
        );
        assert_eq!(
            parse_thread_id(&json!({"msg":{"thread":{"id":"thread-002"}}})),
            Some("thread-002".into())
        );
        assert_eq!(
            parse_thread_id(&json!({"msg":{"conversationId":"conv-3","thread":{"id":"thread-003"}}})),
            Some("thread-003".into())
        );
        assert_eq!(
            parse_thread_id(&json!({"result":{"thread":{"id":"thread-004"}}})),
            Some("thread-004".into())
        );
    }

    #[test]
    fn parse_turn_id_supports_strict_response_field() {
        assert_eq!(
            parse_turn_id(&json!({"turnId": "turn-1"})),
            Some("turn-1".into())
        );
        assert_eq!(
            parse_turn_id(&json!({"turn": {"id": "turn-3"}})),
            Some("turn-3".into())
        );
        assert_eq!(
            parse_turn_id(&json!({"turn_id": "turn-1"})),
            Some("turn-1".into())
        );
        assert_eq!(parse_turn_id(&json!({"turn_id": 99})), Some("99".into()));
        assert_eq!(
            parse_turn_id(&json!({"msg":{"turnId":"turn-msg-1"}})),
            Some("turn-msg-1".into())
        );
        assert_eq!(
            parse_turn_id(&json!({"msg":{"turn":{"id":"turn-msg-2"}}})),
            Some("turn-msg-2".into())
        );
        assert_eq!(
            parse_turn_id(&json!({"msg":{"conversation":{"turn_id":"turn-msg-3"}}})),
            Some("turn-msg-3".into())
        );
        assert_eq!(
            parse_turn_id(&json!({"result":{"turn":{"id":"turn-result-1"}}})),
            Some("turn-result-1".into())
        );
    }
}
