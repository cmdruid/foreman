use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

mod common;

#[tokio::test]
async fn test_standalone_agent_flow_with_callbacks() {
    let webhook = common::start_webhook_capture().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let webhook_url = format!("{}/callback", webhook.url);

    let service_config_contents = service_config_template(&webhook_url);
    common::write_service_config(&service_config, service_config_contents.as_str())
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");

    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let create = client
        .post(format!("{}/agents", harness.base_url))
        .json(&json!({
            "prompt": "alpha startup",
            "callback_profile": "record_webhook",
            "callback_events": ["turn/completed"],
            "callback_vars": {"scope": "standalone"},
        }))
        .send()
        .await
        .expect("create agent request");

    assert_eq!(create.status(), StatusCode::CREATED);
    let agent = create.json::<Value>().await.expect("agent response json");
    let agent_id = agent["id"].as_str().expect("agent id");

    assert!(webhook.wait_for_events(1, Duration::from_secs(3)).await);

    let callback_events = webhook.events().await;
    assert_eq!(
        callback_events[0]["method"].as_str(),
        Some("turn/completed")
    );
    assert_eq!(callback_events[0]["agent_id"].as_str(), Some(agent_id));

    let command_capture = std::path::PathBuf::from(format!(
        "/tmp/foreman-command-callback-{}",
        common::random_port()
    ));
    let override_callback = client
        .post(format!("{}/agents/{agent_id}/send", harness.base_url))
        .json(&json!({
            "callback_profile": "record_cmd",
            "callback_events": ["turn/completed"],
            "callback_vars": {
                "callback_file": command_capture.to_string_lossy().to_string(),
            },
        }))
        .send()
        .await
        .expect("callback override request");
    assert_eq!(override_callback.status(), StatusCode::OK);

    let send_turn = client
        .post(format!("{}/agents/{agent_id}/send", harness.base_url))
        .json(&json!({"prompt": "second turn"}))
        .send()
        .await
        .expect("send turn request");
    assert_eq!(send_turn.status(), StatusCode::OK);

    let command_fired = wait_for_file_has_content(&command_capture, Duration::from_secs(2)).await;
    assert!(command_fired, "command callback should write payload");

    let command_contents =
        fs::read_to_string(&command_capture).expect("read command callback output");
    let command_payload: Value =
        serde_json::from_str(&command_contents).expect("command payload json");
    assert_eq!(command_payload["method"].as_str(), Some("turn/completed"));

    let steer = client
        .post(format!("{}/agents/{agent_id}/steer", harness.base_url))
        .json(&json!({"prompt": "brief and precise"}))
        .send()
        .await
        .expect("steer request");
    assert_eq!(steer.status(), StatusCode::OK);

    let interrupt = client
        .post(format!("{}/agents/{agent_id}/interrupt", harness.base_url))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("interrupt request");
    assert!(
        interrupt.status() == StatusCode::OK || interrupt.status() == StatusCode::BAD_REQUEST,
        "interrupt may be no-op after completion"
    );

    let get_agent = client
        .get(format!("{}/agents/{agent_id}", harness.base_url))
        .send()
        .await
        .expect("get agent request");
    assert_eq!(get_agent.status(), StatusCode::OK);

    let delete_agent = client
        .delete(format!("{}/agents/{agent_id}", harness.base_url))
        .send()
        .await
        .expect("delete agent request");
    assert_eq!(delete_agent.status(), StatusCode::NO_CONTENT);

    let missing_after_delete = client
        .get(format!("{}/agents/{agent_id}", harness.base_url))
        .send()
        .await
        .expect("get deleted agent request");
    assert_eq!(missing_after_delete.status(), StatusCode::NOT_FOUND);

    harness.terminate().await;
    webhook.stop().await;
}

#[tokio::test]
async fn test_agent_result_and_wait_endpoints() {
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let service_config_contents = service_config_template_simple();
    common::write_service_config(&service_config, service_config_contents.as_str())
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let create = client
        .post(format!("{}/agents", harness.base_url))
        .json(&json!({
            "prompt": "result + wait endpoint probe",
        }))
        .send()
        .await
        .expect("create agent request");
    assert_eq!(create.status(), StatusCode::CREATED);
    let created = create.json::<Value>().await.expect("create response");
    let agent_id = created["id"].as_str().expect("agent id");

    let wait = client
        .get(format!(
            "{}/agents/{agent_id}/wait?timeout_ms=5000&poll_ms=25&include_events=true",
            harness.base_url
        ))
        .send()
        .await
        .expect("wait request");
    assert_eq!(wait.status(), StatusCode::OK);
    let wait_result = wait.json::<Value>().await.expect("wait response");
    assert_eq!(wait_result["timed_out"].as_bool(), Some(false));
    let status = wait_result["status"]
        .as_str()
        .expect("agent result status in wait response");
    assert!(
        status == "idle" || status == "completed" || status == "running",
        "unexpected status from wait response: {status}"
    );
    assert_eq!(
        wait_result["completion_method"].as_str(),
        Some("turn/completed")
    );
    assert_eq!(wait_result["turn_id"].as_str(), created["turn_id"].as_str());
    assert_eq!(wait_result["agent_id"].as_str(), Some(agent_id));
    assert_eq!(wait_result["timed_out"].as_bool(), Some(false));
    assert!(wait_result["events"].is_array());
    assert!(
        !wait_result["events"]
            .as_array()
            .expect("wait events")
            .is_empty()
    );

    let events = client
        .get(format!(
            "{}/agents/{agent_id}/events?tail=2",
            harness.base_url
        ))
        .send()
        .await
        .expect("agent events request");
    assert_eq!(events.status(), StatusCode::OK);
    let events = events.json::<Value>().await.expect("agent events response");
    assert!(events.is_array());
    assert!(!events.as_array().expect("events array").is_empty());

    let result_endpoint = client
        .get(format!("{}/agents/{agent_id}/result", harness.base_url))
        .send()
        .await
        .expect("result request");
    assert_eq!(result_endpoint.status(), StatusCode::OK);
    let result_payload = result_endpoint
        .json::<Value>()
        .await
        .expect("agent result response");
    assert_eq!(result_payload["agent_id"].as_str(), Some(agent_id));
    assert!(
        result_payload["status"].as_str() == Some("idle")
            || result_payload["status"].as_str() == Some("completed")
            || result_payload["status"].as_str() == Some("running"),
        "unexpected status on agent result endpoint: {:?}",
        result_payload["status"].as_str()
    );
    assert_eq!(
        result_payload["completion_method"].as_str(),
        Some("turn/completed")
    );
    assert!(result_payload.get("event_count").is_some());

    let missing = client
        .get(format!(
            "{}/agents/00000000-0000-0000-0000-000000000000/result",
            harness.base_url
        ))
        .send()
        .await
        .expect("missing wait request");
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    harness.terminate().await;
}

#[tokio::test]
async fn test_callback_prompt_prefix_passes_rendered_event_payload_to_command() {
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");

    let output_file = std::path::PathBuf::from(format!(
        "/tmp/foreman-prefix-callback-{}",
        common::random_port()
    ));

    let service_config_contents = service_config_template_prefix_profile();
    common::write_service_config(&service_config, service_config_contents.as_str())
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;

    let client = common::unix_client(&harness.socket_path).expect("unix socket client");
    let create = client
        .post(format!("{}/agents", harness.base_url))
        .json(&json!({
            "prompt": "alpha callback prefix test",
            "callback_prompt_prefix": "OPENCLAW EVENT",
            "callback_vars": {
                "callback_file": output_file.to_string_lossy().to_string(),
            },
        }))
        .send()
        .await
        .expect("create agent request");
    assert_eq!(create.status(), StatusCode::CREATED);

    assert!(
        wait_for_file_has_content(&output_file, Duration::from_secs(2)).await,
        "callback output should be written"
    );

    let command_payload = fs::read_to_string(&output_file).expect("read callback output");
    let prefix = "OPENCLAW EVENT\n\n";
    assert!(command_payload.starts_with(prefix));
    assert!(command_payload.contains("\"method\":\"turn/completed\""));
    let json_pos = command_payload
        .find('{')
        .expect("command callback payload should contain JSON object");
    let payload_json: Value = serde_json::from_str(&command_payload[json_pos..])
        .expect("command payload json suffix should be parseable");
    assert!(
        payload_json.get("result").is_some(),
        "result should be present"
    );

    harness.terminate().await;
}

#[tokio::test]
async fn test_command_callback_strictly_requires_template_variables() {
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let unresolved_output =
        std::path::PathBuf::from("/tmp/foreman-unresolved-callback-{{callback_file}}");
    let _ = fs::remove_file(&unresolved_output);

    let service_config_contents = service_config_template_with_unresolved_token();
    common::write_service_config(&service_config, service_config_contents.as_str())
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let create = client
        .post(format!("{}/agents", harness.base_url))
        .json(&json!({
            "prompt": "alpha unresolved callback var test",
            "callback_profile": "record_cmd",
            "callback_events": ["turn/completed"],
        }))
        .send()
        .await
        .expect("create agent request");
    assert_eq!(create.status(), StatusCode::CREATED);

    let agent = create.json::<Value>().await.expect("agent response json");
    let agent_id = agent["id"].as_str().expect("agent id");

    let wait = client
        .get(format!(
            "{}/agents/{agent_id}/wait?timeout_ms=5000&poll_ms=25",
            harness.base_url
        ))
        .send()
        .await
        .expect("wait request");
    assert_eq!(wait.status(), StatusCode::OK);

    let output_missing =
        wait_for_path_absence(&unresolved_output, Duration::from_millis(1200)).await;
    assert!(
        output_missing,
        "unresolved callback token should not be rendered to a literal filename"
    );

    harness.terminate().await;
}

#[tokio::test]
async fn test_api_auth() {
    let temp = tempfile::tempdir().expect("temp dir");
    let service_config = temp.path().join("service.toml");
    let state_path = temp.path().join("foreman-state.json");

    let service_config_contents = service_config_template_with_auth("alpha-token");
    common::write_service_config(&service_config, service_config_contents.as_str())
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let unauth = client
        .get(format!("{}/agents", harness.base_url))
        .send()
        .await
        .expect("unauth request");
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    let auth_health = client
        .get(format!("{}/health", harness.base_url))
        .send()
        .await
        .expect("health request");
    assert_eq!(auth_health.status(), StatusCode::OK);

    let auth_list = client
        .get(format!("{}/agents", harness.base_url))
        .header("authorization", "alpha-token")
        .send()
        .await
        .expect("authenticated request");
    assert_eq!(auth_list.status(), StatusCode::OK);

    harness.terminate().await;
}

