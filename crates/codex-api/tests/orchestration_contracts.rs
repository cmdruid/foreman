use std::time::Duration;

use codex_api::{
    AppServerClient, EmptyResponse, TextPayload, ThreadStartRequest, TurnInterruptRequest,
    TurnStartRequest, TurnSteerRequest,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tempfile::tempdir;
use tokio::sync::broadcast;
use tokio::time::{sleep, timeout};

fn write_fake_app_server_script(path: &std::path::Path, contents: &str) {
    std::fs::write(path, contents).expect("write fake app-server script");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(path)
            .expect("script metadata")
            .permissions();
        perm.set_mode(0o755);
    std::fs::set_permissions(path, perm).expect("make script executable");
}
}

fn assert_json_roundtrip<T>(value: &T)
where
    T: serde::Serialize + DeserializeOwned + core::fmt::Debug,
{
    let encoded = serde_json::to_value(value).expect("serialize");
    let decoded: T = serde_json::from_value(encoded.clone()).expect("deserialize");
    let reparsed = serde_json::to_value(decoded).expect("reserialize");
    assert_eq!(encoded, reparsed);
}

fn get_param_string<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| value.get(*key).and_then(Value::as_str))
}

fn request_id_text(entry: &Value) -> Option<String> {
    entry
        .get("id")
        .and_then(|id| id.as_str().map(str::to_string))
        .or_else(|| entry.get("id").and_then(|id| id.as_u64().map(|id| id.to_string())))
        .or_else(|| {
            entry
                .get("id")
                .and_then(|id| id.as_i64().map(|id| id.to_string()))
        })
}

#[test]
fn protocol_request_contracts_are_deterministic_roundtrips() {
    use codex_api::{
        JSONRPCError, JSONRPCErrorError, JSONRPCMessage, JSONRPCNotification, JSONRPCRequest,
        JSONRPCResponse, RequestId,
    };

    assert_json_roundtrip(&ThreadStartRequest {
        model: None,
        model_provider: None,
        cwd: None,
        sandbox: None,
    });
    assert_json_roundtrip(&ThreadStartRequest {
        model: Some("gpt-4o".into()),
        model_provider: Some("openai".into()),
        cwd: Some("/tmp".into()),
        sandbox: Some("safe".into()),
    });
    assert_json_roundtrip(&TurnStartRequest {
        thread_id: "thread-1".into(),
        input: vec![TextPayload::text("first input")],
    });
    assert_json_roundtrip(&TurnSteerRequest {
        thread_id: "thread-1".into(),
        expected_turn_id: "turn-1".into(),
        input: vec![
            TextPayload::text("steer"),
            TextPayload {
                kind: "text".into(),
                text: "with-elements".into(),
                text_elements: vec![serde_json::json!({"type":"code","text":"echo test"})],
            },
        ],
    });
    assert_json_roundtrip(&TurnInterruptRequest {
        thread_id: "thread-1".into(),
        turn_id: "turn-1".into(),
    });
    assert_json_roundtrip(&JSONRPCRequest {
        id: RequestId::String("request-1".into()),
        method: "thread/start".into(),
        params: Some(serde_json::json!({ "thread_id": "thread-1" })),
    });
    assert_json_roundtrip(&JSONRPCRequest {
        id: RequestId::Integer(7),
        method: "turn/start".into(),
        params: None,
    });
    assert_json_roundtrip(&JSONRPCNotification {
        method: "turn/completed".into(),
        params: Some(serde_json::json!({ "thread_id":"thread-1","turn_id":"turn-1" })),
    });
    assert_json_roundtrip(&JSONRPCResponse {
        id: RequestId::String("request-1".into()),
        result: serde_json::json!({ "thread_id": "thread-1" }),
    });
    assert_json_roundtrip(&JSONRPCError {
        id: RequestId::Integer(99),
        error: JSONRPCErrorError {
            code: -32601,
            message: "Request not handled".into(),
            data: Some(serde_json::json!({
                "method": "item/commandExecution/requestApproval",
                "status": "unhandled",
            })),
        },
    });

    let request_message = JSONRPCMessage::Response(JSONRPCResponse {
        id: RequestId::String("request-1".into()),
        result: serde_json::json!({ "thread_id": "thread-1" }),
    });
    assert_json_roundtrip(&request_message);
    let request_message = JSONRPCMessage::Error(JSONRPCError {
        id: RequestId::Integer(7),
        error: JSONRPCErrorError {
            code: -32000,
            message: "Injected failure".into(),
            data: None,
        },
    });
    assert_json_roundtrip(&request_message);

    for method in ["thread/start", "turn/start", "turn/steer", "turn/interrupt"] {
        let value = serde_json::json!({"jsonrpc":"2.0","method":method});
        assert!(value.get("method").and_then(Value::as_str) == Some(method));
    }
}

