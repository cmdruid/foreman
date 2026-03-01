use std::time::Duration;

use codex_api::AppServerClient;
use serde_json::Value;
use tempfile::tempdir;
use tokio::sync::broadcast;
use tokio::time::timeout;

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
async fn client_reports_lifecycle_and_notifications() {
    let temp_dir = tempdir().expect("tempdir");
    let request_log = temp_dir.path().join("requests.log");
    let log_path = request_log.to_string_lossy().to_string();
    let script_path = temp_dir.path().join("fake_app_server_lifecycle.sh");
    let script = r#"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi

request_index=0
touch "__LOG_PATH__"

extract_id() {
  echo "$1" | sed -n 's/.*\"id\": *\"\?\([^\",} ]*\)\"*.*/\1/p'
}

while read -r line; do
  [ -z "$line" ] && continue
  echo "$line" >> "__LOG_PATH__"

  if echo "$line" | grep -q '\"id\"'; then
    request_index=$((request_index + 1))
    id="$(extract_id "$line")"
  else
    continue
  fi

  case "$request_index" in
    1)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
      ;;
    2)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"thread_id\":\"thread-1\"}}"
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"thread/status/changed\",\"params\":{\"thread_id\":\"thread-1\",\"status\":\"running\"}}"
      echo '{"method":"thread/status/changed"}' >> "__LOG_PATH__"
      ;;
    3)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"turn_id\":\"turn-1\"}}"
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"turn/started\",\"params\":{\"thread_id\":\"thread-1\",\"turn_id\":\"turn-1\"}}"
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"turn/completed\",\"params\":{\"thread_id\":\"thread-1\",\"turn_id\":\"turn-1\"}}"
      echo '{"method":"turn/completed"}' >> "__LOG_PATH__"
      ;;
    4)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"turn_id\":\"turn-2\",\"expected_turn_id\":\"turn-1\"}}"
      ;;
    5)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"turn/aborted\",\"params\":{\"thread_id\":\"thread-1\",\"turn_id\":\"turn-1\"}}"
      echo '{"method":"turn/aborted"}' >> "__LOG_PATH__"
      ;;
    *)
      if [ -n "$id" ]; then
        printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"error\":{\"code\":-32601,\"message\":\"unsupported method index $request_index\"}}"
      fi
      ;;
  esac
done
"#
    .replace("__LOG_PATH__", &log_path);
    write_fake_app_server_script(&script_path, &script);

    let (event_tx, mut event_rx) = broadcast::channel(16);
    let client = AppServerClient::connect(script_path.to_str().expect("path"), &[], event_tx)
        .await
        .expect("connect fake app-server");

    assert_eq!(
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
            .expect("thread start"),
        serde_json::json!({"thread_id":"thread-1"})
    );
    assert_eq!(
        client
            .request::<_, Value>(
                "turn/start",
                &serde_json::json!({
                    "thread_id": "thread-1",
                    "input": []
                }),
            )
            .await
            .expect("turn start"),
        serde_json::json!({"turn_id":"turn-1"})
    );
    assert_eq!(
        client
            .request::<_, Value>(
                "turn/steer",
                &serde_json::json!({
                    "thread_id": "thread-1",
                    "expected_turn_id": "turn-1",
                    "input": []
                }),
            )
            .await
            .expect("turn steer"),
        serde_json::json!({"turn_id":"turn-2","expected_turn_id":"turn-1"})
    );
    assert_eq!(
        client
            .request::<_, Value>(
                "turn/interrupt",
                &serde_json::json!({
                "thread_id": "thread-1",
                    "turn_id": "turn-1"
                }),
            )
            .await
            .expect("turn interrupt"),
        serde_json::json!({})
    );

    let event = timeout(Duration::from_millis(1000), async {
        loop {
            let event = event_rx.recv().await.expect("event should be delivered");
            if event.method == "turn/completed" {
                break event;
            }
        }
    })
    .await
    .expect("event");
    assert_eq!(event.method, "turn/completed");
    assert_eq!(event.params["thread_id"], "thread-1");
    assert_eq!(event.params["turn_id"], "turn-1");

    let request_lines = std::fs::read_to_string(&request_log).expect("read request log");
    assert!(request_lines.contains("\"method\":\"initialize\""));
    assert!(request_lines.contains("\"method\":\"thread/start\""));
    assert!(request_lines.contains("\"method\":\"turn/start\""));
    assert!(request_lines.contains("\"method\":\"turn/completed\""));
}