#[tokio::test]
async fn test_service_status_endpoint() {
    let temp = tempfile::tempdir().expect("temp dir");
    let service_config = temp.path().join("service.toml");
    let state_path = temp.path().join("foreman-state.json");

    let service_config_contents = service_config_template_with_auth("alpha-token");
    common::write_service_config(&service_config, service_config_contents.as_str())
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness = common::start_foreman_with_auth(
        &socket_path,
        &service_config,
        &state_path,
        &fake_codex,
        Some("alpha-token"),
    )
    .await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let status = client
        .get(format!("{}/status", harness.base_url))
        .header("authorization", "alpha-token")
        .send()
        .await
        .expect("status request");
    assert_eq!(status.status(), StatusCode::OK);
    let payload = status.json::<Value>().await.expect("status payload");
    assert_eq!(payload["status"].as_str(), Some("ready"));
    assert!(payload["foreman_pid"].as_u64().is_some());
    assert!(payload["app_server_pid"].as_u64().is_some());
    assert!(payload["version"].as_str().is_some());
    assert!(payload["agent_count"].as_u64().is_some());

    harness.terminate().await;
}

#[tokio::test]
async fn test_api_auth_header_scheme_and_custom_header() {
    let temp = tempfile::tempdir().expect("temp dir");
    let service_config = temp.path().join("service.toml");
    let state_path = temp.path().join("foreman-state.json");
    common::write_service_config(
        &service_config,
        r#"[security.auth]
enabled = true
token = "alpha-token"
header_name = "x-foreman-token"
header_scheme = "Token"
skip_paths = ["/health", "/status"]

[callbacks]
"#,
    )
    .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let status_skip = client
        .get(format!("{}/status", harness.base_url))
        .send()
        .await
        .expect("status skip path request");
    assert_eq!(status_skip.status(), StatusCode::OK);

    let no_auth = client
        .get(format!("{}/agents", harness.base_url))
        .send()
        .await
        .expect("no auth request");
    assert_eq!(no_auth.status(), StatusCode::UNAUTHORIZED);

    let wrong_scheme = client
        .get(format!("{}/agents", harness.base_url))
        .header("x-foreman-token", "Bearer alpha-token")
        .send()
        .await
        .expect("wrong scheme request");
    assert_eq!(wrong_scheme.status(), StatusCode::UNAUTHORIZED);

    let correct = client
        .get(format!("{}/agents", harness.base_url))
        .header("x-foreman-token", "Token alpha-token")
        .send()
        .await
        .expect("correct auth request");
    assert_eq!(correct.status(), StatusCode::OK);

    harness.terminate().await;
}

#[tokio::test]
async fn test_api_auth_token_env_with_custom_header() {
    let temp = tempfile::tempdir().expect("temp dir");
    let service_config = temp.path().join("service.toml");
    let state_path = temp.path().join("foreman-state.json");
    common::write_service_config(
        &service_config,
        r#"[security.auth]
enabled = true
token_env = "CODEX_FOREMAN_API_TOKEN"
header_name = "x-auth-token"
skip_paths = ["/health", "/status"]

[callbacks]
"#,
    )
    .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness = common::start_foreman_with_auth(
        &socket_path,
        &service_config,
        &state_path,
        &fake_codex,
        Some("env-token"),
    )
    .await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let no_auth = client
        .get(format!("{}/agents", harness.base_url))
        .send()
        .await
        .expect("no auth request");
    assert_eq!(no_auth.status(), StatusCode::UNAUTHORIZED);

    let with_env_token = client
        .get(format!("{}/agents", harness.base_url))
        .header("x-auth-token", "env-token")
        .send()
        .await
        .expect("env token auth request");
    assert_eq!(with_env_token.status(), StatusCode::OK);

    harness.terminate().await;
}

#[tokio::test]
async fn test_command_callback_timeout_isolation() {
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let output_file = std::path::PathBuf::from(format!(
        "/tmp/foreman-timeout-callback-{}",
        common::random_port()
    ));
    let service_config_contents = service_config_template_timeout_profile();
    common::write_service_config(&service_config, &service_config_contents)
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let create = client
        .post(format!("{}/agents", harness.base_url))
        .json(&json!({
            "prompt": "alpha startup with slow callback",
            "callback_profile": "slow_cmd",
            "callback_events": ["turn/completed"],
            "callback_vars": {
                "callback_file": output_file.to_string_lossy().to_string(),
            }
        }))
        .send()
        .await
        .expect("create agent request");
    assert_eq!(create.status(), StatusCode::CREATED);

    let write_happened = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if fs::read_to_string(&output_file).is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .is_ok();
    assert!(
        !write_happened,
        "callback output file should remain unwritten while timeout branch is active"
    );

    harness.terminate().await;
}

#[tokio::test]
async fn test_recovery_after_process_crash() {
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let service_config_contents = service_config_template_simple();
    common::write_service_config(&service_config, service_config_contents.as_str())
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let agent_create = client
        .post(format!("{}/agents", harness.base_url))
        .json(&json!({ "prompt": "crash recovery probe" }))
        .send()
        .await
        .expect("create agent");
    assert_eq!(agent_create.status(), StatusCode::CREATED);
    let agent = agent_create.json::<Value>().await.expect("agent response");
    let agent_id = agent["id"].as_str().expect("agent id");

    let before_restart = client
        .get(format!("{}/agents/{agent_id}/result", harness.base_url))
        .send()
        .await
        .expect("result before restart");
    assert_eq!(before_restart.status(), StatusCode::OK);
    let before_payload = before_restart
        .json::<Value>()
        .await
        .expect("before restart result");
    assert_eq!(before_payload["agent_id"].as_str(), Some(agent_id));

    // Give asynchronous persistence a chance before crashing.
    tokio::time::sleep(Duration::from_millis(250)).await;
    harness.terminate().await;

    let restarted =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let recovered_agents = client
        .get(format!("{}/agents", restarted.base_url))
        .send()
        .await
        .expect("list agents");
    assert_eq!(recovered_agents.status(), StatusCode::OK);
    let agents = recovered_agents.json::<Value>().await.expect("agents json");
    let ids: Vec<String> = agents
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry["id"].as_str().map(str::to_string))
                .collect()
        })
        .expect("agents array");
    assert!(
        ids.contains(&agent_id.to_string()),
        "agent should recover after restart"
    );

    let after_restart = client
        .get(format!("{}/agents/{agent_id}/result", restarted.base_url))
        .send()
        .await
        .expect("result after restart");
    assert_eq!(after_restart.status(), StatusCode::OK);
    let after_payload = after_restart
        .json::<Value>()
        .await
        .expect("after restart result");
    assert_eq!(after_payload["agent_id"].as_str(), Some(agent_id));
    assert_eq!(
        after_payload["turn_id"].as_str(),
        before_payload["turn_id"].as_str()
    );

    restarted.terminate().await;
}

#[tokio::test]
async fn test_project_lifecycle_and_recovery() {
    let (_temp_project, project_path) =
        common::temporary_project("project-valid").expect("fixture copy");
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let webhook = common::start_webhook_capture().await;
    let webhook_url = format!("{}/callback", webhook.url);
    let service_config_contents = service_config_template(&webhook_url);
    common::write_service_config(&service_config, service_config_contents.as_str())
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");

    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let project_response = client
        .post(format!("{}/projects", harness.base_url))
        .json(&json!({
            "path": project_path.to_string_lossy().to_string(),
            "start_prompt": "start this project",
            "callback_overrides": {
                "callback_profile": "record_webhook",
            }
        }))
        .send()
        .await
        .expect("create project");
    assert_eq!(project_response.status(), StatusCode::CREATED);
    let project_data = project_response
        .json::<Value>()
        .await
        .expect("project response");
    let project_id = project_data["project_id"].as_str().expect("project_id");
    let foreman_agent_id = project_data["foreman_agent_id"]
        .as_str()
        .expect("foreman id");

    let list_projects = client
        .get(format!("{}/projects", harness.base_url))
        .send()
        .await
        .expect("list projects");
    let projects = list_projects.json::<Value>().await.expect("projects json");
    assert_eq!(projects[0]["id"], project_id);

    let worker_response = client
        .post(format!(
            "{}/projects/{}/workers",
            harness.base_url, project_id
        ))
        .json(&json!({
            "prompt": "do a focused check",
            "model": "gpt-test",
            "callback_overrides": {
                "callback_profile": "record_webhook",
                "callback_events": ["turn/completed"]
            }
        }))
        .send()
        .await
        .expect("spawn worker");
    assert_eq!(worker_response.status(), StatusCode::CREATED);

    let compact_response = client
        .post(format!(
            "{}/projects/{}/compact",
            harness.base_url, project_id
        ))
        .json(&json!({
            "reason": "test threshold compact",
            "prompt": "summarize key outcomes"
        }))
        .send()
        .await
        .expect("compact project");
    assert_eq!(compact_response.status(), StatusCode::OK);

    let project_get = client
        .get(format!("{}/projects/{project_id}", harness.base_url))
        .send()
        .await
        .expect("get project");
    assert_eq!(project_get.status(), StatusCode::OK);
    let project_state = project_get.json::<Value>().await.expect("project state");
    assert_eq!(project_state["id"], project_id);
    assert_eq!(
        project_state["foreman_agent_id"].as_str(),
        Some(foreman_agent_id)
    );

    assert!(webhook.wait_for_events(1, Duration::from_secs(3)).await);

    harness.terminate().await;
    let restarted =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let recovered_projects = client
        .get(format!("{}/projects", restarted.base_url))
        .send()
        .await
        .expect("list recovered projects");
    assert_eq!(recovered_projects.status(), StatusCode::OK);
    let recovered_projects = recovered_projects
        .json::<Value>()
        .await
        .expect("recovered projects json");
    let recovered_ids: Vec<String> = recovered_projects
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry["id"].as_str().map(str::to_string))
                .collect()
        })
        .expect("projects array");
    assert!(recovered_ids.contains(&project_id.to_string()));

    let recovered_agents = client
        .get(format!("{}/agents", restarted.base_url))
        .send()
        .await
        .expect("recovered agents");
    assert_eq!(recovered_agents.status(), StatusCode::OK);
    let agents = recovered_agents.json::<Value>().await.expect("agents json");
    let recovered_agent_ids: Vec<String> = agents
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry["id"].as_str().map(str::to_string))
                .collect()
        })
        .expect("agents array");
    assert!(recovered_agent_ids.contains(&foreman_agent_id.to_string()));

    let close_project = client
        .delete(format!("{}/projects/{project_id}", restarted.base_url))
        .send()
        .await
        .expect("close project");
    assert_eq!(close_project.status(), StatusCode::NO_CONTENT);

    webhook.stop().await;
    restarted.terminate().await;
}