#[tokio::test]
async fn foreman_entrypoint_action_matrix_has_exact_app_server_method_contract() {
    #[derive(Clone, Copy)]
    enum MatrixAction {
        SpawnAgent,
        SendTurn,
        Steer,
        Interrupt,
    }

    let matrix = [
        (MatrixAction::SpawnAgent, vec!["thread/start", "turn/start"]),
        (MatrixAction::SendTurn, vec!["turn/start"]),
        (MatrixAction::Steer, vec!["turn/steer"]),
        (MatrixAction::Interrupt, vec!["turn/interrupt"]),
    ];

    for (action, expected_methods) in matrix {
        let temp_dir = tempdir().expect("tempdir");
        let request_log = temp_dir.path().join("methods.log");
        let script_path = temp_dir.path().join("fake_app_server_matrix.sh");
        let script = r#"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi

touch "__REQUEST_LOG__"

extract_method() {
  echo "$1" | sed -n 's/.*"method":"\([^\"]*\)".*/\1/p'
}
extract_id() {
  echo "$1" | sed -n 's/.*"id":"\([^\"]*\)".*/\1/p'
}

while read -r line; do
  [ -z "$line" ] && continue
  echo "$line" >> "__REQUEST_LOG__"

  method="$(extract_method "$line")"
  id="$(extract_id "$line")"
  [ -z "$id" ] && continue

  case "$method" in
    initialize)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
      ;;
    "thread/start")
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"thread_id\":\"thread-1\"}}"
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"thread/status/changed\",\"params\":{\"thread_id\":\"thread-1\",\"status\":\"running\"}}"
      ;;
    "turn/start")
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"turn_id\":\"turn-1\"}}"
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"turn/started\",\"params\":{\"thread_id\":\"thread-1\",\"turn_id\":\"turn-1\"}}"
      ;;
    "turn/steer")
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"turn_id\":\"turn-2\",\"expected_turn_id\":\"turn-1\"}}"
      ;;
    "turn/interrupt")
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
      ;;
    *)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"error\":{\"code\":-32601,\"message\":\"unsupported method $method\"}}"
      ;;
  esac