#[tokio::test]
async fn client_retries_launch_after_transient_startup_failure() {
    let temp_dir = tempdir().expect("tempdir");
    let fail_marker = temp_dir.path().join("did_fail_once");
    let fail_marker = fail_marker.to_string_lossy().to_string();
    let script_path = temp_dir.path().join("fake_app_server_retry.sh");
    let script = r#"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi

if [ ! -f "__FAIL_MARKER__" ]; then
  touch "__FAIL_MARKER__"
  exit 1
fi

extract_id() {
  echo "$1" | sed -n 's/.*\"id\": *\"\?\([^\",} ]*\)\"*.*/\1/p'
}
while read -r line; do
  if echo "$line" | grep -q '\"id\"'; then
    id="$(extract_id "$line")"
    printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
  fi
done
"#
    .replace("__FAIL_MARKER__", &fail_marker);
    write_fake_app_server_script(&script_path, &script);

    let (event_tx, _event_rx) = broadcast::channel(16);
    let start = std::time::Instant::now();
    let _client = AppServerClient::connect(script_path.to_str().expect("path"), &[], event_tx)
        .await
        .expect("connect after retry");

    assert!(
        start.elapsed() >= Duration::from_millis(200),
        "expected retry backoff to apply"
    );
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "unexpectedly slow reconnect: {}",
        start.elapsed().as_millis()
    );
}

#[tokio::test]
async fn client_requests_return_rpc_errors() {
    let temp_dir = tempdir().expect("tempdir");
    let script_path = temp_dir.path().join("fake_app_server_error.sh");
    let script = r#"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi

request_index=0
extract_id() {
  echo "$1" | sed -n 's/.*\"id\": *\"\?\([^\",} ]*\)\"*.*/\1/p'
}
while read -r line; do
  if echo "$line" | grep -q '\"id\"'; then
    request_index=$((request_index + 1))
    id="$(extract_id "$line")"
  else
    continue
  fi

  case "$request_index" in
    1)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
      ;;
    2)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"error\":{\"code\":-32000,\"message\":\"injected error\"}}"
      ;;
    *)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
      ;;
  esac
done
"#;
    write_fake_app_server_script(&script_path, script);

    let (event_tx, _event_rx) = broadcast::channel(16);
    let client = AppServerClient::connect(script_path.to_str().expect("path"), &[], event_tx)
        .await
        .expect("connect");

    let result = client
        .request::<_, Value>("thread/start", &serde_json::json!({"cwd": null}))
        .await
        .expect_err("request expected to fail");
    let summary = format!("{result:#?}");
    assert!(
        summary.contains("injected error"),
        "unexpected error: {summary}"
    );
}

#[tokio::test]
async fn client_auto_approves_known_server_requests() {
    let temp_dir = tempdir().expect("tempdir");
    let output_path = temp_dir.path().join("request.log");
    let output_path = output_path.to_string_lossy().to_string();
    let script_path = temp_dir.path().join("fake_app_server_auto_approve.sh");
    let script = r#"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi

touch "__OUTPUT_PATH__"
extract_id() {
  echo "$1" | sed -n 's/.*\"id\": *\"\?\([^\",} ]*\)\"*.*/\1/p'
}
while read -r line; do
  [ -z "$line" ] && continue
  echo "$line" >> "__OUTPUT_PATH__"

  if [ -z "$line" ]; then
    continue
  fi

  if echo "$line" | grep -q '\"id\"'; then
    id="$(extract_id "$line")"
  else
    id=""
  fi

  case "$line" in
    *"\"method\":\"initialize\""*)
      if [ -n "$id" ]; then
        printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
      fi
      ;;
    *"\"method\":\"thread/start\""*)
      if [ -n "$id" ]; then
        printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"thread_id\":\"thread-1\"}}"
        printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"item/commandExecution/requestApproval\",\"id\":\"server-approve-1\",\"params\":{\"command\":\"noop\"}}"
        if read -r reply; then
          echo "$reply" >> "__OUTPUT_PATH__"
        fi
        printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"item/tool/call\",\"id\":\"server-tool-call\",\"params\":{\"tool\":\"noop\"}}"
        if read -r reply; then
          echo "$reply" >> "__OUTPUT_PATH__"
        fi
      fi
      ;;
    *)
      :
      ;;
  esac
done
"#
    .replace("__OUTPUT_PATH__", &output_path);
    write_fake_app_server_script(&script_path, &script);

    let (event_tx, _event_rx) = broadcast::channel(16);
    let client = AppServerClient::connect(script_path.to_str().expect("path"), &[], event_tx)
        .await
        .expect("connect");

    let thread_result = client
        .request::<_, Value>("thread/start", &serde_json::json!({"cwd": null}))
        .await
        .expect("start thread");
    assert_eq!(thread_result["thread_id"], "thread-1");

    let logs = timeout(Duration::from_millis(500), async {
        loop {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let contents = std::fs::read_to_string(&output_path).unwrap_or_default();
            if contents.contains("server-approve-1") && contents.contains("decision\":\"accept\"") {
                if contents.contains("server-tool-call")
                    && contents.contains("\"content_items\":[]")
                {
                    return contents;
                }
            }
        }
    })
    .await
    .expect("approval logs wait");
    assert!(logs.contains("\"id\":\"server-approve-1\""));
}