#[tokio::test]
async fn test_project_worker_callback_overrides_with_nested_object() {
    let (_temp_project, project_path) =
        common::temporary_project("project-valid").expect("fixture copy");
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let webhook = common::start_webhook_capture().await;
    let webhook_url = format!("{}/callback", webhook.url);
    let service_config_contents = service_config_template(&webhook_url);
    common::write_service_config(&service_config, service_config_contents.as_str())
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");

    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let project_response = client
        .post(format!("{}/projects", harness.base_url))
        .json(&json!({
            "path": project_path.to_string_lossy().to_string(),
            "start_prompt": "start this project"
        }))
        .send()
        .await
        .expect("create project");
    assert_eq!(project_response.status(), StatusCode::CREATED);
    let project_data = project_response
        .json::<Value>()
        .await
        .expect("project response");
    let project_id = project_data["project_id"].as_str().expect("project_id");

    assert!(
        webhook.wait_for_events(1, Duration::from_secs(3)).await,
        "project startup callback should be visible"
    );
    let baseline_events = webhook.events().await.len();

    let worker_response = client
        .post(format!(
            "{}/projects/{}/workers",
            harness.base_url, project_id
        ))
        .json(&json!({
            "prompt": "validate callback overrides",
            "callback_overrides": {
                "callback_profile": "record_webhook",
                "callback_events": ["turn/completed"]
            }
        }))
        .send()
        .await
        .expect("spawn worker");
    assert_eq!(worker_response.status(), StatusCode::CREATED);

    assert!(
        webhook
            .wait_for_events(baseline_events + 1, Duration::from_secs(3))
            .await,
        "callback override should route worker completion through webhook profile"
    );

    let events = webhook.events().await;
    assert!(events.len() > baseline_events);
    let has_worker_turn_completed = events
        .iter()
        .any(|event| event["method"].as_str() == Some("turn/completed"));
    assert!(
        has_worker_turn_completed,
        "worker override should emit callback event"
    );

    harness.terminate().await;
    webhook.stop().await;
}

#[tokio::test]
async fn test_project_job_creation_and_result() {
    let (_temp_project, project_path) =
        common::temporary_project("project-valid").expect("fixture copy");
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let webhook = common::start_webhook_capture().await;
    let webhook_url = format!("{}/callback", webhook.url);
    let service_config_contents = service_config_template(&webhook_url);
    common::write_service_config(&service_config, service_config_contents.as_str())
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");

    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let project_response = client
        .post(format!("{}/projects", harness.base_url))
        .json(&json!({
            "path": project_path.to_string_lossy().to_string(),
            "start_prompt": "start this project",
        }))
        .send()
        .await
        .expect("create project");
    assert_eq!(project_response.status(), StatusCode::CREATED);
    let project_data = project_response
        .json::<Value>()
        .await
        .expect("project response");
    let project_id = project_data["project_id"].as_str().expect("project_id");

    let jobs = vec![
        json!({
            "prompt": "analyze package layout",
            "labels": {"priority": "high", "team": "alpha"},
            "callback_overrides": {
                "callback_profile": "record_webhook",
                "callback_events": ["turn/completed"],
            },
        }),
        json!({
            "prompt": "summarize open items",
            "labels": {"priority": "medium", "team": "alpha"},
            "callback_overrides": {
                "callback_profile": "record_webhook",
                "callback_events": ["turn/completed"],
            },
        }),
    ];

    let create_jobs = client
        .post(format!("{}/projects/{}/jobs", harness.base_url, project_id))
        .json(&json!({"workers": jobs}))
        .send()
        .await
        .expect("spawn project jobs");
    assert_eq!(create_jobs.status(), StatusCode::CREATED);
    let job_data = create_jobs.json::<Value>().await.expect("job response");
    let job_id = job_data["job_id"].as_str().expect("job_id");
    let worker_count = job_data["worker_count"].as_u64().expect("worker_count");
    assert_eq!(worker_count, 2);

    let labels = job_data["labels"].as_object().expect("labels object");
    let team_values = labels["team"].as_array().expect("team labels");
    assert_eq!(team_values.len(), 2);

    let list = client
        .get(format!("{}/jobs", harness.base_url))
        .send()
        .await
        .expect("list jobs");
    assert_eq!(list.status(), StatusCode::OK);
    let list = list.json::<Value>().await.expect("jobs list");
    assert!(!list.as_array().expect("jobs array").is_empty());

    let lookup = client
        .get(format!("{}/jobs/{job_id}", harness.base_url))
        .send()
        .await
        .expect("get job");
    assert_eq!(lookup.status(), StatusCode::OK);
    let lookup_payload = lookup.json::<Value>().await.expect("job lookup response");
    assert_eq!(lookup_payload["id"].as_str(), Some(job_id));
    let initial_status = lookup_payload["status"].as_str().unwrap_or_default();
    assert!(initial_status == "running" || initial_status == "completed");

    let result = client
        .get(format!("{}/jobs/{}/result", harness.base_url, job_id))
        .send()
        .await
        .expect("job result");
    assert_eq!(result.status(), StatusCode::OK);
    let result_payload = result.json::<Value>().await.expect("job result payload");
    assert_eq!(result_payload["id"].as_str(), Some(job_id));
    assert_eq!(result_payload["project_id"].as_str(), Some(project_id));
    assert_eq!(result_payload["total_workers"].as_u64(), Some(worker_count));
    assert_eq!(result_payload["worker_count"].as_u64(), Some(worker_count));
    assert_eq!(
        result_payload["workers"]
            .as_array()
            .expect("workers array")
            .len(),
        2
    );

    let closed = client
        .delete(format!("{}/projects/{project_id}", harness.base_url))
        .send()
        .await
        .expect("close project");
    assert_eq!(closed.status(), StatusCode::NO_CONTENT);

    harness.terminate().await;
    webhook.stop().await;
}

#[tokio::test]
async fn test_project_foreman_send_and_steer_endpoints() {
    let (_temp_project, project_path) =
        common::temporary_project("project-generic-worktree").expect("fixture copy");
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    common::write_service_config(&service_config, service_config_template_simple().as_str())
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let project_response = client
        .post(format!("{}/projects", harness.base_url))
        .json(&json!({
            "path": project_path.to_string_lossy().to_string(),
            "start_prompt": "start this project",
        }))
        .send()
        .await
        .expect("create project");
    assert_eq!(project_response.status(), StatusCode::CREATED);
    let project_data = project_response
        .json::<Value>()
        .await
        .expect("project response");
    let project_id = project_data["project_id"].as_str().expect("project_id");

    let send_response = client
        .post(format!(
            "{}/projects/{}/foreman/send",
            harness.base_url, project_id
        ))
        .json(&json!({
            "prompt": "send a follow-up foreman turn",
        }))
        .send()
        .await
        .expect("send project foreman turn");
    assert_eq!(send_response.status(), StatusCode::OK);

    let steer_response = client
        .post(format!(
            "{}/projects/{}/foreman/steer",
            harness.base_url, project_id
        ))
        .json(&json!({
            "prompt": "keep the response concise",
        }))
        .send()
        .await
        .expect("steer project foreman");
    assert!(
        steer_response.status() == StatusCode::OK
            || steer_response.status() == StatusCode::BAD_REQUEST,
        "steer endpoint should be reachable even if active turn ended quickly"
    );

    harness.terminate().await;
}

#[tokio::test]
async fn test_jobs_list_endpoint_includes_created_job() {
    let (_temp_project, project_path) =
        common::temporary_project("project-generic-worktree").expect("fixture copy");
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    common::write_service_config(&service_config, service_config_template_simple().as_str())
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let project_response = client
        .post(format!("{}/projects", harness.base_url))
        .json(&json!({
            "path": project_path.to_string_lossy().to_string(),
            "start_prompt": "start this project",
        }))
        .send()
        .await
        .expect("create project");
    assert_eq!(project_response.status(), StatusCode::CREATED);
    let project_data = project_response
        .json::<Value>()
        .await
        .expect("project response");
    let project_id = project_data["project_id"].as_str().expect("project_id");

    let create_jobs = client
        .post(format!("{}/projects/{}/jobs", harness.base_url, project_id))
        .json(&json!({
            "workers": [
                {"prompt": "worker one"},
                {"prompt": "worker two"}
            ]
        }))
        .send()
        .await
        .expect("create project jobs");
    assert_eq!(create_jobs.status(), StatusCode::CREATED);
    let create_jobs = create_jobs
        .json::<Value>()
        .await
        .expect("create jobs response");
    let job_id = create_jobs["job_id"].as_str().expect("job id");

    let list_jobs = client
        .get(format!("{}/jobs", harness.base_url))
        .send()
        .await
        .expect("list jobs request");
    assert_eq!(list_jobs.status(), StatusCode::OK);
    let jobs = list_jobs.json::<Value>().await.expect("jobs response");
    let contains_job = jobs
        .as_array()
        .expect("jobs should be an array")
        .iter()
        .any(|entry| entry["id"].as_str() == Some(job_id));
    assert!(contains_job, "newly created job should appear in /jobs");

    harness.terminate().await;
}

fn write_fake_codex_deliverable_server_script(path: &Path) {
    let state_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("/tmp"))
        .join("fake_codex_state");
    let state_dir = state_dir.to_string_lossy().to_string();
    let script = r##"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi

