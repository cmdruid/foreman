use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::Context;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Mutex, broadcast, oneshot},
};
use tracing::{debug, warn};

use crate::protocol::{
    JSONRPCError, JSONRPCErrorError, JSONRPCMessage, JSONRPCNotification, JSONRPCRequest,
    JSONRPCResponse, RequestId, parse_thread_id,
};

type PendingResponses = HashMap<String, oneshot::Sender<Result<serde_json::Value, String>>>;

#[derive(Debug, Clone)]
pub struct RawNotification {
    pub method: String,
    pub params: serde_json::Value,
}

pub struct AppServerClient {
    child: Option<Child>,
    stdin: Arc<Mutex<tokio::io::BufWriter<ChildStdin>>>,
    pending: Arc<Mutex<PendingResponses>>,
    event_tx: broadcast::Sender<RawNotification>,
}

impl AppServerClient {
    pub async fn connect(
        codex_binary: &str,
        config_overrides: &[String],
        event_tx: broadcast::Sender<RawNotification>,
    ) -> anyhow::Result<Arc<Self>> {
        let mut cmd = Command::new(codex_binary);
        cmd.arg("app-server");
        for override_kv in config_overrides {
            cmd.arg("--config").arg(override_kv);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        if let Some(parent) = PathBuf::from(codex_binary).parent() {
            if let Some(path) = std::env::var_os("PATH") {
                let mut new_path = parent.as_os_str().to_owned();
                new_path.push(":" as _);
                new_path.push(path);
                cmd.env("PATH", new_path);
            } else {
                cmd.env("PATH", parent);
            }
        }

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

        let client = Arc::new(Self {
            child: Some(child),
            stdin: Arc::new(Mutex::new(tokio::io::BufWriter::new(stdin))),
            pending: Arc::new(Mutex::new(HashMap::new())),
            event_tx,
        });

        client.spawn_reader_loop(BufReader::new(stdout));
        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&self) -> anyhow::Result<()> {
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

    pub async fn request<T, R>(&self, method: &str, params: &T) -> anyhow::Result<R>
    where
        T: Serialize + Send + Sync,
        R: DeserializeOwned + Send,
    {
        let request_id = RequestId::new();
        let request_id_key = request_id.as_key();

        let (response_tx, response_rx) = oneshot::channel::<Result<serde_json::Value, String>>();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(request_id_key, response_tx);
        }

        let message = JSONRPCMessage::Request(JSONRPCRequest {
            id: request_id,
            method: method.to_string(),
            params: Some(serde_json::to_value(params)?),
        });
        self.write_message(message).await?;

        let value = response_rx
            .await
            .context("app-server response channel closed")?
            .context("app-server request failed")?;

        let parsed = serde_json::from_value(value)
            .context("failed to deserialize app-server response payload")?;
        Ok(parsed)
    }

    async fn notify<T>(&self, method: &str, params: &T) -> anyhow::Result<()>
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

    fn spawn_reader_loop(self: &Arc<Self>, stdout: BufReader<ChildStdout>) {
        let client = Arc::clone(self);

        tokio::spawn(async move {
            let mut lines = stdout.lines();
            loop {
                let mut line = String::new();
                match lines.read_line(&mut line).await {
                    Ok(0) => {
                        warn!("app-server stdout closed");
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }

                        match serde_json::from_str::<JSONRPCMessage>(trimmed) {
                            Ok(message) => match message {
                                JSONRPCMessage::Response(JSONRPCResponse { id, result }) => {
                                    let key = id.as_key();
                                    if let Some(sender) = client.pending.lock().await.remove(&key) {
                                        let _ = sender.send(Ok(result));
                                    }
                                }
                                JSONRPCMessage::Error(error) => {
                                    let key = error.id.as_key();
                                    let text = error.error.message;
                                    if let Some(sender) = client.pending.lock().await.remove(&key) {
                                        let _ = sender.send(Err(text));
                                    } else {
                                        warn!(%text, "unmatched app-server error");
                                    }
                                }
                                JSONRPCMessage::Notification(notification) => {
                                    let payload = notification
                                        .params
                                        .unwrap_or_else(|| serde_json::Value::Null);
                                    let _ = client.event_tx.send(RawNotification {
                                        method: notification.method,
                                        params: payload,
                                    });
                                }
                                JSONRPCMessage::Request(request) => {
                                    if let Err(err) =
                                        client.respond_to_server_request(request).await
                                    {
                                        warn!(%err, "failed to respond to app-server request");
                                    }
                                }
                            },
                            Err(err) => {
                                warn!(%err, line = %trimmed, "ignored malformed message from app-server");
                            }
                        }
                    }
                    Err(err) => {
                        warn!(%err, "failed reading app-server stdout");
                        break;
                    }
                }
            }
        });
    }

    async fn respond_to_server_request(&self, request: JSONRPCRequest) -> anyhow::Result<()> {
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
            Some(serde_json::json!({"success": true, "contentItems": []}))
        } else {
            None
        };

        if let Some(payload) = response {
            let reply = JSONRPCMessage::Response(crate::protocol::JSONRPCResponse {
                id: request.id,
                result: payload,
            });
            return self.write_message(reply).await;
        }

        let data = serde_json::json!({
            "method": request.method,
            "threadId": parse_thread_id(&request.params.unwrap_or_else(|| serde_json::Value::Null))
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

    async fn write_message(&self, message: JSONRPCMessage) -> anyhow::Result<()> {
        let payload = serde_json::to_string(&message)?;
        let mut stdin = self.stdin.lock().await;
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

    pub fn subscribe_events(&self) -> broadcast::Receiver<RawNotification> {
        self.event_tx.subscribe()
    }
}

impl Drop for AppServerClient {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
        }
    }
}
