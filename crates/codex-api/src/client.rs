use std::{collections::HashMap, path::PathBuf, process::Stdio as StdStdio, sync::Arc};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, broadcast, oneshot},
    time::{self, Duration},
};
use tracing::{debug, warn};

use crate::protocol::{
    JSONRPCError, JSONRPCErrorError, JSONRPCMessage, JSONRPCNotification, JSONRPCRequest,
    JSONRPCResponse, RequestId, parse_thread_id,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TextPayload {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub text_elements: Vec<serde_json::Value>,
}

impl TextPayload {
    pub fn text<T: Into<String>>(text: T) -> Self {
        Self {
            kind: "text".to_string(),
            text: text.into(),
            text_elements: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadStartRequest {
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub cwd: Option<String>,
    pub sandbox: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadStartResponse {
    pub thread_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnStartRequest {
    pub thread_id: String,
    pub input: Vec<TextPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnStartResponse {
    pub turn_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnSteerRequest {
    pub thread_id: String,
    pub expected_turn_id: String,
    pub input: Vec<TextPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnSteerResponse {
    pub turn_id: String,
    #[serde(default)]
    pub expected_turn_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnInterruptRequest {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmptyResponse {}

type PendingResponses = HashMap<String, oneshot::Sender<Result<serde_json::Value, String>>>;

#[derive(Debug, Clone)]
pub struct RawNotification {
    pub method: String,
    pub params: serde_json::Value,
}

#[derive(Debug)]
pub struct AppServerClient {
    child: Option<Child>,
    writer: Arc<Mutex<BufWriter<ChildStdin>>>,
    pending: Arc<Mutex<PendingResponses>>,
    event_tx: broadcast::Sender<RawNotification>,
    request_timeout_ms: u64,
}

impl AppServerClient {
    const CONNECT_RETRY_ATTEMPTS: usize = 3;
    const CONNECT_RETRY_DELAY_MS: u64 = 250;
    #[allow(dead_code)]
    const INITIALIZE_TIMEOUT_MS: u64 = 5_000;
    #[allow(dead_code)]
    const REQUEST_TIMEOUT_MS: u64 = 30_000;

    pub fn app_server_pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(|child| child.id())
    }

    #[allow(dead_code)]
    pub async fn connect(
        codex_binary: &str,
        config_overrides: &[String],
        event_tx: broadcast::Sender<RawNotification>,
    ) -> Result<Arc<Self>> {
        Self::connect_with_timeouts(
            codex_binary,
            config_overrides,
            event_tx,
            Self::INITIALIZE_TIMEOUT_MS,
            Self::REQUEST_TIMEOUT_MS,
        )
        .await
    }

    pub async fn connect_with_timeouts(
        codex_binary: &str,
        config_overrides: &[String],
        event_tx: broadcast::Sender<RawNotification>,
        initialize_timeout_ms: u64,
        request_timeout_ms: u64,
    ) -> Result<Arc<Self>> {
        let mut last_error: Option<anyhow::Error> = None;

        for attempt in 1..=Self::CONNECT_RETRY_ATTEMPTS {
            match Self::connect_once_with_timeouts(
                codex_binary,
                config_overrides,
                event_tx.clone(),
                initialize_timeout_ms,
                request_timeout_ms,
            )
            .await
            {
                Ok(client) => return Ok(client),
                Err(err) if attempt < Self::CONNECT_RETRY_ATTEMPTS => {
                    warn!(attempt, %err, "app-server connect attempt failed, retrying");
                    last_error = Some(err);
                    time::sleep(Duration::from_millis(Self::CONNECT_RETRY_DELAY_MS)).await;
                }
                Err(err) => {
                    return Err(err);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("failed to connect to app-server")))
    }

    async fn connect_once_with_timeouts(
        codex_binary: &str,
        config_overrides: &[String],
        event_tx: broadcast::Sender<RawNotification>,
        initialize_timeout_ms: u64,
        request_timeout_ms: u64,
    ) -> Result<Arc<Self>> {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (child, writer, stdout) = connect_stdio(codex_binary, config_overrides).await?;
        let child = Some(child);
        let client = Arc::new(Self {
            child,
            writer,
            pending: Arc::clone(&pending),
            event_tx: event_tx.clone(),
            request_timeout_ms,
        });
        client.spawn_stdio_reader(BufReader::new(stdout)).await;

        let initialized = time::timeout(
            Duration::from_millis(initialize_timeout_ms),
            client.initialize(),
        )
        .await;
        match initialized {
            Ok(result) => result?,
            Err(err) => {
                client.fail_all_pending(err.to_string()).await;
                return Err(anyhow::anyhow!(err)).context(format!(
                    "app-server did not initialize within {}ms",
                    initialize_timeout_ms
                ));
            }
        }

        Ok(client)
    }

    async fn spawn_stdio_reader(self: &Arc<Self>, stdout: BufReader<ChildStdout>) {
        let client = Arc::clone(self);
        tokio::spawn(async move {
            let mut lines = stdout;
            loop {
                let mut line = String::new();
                match lines.read_line(&mut line).await {
                    Ok(0) => {
                        warn!("app-server stdout closed");
                        client
                            .fail_all_pending(
                                "app-server stdout closed while awaiting response".to_string(),
                            )
                            .await;
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }

                        if let Err(err) = client.handle_message(trimmed).await {
                            warn!(%err, line = %trimmed, "failed to handle app-server message");
                        }
                    }
                    Err(err) => {
                        warn!(%err, "failed reading app-server stdout");
                        client
                            .fail_all_pending("failed reading app-server stdout".to_string())
                            .await;
                        break;
                    }
                }
            }
        });
    }

    async fn initialize(&self) -> Result<()> {
        let params = serde_json::json!({
            "clientInfo": {
                "name": "codex-foreman",
                "title": "Codex Foreman",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {
                "experimentalApi": false,
            },
        });

        let _: serde_json::Value = self.request("initialize", &params).await?;
        self.notify("initialized", &serde_json::Value::Null).await?;
        Ok(())
    }

    pub async fn request<T, R>(&self, method: &str, params: &T) -> Result<R>
    where
        T: Serialize + Send + Sync,
        R: DeserializeOwned + Send,
    {
        let request_id = RequestId::new();
        let request_id_key = request_id.as_key();

        let (response_tx, response_rx) = oneshot::channel::<Result<serde_json::Value, String>>();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(request_id_key.clone(), response_tx);
        }

        let message = JSONRPCMessage::Request(JSONRPCRequest {
            id: request_id,
            method: method.to_string(),
            params: Some(serde_json::to_value(params)?),
        });
        if let Err(err) = self.write_message(message).await {
            let mut pending = self.pending.lock().await;
            pending.remove(&request_id_key);
            return Err(err);
        }

        let value =
            match time::timeout(Duration::from_millis(self.request_timeout_ms), response_rx).await
            {
                Ok(value) => value,
                Err(_) => {
                    let mut pending = self.pending.lock().await;
                    pending.remove(&request_id_key);
                    return Err(anyhow::anyhow!(
                        "app-server request timed out after {}ms",
                        self.request_timeout_ms
                    ));
                }
            }
            .context("app-server response channel closed")?;

        let value = value
            .map_err(|err| anyhow::anyhow!(err))
            .context("app-server request failed")?;
        let parsed = serde_json::from_value(value)
            .context("failed to deserialize app-server response payload")?;

        Ok(parsed)
    }

    pub async fn thread_start(&self, request: &ThreadStartRequest) -> Result<ThreadStartResponse> {
        self.request("thread/start", request).await
    }

    pub async fn turn_start(&self, request: &TurnStartRequest) -> Result<TurnStartResponse> {
        self.request("turn/start", request).await
    }

    pub async fn turn_steer(&self, request: &TurnSteerRequest) -> Result<TurnSteerResponse> {
        self.request("turn/steer", request).await
    }

    pub async fn turn_interrupt(&self, request: &TurnInterruptRequest) -> Result<EmptyResponse> {
        self.request("turn/interrupt", request).await
    }

    async fn fail_all_pending(&self, reason: String) {
        let mut pending = self.pending.lock().await;
        for (_id, sender) in pending.drain() {
            let _ = sender.send(Err(reason.clone()));
        }
    }

    async fn notify<T>(&self, method: &str, params: &T) -> Result<()>
    where
        T: Serialize + Send + Sync,
    {
        let params = serde_json::to_value(params)?;
        let message = JSONRPCMessage::Notification(JSONRPCNotification {
            method: method.to_string(),
            params: Some(params),
        });
        self.write_message(message).await
    }

    async fn write_message(&self, message: JSONRPCMessage) -> Result<()> {
        let payload = serde_json::to_string(&message)?;
        let mut stdin = self.writer.lock().await;
        stdin
            .write_all(payload.as_bytes())
            .await
            .context("failed to write message to app-server stdin")?;
        stdin
            .write_all(b"\n")
            .await
            .context("failed to write newline to app-server stdin")?;
        stdin
            .flush()
            .await
            .context("failed to flush app-server stdin")?;
        debug!(payload = %payload, "app-server request sent");
        Ok(())
    }

    async fn handle_message(self: &Arc<Self>, payload: &str) -> Result<()> {
        let message = serde_json::from_str::<JSONRPCMessage>(payload)
            .context("ignored malformed message from app-server")?;

        match message {
            JSONRPCMessage::Response(JSONRPCResponse { id, result }) => {
                let key = id.as_key();
                if let Some(sender) = self.pending.lock().await.remove(&key) {
                    let _ = sender.send(Ok(result));
                }
            }
            JSONRPCMessage::Error(error) => {
                let key = error.id.as_key();
                let text = error.error.message;
                if let Some(sender) = self.pending.lock().await.remove(&key) {
                    let _ = sender.send(Err(text));
                } else {
                    warn!(%text, "unmatched app-server error");
                }
            }
            JSONRPCMessage::Notification(notification) => {
                let payload = notification.params.unwrap_or(serde_json::Value::Null);
                let _ = self.event_tx.send(RawNotification {
                    method: notification.method,
                    params: payload,
                });
            }
            JSONRPCMessage::Request(request) => {
                if let Err(err) = self.respond_to_server_request(request).await {
                    warn!(%err, "failed to respond to app-server request");
                }
            }
        }
        Ok(())
    }

    async fn respond_to_server_request(&self, request: JSONRPCRequest) -> Result<()> {
        let method = request.method.as_str();
        let response = if matches!(
            method,
            "item/commandExecution/requestApproval"
                | "item/fileChange/requestApproval"
                | "item/requestUserInput"
                | "item/tool/requestUserInput"
        ) {
            Some(serde_json::json!({"decision": "accept"}))
        } else if method == "item/tool/call" {
            Some(serde_json::json!({"success": true, "content_items": []}))
        } else {
            None
        };

        if let Some(payload) = response {
            let reply = JSONRPCMessage::Response(JSONRPCResponse {
                id: request.id,
                result: payload,
            });
            return self.write_message(reply).await;
        }

        let data = serde_json::json!({
            "method": request.method,
            "thread_id": parse_thread_id(&request.params.unwrap_or(serde_json::Value::Null))
                .unwrap_or_default(),
            "status": "unhandled",
        });

        let reply = JSONRPCMessage::Error(JSONRPCError {
            id: request.id,
            error: JSONRPCErrorError {
                code: -32601,
                message: "Request not handled by codex-foreman".to_string(),
                data: Some(data),
            },
        });
        self.write_message(reply).await
    }

    #[allow(dead_code)]
    pub fn subscribe_events(&self) -> broadcast::Receiver<RawNotification> {
        self.event_tx.subscribe()
    }
}

impl Drop for AppServerClient {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AppServerClient;
    use std::{fs, path::Path, time::Duration};
    use tempfile::tempdir;
    use tokio::sync::broadcast;
    use tokio::time::timeout;

    fn write_fake_app_server_script(path: &Path, contents: &str) {
        fs::write(path, contents).expect("write fake app-server script");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = fs::metadata(path).expect("script metadata").permissions();
            perm.set_mode(0o755);
            fs::set_permissions(path, perm).expect("make script executable");
        }
    }

    #[tokio::test]
    async fn connect_times_out_if_initialize_is_not_responded() {
        let temp_dir = tempdir().expect("tempdir");
        let script_path = temp_dir.path().join("fake_app_server_no_initialize.sh");
        write_fake_app_server_script(
            &script_path,
            r#"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi
sleep 8
"#,
        );

        let (event_tx, _event_rx) = broadcast::channel(16);
        let start = std::time::Instant::now();
        let result =
            AppServerClient::connect(script_path.to_str().expect("script path"), &[], event_tx)
                .await;
        assert!(
            result.is_err(),
            "connecting should fail when app-server doesn't initialize"
        );

        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(5_000),
            "timeout should wait at least initialize timeout window, got {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(20),
            "test timeout wait exceeded expected range: {elapsed:?}"
        );

        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("did not initialize within"),
            "unexpected connect error: {err}"
        );
    }

    #[tokio::test]
    async fn connect_times_out_uses_configurable_initialize_timeout() {
        let temp_dir = tempdir().expect("tempdir");
        let script_path = temp_dir.path().join("fake_app_server_hangs_initialize.sh");
        write_fake_app_server_script(
            &script_path,
            r#"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi
sleep 8
"#,
        );

        let (event_tx, _event_rx) = broadcast::channel(16);
        let start = std::time::Instant::now();
        let result = AppServerClient::connect_with_timeouts(
            script_path.to_str().expect("script path"),
            &[],
            event_tx,
            50,
            5_000,
        )
        .await;

        assert!(
            result.is_err(),
            "connecting should fail when initialize is delayed"
        );

        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(1_000),
            "connect should fail quickly using configured initialize timeout, got {elapsed:?}"
        );

        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("did not initialize within 50ms"),
            "unexpected connect error: {err}"
        );
    }

    #[tokio::test]
    async fn request_is_failing_with_pending_cleanup_when_app_server_stdout_closes() {
        let temp_dir = tempdir().expect("tempdir");
        let script_path = temp_dir.path().join("fake_app_server_close_stdout.sh");
        write_fake_app_server_script(
            &script_path,
            r#"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi
read -r line
if [ -z "$line" ]; then
  exit 0
fi
id=$(printf '%s' "$line" | sed -n 's/.*\"id\":\"\([^\"]*\)\".*/\1/p')
printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":\"${id}\",\"result\":{}}"
read -r _request
sleep 1
"#,
        );

        let (event_tx, _event_rx) = broadcast::channel(16);
        let client =
            AppServerClient::connect(script_path.to_str().expect("script path"), &[], event_tx)
                .await
                .expect("connect to fake app-server");

        let request = async {
            client
                .request::<_, serde_json::Value>(
                    "thread/start",
                    &serde_json::json!({
                        "cwd": null,
                        "model": null,
                        "model_provider": null,
                        "sandbox": null
                    }),
                )
                .await
        };
        let request_result = timeout(Duration::from_secs(5), request).await;
        assert!(request_result.is_ok(), "request should finish promptly");
        let request_result = request_result.expect("request timeout");
        assert!(
            request_result.is_err(),
            "request should fail when app-server closes stdout"
        );
        let message = request_result.unwrap_err().to_string();
        assert!(
            message.contains("app-server request failed"),
            "unexpected request error: {message}"
        );
    }

    #[tokio::test]
    async fn request_times_out_quickly_when_server_does_not_respond() {
        let temp_dir = tempdir().expect("tempdir");
        let script_path = temp_dir
            .path()
            .join("fake_app_server_no_response_to_thread_start.sh");
        write_fake_app_server_script(
            &script_path,
            r#"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi
read -r line
if [ -z "$line" ]; then
  exit 0
fi
init_id=$(printf '%s' "$line" | sed -n 's/.*\"id\":\"\([^\"]*\)\".*/\1/p')
printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":\"${init_id}\",\"result\":{}}"
while read -r _line; do
  :
done
"#,
        );

        let (event_tx, _event_rx) = broadcast::channel(16);
        let client = AppServerClient::connect_with_timeouts(
            script_path.to_str().expect("script path"),
            &[],
            event_tx,
            5_000,
            100,
        )
        .await
        .expect("connect to fake app-server");

        let start = std::time::Instant::now();
        let request = async {
            client
                .request::<_, serde_json::Value>(
                    "thread/start",
                    &serde_json::json!({
                        "cwd": null,
                        "model": null,
                        "model_provider": null,
                        "sandbox": null
                    }),
                )
                .await
        };
        let request_result = timeout(Duration::from_secs(1), request).await;
        assert!(
            request_result.is_ok(),
            "request should return before test timeout"
        );
        let request_result = request_result.expect("request timeout");
        assert!(
            request_result.is_err(),
            "request should fail on response timeout"
        );

        let message = request_result.unwrap_err().to_string();
        assert!(
            message.contains("app-server request timed out after 100ms"),
            "unexpected request error: {message}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "request timeout should complete quickly"
        );
    }

    #[tokio::test]
    async fn endpoint_contract_covers_lifecycle_methods() {
        let temp_dir = tempdir().expect("tempdir");
        let request_log = temp_dir.path().join("codex-api-requests.log");
        let script_path = temp_dir.path().join("fake_app_server_contract.sh");
        let script = r#"#!/usr/bin/env sh
if [ "$1" != "app-server" ]; then
  exit 1
fi
rm -f "__LOG_FILE__"
touch "__LOG_FILE__"
while read -r line; do
  [ -z "$line" ] && continue
  echo "$line" >> "__LOG_FILE__"

  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  if [ "$method" = "initialize" ]; then
    printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
    continue
  fi

  case "$method" in
    thread/start)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"thread_id\":\"thread-1\"}}"
      ;;
    turn/start)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"turn_id\":\"turn-1\"}}"
      ;;
    turn/steer)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{\"turn_id\":\"turn-2\",\"expected_turn_id\":\"turn-1\"}}"
      ;;
    turn/interrupt)
      printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"result\":{}}"
      ;;
    *)
      if [ -n "$id" ]; then
        printf "%s\n" "{\"jsonrpc\":\"2.0\",\"id\":\"$id\",\"error\":{\"code\":-32601,\"message\":\"unsupported method: $method\"}}"
      fi
      ;;
  esac
done
"#.replace("__LOG_FILE__", &request_log.to_string_lossy());
        write_fake_app_server_script(&script_path, &script);

        let (event_tx, _event_rx) = broadcast::channel(16);
        let script = script_path.to_str().expect("script path");
        let client = AppServerClient::connect(script, &[], event_tx)
            .await
            .expect("connect fake app-server");

        let thread_result = client
            .request::<_, serde_json::Value>(
                "thread/start",
                &serde_json::json!({
                    "cwd": null,
                    "model": null,
                    "model_provider": null,
                    "sandbox": null
                }),
            )
            .await
            .expect("thread start");
        assert_eq!(thread_result, serde_json::json!({"thread_id": "thread-1"}));

        let turn_result = client
            .request::<_, serde_json::Value>(
                "turn/start",
                &serde_json::json!({
                    "thread_id": "thread-1",
                    "input": []
                }),
            )
            .await
            .expect("turn start");
        assert_eq!(turn_result, serde_json::json!({"turn_id": "turn-1"}));

        let steer_result = client
            .request::<_, serde_json::Value>(
                "turn/steer",
                &serde_json::json!({
                    "thread_id": "thread-1",
                    "expected_turn_id": "turn-1",
                    "input": []
                }),
            )
            .await
            .expect("turn steer");
        assert_eq!(
            steer_result,
            serde_json::json!({"turn_id": "turn-2", "expected_turn_id": "turn-1"})
        );

        let interrupt_result = client
            .request::<_, serde_json::Value>(
                "turn/interrupt",
                &serde_json::json!({
                    "thread_id": "thread-1",
                    "turn_id": "turn-1"
                }),
            )
            .await
            .expect("turn interrupt");
        assert_eq!(interrupt_result, serde_json::json!({}));

        let request_lines = std::fs::read_to_string(&request_log).expect("read request log");
        assert!(request_lines.contains("\"method\":\"initialize\""));
        assert!(request_lines.contains("\"method\":\"thread/start\""));
        assert!(request_lines.contains("\"method\":\"turn/start\""));
        assert!(request_lines.contains("\"method\":\"turn/steer\""));
        assert!(request_lines.contains("\"method\":\"turn/interrupt\""));
        assert!(request_lines.contains("\"method\":\"initialized\""));
    }
}

async fn connect_stdio(
    codex_binary: &str,
    config_overrides: &[String],
) -> Result<(Child, Arc<Mutex<BufWriter<ChildStdin>>>, ChildStdout)> {
    let mut cmd = Command::new(codex_binary);
    cmd.arg("app-server");
    for override_kv in config_overrides {
        cmd.arg("--config").arg(override_kv);
    }
    cmd.stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::inherit());
    configure_command_path(codex_binary, &mut cmd);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn `{codex_binary} app-server`"))?;

    let stdin = child
        .stdin
        .take()
        .context("app-server stdin unavailable (expected piped)")?;
    let stdout = child
        .stdout
        .take()
        .context("app-server stdout unavailable (expected piped)")?;

    let writer = Arc::new(Mutex::new(BufWriter::new(stdin)));
    Ok((child, writer, stdout))
}

fn configure_command_path(binary_path: &str, cmd: &mut Command) {
    if let Some(parent) = PathBuf::from(binary_path).parent() {
        if let Some(path) = std::env::var_os("PATH") {
            let mut new_path = parent.as_os_str().to_owned();
            new_path.push(":");
            new_path.push(path);
            cmd.env("PATH", new_path);
        } else {
            cmd.env("PATH", parent);
        }
    }
}
