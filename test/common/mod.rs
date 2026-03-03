#![allow(dead_code)]

use anyhow::Result;
use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use reqwest::Client;
use serde_json::Value;
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tempfile::TempDir;
use tokio::{
    net::TcpListener,
    process::{Child, Command},
    sync::Mutex,
    task::JoinHandle,
    time::sleep,
};

#[derive(Clone)]
pub struct WebhookCapture {
    events: Arc<Mutex<Vec<Value>>>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    pub url: String,
    delay: Duration,
}

impl WebhookCapture {
    pub async fn events(&self) -> Vec<Value> {
        self.events.lock().await.clone()
    }

    pub async fn delay(&self) -> Duration {
        self.delay
    }

    pub async fn wait_for_events(&self, expected: usize, timeout: Duration) -> bool {
        let start = tokio::time::Instant::now();
        loop {
            if self.events.lock().await.len() >= expected {
                return true;
            }

            if start.elapsed() >= timeout {
                return false;
            }

            sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn stop(&self) {
        if let Some(handle) = self.handle.lock().await.take() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

pub struct ForemanHarness {
    pub base_url: String,
    pub socket_path: String,
    pub child: Child,
}

impl ForemanHarness {
    pub async fn terminate(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

pub fn fixture_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test")
        .join("fixtures")
        .join(name)
}

pub fn temp_copy_dir(source: &Path, target: &Path) -> io::Result<()> {
    if !target.exists() {
        fs::create_dir_all(target)?;
    }

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());

        if source_path.is_dir() {
            temp_copy_dir(&source_path, &target_path)?;
        } else {
            fs::copy(&source_path, &target_path)?;
        }
    }

    Ok(())
}

pub fn temporary_project(source: &str) -> io::Result<(TempDir, PathBuf)> {
    let temp_dir = tempfile::tempdir()?;
    let dest = temp_dir.path().to_path_buf();
    temp_copy_dir(&fixture_dir(source), &dest)?;
    Ok((temp_dir, dest))
}

pub fn random_port() -> u16 {
    // Best effort only; callers should handle bind failures by retrying the test harness.
    if let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0")
        && let Ok(port) = listener.local_addr().map(|addr| addr.port())
    {
        return port;
    }
    8787
}

pub async fn wait_for_http(base_url: &str, socket_path: &str) {
    let client = unix_client(socket_path).expect("prepare unix-socket client");
    for _ in 0..80 {
        if let Ok(response) = client.get(format!("{base_url}/health")).send().await
            && response.status() == StatusCode::OK
        {
            return;
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("foreman server did not become ready");
}

pub async fn start_webhook_capture() -> WebhookCapture {
    start_webhook_capture_with_delay(Duration::from_millis(0)).await
}

pub async fn start_webhook_capture_with_delay(delay: Duration) -> WebhookCapture {
    let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let state = (events.clone(), delay);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind webhook test listener");
    let addr = listener.local_addr().expect("webhook local addr");

    let app = Router::new()
        .route("/callback", post(webhook_handler))
        .with_state(state);

    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("webhook capture server failed");
    });

    WebhookCapture {
        events,
        handle: Arc::new(Mutex::new(Some(handle))),
        url: format!("http://{}", addr),
        delay,
    }
}

async fn webhook_handler(
    State((events, delay)): State<(Arc<Mutex<Vec<Value>>>, Duration)>,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    if delay > Duration::ZERO {
        sleep(delay).await;
    }
    events.lock().await.push(payload);
    (StatusCode::OK, Json(serde_json::json!({"ok": true})))
}

pub async fn start_foreman(
    socket_path: &str,
    service_config: &Path,
    state_path: &Path,
    codex_binary: &str,
) -> ForemanHarness {
    let foreman_binary = binary_path("foreman");
    let mut project_path = state_path.to_path_buf();
    project_path.set_file_name("project.toml");

    let child = Command::new(foreman_binary)
        .arg("--socket-path")
        .arg(socket_path)
        .arg("--codex-binary")
        .arg(codex_binary)
        .arg("--service-config")
        .arg(service_config)
        .arg("--state-path")
        .arg(state_path)
        .arg("--project")
        .arg(&project_path)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("start foreman");

    let base_url = "http://localhost".to_string();
    wait_for_http(&base_url, socket_path).await;

    ForemanHarness {
        base_url,
        socket_path: socket_path.to_string(),
        child,
    }
}

pub async fn start_foreman_with_auth(
    socket_path: &str,
    service_config: &Path,
    state_path: &Path,
    codex_binary: &str,
    api_token: Option<&str>,
) -> ForemanHarness {
    let foreman_binary = binary_path("foreman");
    let mut command = Command::new(foreman_binary);
    let mut project_path = state_path.to_path_buf();
    project_path.set_file_name("project.toml");

    command
        .arg("--socket-path")
        .arg(socket_path)
        .arg("--codex-binary")
        .arg(codex_binary)
        .arg("--service-config")
        .arg(service_config)
        .arg("--state-path")
        .arg(state_path)
        .arg("--project")
        .arg(&project_path)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    if let Some(token) = api_token {
        command.env("CODEX_FOREMAN_API_TOKEN", token);
    }

    let child = command.spawn().expect("start foreman");

    let base_url = "http://localhost".to_string();
    wait_for_http(&base_url, socket_path).await;

    ForemanHarness {
        base_url,
        socket_path: socket_path.to_string(),
        child,
    }
}

pub fn unix_client(socket_path: &str) -> Result<Client> {
    Client::builder()
        .unix_socket(socket_path)
        .build()
        .map_err(Into::into)
}

pub fn binary_path(name: &str) -> String {
    let normalized_names = vec![name.to_string()];
    for candidate in &normalized_names {
        let env_key = format!("CARGO_BIN_EXE_{candidate}");
        if let Ok(path) = std::env::var(env_key) {
            return path;
        }
    }
    resolve_binary(name)
}

pub async fn stop_capture(capture: &WebhookCapture) {
    capture.stop().await;
}

fn resolve_binary(name: &str) -> String {
    let normalized_names = [name.to_string()];
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug");
    for exe_name in normalized_names {
        let candidate = if cfg!(windows) {
            format!("{exe_name}.exe")
        } else {
            exe_name
        };
        let path = base.join(&candidate);
        if path.exists() {
            return path.to_string_lossy().to_string();
        }
    }
    base.join(if cfg!(windows) {
        format!("{}.exe", name)
    } else {
        name.to_string()
    })
    .to_string_lossy()
    .to_string()
}

pub fn write_service_config(path: &Path, contents: &str) -> io::Result<()> {
    fs::write(path, contents)
}