STATE_DIR="__STATE_DIR__"
mkdir -p "$STATE_DIR"
rm -f "$STATE_DIR"/*.cwd 2>/dev/null || true

thread_counter=0
turn_counter=0

extract_id() {
  echo "$1" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p'
}

extract_method() {
  echo "$1" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p'
}

extract_field() {
  echo "$1" | sed -n "s/.*\"$2\":\"\\([^\\\"]*\\)\".*/\\1/p"
}

send_line() {
  echo "$1"
}

send_response() {
  send_line "{\"jsonrpc\":\"2.0\",\"id\":\"$1\",\"result\":$2}"
}

send_notification() {
  send_line "{\"jsonrpc\":\"2.0\",\"method\":\"$1\",\"params\":$2}"
}

send_error() {
  send_line "{\"jsonrpc\":\"2.0\",\"id\":\"$1\",\"error\":{\"code\":$2,\"message\":\"$3\"}}"
}

prompt_from_line() {
  line="$1"
  prompt="$(printf '%s' "$line" | awk -F '\"text\":\"' '{print $2}' | awk -F '\"' '{print $1}')"
  printf '%b' "$prompt"
}

extract_marker() {
  echo "$1" | tr '\n' ' ' | sed -n "s/.*$2=\\([^ ]*\\).*/\\1/p"
}

extract_worktree_path() {
  echo "$1" | sed -n 's/.*WORKTREE_PATH=\([^ ]*\).*/\1/p'
}

mock_write_deliverable() {
  thread_id="$1"
  turn_id="$2"
  category="$3"
  worktree_path="$4"

  case "$worktree_path" in
    ""|*"<path>"*)
      worktree_path="/tmp/fake-worktree-${thread_id}-${turn_id}-${category}"
      ;;
  esac

  mkdir -p "$worktree_path/.audit-generated"
  printf '%s\n' "# Worker Deliverable" > "$worktree_path/.audit-generated/${category}-worker-deliverable.md"
  printf '%s\n' "Category: $category" >> "$worktree_path/.audit-generated/${category}-worker-deliverable.md"
  printf '%s\n' "Thread: $thread_id" >> "$worktree_path/.audit-generated/${category}-worker-deliverable.md"
  printf '%s\n' "Turn: $turn_id" >> "$worktree_path/.audit-generated/${category}-worker-deliverable.md"
  printf '%s\n' "Worktree: $worktree_path" >> "$worktree_path/.audit-generated/${category}-worker-deliverable.md"
  send_notification "turn/started" "{\"threadId\":\"$thread_id\",\"turnId\":\"$turn_id\",\"category\":\"$category\",\"worktreePath\":\"$worktree_path\"}"
  send_notification "codex/event/exec_command_begin" "{\"threadId\":\"$thread_id\",\"turnId\":\"$turn_id\",\"command\":\"write worker deliverable\"}"
  send_notification "codex/event/exec_command_end" "{\"threadId\":\"$thread_id\",\"turnId\":\"$turn_id\",\"status\":\"success\"}"
  send_notification "turn/completed" "{\"threadId\":\"$thread_id\",\"turnId\":\"$turn_id\",\"category\":\"$category\"}"
}

while read -r line; do
  [ -z "$line" ] && continue

  method="$(extract_method "$line")"
  id="$(extract_id "$line")"
  [ -z "$id" ] && continue

  case "$method" in
    initialize)
      send_response "$id" "{}"
      ;;
    "thread/start")
      thread_id="thread-$thread_counter"
      thread_counter=$((thread_counter + 1))
      cwd="$(extract_field "$line" "cwd")"
      [ -z "$cwd" ] && cwd="/tmp"
      printf '%s' "$cwd" > "$STATE_DIR/${thread_id}.cwd"
      send_response "$id" "{\"threadId\":\"$thread_id\"}"
      ;;
    "turn/start")
      thread_id="$(extract_field "$line" "threadId")"
      turn_id="turn-$turn_counter"
      turn_counter=$((turn_counter + 1))

      prompt="$(prompt_from_line "$line")"
      category="$(extract_marker "$prompt" "CATEGORY")"
      [ -z "$category" ] && category="worker"
      worktree_path="$(extract_worktree_path "$prompt")"
      fail_status="$(extract_marker "$prompt" "FAIL_STATUS")"

      send_response "$id" "{\"turnId\":\"$turn_id\"}"
      send_notification "thread/status/changed" "{\"threadId\":\"$thread_id\",\"status\":\"running\"}"
      send_notification "turn/started" "{\"threadId\":\"$thread_id\",\"turn\":{\"id\":\"$turn_id\",\"status\":\"running\",\"items\":[]}}"

      if [ -n "$fail_status" ]; then
        send_notification "thread/status/changed" "{\"threadId\":\"$thread_id\",\"turnId\":\"$turn_id\",\"status\":\"$fail_status\",\"error\":\"simulated thread failure from test marker\"}"
        continue
      fi

      mock_write_deliverable "$thread_id" "$turn_id" "$category" "$worktree_path"
      send_notification "turn/completed" "{\"threadId\":\"$thread_id\",\"turn\":{\"id\":\"$turn_id\",\"status\":\"completed\",\"items\":[{\"type\":\"assistantMessage\",\"role\":\"assistant\",\"text\":\"WROTE ${worktree_path}/.audit-generated/${category}-worker-deliverable.md\"}]}}"
      ;;
    "turn/steer")
      thread_id="$(extract_field "$line" "threadId")"
      turn_id="turn-$turn_counter"
      turn_counter=$((turn_counter + 1))
      expected_turn_id="$(extract_field "$line" "expectedTurnId")"
      send_response "$id" "{\"turnId\":\"$turn_id\",\"expectedTurnId\":\"$expected_turn_id\"}"
      send_notification "turn/started" "{\"threadId\":\"$thread_id\",\"turn\":{\"id\":\"$turn_id\",\"status\":\"running\",\"items\":[]}}"
      send_notification "turn/completed" "{\"threadId\":\"$thread_id\",\"turn\":{\"id\":\"$turn_id\",\"status\":\"completed\",\"items\":[{\"type\":\"text\",\"text\":\"steered\"}]}}"
      ;;
    "turn/interrupt")
      thread_id="$(extract_field "$line" "threadId")"
      turn_id="$(extract_field "$line" "turnId")"
      send_response "$id" "{}"
      send_notification "turn/aborted" "{\"threadId\":\"$thread_id\",\"turnId\":\"$turn_id\"}"
      ;;
    *)
      send_error "$id" -32601 "unsupported method $method"
      ;;
  esac
done
"##
    .replace("__STATE_DIR__", &state_dir);
    std::fs::write(path, script).expect("write fake app-server script");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)
            .expect("fake app-server metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).expect("make fake app-server executable");
    }
}

#[tokio::test]
async fn test_project_job_dispatches_workers_with_fake_app_server() {
    let (_temp_project, project_path) =
        common::temporary_project("project-generic-worktree").expect("fixture copy");
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let fake_codex = temp.path().join("fake_codex_worktree");

    write_fake_codex_deliverable_server_script(&fake_codex);
    let service_config_contents = "[callbacks]\n";
    common::write_service_config(&service_config, service_config_contents)
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let harness = common::start_foreman(
        &socket_path,
        &service_config,
        &state_path,
        fake_codex.to_str().expect("fake app-server path"),
    )
    .await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let project_response = client
        .post(format!("{}/projects", harness.base_url))
        .json(&json!({
            "path": project_path.to_string_lossy().to_string(),
            "start_prompt": "run generic project worker plan"
        }))
        .send()
        .await
        .expect("create project");
    assert_eq!(project_response.status(), StatusCode::CREATED);
    let project_data = project_response
        .json::<Value>()
        .await
        .expect("project response");
    let project_id = project_data["project_id"].as_str().expect("project id");

    let work_items = [
        ("security", "identify and fix auth or permission issues"),
        ("robustness", "identify and fix error-path gaps"),
        ("code-quality", "identify and fix one quality regression"),
    ];
    let worktree_base = temp.path().join("mock-trees");
    fs::create_dir_all(&worktree_base).expect("create mock worktree base");

    let mut workers = Vec::new();
    for (category, task) in work_items.iter() {
        let worktree_path = worktree_base.join(category);
        let prompt = format!(
            "Run mock work for category {category}: {task}\n\
            CATEGORY={category}\
            \n\
            WORKTREE_PATH={}",
            worktree_path.display()
        );
        workers.push(json!({
            "prompt": prompt,
            "labels": {
                "category": category,
                "worktree_path": worktree_path.to_string_lossy().to_string(),
            }
        }));
    }

    let create_jobs = client
        .post(format!("{}/projects/{}/jobs", harness.base_url, project_id))
        .json(&json!({ "workers": workers }))
        .send()
        .await
        .expect("spawn project jobs");
    assert_eq!(create_jobs.status(), StatusCode::CREATED);
    let job_data = create_jobs.json::<Value>().await.expect("job response");
    let job_id = job_data["job_id"].as_str().expect("job id");

    let wait = client
        .get(format!(
            "{}/jobs/{}/wait?timeout_ms=10000&poll_ms=50&include_workers=true",
            harness.base_url, job_id
        ))
        .send()
        .await
        .expect("wait for job");
    assert_eq!(wait.status(), StatusCode::OK);

    let result = wait.json::<Value>().await.expect("job wait payload");
    assert_eq!(result["timed_out"].as_bool(), Some(false));
    let status = result["status"].as_str().expect("job status");
    assert!(matches!(status, "completed" | "partial" | "failed"));
    let workers_result = result["workers"].as_array().expect("job workers array");
    assert_eq!(workers_result.len(), work_items.len());
    assert_eq!(
        result["total_workers"].as_u64().expect("total workers"),
        work_items.len() as u64
    );

    for (idx, worker) in workers_result.iter().enumerate() {
        let mut worker_final_text = worker["final_text"].as_str().map(|text| text.to_string());
        let worker_id = worker["agent_id"].as_str().expect("worker has agent_id");
        let mut worker_status = worker["status"].as_str().unwrap_or("unknown").to_string();
        let mut completion_method = worker["completion_method"]
            .as_str()
            .map(std::string::ToString::to_string);

        if worker_final_text.is_none() {
            let worker_wait = client
                .get(format!(
                    "{}/agents/{}/wait?timeout_ms=30000&poll_ms=25&include_events=true",
                    harness.base_url, worker_id
                ))
                .send()
                .await
                .expect("wait for worker completion");
            assert_eq!(worker_wait.status(), StatusCode::OK);

            let worker_wait_response = worker_wait
                .json::<Value>()
                .await
                .expect("worker wait payload");
            assert_eq!(worker_wait_response["timed_out"].as_bool(), Some(false));
            worker_status = worker_wait_response["status"]
                .as_str()
                .unwrap_or(worker_status.as_str())
                .to_string();
            completion_method = worker_wait_response["completion_method"]
                .as_str()
                .map(std::string::ToString::to_string);
            if completion_method.as_deref() == Some("turn/completed") {
                worker_final_text = worker_wait_response["final_text"]
                    .as_str()
                    .map(|text| text.to_string());
            }
        }

        let labels = &worker["labels"];
        let category = label_value_to_string(labels, "category")
            .unwrap_or_else(|| work_items[idx].0.to_string());
        let default_worktree_path = worktree_base.join(&category);
        let default_worktree = default_worktree_path.to_string_lossy().to_string();
        let worktree_path =
            label_value_to_string(labels, "worktree_path").unwrap_or(default_worktree);

        let worker_events = client
            .get(format!("{}/agents/{}/events", harness.base_url, worker_id))
            .send()
            .await
            .expect("worker events")
            .json::<Value>()
            .await
            .expect("worker events payload");
        let events = worker_events.as_array().expect("worker events as array");

        let deliverable = worker_final_text
            .as_deref()
            .and_then(extract_written_path)
            .or_else(|| {
                Some(
                    PathBuf::from(worktree_path)
                        .join(".audit-generated")
                        .join(format!("{category}-worker-deliverable.md"))
                        .to_string_lossy()
                        .to_string(),
                )
            })
            .map(PathBuf::from)
            .expect("deliverable path");

        let has_command_event = events
            .iter()
            .any(|event| is_command_like_event(event["method"].as_str().unwrap_or("")));
        assert!(
            has_command_event,
            "worker {worker_id} should emit command-like event stream"
        );

        if let Some(final_text) = worker_final_text.as_deref() {
            assert!(final_text.starts_with("WROTE"));
            assert!(final_text.contains(".audit-generated"));
        } else {
            assert_eq!(worker_status, "completed");
            assert_eq!(completion_method.as_deref(), Some("turn/completed"));
        }

        assert!(
            deliverable.exists(),
            "missing worker deliverable at {deliverable:?}"
        );
        let contents = fs::read_to_string(&deliverable).expect("read worker deliverable");
        assert!(
            !contents.trim().is_empty(),
            "worker deliverable should be non-empty: {deliverable:?}"
        );
    }

    let project_delete = client
        .delete(format!("{}/projects/{project_id}", harness.base_url))
        .send()
        .await
        .expect("close project");
    assert_eq!(project_delete.status(), StatusCode::NO_CONTENT);

    harness.terminate().await;
}

