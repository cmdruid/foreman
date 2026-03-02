use std::{env, fs, path::PathBuf, time::{Duration, Instant}};

use codex_api::{
    AppServerClient, TextPayload, ThreadStartRequest, TurnInterruptRequest, TurnStartRequest,
    TurnSteerRequest,
};
use serde_json::Value;
use tempfile::tempdir;
use tokio::{sync::broadcast, time::timeout};

const DEFAULT_SMOKE_MODEL: &str = "gpt-5.3-codex-spark";
const SMOKE_FILE_REL_PATH: &str = ".audit-generated/smoke-app-server.txt";
const SMOKE_TIMEOUT: Duration = Duration::from_secs(120);
const SMOKE_EVENT_POLL_MS: u64 = 500;
const SMOKE_TURN_STEER_TIMEOUT: Duration = Duration::from_secs(45);

fn smoke_test_enabled() -> bool {
    matches!(
        env::var("CODEX_API_SMOKE_TEST")
            .ok()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn resolve_codex_binary() -> String {
    env::var("CODEX_BIN").unwrap_or_else(|_| "codex".to_string())
}

#[derive(Clone)]
struct SmokeModel {
    id: String,
    provider: Option<String>,
}

fn select_model_for_smoke(response: &Value) -> SmokeModel {
    let data = match response.get("data").and_then(Value::as_array) {
        Some(data) => data,
        None => return SmokeModel {
            id: DEFAULT_SMOKE_MODEL.to_string(),
            provider: None,
        },
    };

    for candidate in data {
        if let Some(model) = candidate
            .get("model")
            .and_then(Value::as_str)
            .or_else(|| candidate.get("id").and_then(Value::as_str))
        {
            if model.contains("codex") {
                return SmokeModel {
                    id: model.to_string(),
                    provider: candidate
                        .get("provider")
                        .and_then(Value::as_str)
                        .or_else(|| candidate.get("model_provider").and_then(Value::as_str))
                        .map(|provider| provider.to_string()),
                };
            }
        }
    }

    data.first()
        .and_then(|first| {
            let provider = first
                .get("provider")
                .and_then(Value::as_str)
                .or_else(|| first.get("model_provider").and_then(Value::as_str))
                .map(|provider| provider.to_string());

            first
                .get("model")
                .or_else(|| first.get("id"))
                .and_then(Value::as_str)
                .map(|model| SmokeModel {
                    id: model.to_string(),
                    provider,
                })
        })
        .unwrap_or_else(|| SmokeModel {
            id: DEFAULT_SMOKE_MODEL.to_string(),
            provider: None,
        })
}

fn is_command_like_event(method: &str) -> bool {
    method.contains("commandExecution")
        || method.contains("tool/call")
        || method.contains("fileChange")
        || method.contains("exec_command")
}

#[tokio::test]
#[ignore = "run with CODEX_API_SMOKE_TEST=1 for a live app-server validation"]
async fn codex_api_smoke_app_server_creates_real_file() {
    if !smoke_test_enabled() {
        return;
    }

    let codex_bin = resolve_codex_binary();
    let temp_dir = tempdir().expect("temporary smoke workspace");
    let workspace: PathBuf = temp_dir.path().to_path_buf();
    let output_path = workspace.join(SMOKE_FILE_REL_PATH);
    let output_text = "codex-api smoke test completed";

    let (event_tx, _event_rx) = broadcast::channel(32);
    let client = AppServerClient::connect(codex_bin.as_str(), &[], event_tx)
        .await
        .expect("connect to codex app-server");

    let model = client
        .request::<_, Value>("model/list", &serde_json::json!({}))
        .await
        .map(|model_list| select_model_for_smoke(&model_list))
        .unwrap_or_else(|_| SmokeModel {
            id: DEFAULT_SMOKE_MODEL.to_string(),
            provider: None,
        });

    let thread = client
        .thread_start(&ThreadStartRequest {
            model: Some(model.id),
            model_provider: model.provider,
            cwd: Some(workspace.to_string_lossy().to_string()),
            sandbox: Some("workspace-write".to_string()),
        })
        .await
        .expect("start app-server thread");

    let prompt = format!(
        "Create the file `{}` using shell commands and write exactly this content: `{}`. Return DONE.",
        SMOKE_FILE_REL_PATH,
        output_text
    );

    let mut event_rx = client.subscribe_events();
    let _turn = client
        .turn_start(&TurnStartRequest {
            thread_id: thread.thread_id,
            input: vec![TextPayload::text(prompt)],
        })
        .await
        .expect("start turn with file creation prompt");

    let mut saw_command_execution = false;
    let mut saw_turn_complete = false;
    let start = Instant::now();

    while start.elapsed() < SMOKE_TIMEOUT {
        if output_path.exists() && saw_command_execution && saw_turn_complete {
            break;
        }

        match timeout(Duration::from_millis(SMOKE_EVENT_POLL_MS), event_rx.recv()).await {
            Ok(Ok(notification)) => {
                if is_command_like_event(&notification.method) {
                    saw_command_execution = true;
                }
                if notification.method == "turn/completed"
                    || notification.method == "codex/event/task_complete"
                {
                    saw_turn_complete = true;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => {}
        }
    }

    assert!(
        start.elapsed() < SMOKE_TIMEOUT,
        "smoke test timed out after {SMOKE_TIMEOUT:?}; saw_command_execution={saw_command_execution}, saw_turn_complete={saw_turn_complete}, file_exists={}",
        output_path.exists()
    );
    assert!(output_path.exists(), "smoke expected output file {SMOKE_FILE_REL_PATH} to exist");

    let generated = fs::read_to_string(output_path.clone()).expect("smoke file should be written");
    assert!(
        !generated.is_empty(),
        "smoke file was created but empty"
    );
    assert!(
        generated.contains(output_text),
        "unexpected smoke file content: {generated}"
    );
    assert!(
        saw_command_execution,
        "expected at least one exec command event from codex"
    );
    assert!(
        saw_turn_complete,
        "expected turn completion event from codex"
    );
}

#[tokio::test]
#[ignore = "run with CODEX_API_SMOKE_TEST=1 for a live app-server validation"]
async fn codex_api_smoke_app_server_turn_contract() {
    if !smoke_test_enabled() {
        return;
    }

    let codex_bin = resolve_codex_binary();
    let temp_dir = tempdir().expect("temporary smoke workspace");
    let workspace: PathBuf = temp_dir.path().to_path_buf();

    let (event_tx, _event_rx) = broadcast::channel(32);
    let client = AppServerClient::connect(codex_bin.as_str(), &[], event_tx)
        .await
        .expect("connect to codex app-server");

    let model = client
        .request::<_, Value>("model/list", &serde_json::json!({}))
        .await
        .map(|model_list| select_model_for_smoke(&model_list))
        .unwrap_or_else(|_| SmokeModel {
            id: DEFAULT_SMOKE_MODEL.to_string(),
            provider: None,
        });

    let thread = client
        .thread_start(&ThreadStartRequest {
            model: Some(model.id),
            model_provider: model.provider,
            cwd: Some(workspace.to_string_lossy().to_string()),
            sandbox: Some("workspace-write".to_string()),
        })
        .await
        .expect("start app-server thread");

    let mut event_rx = client.subscribe_events();
    let turn = client
        .turn_start(&TurnStartRequest {
            thread_id: thread.thread_id.clone(),
            input: vec![TextPayload::text(
                "Run the shell command: `sleep 8 && echo turn-started` and return DONE after it completes."
                    .to_string(),
            )],
        })
        .await
        .expect("start initial turn");

    let mut saw_turn_started = false;
    let mut saw_turn_completed = false;
    let mut saw_command_event = false;
    let start = Instant::now();

    while start.elapsed() < SMOKE_TURN_STEER_TIMEOUT {
        if saw_turn_started {
            break;
        }

        match timeout(Duration::from_millis(SMOKE_EVENT_POLL_MS), event_rx.recv()).await {
            Ok(Ok(notification)) => {
                if notification.method == "turn/started" {
                    saw_turn_started = true;
                }
                if is_command_like_event(&notification.method) {
                    saw_command_event = true;
                }
                if notification.method == "turn/completed" {
                    saw_turn_completed = true;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => {}
        }
    }

    // `turn/steer` is only valid while a turn is active in some backends.
    // Treat the expected inactive-turn error as a valid outcome here so the smoke
    // checks request/response plumbing without hanging on short-lived turns.
    let steer = client
        .turn_steer(&TurnSteerRequest {
            thread_id: thread.thread_id.clone(),
            expected_turn_id: turn.turn_id.clone(),
            input: vec![TextPayload::text(
                "If the previous command is still running, do not cancel it. Return DONE.".to_string(),
            )],
        })
        .await;

    match steer {
        Ok(steer) => assert!(!steer.turn_id.is_empty(), "steer returned empty turn id"),
        Err(err) => {
            let msg = err.to_string().to_lowercase();
            assert!(
                msg.contains("no active turn"),
                "unexpected steer error: {msg}"
            );
        }
    }

    while start.elapsed() < SMOKE_TURN_STEER_TIMEOUT {
        match timeout(Duration::from_millis(SMOKE_EVENT_POLL_MS), event_rx.recv()).await {
            Ok(Ok(notification)) => {
                if notification.method == "turn/completed" {
                    saw_turn_completed = true;
                }
                if is_command_like_event(&notification.method) {
                    saw_command_event = true;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => {}
        }
    }

    assert!(
        saw_turn_started || saw_turn_completed || saw_command_event,
        "expected turn lifecycle or command-related events"
    );
    assert!(saw_command_event, "expected command-related event during steer smoke");
    assert!(saw_turn_started, "expected turn/started notification");
}

#[tokio::test]
#[ignore = "run with CODEX_API_SMOKE_TEST=1 for a live app-server validation"]
async fn codex_api_smoke_app_server_model_list() {
    if !smoke_test_enabled() {
        return;
    }

    let codex_bin = resolve_codex_binary();
    let (event_tx, _event_rx) = broadcast::channel(16);
    let client = AppServerClient::connect(codex_bin.as_str(), &[], event_tx)
        .await
        .expect("connect to codex app-server");

    let response = client
        .request::<_, Value>("model/list", &serde_json::json!({}))
        .await
        .expect("query model list");

    assert!(
        response.get("data").is_some() || response.is_object(),
        "model/list should return structured data, got: {response}"
    );

    let models = response
        .get("data")
        .or_else(|| response.get("models"))
        .and_then(Value::as_array)
        .expect("model list payload should include an array under `data` or `models`");
    assert!(
        !models.is_empty(),
        "model/list returned no models"
    );
}

#[tokio::test]
#[ignore = "run with CODEX_API_SMOKE_TEST=1 for a live app-server validation"]
async fn codex_api_smoke_app_server_turn_interrupt() {
    if !smoke_test_enabled() {
        return;
    }

    let codex_bin = resolve_codex_binary();
    let temp_dir = tempdir().expect("temporary smoke workspace");
    let workspace: PathBuf = temp_dir.path().to_path_buf();

    let (event_tx, _event_rx) = broadcast::channel(32);
    let client = AppServerClient::connect(codex_bin.as_str(), &[], event_tx)
        .await
        .expect("connect to codex app-server");

    let model = client
        .request::<_, Value>("model/list", &serde_json::json!({}))
        .await
        .map(|model_list| select_model_for_smoke(&model_list))
        .unwrap_or_else(|_| SmokeModel {
            id: DEFAULT_SMOKE_MODEL.to_string(),
            provider: None,
        });

    let thread = client
        .thread_start(&ThreadStartRequest {
            model: Some(model.id),
            model_provider: model.provider,
            cwd: Some(workspace.to_string_lossy().to_string()),
            sandbox: Some("workspace-write".to_string()),
        })
        .await
        .expect("start app-server thread");

    let turn = client
        .turn_start(&TurnStartRequest {
            thread_id: thread.thread_id.clone(),
            input: vec![TextPayload::text(
                "Echo `interrupt-smoke` and return DONE."
                    .to_string(),
            )],
        })
        .await
        .expect("start interrupt-target turn");

    tokio::time::sleep(Duration::from_secs(3)).await;

    let interrupt_result = client
        .turn_interrupt(&TurnInterruptRequest {
            thread_id: thread.thread_id,
            turn_id: turn.turn_id.clone(),
        })
        .await;

    if let Err(err) = interrupt_result {
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("no active turn") || msg.contains("not found") || msg.contains("already completed"),
            "unexpected interrupt error: {msg}"
        );
        return;
    }

}

#[tokio::test]
#[ignore = "run with CODEX_API_SMOKE_TEST=1 for a live app-server validation"]
async fn codex_api_smoke_app_server_thread_interrupt() {
    if !smoke_test_enabled() {
        return;
    }

    let codex_bin = resolve_codex_binary();
    let temp_dir = tempdir().expect("temporary smoke workspace");
    let workspace: PathBuf = temp_dir.path().to_path_buf();

    let (event_tx, _event_rx) = broadcast::channel(16);
    let client = AppServerClient::connect(codex_bin.as_str(), &[], event_tx)
        .await
        .expect("connect to codex app-server");

    let model = client
        .request::<_, Value>("model/list", &serde_json::json!({}))
        .await
        .map(|model_list| select_model_for_smoke(&model_list))
        .unwrap_or_else(|_| SmokeModel {
            id: DEFAULT_SMOKE_MODEL.to_string(),
            provider: None,
        });

    let thread = client
        .thread_start(&ThreadStartRequest {
            model: Some(model.id),
            model_provider: model.provider,
            cwd: Some(workspace.to_string_lossy().to_string()),
            sandbox: Some("workspace-write".to_string()),
        })
        .await
        .expect("start app-server thread");

    let thread_interrupt = client
        .request::<_, Value>("thread/interrupt", &serde_json::json!({ "thread_id": thread.thread_id }))
        .await;

    match thread_interrupt {
        Ok(value) => {
            assert!(
                value.is_object() || value.is_null(),
                "thread/interrupt response payload should be object or null: {value}"
            );
        }
        Err(err) => {
            let msg = err.to_string().to_lowercase();
            assert!(
                !msg.contains("timed out"),
                "thread/interrupt unexpectedly timed out: {msg}"
            );
        }
    }
}