done
"#
        .replace("__REQUEST_LOG__", &request_log.to_string_lossy());
        write_fake_app_server_script(&script_path, &script);

        let (event_tx, _event_rx) = broadcast::channel(16);
        let client = AppServerClient::connect(script_path.to_str().expect("script path"), &[], event_tx)
            .await
            .expect("connect fake app-server");

        match action {
            MatrixAction::SpawnAgent => {
                let thread = client
                    .thread_start(&ThreadStartRequest {
                        model: Some("gpt-4o".into()),
                        model_provider: Some("openai".into()),
                        cwd: Some("/tmp".into()),
                        sandbox: Some("default".into()),
                    })
                    .await
                    .expect("thread start");

                client
                    .turn_start(&TurnStartRequest {
                        thread_id: thread.thread_id,
                        input: vec![TextPayload::text("first message")],
                    })
                    .await
                    .expect("turn start");
            }
            MatrixAction::SendTurn => {
                client
                    .turn_start(&TurnStartRequest {
                        thread_id: "thread-send-turn".into(),
                        input: vec![TextPayload::text("send turn message")],
                    })
                    .await
                    .expect("turn start");
            }
            MatrixAction::Steer => {
                client
                    .turn_steer(&TurnSteerRequest {
                        thread_id: "thread-steer".into(),
                        expected_turn_id: "turn-1".into(),
                        input: vec![TextPayload::text("steer message")],
                    })
                    .await
                    .expect("turn steer");
            }
            MatrixAction::Interrupt => {
                client
                    .turn_interrupt(&TurnInterruptRequest {
                        thread_id: "thread-interrupt".into(),
                        turn_id: "turn-1".into(),
                    })
                    .await
                    .expect("turn interrupt");
            }
        }

        let logged_methods: Vec<String> = std::fs::read_to_string(&request_log)
            .expect("read request log")
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|line| {
                let has_id = line.get("id").and_then(Value::as_str).is_some();
                if !has_id {
                    return None;
                }
                let method = line.get("method").and_then(Value::as_str)?;
                if method == "initialize" {
                    return None;
                }
                Some(method.to_string())
            })
            .collect();

        let expected_methods: Vec<String> = expected_methods
            .iter()
            .map(|method| (*method).to_string())
            .collect();
        assert_eq!(logged_methods, expected_methods);
    }
}

#[tokio::test]
async fn client_exposes_foreman_spawn_send_steer_interrupt_contract() {
    let temp_dir = tempdir().expect("tempdir");
    let request_log = temp_dir.path().join("requests.log");
    let script_path = temp_dir.path().join("fake_app_server_orchestration.sh");
    let script = r#"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi

touch "__REQUEST_LOG__"

extract_method() {
  echo "$1" | sed -n 's/.*"method":"\([^\"]*\)".*/\1/p'
}

extract_id() {
  echo "$1" | sed -n 's/.*"id":"\([^\"]*\)".*/\1/p'
}

while read -r line; do
  [ -z "$line" ] && continue
  echo "$line" >> "__REQUEST_LOG__"
  method="$(extract_method "$line")"
  id="$(extract_id "$line")"
  [ -z "$id" ] && continue

  case "$method" in
    initialize)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
      ;;
    "thread/start")
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"thread_id\":\"thread-1\"}}"
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"thread/status/changed\",\"params\":{\"thread_id\":\"thread-1\",\"status\":\"running\"}}"
      ;;
    "turn/start")
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"turn_id\":\"turn-1\"}}"
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"turn/started\",\"params\":{\"thread_id\":\"thread-1\",\"turn_id\":\"turn-1\"}}"
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"turn/completed\",\"params\":{\"thread_id\":\"thread-1\",\"turn_id\":\"turn-1\"}}"
      ;;
    "turn/steer")
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"turn_id\":\"turn-2\",\"expected_turn_id\":\"turn-1\"}}"
      ;;
    "turn/interrupt")
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
      ;;
    *)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"error\":{\"code\":-32601,\"message\":\"unsupported method $method\"}}"
      ;;
  esac