#[tokio::test]
async fn test_project_job_failures_from_thread_status_status_map_to_failed_results() {
    let (_temp_project, project_path) =
        common::temporary_project("project-generic-worktree").expect("fixture copy");
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let fake_codex = temp.path().join("fake_codex_worktree_failure");

    write_fake_codex_deliverable_server_script(&fake_codex);
    let service_config_contents = "[callbacks]\n";
    common::write_service_config(&service_config, service_config_contents)
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let harness = common::start_foreman(
        &socket_path,
        &service_config,
        &state_path,
        fake_codex.to_str().expect("fake app-server path"),
    )
    .await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let project_response = client
        .post(format!("{}/projects", harness.base_url))
        .json(&json!({
            "path": project_path.to_string_lossy().to_string(),
            "start_prompt": "run mixed success/failure project workers"
        }))
        .send()
        .await
        .expect("create project");
    assert_eq!(project_response.status(), StatusCode::CREATED);
    let project_data = project_response
        .json::<Value>()
        .await
        .expect("project response");
    let project_id = project_data["project_id"].as_str().expect("project id");

    let worktree_base = temp.path().join("mock-trees-failure");
    fs::create_dir_all(&worktree_base).expect("create mock worktree base");
    let success_worktree = worktree_base.join("security");
    let fail_worktree = worktree_base.join("robustness");

    let workers = vec![
        json!({
            "prompt": format!(
                "Run mock work for category security: create final report\n\
                CATEGORY=security\
                \n\
                WORKTREE_PATH={}",
                success_worktree.display()
            ),
            "labels": {
                "category": "security",
                "worktree_path": success_worktree.to_string_lossy().to_string(),
            }
        }),
        json!({
            "prompt": format!(
                "Run mock work for category robustness: simulate terminal failure\n\
                CATEGORY=robustness\
                \n\
                WORKTREE_PATH={}\n\
                FAIL_STATUS=error\
                ",
                fail_worktree.display()
            ),
            "labels": {
                "category": "robustness",
                "worktree_path": fail_worktree.to_string_lossy().to_string(),
            }
        }),
    ];

    let create_jobs = client
        .post(format!("{}/projects/{}/jobs", harness.base_url, project_id))
        .json(&json!({ "workers": workers }))
        .send()
        .await
        .expect("spawn project jobs");
    assert_eq!(create_jobs.status(), StatusCode::CREATED);
    let job_data = create_jobs.json::<Value>().await.expect("job response");
    let job_id = job_data["job_id"].as_str().expect("job id");

    let wait = client
        .get(format!(
            "{}/jobs/{}/wait?timeout_ms=10000&poll_ms=50&include_workers=true",
            harness.base_url, job_id
        ))
        .send()
        .await
        .expect("wait for job");
    assert_eq!(wait.status(), StatusCode::OK);
    let wait_payload = wait.json::<Value>().await.expect("job wait payload");
    assert_eq!(wait_payload["timed_out"].as_bool(), Some(false));
    let status = wait_payload["status"].as_str().expect("job status");
    assert!(matches!(status, "completed" | "partial" | "failed"));

    let result = client
        .get(format!("{}/jobs/{}/result", harness.base_url, job_id))
        .send()
        .await
        .expect("job result");
    assert_eq!(result.status(), StatusCode::OK);
    let job_result = result.json::<Value>().await.expect("job result payload");
    assert_eq!(job_result["total_workers"].as_u64(), Some(2));
    assert_eq!(job_result["worker_count"].as_u64(), Some(2));
    assert_eq!(
        job_result["failed_workers"].as_u64(),
        Some(1),
        "job_result={job_result}"
    );
    assert_eq!(
        job_result["workers"]
            .as_array()
            .expect("workers array")
            .len(),
        2
    );

    let workers = job_result["workers"].as_array().expect("workers array");
    assert_eq!(workers[0]["status"].as_str(), Some("completed"));
    assert_eq!(workers[1]["status"].as_str(), Some("failed"));
    assert_eq!(
        workers[1]["completion_method"].as_str(),
        Some("turn/aborted")
    );
    assert!(workers[0]["final_text"].as_str().is_some());
    assert!(workers[1]["final_text"].is_null() || workers[1]["final_text"].as_str().is_none());

    let failed_worker_id = workers[1]["agent_id"].as_str().expect("failed worker id");
    let events = client
        .get(format!(
            "{}/agents/{}/events",
            harness.base_url, failed_worker_id
        ))
        .send()
        .await
        .expect("worker events");
    assert_eq!(events.status(), StatusCode::OK);
    let events = events.json::<Value>().await.expect("worker events payload");
    let events = events.as_array().expect("worker events as array");
    assert!(events.iter().any(|event| {
        event["method"].as_str() == Some("thread/status/changed")
            && event["params"]["status"]
                .as_str()
                .is_some_and(|status| status == "error")
    }));

    let success_deliverable =
        success_worktree.join(".audit-generated/security-worker-deliverable.md");
    assert!(
        success_deliverable.exists(),
        "successful worker should create deliverable"
    );
    let failure_deliverable =
        fail_worktree.join(".audit-generated/robustness-worker-deliverable.md");
    assert!(
        !failure_deliverable.exists(),
        "failed worker should not create deliverable"
    );

    let project_delete = client
        .delete(format!("{}/projects/{project_id}", harness.base_url))
        .send()
        .await
        .expect("close project");
    assert_eq!(project_delete.status(), StatusCode::NO_CONTENT);

    harness.terminate().await;
}

#[tokio::test]
async fn test_worker_monitoring_restarts_stalled_worker() {
    let (_temp_project, project_path) =
        common::temporary_project("project-valid").expect("fixture copy");
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let service_config_contents = service_config_template_worker_monitoring(6_000, 1, 300);
    common::write_service_config(&service_config, service_config_contents.as_str())
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let project_response = client
        .post(format!("{}/projects", harness.base_url))
        .json(&json!({
            "path": project_path.to_string_lossy().to_string(),
            "start_prompt": "monitoring audit baseline",
        }))
        .send()
        .await
        .expect("create project");
    assert_eq!(project_response.status(), StatusCode::CREATED);
    let project_data = project_response
        .json::<Value>()
        .await
        .expect("project response");
    let project_id = project_data["project_id"].as_str().expect("project_id");

    let worker_response = client
        .post(format!(
            "{}/projects/{}/workers",
            harness.base_url, project_id
        ))
        .json(&json!({
            "prompt": "STALL_ONCE_THEN_COMPLETE create new audit report"
        }))
        .send()
        .await
        .expect("spawn worker");
    assert_eq!(worker_response.status(), StatusCode::CREATED);
    let worker_data = worker_response
        .json::<Value>()
        .await
        .expect("worker response");
    let worker_id = worker_data["id"].as_str().expect("worker id");

    let wait = client
        .get(format!(
            "{}/agents/{}/wait?timeout_ms=15000&poll_ms=25&include_events=true",
            harness.base_url, worker_id
        ))
        .send()
        .await
        .expect("wait for worker result");
    assert_eq!(wait.status(), StatusCode::OK);
    let wait_payload = wait.json::<Value>().await.expect("wait payload");
    assert_eq!(wait_payload["timed_out"].as_bool(), Some(false));
    assert_eq!(wait_payload["status"].as_str(), Some("completed"));

    let events = client
        .get(format!("{}/agents/{}/events", harness.base_url, worker_id))
        .send()
        .await
        .expect("worker events");
    assert_eq!(events.status(), StatusCode::OK);
    let events = events.json::<Value>().await.expect("worker events payload");
    let events = events.as_array().expect("worker events as array");
    assert!(
        events
            .iter()
            .any(|event| event["method"].as_str() == Some("turn/aborted"))
    );
    assert!(
        events
            .iter()
            .any(|event| event["method"].as_str() == Some("turn/completed"))
    );

    harness.terminate().await;
}

