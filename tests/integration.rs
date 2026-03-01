use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;

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

    let bind = format!("127.0.0.1:{}", common::random_port());
    let fake_codex = common::binary_path("fake_codex");

    let harness = common::start_foreman(&bind, &service_config, &state_path, &fake_codex).await;
    let client = reqwest::Client::new();

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
        "/tmp/codex-foreman-command-callback-{}",
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

    let bind = format!("127.0.0.1:{}", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness = common::start_foreman(&bind, &service_config, &state_path, &fake_codex).await;
    let client = reqwest::Client::new();

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
        "/tmp/codex-foreman-prefix-callback-{}",
        common::random_port()
    ));

    let service_config_contents = service_config_template_prefix_profile();
    common::write_service_config(&service_config, service_config_contents.as_str())
        .expect("write service config");

    let bind = format!("127.0.0.1:{}", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness = common::start_foreman(&bind, &service_config, &state_path, &fake_codex).await;

    let client = reqwest::Client::new();
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
        std::path::PathBuf::from("/tmp/codex-foreman-unresolved-callback-{{callback_file}}");
    let _ = fs::remove_file(&unresolved_output);

    let service_config_contents = service_config_template_with_unresolved_token();
    common::write_service_config(&service_config, service_config_contents.as_str())
        .expect("write service config");

    let bind = format!("127.0.0.1:{}", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness = common::start_foreman(&bind, &service_config, &state_path, &fake_codex).await;
    let client = reqwest::Client::new();

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

    let bind = format!("127.0.0.1:{}", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness = common::start_foreman(&bind, &service_config, &state_path, &fake_codex).await;
    let client = reqwest::Client::new();

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

    let bind = format!("127.0.0.1:{}", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness = common::start_foreman_with_auth(
        &bind,
        &service_config,
        &state_path,
        &fake_codex,
        Some("alpha-token"),
    )
    .await;
    let client = reqwest::Client::new();

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
async fn test_command_callback_timeout_isolation() {
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let output_file = std::path::PathBuf::from(format!(
        "/tmp/codex-foreman-timeout-callback-{}",
        common::random_port()
    ));
    let service_config_contents = service_config_template_timeout_profile();
    common::write_service_config(&service_config, &service_config_contents)
        .expect("write service config");

    let bind = format!("127.0.0.1:{}", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness = common::start_foreman(&bind, &service_config, &state_path, &fake_codex).await;
    let client = reqwest::Client::new();

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

    let bind = format!("127.0.0.1:{}", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness = common::start_foreman(&bind, &service_config, &state_path, &fake_codex).await;
    let client = reqwest::Client::new();

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

    let restarted = common::start_foreman(&bind, &service_config, &state_path, &fake_codex).await;
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

    let bind = format!("127.0.0.1:{}", common::random_port());
    let fake_codex = common::binary_path("fake_codex");

    let harness = common::start_foreman(&bind, &service_config, &state_path, &fake_codex).await;
    let client = reqwest::Client::new();

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
    let restarted = common::start_foreman(&bind, &service_config, &state_path, &fake_codex).await;
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

    let bind = format!("127.0.0.1:{}", common::random_port());
    let fake_codex = common::binary_path("fake_codex");

    let harness = common::start_foreman(&bind, &service_config, &state_path, &fake_codex).await;
    let client = reqwest::Client::new();

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

    let bind = format!("127.0.0.1:{}", common::random_port());
    let fake_codex = common::binary_path("fake_codex");

    let harness = common::start_foreman(&bind, &service_config, &state_path, &fake_codex).await;
    let client = reqwest::Client::new();

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
async fn test_worker_monitoring_restarts_stalled_worker() {
    let (_temp_project, project_path) =
        common::temporary_project("project-valid").expect("fixture copy");
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let service_config_contents =
        service_config_template_worker_monitoring(1_000, 1, 300);
    common::write_service_config(&service_config, service_config_contents.as_str())
        .expect("write service config");

    let bind = format!("127.0.0.1:{}", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness = common::start_foreman(&bind, &service_config, &state_path, &fake_codex).await;
    let client = reqwest::Client::new();

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
    let events = events
        .as_array()
        .expect("worker events as array");
    assert!(events
        .iter()
        .any(|event| event["method"].as_str() == Some("turn/aborted")));
    assert!(events
        .iter()
        .any(|event| event["method"].as_str() == Some("turn/completed")));

    harness.terminate().await;
}

#[tokio::test]
async fn test_worker_monitoring_failures_when_restart_budget_exhausted() {
    let (_temp_project, project_path) =
        common::temporary_project("project-valid").expect("fixture copy");
    let temp = tempfile::tempdir().expect("temp dir");
    let state_path = temp.path().join("foreman-state.json");
    let service_config = temp.path().join("service.toml");
    let service_config_contents =
        service_config_template_worker_monitoring(1_000, 0, 250);
    common::write_service_config(&service_config, service_config_contents.as_str())
        .expect("write service config");

    let bind = format!("127.0.0.1:{}", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness = common::start_foreman(&bind, &service_config, &state_path, &fake_codex).await;
    let client = reqwest::Client::new();

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
    let error = wait_payload["error"]
        .as_str()
        .expect("worker error text");
    assert!(error.contains("stalled"));

    let events = client
        .get(format!("{}/agents/{}/events", harness.base_url, worker_id))
        .send()
        .await
        .expect("worker events");
    assert_eq!(events.status(), StatusCode::OK);
    let events = events.json::<Value>().await.expect("worker events payload");
    let events = events
        .as_array()
        .expect("worker events as array");
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

    let bind = format!("127.0.0.1:{}", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness = common::start_foreman(&bind, &service_config, &state_path, &fake_codex).await;
    let client = reqwest::Client::new();

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

    let bind = format!("127.0.0.1:{}", common::random_port());
    let fake_codex = common::binary_path("fake_codex");
    let harness = common::start_foreman(&bind, &service_config, &state_path, &fake_codex).await;
    let client = reqwest::Client::new();

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
args = ["-c", "printf '%s' \"$CODEx_EVENT_PAYLOAD\" > \"/tmp/codex-foreman-unresolved-callback-{{callback_file}}\""]
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