done
"#
    .replace("__REQUEST_LOG__", &request_log.to_string_lossy());
    write_fake_app_server_script(&script_path, &script);

    let (event_tx, mut event_rx) = broadcast::channel(16);
    let client = AppServerClient::connect(script_path.to_str().expect("script path"), &[], event_tx)
        .await
        .expect("connect fake app-server");

    let thread = client
        .thread_start(&ThreadStartRequest {
            model: Some("gpt-4o".into()),
            model_provider: Some("openai".into()),
            cwd: Some("/tmp".into()),
            sandbox: Some("default".into()),
        })
        .await
        .expect("thread start");
    assert_eq!(thread.thread_id, "thread-1");

    let turn = client
        .turn_start(&TurnStartRequest {
            thread_id: thread.thread_id.clone(),
            input: vec![TextPayload::text("first prompt")],
        })
        .await
        .expect("turn start");
    assert_eq!(turn.turn_id, "turn-1");

    let steer = client
        .turn_steer(&TurnSteerRequest {
            thread_id: thread.thread_id.clone(),
            expected_turn_id: turn.turn_id.clone(),
            input: vec![TextPayload::text("steer prompt")],
        })
        .await
        .expect("turn steer");
    assert_eq!(steer.turn_id, "turn-2");
    assert_eq!(steer.expected_turn_id, Some("turn-1".into()));

    let _: EmptyResponse = client
        .turn_interrupt(&TurnInterruptRequest {
            thread_id: thread.thread_id,
            turn_id: "turn-2".into(),
        })
        .await
        .expect("turn interrupt");

    let completed = timeout(Duration::from_millis(500), async {
        loop {
            let event = event_rx.recv().await.expect("event");
            if event.method == "turn/completed" {
                return event;
            }
        }
    })
    .await
    .expect("completed event");
    assert_eq!(completed.params["thread_id"], "thread-1");
    assert_eq!(completed.params["turn_id"], "turn-1");

    let request_lines = std::fs::read_to_string(&request_log).expect("read request log");
    let requests: Vec<Value> = request_lines
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();

    let thread_request = requests
        .iter()
        .find(|line| line.get("method") == Some(&Value::String("thread/start".into())))
        .expect("thread/start request");
    assert_eq!(thread_request["params"]["cwd"], "/tmp");
    assert_eq!(thread_request["params"]["model"], "gpt-4o");
    assert_eq!(
        get_param_string(&thread_request["params"], &["model_provider", "modelProvider"]),
        Some("openai")
    );
    assert_eq!(thread_request["params"]["sandbox"], "default");

    let turn_request = requests
        .iter()
        .find(|line| line.get("method") == Some(&Value::String("turn/start".into())))
        .expect("turn/start request");
    let turn_payload = &turn_request["params"]["input"];
    assert_eq!(turn_payload.as_array().map(|v| v.len()), Some(1));
    assert_eq!(turn_payload[0]["type"], "text");
    assert_eq!(turn_payload[0]["text"], "first prompt");

    let steer_request = requests
        .iter()
        .find(|line| line.get("method") == Some(&Value::String("turn/steer".into())))
        .expect("turn/steer request");
    assert_eq!(get_param_string(&steer_request["params"], &["thread_id", "threadId"]), Some("thread-1"));
    assert_eq!(get_param_string(&steer_request["params"], &["expected_turn_id", "expectedTurnId"]), Some("turn-1"));
    assert_eq!(steer_request["params"]["input"][0]["text"], "steer prompt");

    let interrupt_request = requests
        .iter()
        .find(|line| line.get("method") == Some(&Value::String("turn/interrupt".into())))
        .expect("turn/interrupt request");
    assert_eq!(get_param_string(&interrupt_request["params"], &["thread_id", "threadId"]), Some("thread-1"));
    assert_eq!(get_param_string(&interrupt_request["params"], &["turn_id", "turnId"]), Some("turn-2"));
}

#[tokio::test]
async fn client_uses_app_server_compatible_request_casing_and_response_shapes() {
    let temp_dir = tempdir().expect("tempdir");
    let request_log = temp_dir.path().join("requests.log");
    let script_path = temp_dir.path().join("fake_app_server_compat.sh");
    let script = r#"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi

touch "__REQUEST_LOG__"

extract_method() {
  echo "$1" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p'
}
extract_id() {
  echo "$1" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p'
}

while read -r line; do
  [ -z "$line" ] && continue
  echo "$line" >> "__REQUEST_LOG__"

  method="$(extract_method "$line")"
  id="$(extract_id "$line")"
  [ -z "$id" ] && continue

  case "$method" in
    initialize)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
      ;;
    "thread/start")
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"thread\":{\"id\":\"thread-1\"}}}"
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"thread/status/changed\",\"params\":{\"thread_id\":\"thread-1\",\"status\":\"running\"}}"
      ;;
    "turn/start")
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"turn\":{\"id\":\"turn-1\"}}}"
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"turn/started\",\"params\":{\"thread_id\":\"thread-1\",\"turn_id\":\"turn-1\"}}"
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"turn/completed\",\"params\":{\"thread_id\":\"thread-1\",\"turn_id\":\"turn-1\"}}"
      ;;
    "turn/steer")
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"turn\":{\"id\":\"turn-2\",\"expected_turn_id\":\"turn-1\"}}}"
      ;;
    "turn/interrupt")
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
      ;;
    *)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"error\":{\"code\":-32601,\"message\":\"unsupported method $method\"}}"
      ;;
  esac