#[tokio::test]
async fn test_worker_monitoring_failures_when_restart_budget_exhausted() {
    let (_temp_project, project_path) =
        common::temporary_project("project-valid").expect("fixture copy");
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let service_config_contents = service_config_template_worker_monitoring(1_000, 0, 250);
    common::write_service_config(&service_config, service_config_contents.as_str())
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let project_response = client
        .post(format!("{}/projects", harness.base_url))
        .json(&json!({
            "path": project_path.to_string_lossy().to_string(),
            "start_prompt": "restart budget exhausted baseline",
        }))
        .send()
        .await
        .expect("create project");
    assert_eq!(project_response.status(), StatusCode::CREATED);
    let project_data = project_response
        .json::<Value>()
        .await
        .expect("project response");
    let project_id = project_data["project_id"].as_str().expect("project_id");

    let worker_response = client
        .post(format!(
            "{}/projects/{}/workers",
            harness.base_url, project_id
        ))
        .json(&json!({
            "prompt": "STALL_ONCE_THEN_COMPLETE will exhaust worker monitoring budget"
        }))
        .send()
        .await
        .expect("spawn worker");
    assert_eq!(worker_response.status(), StatusCode::CREATED);
    let worker_data = worker_response
        .json::<Value>()
        .await
        .expect("worker response");
    let worker_id = worker_data["id"].as_str().expect("worker id");

    let wait = client
        .get(format!(
            "{}/agents/{}/wait?timeout_ms=12000&poll_ms=50&include_events=true",
            harness.base_url, worker_id
        ))
        .send()
        .await
        .expect("wait for worker result");
    assert_eq!(wait.status(), StatusCode::OK);
    let wait_payload = wait.json::<Value>().await.expect("wait payload");
    assert_eq!(wait_payload["timed_out"].as_bool(), Some(false));
    assert_eq!(wait_payload["status"].as_str(), Some("aborted"));
    assert_eq!(
        wait_payload["completion_method"].as_str(),
        Some("turn/aborted")
    );
    let error = wait_payload["error"].as_str().expect("worker error text");
    assert!(error.contains("stalled"));

    let events = client
        .get(format!("{}/agents/{}/events", harness.base_url, worker_id))
        .send()
        .await
        .expect("worker events");
    assert_eq!(events.status(), StatusCode::OK);
    let events = events.json::<Value>().await.expect("worker events payload");
    let events = events.as_array().expect("worker events as array");
    assert!(
        events
            .iter()
            .any(|event| event["method"].as_str() == Some("turn/started")),
        "failed worker should include a started event"
    );

    let project_delete = client
        .delete(format!("{}/projects/{project_id}", harness.base_url))
        .send()
        .await
        .expect("close project");
    assert_eq!(project_delete.status(), StatusCode::NO_CONTENT);

    harness.terminate().await;
}

#[tokio::test]
async fn test_worker_monitoring_single_restart_reaches_completion() {
    let (_temp_project, project_path) =
        common::temporary_project("project-valid").expect("fixture copy");
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let service_config_contents = service_config_template_worker_monitoring(6_000, 1, 250);
    common::write_service_config(&service_config, service_config_contents.as_str())
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let project_response = client
        .post(format!("{}/projects", harness.base_url))
        .json(&json!({
            "path": project_path.to_string_lossy().to_string(),
            "start_prompt": "single restart boundary baseline",
        }))
        .send()
        .await
        .expect("create project");
    assert_eq!(project_response.status(), StatusCode::CREATED);
    let project_data = project_response
        .json::<Value>()
        .await
        .expect("project response");
    let project_id = project_data["project_id"].as_str().expect("project_id");

    let worker_response = client
        .post(format!(
            "{}/projects/{}/workers",
            harness.base_url, project_id
        ))
        .json(&json!({
            "prompt": "STALL_ONCE_THEN_COMPLETE should recover with one restart"
        }))
        .send()
        .await
        .expect("spawn worker");
    assert_eq!(worker_response.status(), StatusCode::CREATED);
    let worker_data = worker_response
        .json::<Value>()
        .await
        .expect("worker response");
    let worker_id = worker_data["id"].as_str().expect("worker id");

    let wait = client
        .get(format!(
            "{}/agents/{}/wait?timeout_ms=15000&poll_ms=25&include_events=true",
            harness.base_url, worker_id
        ))
        .send()
        .await
        .expect("wait for worker result");
    assert_eq!(wait.status(), StatusCode::OK);
    let wait_payload = wait.json::<Value>().await.expect("wait payload");
    assert_eq!(wait_payload["timed_out"].as_bool(), Some(false));
    assert_eq!(wait_payload["status"].as_str(), Some("completed"));
    assert_eq!(
        wait_payload["completion_method"].as_str(),
        Some("turn/completed")
    );

    let events = client
        .get(format!("{}/agents/{}/events", harness.base_url, worker_id))
        .send()
        .await
        .expect("worker events");
    assert_eq!(events.status(), StatusCode::OK);
    let events = events.json::<Value>().await.expect("worker events payload");
    let events = events.as_array().expect("worker events as array");
    let has_started = events
        .iter()
        .any(|event| event["method"].as_str() == Some("turn/started"));
    let aborted_count = events
        .iter()
        .filter(|event| event["method"].as_str() == Some("turn/aborted"))
        .count();
    let completed_count = events
        .iter()
        .filter(|event| event["method"].as_str() == Some("turn/completed"))
        .count();
    assert!(
        has_started,
        "single-restart worker should include started event"
    );
    assert!(
        aborted_count >= 1,
        "single-restart worker should include an abort event"
    );
    assert!(completed_count >= 1, "worker should eventually complete");
    assert!(completed_count > 0, "expected completion after restart");

    let project_delete = client
        .delete(format!("{}/projects/{project_id}", harness.base_url))
        .send()
        .await
        .expect("close project");
    assert_eq!(project_delete.status(), StatusCode::NO_CONTENT);

    harness.terminate().await;
}

#[tokio::test]
async fn test_job_wait_endpoint() {
    let (_temp_project, project_path) =
        common::temporary_project("project-valid").expect("fixture copy");
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let service_config_contents = r#"[callbacks]
default_profile = "record_webhook"

[callbacks.profiles.record_webhook]
type = "webhook"
url = "http://127.0.0.1:1/callback"
events = ["turn/completed"]

[callbacks.profiles.record_cmd]
type = "command"
program = "/bin/true"
events = ["turn/completed"]
"#;
    common::write_service_config(&service_config, service_config_contents)
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let project_response = client
        .post(format!("{}/projects", harness.base_url))
        .json(&json!({
            "path": project_path.to_string_lossy().to_string(),
            "start_prompt": "start this project",
        }))
        .send()
        .await
        .expect("create project");
    assert_eq!(project_response.status(), StatusCode::CREATED);
    let project_data = project_response
        .json::<Value>()
        .await
        .expect("project response");
    let project_id = project_data["project_id"].as_str().expect("project_id");

    let workers = vec![
        json!({
            "prompt": "analyze package layout",
            "labels": {"category": "security"},
        }),
        json!({
            "prompt": "analyze runtime behavior",
            "labels": {"category": "robustness"},
        }),
    ];

    let create_jobs = client
        .post(format!("{}/projects/{}/jobs", harness.base_url, project_id))
        .json(&json!({"workers": workers}))
        .send()
        .await
        .expect("spawn project jobs");
    assert_eq!(create_jobs.status(), StatusCode::CREATED);
    let job_data = create_jobs.json::<Value>().await.expect("job response");
    let job_id = job_data["job_id"].as_str().expect("job_id");

    let wait_with_workers = client
        .get(format!(
            "{}/jobs/{}/wait?timeout_ms=15000&poll_ms=25&include_workers=true",
            harness.base_url, job_id
        ))
        .send()
        .await
        .expect("wait for job");
    assert_eq!(wait_with_workers.status(), StatusCode::OK);
    let response = wait_with_workers
        .json::<Value>()
        .await
        .expect("wait response");
    assert_eq!(response["timed_out"].as_bool(), Some(false));
    let status = response["status"].as_str().expect("job status");
    assert!(
        matches!(status, "completed" | "partial" | "failed"),
        "unexpected job status from wait endpoint: {status}"
    );
    assert_eq!(
        response["workers"].as_array().expect("workers array").len(),
        2
    );

    let wait_without_workers = client
        .get(format!(
            "{}/jobs/{}/wait?timeout_ms=5000&poll_ms=25",
            harness.base_url, job_id
        ))
        .send()
        .await
        .expect("wait without workers");
    assert_eq!(wait_without_workers.status(), StatusCode::OK);
    let response_without_workers = wait_without_workers
        .json::<Value>()
        .await
        .expect("wait response without workers");
    assert!(
        response_without_workers["workers"]
            .as_array()
            .is_some_and(|workers| workers.is_empty())
    );

    harness.terminate().await;
}

