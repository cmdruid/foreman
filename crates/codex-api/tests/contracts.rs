use std::time::Duration;

use codex_api::AppServerClient;
use serde_json::Value;
use tempfile::tempdir;
use tokio::sync::broadcast;

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

#[tokio::test]
async fn client_contract_covers_supported_request_methods() {
    let temp_dir = tempdir().expect("tempdir");
    let request_log = temp_dir.path().join("requests.log");
    let script_path = temp_dir.path().join("fake_app_server_contract.sh");
    let script = r#"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi

touch "__REQUEST_LOG__"

extract_id() {
  echo "$1" | sed -n 's/.*\"id\":\"\([^\"]*\)\".*/\1/p'
}

while read -r line; do
  [ -z "$line" ] && continue
  echo "$line" >> "__REQUEST_LOG__"
  id="$(echo "$line" | sed -n 's/.*\"id\":\"\([^\"]*\)\".*/\1/p')"
  method="$(echo "$line" | sed -n 's/.*\"method\":\"\([^\"]*\)\".*/\1/p')"

  if [ -z "$id" ]; then
    continue
  fi

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

    let (event_tx, _event_rx) = broadcast::channel(16);
    let client = AppServerClient::connect(script_path.to_str().expect("path"), &[], event_tx)
        .await
        .expect("connect fake app-server");

    let thread = client
        .request::<_, Value>(
            "thread/start",
            &serde_json::json!({
                "cwd": null,
                "model": null,
                    "model_provider": null,
                "sandbox": null
            }),
        )
        .await
        .expect("thread/start");
    assert_eq!(thread["thread_id"], "thread-1");

    let turn = client
        .request::<_, Value>(
            "turn/start",
            &serde_json::json!({
                "thread_id": "thread-1",
                "input": []
            }),
        )
        .await
        .expect("turn/start");
    assert_eq!(turn["turn_id"], "turn-1");

    let steer = client
        .request::<_, Value>(
            "turn/steer",
            &serde_json::json!({
                "thread_id": "thread-1",
                "expected_turn_id": "turn-1",
                "input": []
            }),
        )
        .await
        .expect("turn/steer");
    assert_eq!(steer["expected_turn_id"], "turn-1");

    client
        .request::<_, Value>(
            "turn/interrupt",
            &serde_json::json!({
                "thread_id": "thread-1",
                "turn_id": "turn-1"
            }),
        )
        .await
        .expect("turn/interrupt");

    let raw = std::fs::read_to_string(&request_log).expect("read request log");
    let lines: Vec<_> = raw.lines().collect();

    assert!(
        lines
            .iter()
            .any(|line| line.contains("\"method\":\"initialize\""))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("\"method\":\"thread/start\""))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("\"method\":\"turn/start\""))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("\"method\":\"turn/steer\""))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("\"method\":\"turn/interrupt\""))
    );

    for method in [
        "initialize",
        "thread/start",
        "turn/start",
        "turn/steer",
        "turn/interrupt",
    ] {
        let count = lines
            .iter()
            .filter(|line| line.contains(&format!("\"method\":\"{method}\"")))
            .count();
        assert!(
            count >= 1,
            "expected at least one request for {method}; saw {count}"
        );
        assert!(
            lines
                .iter()
                .filter(|line| line.contains(&format!("\"method\":\"{method}\"")))
                .all(|line| line.contains("\"id\":\"")),
            "request for {method} missing id"
        );
    }
}

#[tokio::test]
async fn client_replies_to_unknown_server_requests_as_unhandled_error() {
    let temp_dir = tempdir().expect("tempdir");
    let request_log = temp_dir.path().join("request.log");
    let script_path = temp_dir.path().join("fake_app_server_unknown_request.sh");
    let script = r#"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi

touch "__REQUEST_LOG__"

extract_id() {
  echo "$1" | sed -n 's/.*\"id\":\"\([^\"]*\)\".*/\1/p'
}

while read -r line; do
  [ -z "$line" ] && continue
  echo "$line" >> "__REQUEST_LOG__"

  id="$(echo "$line" | sed -n 's/.*\"id\":\"\([^\"]*\)\".*/\1/p')"
  if [ -z "$id" ]; then
    continue
  fi

  if echo "$line" | grep -q '"method":"initialize"'; then
    printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
  elif echo "$line" | grep -q '"method":"thread/start"'; then
    printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"thread_id\":\"thread-1\"}}"
    printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"item/unknown/request\",\"id\":\"srv-1\",\"params\":{\"thread_id\":\"thread-1\"}}"
    if read -r response_line; then
      echo "$response_line" >> "__REQUEST_LOG__"
    fi
  else
    printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
  fi
done
"#
    .replace("__REQUEST_LOG__", &request_log.to_string_lossy());
    write_fake_app_server_script(&script_path, &script);

    let (event_tx, _event_rx) = broadcast::channel(16);
    let client = AppServerClient::connect(script_path.to_str().expect("path"), &[], event_tx)
        .await
        .expect("connect fake app-server");

    client
        .request::<_, Value>(
            "thread/start",
            &serde_json::json!({
                "cwd": null,
                "model": null,
                "model_provider": null,
                "sandbox": null
            }),
        )
        .await
        .expect("thread/start");

    let logs = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let logs = std::fs::read_to_string(&request_log).expect("read request log");
            if logs.contains("\"error\":{\"code\":-32601")
                && logs.contains("\"method\":\"item/unknown/request\"")
                && logs.contains("\"status\":\"unhandled\"")
            {
                return logs;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("client response to unknown server request");

    assert!(logs.contains("\"method\":\"item/unknown/request\""));
    assert!(logs.contains("\"status\":\"unhandled\""));
}

#[tokio::test]
async fn client_ignores_malformed_notifications_and_keeps_requesting() {
    let temp_dir = tempdir().expect("tempdir");
    let script_path = temp_dir.path().join("fake_app_server_malformed.sh");
    let log_path = temp_dir.path().join("lines.log");
    let script = r#"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi

touch "__LOG__"
extract_id() {
  echo "$1" | sed -n 's/.*\"id\":\"\([^\"]*\)\".*/\1/p'
}

while read -r line; do
  [ -z "$line" ] && continue
  echo "$line" >> "__LOG__"
  id="$(extract_id "$line")"
  method="$(echo "$line" | sed -n 's/.*\"method\":\"\([^\"]*\)\".*/\1/p')"

  if [ -z "$id" ]; then
    continue
  fi

  if echo "$line" | grep -q '"method":"initialize"'; then
    printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
    printf "%s\n" "{not-json"
    printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"thread/status/changed\",\"params\":{\"thread_id\":\"thread-1\",\"status\":\"running\"}}"
  elif echo "$line" | grep -q '"method":"thread/start"'; then
    printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"thread_id\":\"thread-1\"}}"
  else
    printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
  fi
done
"#
    .replace("__LOG__", &log_path.to_string_lossy());
    write_fake_app_server_script(&script_path, &script);

    let (event_tx, mut event_rx) = broadcast::channel(16);
    let client = AppServerClient::connect(script_path.to_str().expect("path"), &[], event_tx)
        .await
        .expect("connect fake app-server");

    let thread = client
        .request::<_, Value>(
            "thread/start",
            &serde_json::json!({
                "cwd": null,
                "model": null,
                "model_provider": null,
                "sandbox": null
            }),
        )
        .await
        .expect("thread/start");
    assert_eq!(thread["thread_id"], "thread-1");

    let event = tokio::time::timeout(Duration::from_millis(500), event_rx.recv())
        .await
        .expect("notification")
        .expect("event");
    assert_eq!(event.method, "thread/status/changed");
}