done
"#
    .replace("__REQUEST_LOG__", &request_log.to_string_lossy());
    write_fake_app_server_script(&script_path, &script);

    let (event_tx, mut event_rx) = broadcast::channel(16);
    let client = AppServerClient::connect(script_path.to_str().expect("script path"), &[], event_tx)
        .await
        .expect("connect fake app-server");

    let thread = client
        .thread_start(&ThreadStartRequest {
            model: Some("gpt-4o".into()),
            model_provider: Some("openai".into()),
            cwd: Some("/tmp".into()),
            sandbox: Some("default".into()),
        })
        .await
        .expect("thread start");
    assert_eq!(thread.thread_id, "thread-1");

    let turn = client
        .turn_start(&TurnStartRequest {
            thread_id: thread.thread_id.clone(),
            input: vec![TextPayload::text("compat test")],
        })
        .await
        .expect("turn start");
    assert_eq!(turn.turn_id, "turn-1");

    let steer = client
        .turn_steer(&TurnSteerRequest {
            thread_id: thread.thread_id.clone(),
            expected_turn_id: "turn-1".into(),
            input: vec![TextPayload::text("compat steer")],
        })
        .await
        .expect("turn steer");
    assert_eq!(steer.turn_id, "turn-2");
    assert_eq!(steer.expected_turn_id, Some("turn-1".into()));

    client
        .turn_interrupt(&TurnInterruptRequest {
            thread_id: thread.thread_id,
            turn_id: "turn-2".into(),
        })
        .await
        .expect("turn interrupt");

    let _completed = timeout(Duration::from_millis(500), async {
        loop {
            let event = event_rx.recv().await.expect("event");
            if event.method == "turn/completed" {
                return event;
            }
        }
    })
    .await
    .expect("turn completed");

    let requests: Vec<Value> = std::fs::read_to_string(&request_log)
        .expect("read request log")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();

    let turn_start = requests
        .iter()
        .find(|line| line.get("method") == Some(&Value::String("turn/start".into())))
        .expect("turn/start request");
    assert!(get_param_string(&turn_start["params"], &["threadId", "thread_id"]).is_some_and(|value| value == "thread-1"));

    let steer_request = requests
        .iter()
        .find(|line| line.get("method") == Some(&Value::String("turn/steer".into())))
        .expect("turn/steer request");
    assert_eq!(get_param_string(&steer_request["params"], &["threadId", "thread_id"]), Some("thread-1"));
    assert_eq!(get_param_string(&steer_request["params"], &["expectedTurnId", "expected_turn_id"]), Some("turn-1"));

    let interrupt_request = requests
        .iter()
        .find(|line| line.get("method") == Some(&Value::String("turn/interrupt".into())))
        .expect("turn/interrupt request");
    assert_eq!(get_param_string(&interrupt_request["params"], &["threadId", "thread_id"]), Some("thread-1"));
    assert_eq!(get_param_string(&interrupt_request["params"], &["turnId", "turn_id"]), Some("turn-2"));
}

#[tokio::test]
async fn client_auto_handles_all_known_server_requests_with_typed_payloads() {
    let temp_dir = tempdir().expect("tempdir");
    let request_log = temp_dir.path().join("auto-responses.log");
    let script_path = temp_dir.path().join("fake_app_server_server_request_acks.sh");
    let script = r#"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi

touch "__REQUEST_LOG__"

json_field() {
  value="$(printf '%s' "$2" | grep -o "\"$1\":\"[^\"]*\"" | sed -E 's/^"[^"]*":"([^"]*)"$/\1/' )";
  echo "$value";
}
extract_method() {
  json_field method "$1"
}
extract_id() {
  json_field id "$1"
}
send() {
  echo "$1" >> "__REQUEST_LOG__"
  printf "%s\n" "$1"
}
send() {
  echo "$1" >> "__REQUEST_LOG__"
  printf "%s\n" "$1"
}