#[tokio::test]
async fn test_project_fixture_validation_errors() {
    let temp = tempfile::tempdir().expect("temp dir");
    let service_config = temp.path().join("service.toml");
    let state_path = temp.path().join("foreman-state.json");
    common::write_service_config(&service_config, r#"[callbacks]"#).expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let (missing_temp, missing_dir) =
        common::temporary_project("project-missing-worker").expect("fixture");
    let missing_request = client
        .post(format!("{}/projects", harness.base_url))
        .json(&json!({
            "path": missing_dir.to_string_lossy().to_string(),
        }))
        .send()
        .await
        .expect("submit missing prompt fixture");
    assert_eq!(missing_request.status(), StatusCode::BAD_REQUEST);
    drop(missing_temp);

    let (_invalid_temp, invalid_dir) =
        common::temporary_project("project-invalid-config").expect("fixture");
    let invalid_request = client
        .post(format!("{}/projects", harness.base_url))
        .json(&json!({
            "path": invalid_dir.to_string_lossy().to_string(),
        }))
        .send()
        .await
        .expect("submit invalid config fixture");
    assert_eq!(invalid_request.status(), StatusCode::BAD_REQUEST);

    harness.terminate().await;
}

#[tokio::test]
async fn test_project_local_callback_profile_executes_for_worker_completed() {
    let (_temp_project, project_path) =
        common::temporary_project("project-valid").expect("fixture copy");
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let callback_output = PathBuf::from(format!("/tmp/foreman-local-callback-{}", common::random_port()));
    let _ = fs::remove_file(&callback_output);

    let project_config = format!(
        r#"name = "local-callback-project"

[prompts]
foreman_file = "FOREMAN.md"
worker_file = "WORKER.md"
runbook_file = "RUNBOOK.md"
handoff_file = "HANDOFF.md"

[callbacks.profiles.local_done]
type = "command"
program = "/bin/sh"
args = ["-c", "printf '%s' \"$FOREMAN_EVENT\" > '{callback_output}'"]
events = ["project/lifecycle/worker/completed"]

[callbacks.profiles.local_done.env]
FOREMAN_EVENT = "{{{{event_json}}}}"

[callbacks.lifecycle.worker_completed]
callback_profile = "local_done"
"#
        ,
        callback_output = callback_output.display()
    );
    fs::write(project_path.join("project.toml"), project_config).expect("write project config");
    common::write_service_config(&service_config, "[callbacks]\n").expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let project_response = client
        .post(format!("{}/projects", harness.base_url))
        .json(&json!({
            "path": project_path.to_string_lossy().to_string(),
            "start_prompt": "start local callback test",
        }))
        .send()
        .await
        .expect("create project");
    assert_eq!(project_response.status(), StatusCode::CREATED);
    let project_data = project_response
        .json::<Value>()
        .await
        .expect("project response");
    let project_id = project_data["project_id"].as_str().expect("project_id");

    let worker_response = client
        .post(format!(
            "{}/projects/{}/workers",
            harness.base_url, project_id
        ))
        .json(&json!({
            "prompt": "complete one worker so lifecycle callback fires",
        }))
        .send()
        .await
        .expect("spawn worker");
    assert_eq!(worker_response.status(), StatusCode::CREATED);

    assert!(
        wait_for_file_has_content(&callback_output, Duration::from_secs(3)).await,
        "project-local callback profile should execute on worker_completed"
    );

    let callback_status = client
        .get(format!(
            "{}/projects/{}/callback-status",
            harness.base_url, project_id
        ))
        .send()
        .await
        .expect("callback status request");
    assert_eq!(callback_status.status(), StatusCode::OK);
    let callback_status = callback_status
        .json::<Value>()
        .await
        .expect("callback status json");
    let callbacks = callback_status["callbacks"]
        .as_object()
        .expect("callback status callbacks object");
    let worker_completed_status = callbacks
        .get("worker_completed")
        .or_else(|| callbacks.get("project/lifecycle/worker/completed"))
        .and_then(Value::as_str);
    assert_eq!(
        worker_completed_status,
        Some("success"),
        "callback_status={callback_status}"
    );

    harness.terminate().await;
}

#[tokio::test]
async fn test_project_local_callback_profile_overrides_same_named_global_profile() {
    let (_temp_project, project_path) =
        common::temporary_project("project-valid").expect("fixture copy");
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let local_output = PathBuf::from(format!(
        "/tmp/foreman-local-override-callback-{}",
        common::random_port()
    ));
    let global_output = PathBuf::from(format!(
        "/tmp/foreman-global-override-callback-{}",
        common::random_port()
    ));
    let _ = fs::remove_file(&local_output);
    let _ = fs::remove_file(&global_output);

    let project_config = format!(
        r#"name = "local-over-global-project"

[prompts]
foreman_file = "FOREMAN.md"
worker_file = "WORKER.md"
runbook_file = "RUNBOOK.md"
handoff_file = "HANDOFF.md"

[callbacks.profiles.on_done]
type = "command"
program = "/bin/sh"
args = ["-c", "printf 'local:%s' \"$FOREMAN_EVENT\" > '{local_output}'"]
events = ["project/lifecycle/worker/completed"]

[callbacks.profiles.on_done.env]
FOREMAN_EVENT = "{{{{method}}}}"

[callbacks.lifecycle.worker_completed]
callback_profile = "on_done"
"#
        ,
        local_output = local_output.display()
    );
    fs::write(project_path.join("project.toml"), project_config).expect("write project config");

    let service_config_contents = format!(
        r#"[callbacks]

[callbacks.profiles.on_done]
type = "command"
program = "/bin/sh"
args = ["-c", "printf 'global:%s' \"$FOREMAN_EVENT\" > '{global_output}'"]
events = ["project/lifecycle/worker/completed"]

[callbacks.profiles.on_done.env]
FOREMAN_EVENT = "{{{{method}}}}"
"#
        ,
        global_output = global_output.display()
    );
    common::write_service_config(&service_config, &service_config_contents)
        .expect("write service config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let project_response = client
        .post(format!("{}/projects", harness.base_url))
        .json(&json!({
            "path": project_path.to_string_lossy().to_string(),
            "start_prompt": "start local-over-global callback test",
        }))
        .send()
        .await
        .expect("create project");
    assert_eq!(project_response.status(), StatusCode::CREATED);
    let project_data = project_response
        .json::<Value>()
        .await
        .expect("project response");
    let project_id = project_data["project_id"].as_str().expect("project_id");

    let worker_response = client
        .post(format!(
            "{}/projects/{}/workers",
            harness.base_url, project_id
        ))
        .json(&json!({
            "prompt": "finish worker and emit lifecycle callback",
        }))
        .send()
        .await
        .expect("spawn worker");
    assert_eq!(worker_response.status(), StatusCode::CREATED);

    assert!(
        wait_for_file_has_content(&local_output, Duration::from_secs(3)).await,
        "project-local profile should execute when same name exists globally"
    );
    let local_payload = fs::read_to_string(&local_output).expect("read local callback output");
    assert!(
        local_payload.starts_with("local:"),
        "local callback marker should be present"
    );

    assert!(
        wait_for_path_absence(&global_output, Duration::from_millis(1200)).await,
        "global same-name callback profile should not execute for this project callback"
    );

    harness.terminate().await;
}

#[tokio::test]
async fn test_project_local_webhook_profile_executes_for_worker_completed() {
    let (_temp_project, project_path) =
        common::temporary_project("project-valid").expect("fixture copy");
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    common::write_service_config(&service_config, "[callbacks]\n").expect("write service config");
    let webhook = common::start_webhook_capture().await;
    let webhook_url = format!("{}/callback", webhook.url);

    let project_config = format!(
        r#"name = "local-webhook-project"

[prompts]
foreman_file = "FOREMAN.md"
worker_file = "WORKER.md"
runbook_file = "RUNBOOK.md"
handoff_file = "HANDOFF.md"

[callbacks.profiles.local_webhook]
type = "webhook"
url = "{webhook_url}"
events = ["project/lifecycle/worker/completed"]

[callbacks.lifecycle.worker_completed]
callback_profile = "local_webhook"
"#
    );
    fs::write(project_path.join("project.toml"), project_config).expect("write project config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let project_response = client
        .post(format!("{}/projects", harness.base_url))
        .json(&json!({
            "path": project_path.to_string_lossy().to_string(),
            "start_prompt": "start local webhook callback project",
        }))
        .send()
        .await
        .expect("create project");
    assert_eq!(project_response.status(), StatusCode::CREATED);
    let project_data = project_response
        .json::<Value>()
        .await
        .expect("project response");
    let project_id = project_data["project_id"].as_str().expect("project_id");

    let worker_response = client
        .post(format!(
            "{}/projects/{}/workers",
            harness.base_url, project_id
        ))
        .json(&json!({
            "prompt": "run one worker for local webhook callback",
        }))
        .send()
        .await
        .expect("spawn worker");
    assert_eq!(worker_response.status(), StatusCode::CREATED);

    assert!(
        webhook.wait_for_events(1, Duration::from_secs(3)).await,
        "project-local webhook callback should emit event"
    );

    let events = webhook.events().await;
    let has_worker_completed_event = events.iter().any(|event| {
        event["method"].as_str() == Some("project/lifecycle/worker/completed")
    });
    assert!(has_worker_completed_event);

    webhook.stop().await;
    harness.terminate().await;
}

#[tokio::test]
async fn test_project_callback_profile_falls_back_to_global_service_profile() {
    let (_temp_project, project_path) =
        common::temporary_project("project-valid").expect("fixture copy");
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let output_path = PathBuf::from(format!(
        "/tmp/foreman-global-fallback-callback-{}",
        common::random_port()
    ));
    let _ = fs::remove_file(&output_path);

    let service_config_contents = format!(
        r#"[callbacks]

[callbacks.profiles.global_done]
type = "command"
program = "/bin/sh"
args = ["-c", "printf '%s' \"$FOREMAN_EVENT\" > '{output_path}'"]
events = ["project/lifecycle/worker/completed"]

[callbacks.profiles.global_done.env]
FOREMAN_EVENT = "{{{{method}}}}"
"#
        ,
        output_path = output_path.display()
    );
    common::write_service_config(&service_config, &service_config_contents)
        .expect("write service config");

    let project_config = r#"name = "global-fallback-project"

[prompts]
foreman_file = "FOREMAN.md"
worker_file = "WORKER.md"
runbook_file = "RUNBOOK.md"
handoff_file = "HANDOFF.md"

[callbacks.lifecycle.worker_completed]
callback_profile = "global_done"
"#;
    fs::write(project_path.join("project.toml"), project_config).expect("write project config");

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness =
        common::start_foreman(&socket_path, &service_config, &state_path, &fake_codex).await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let project_response = client
        .post(format!("{}/projects", harness.base_url))
        .json(&json!({
            "path": project_path.to_string_lossy().to_string(),
            "start_prompt": "start global fallback callback project",
        }))
        .send()
        .await
        .expect("create project");
    assert_eq!(project_response.status(), StatusCode::CREATED);
    let project_data = project_response
        .json::<Value>()
        .await
        .expect("project response");
    let project_id = project_data["project_id"].as_str().expect("project_id");

    let worker_response = client
        .post(format!(
            "{}/projects/{}/workers",
            harness.base_url, project_id
        ))
        .json(&json!({
            "prompt": "run one worker for global fallback callback",
        }))
        .send()
        .await
        .expect("spawn worker");
    assert_eq!(worker_response.status(), StatusCode::CREATED);

    assert!(
        wait_for_file_has_content(&output_path, Duration::from_secs(3)).await,
        "global callback profile should run when project-local profile is absent"
    );

    harness.terminate().await;
}

#[tokio::test]
async fn test_project_local_worker_aborted_callback_profile_executes() {
    let (_temp_project, project_path) =
        common::temporary_project("project-valid").expect("fixture copy");
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    common::write_service_config(&service_config, "[callbacks]\n").expect("write service config");
    let callback_output = PathBuf::from(format!(
        "/tmp/foreman-local-worker-aborted-callback-{}",
        common::random_port()
    ));
    let _ = fs::remove_file(&callback_output);

    let project_config = format!(
        r#"name = "local-worker-aborted-project"

[prompts]
foreman_file = "FOREMAN.md"
worker_file = "WORKER.md"
runbook_file = "RUNBOOK.md"
handoff_file = "HANDOFF.md"

[callbacks.profiles.local_aborted]
type = "command"
program = "/bin/sh"
args = ["-c", "printf '%s' \"$FOREMAN_EVENT\" > '{callback_output}'"]
events = ["project/lifecycle/worker/aborted"]

[callbacks.profiles.local_aborted.env]
FOREMAN_EVENT = "{{{{method}}}}"

[callbacks.lifecycle.worker_aborted]
callback_profile = "local_aborted"
"#
        ,
        callback_output = callback_output.display()
    );
    fs::write(project_path.join("project.toml"), project_config).expect("write project config");

    let fake_codex_script = temp.path().join("fake_codex_aborted.sh");
    write_executable_script(
        &fake_codex_script,
        r#"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi

extract_method() {
  echo "$1" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p'
}

extract_id() {
  echo "$1" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p'
}

while read -r line; do
  [ -z "$line" ] && continue
  method="$(extract_method "$line")"
  id="$(extract_id "$line")"
  [ -z "$id" ] && continue

  case "$method" in
    initialize)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
      ;;
    thread/start)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"threadId\":\"thread-abort-1\"}}"
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"thread/status/changed\",\"params\":{\"threadId\":\"thread-abort-1\",\"status\":\"running\"}}"
      ;;
    turn/start)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"turnId\":\"turn-abort-1\"}}"
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"turn/started\",\"params\":{\"threadId\":\"thread-abort-1\",\"turn\":{\"id\":\"turn-abort-1\",\"status\":\"running\",\"items\":[]}}}"
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"method\":\"turn/aborted\",\"params\":{\"threadId\":\"thread-abort-1\",\"turnId\":\"turn-abort-1\",\"error\":\"forced aborted status for test\"}}"
      ;;
    turn/interrupt)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
      ;;
    turn/steer)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"turnId\":\"turn-abort-1\"}}"
      ;;
    *)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
      ;;
  esac