while read -r line; do
  [ -z "$line" ] && continue
  echo "$line" >> "__REQUEST_LOG__"

  method="$(extract_method "$line")"
  id="$(extract_id "$line")"
  [ -z "$id" ] && continue

  case "$method" in
    initialize)
      send "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
      ;;
    "thread/start")
      send "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"thread_id\":\"thread-1\"}}"
      send "{\"jsonrpc\":\"2.0\",\"method\":\"item/commandExecution/requestApproval\",\"id\":\"server-approval-command\",\"params\":{\"command\":\"git status\"}}"
      if read -r reply; then echo "$reply" >> "__REQUEST_LOG__"; fi
      send "{\"jsonrpc\":\"2.0\",\"method\":\"item/fileChange/requestApproval\",\"id\":\"server-approval-file\",\"params\":{\"path\":\"README.md\"}}"
      if read -r reply; then echo "$reply" >> "__REQUEST_LOG__"; fi
      send "{\"jsonrpc\":\"2.0\",\"method\":\"item/requestUserInput\",\"id\":\"server-request-input\",\"params\":{\"prompt\":\"approve\"}}"
      if read -r reply; then echo "$reply" >> "__REQUEST_LOG__"; fi
      send "{\"jsonrpc\":\"2.0\",\"method\":\"item/tool/requestUserInput\",\"id\":\"server-request-tool-input\",\"params\":{\"tool\":\"noop\"}}"
      if read -r reply; then echo "$reply" >> "__REQUEST_LOG__"; fi
      send "{\"jsonrpc\":\"2.0\",\"method\":\"item/tool/call\",\"id\":\"server-tool-call\",\"params\":{\"tool\":\"noop\"}}"
      if read -r reply; then echo "$reply" >> "__REQUEST_LOG__"; fi
      ;;
    *)
      send "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
      ;;
  esac
done
"#
    .replace("__REQUEST_LOG__", &request_log.to_string_lossy());
    write_fake_app_server_script(&script_path, &script);

    let (event_tx, _event_rx) = broadcast::channel(16);
    let client = AppServerClient::connect(script_path.to_str().expect("script path"), &[], event_tx)
        .await
        .expect("connect fake app-server");

    let _thread = client
        .thread_start(&ThreadStartRequest {
            model: None,
            model_provider: None,
            cwd: None,
            sandbox: None,
        })
        .await
        .expect("thread start");

    let logs = timeout(Duration::from_millis(1_000), async {
        loop {
            let contents = std::fs::read_to_string(&request_log).unwrap_or_default();
            if contents.contains("\"method\":\"item/commandExecution/requestApproval\"")
                && contents.contains("\"method\":\"item/fileChange/requestApproval\"")
                && contents.contains("\"method\":\"item/requestUserInput\"")
                && contents.contains("\"method\":\"item/tool/requestUserInput\"")
                && contents.contains("\"method\":\"item/tool/call\"")
                && contents.contains("\"id\":\"server-approval-command\"")
            {
                return contents;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let contents = std::fs::read_to_string(&request_log).unwrap_or_default();
        panic!("server request/response handshake timed out; logs:\n{contents}");
    });

    let messages: Vec<Value> = logs
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line.get("id").is_some() && line.get("result").is_some())
        .collect();

    let find_reply = |id: &str| -> Option<Value> {
        messages
            .iter()
            .find_map(|msg| (request_id_text(msg).as_deref() == Some(id)).then(|| msg.clone()))
    };

    let command_reply = find_reply("server-approval-command").expect("command approval response");
    assert_eq!(command_reply["result"]["decision"], "accept");

    let file_reply = find_reply("server-approval-file").expect("file approval response");
    assert_eq!(file_reply["result"]["decision"], "accept");

    let input_reply = find_reply("server-request-input").expect("requestUserInput response");
    assert_eq!(input_reply["result"]["decision"], "accept");

    let tool_input_reply = find_reply("server-request-tool-input").expect("tool requestUserInput response");
    assert_eq!(tool_input_reply["result"]["decision"], "accept");

    let tool_call = find_reply("server-tool-call").expect("tool/call response");
    assert_eq!(tool_call["result"]["success"], true);
    assert_eq!(tool_call["result"]["content_items"], Value::Array(vec![]));
}

#[tokio::test]
async fn client_matches_inflight_requests_by_id_when_responses_are_out_of_order() {
    let temp_dir = tempdir().expect("tempdir");
    let request_log = temp_dir.path().join("inflight.log");
    let script_path = temp_dir.path().join("fake_app_server_out_of_order.sh");
    let script = r#"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi

touch "__REQUEST_LOG__"

json_field() {
  value="$(printf '%s' "$2" | grep -o "\"$1\":\"[^\"]*\"" | sed -E 's/^"[^"]*":"([^"]*)"$/\1/' )";
  echo "$value";
}
extract_method() {
  json_field method "$1"
}
extract_id() {
  json_field id "$1"
}
send() {
  echo "$1" >> "__REQUEST_LOG__"
  printf "%s\n" "$1"
}

seen=0
thread_id_1=""
thread_id_2=""
while read -r line; do
  [ -z "$line" ] && continue
  echo "$line" >> "__REQUEST_LOG__"
  method="$(extract_method "$line")"
  id="$(extract_id "$line")"
  [ -z "$id" ] && continue

  if [ "$method" = "initialize" ]; then
    send "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
    continue
  fi

  if [ "$method" = "thread/start" ]; then
    seen=$((seen + 1))
    if [ "$seen" -eq 1 ]; then
      thread_id_1="$id"
    elif [ "$seen" -eq 2 ]; then
      thread_id_2="$id"
      send "{\"jsonrpc\":\"2.0\",\"id\":\"$thread_id_2\",\"result\":{\"thread_id\":\"thread-2\"}}"
      send "{\"jsonrpc\":\"2.0\",\"id\":\"$thread_id_1\",\"result\":{\"thread_id\":\"thread-1\"}}"
    fi
  fi
done
"#
    .replace("__REQUEST_LOG__", &request_log.to_string_lossy());
    write_fake_app_server_script(&script_path, &script);

    let (event_tx, _event_rx) = broadcast::channel(16);
    let client = AppServerClient::connect(script_path.to_str().expect("script path"), &[], event_tx)
        .await
        .expect("connect fake app-server");

    let first_request = ThreadStartRequest {
        model: None,
        model_provider: None,
        cwd: Some("first".into()),
        sandbox: None,
    };
    let second_request = ThreadStartRequest {
        model: None,
        model_provider: None,
        cwd: Some("second".into()),
        sandbox: None,
    };
    let first = client.thread_start(&first_request);
    let second = client.thread_start(&second_request);
    let (first, second) = tokio::join!(first, second);
    let first = first.expect("first thread start");
    let second = second.expect("second thread start");
    assert_eq!(first.thread_id, "thread-1");
    assert_eq!(second.thread_id, "thread-2");

    let request_log_contents = std::fs::read_to_string(&request_log).expect("read log");
    let lines: Vec<Value> = request_log_contents
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();

    let request_ids: Vec<String> = lines
        .iter()
        .filter(|entry| entry.get("method") == Some(&Value::String("thread/start".into())))
        .filter_map(request_id_text)
        .collect();
    if request_ids.len() != 2 {
        panic!(
            "expected 2 thread/start request ids, got {}.\nlog:\n{}",
            request_ids.len(),
            request_log_contents
        );
    }
    assert_eq!(request_ids.len(), 2);
    let response_ids: Vec<String> = lines
        .iter()
        .filter(|entry| entry.get("result").is_some())
        .filter_map(request_id_text)
        .filter(|id| request_ids.contains(id))
        .collect();
    assert_eq!(response_ids.len(), 2);
    assert_eq!(response_ids[0], request_ids[1]);
    assert_eq!(response_ids[1], request_ids[0]);
}