done
"#,
    );

    let socket_path = format!("/tmp/foreman-test-{}.sock", common::random_port());
    let harness = common::start_foreman(
        &socket_path,
        &service_config,
        &state_path,
        fake_codex_script.to_string_lossy().as_ref(),
    )
    .await;
    let client = common::unix_client(&harness.socket_path).expect("unix socket client");

    let project_response = client
        .post(format!("{}/projects", harness.base_url))
        .json(&json!({
            "path": project_path.to_string_lossy().to_string(),
            "start_prompt": "start local worker aborted callback project",
        }))
        .send()
        .await
        .expect("create project");
    assert_eq!(project_response.status(), StatusCode::CREATED);
    let project_data = project_response
        .json::<Value>()
        .await
        .expect("project response");
    let project_id = project_data["project_id"].as_str().expect("project_id");

    let worker_response = client
        .post(format!(
            "{}/projects/{}/workers",
            harness.base_url, project_id
        ))
        .json(&json!({
            "prompt": "spawn worker that will emit turn/aborted",
        }))
        .send()
        .await
        .expect("spawn worker");
    assert_eq!(worker_response.status(), StatusCode::CREATED);
    let worker = worker_response
        .json::<Value>()
        .await
        .expect("worker response");
    let worker_id = worker["id"].as_str().expect("worker id");

    let wait = client
        .get(format!(
            "{}/agents/{}/wait?timeout_ms=8000&poll_ms=25",
            harness.base_url, worker_id
        ))
        .send()
        .await
        .expect("wait for aborted worker");
    assert_eq!(wait.status(), StatusCode::OK);

    assert!(
        wait_for_file_has_content(&callback_output, Duration::from_secs(5)).await,
        "worker_aborted lifecycle callback should execute"
    );

    let callback_status = client
        .get(format!(
            "{}/projects/{}/callback-status",
            harness.base_url, project_id
        ))
        .send()
        .await
        .expect("callback status request");
    assert_eq!(callback_status.status(), StatusCode::OK);
    let callback_status = callback_status
        .json::<Value>()
        .await
        .expect("callback status json");
    assert_eq!(
        callback_status["callbacks"]["worker_aborted"].as_str(),
        Some("success")
    );

    harness.terminate().await;
}

fn service_config_template(webhook_url: &str) -> String {
    format!(
        r#"[callbacks]
default_profile = "record_webhook"

[callbacks.profiles.record_webhook]
type = "webhook"
url = "{webhook_url}"
events = ["turn/completed", "turn/aborted"]

[callbacks.profiles.record_cmd]
type = "command"
program = "/bin/sh"
event_prompt_variable = "event_payload"
 args = [
 "-c",
 "printf '%s' \"$CODEx_EVENT_PAYLOAD\" > \"{{{{callback_file}}}}\"",
]
events = ["turn/completed"]
[callbacks.profiles.record_cmd.env]
CODEx_EVENT_PAYLOAD = "{{{{event_json}}}}"
"#
    )
}

fn service_config_template_prefix_profile() -> String {
    r#"[callbacks]
default_profile = "record_cmd"

[callbacks.profiles.record_cmd]
type = "command"
program = "/bin/sh"
args = ["-c", "printf '%s' \"$CODEx_EVENT_PROMPT\" > \"$CODEx_CALLBACK_FILE\""]
event_prompt_variable = "event_payload"
timeout_ms = 10000
events = ["turn/completed"]

[callbacks.profiles.record_cmd.env]
CODEx_CALLBACK_FILE = "{{callback_file}}"
CODEx_EVENT_PROMPT = "{{event_prompt}}"
"#
    .to_string()
}

fn service_config_template_with_unresolved_token() -> String {
    r#"[callbacks]
default_profile = "record_cmd"

[callbacks.profiles.record_cmd]
type = "command"
program = "/bin/sh"
args = ["-c", "printf '%s' \"$CODEx_EVENT_PAYLOAD\" > \"/tmp/foreman-unresolved-callback-{{callback_file}}\""]
event_prompt_variable = "event_payload"
events = ["turn/completed"]

[callbacks.profiles.record_cmd.env]
CODEx_EVENT_PAYLOAD = "{{event_json}}"
"#
    .to_string()
}

fn service_config_template_with_auth(token: &str) -> String {
    format!(
        r#"[security.auth]
enabled = true
token = "{token}"

[callbacks]
default_profile = "record_webhook"

[callbacks.profiles.record_webhook]
type = "webhook"
url = "http://127.0.0.1:1/callback"
"#
    )
}

fn service_config_template_worker_monitoring(
    inactivity_timeout_ms: u64,
    max_restarts: u32,
    watch_interval_ms: u64,
) -> String {
    format!(
        r#"[callbacks]
default_profile = "record_webhook"

[callbacks.profiles.record_webhook]
type = "webhook"
url = "http://127.0.0.1:1/callback"

[callbacks.profiles.record_cmd]
type = "command"
program = "/bin/true"

[worker_monitoring]
enabled = true
inactivity_timeout_ms = {inactivity_timeout_ms}
max_restarts = {max_restarts}
watch_interval_ms = {watch_interval_ms}
        "#
    )
}

fn service_config_template_timeout_profile() -> String {
    r#"[callbacks]
default_profile = "slow_cmd"

[callbacks.profiles.slow_cmd]
type = "command"
program = "/bin/sh"
args = ["-c", "sleep 5; printf '%s' \"done\" > \"$CODEx_CALLBACK_FILE\""]
event_prompt_variable = "event_payload"
timeout_ms = 100
events = ["turn/completed"]

[callbacks.profiles.slow_cmd.env]
CODEx_CALLBACK_FILE = "{{callback_file}}"
"#
    .to_string()
}

fn service_config_template_simple() -> String {
    r#"[callbacks]
default_profile = "record_webhook"

[callbacks.profiles.record_webhook]
type = "webhook"
url = "http://127.0.0.1:1/callback"
events = ["turn/completed"]
"#
    .to_string()
}

async fn wait_for_file_has_content(path: &PathBuf, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        if let Ok(contents) = fs::read_to_string(path)
            && !contents.trim().is_empty()
        {
            return true;
        }

        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_path_absence(path: &PathBuf, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        if fs::metadata(path).is_err() {
            return true;
        }

        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn write_executable_script(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write executable test script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path).expect("script metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("set script executable permissions");
    }
}

fn is_command_like_event(method: &str) -> bool {
    method.contains("exec_command")
        || method.contains("command")
        || method.contains("file")
        || method == "turn/started"
        || method == "turn/completed"
}

fn label_value_to_string(labels: &Value, key: &str) -> Option<String> {
    match labels.get(key)? {
        Value::String(value) => Some(value.to_string()),
        Value::Array(values) => values
            .first()
            .and_then(Value::as_str)
            .map(std::string::ToString::to_string),
        _ => None,
    }
}

fn extract_written_path(final_text: &str) -> Option<String> {
    final_text
        .split_once("WROTE ")
        .map(|(_, path)| path)
        .map(|path| {
            path.split_whitespace()
                .next()
                .unwrap_or(path)
                .trim()
                .to_string()
        })
}
