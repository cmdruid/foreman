use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Write};
use std::sync::atomic::{AtomicU64, Ordering};

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Real usage in foreman: `<binary> app-server ...`
    // The test shim accepts any invocation that includes `app-server` and
    // ignores all subsequent flags.
    if args.len() < 2 || args[1] != "app-server" {
        eprintln!("fake_codex expected to be invoked as `app-server`");
        std::process::exit(1);
    }

    run_stub_server();
}

fn run_stub_server() {
    let stdin = BufReader::new(io::stdin());
    let mut stdout = io::BufWriter::new(io::stdout());

    let thread_counter = AtomicU64::new(0);
    let turn_counter = AtomicU64::new(0);

    for line_result in stdin.lines() {
        let line = match line_result {
            Ok(line) => line,
            Err(err) => {
                eprintln!("fake_codex read error: {err}");
                continue;
            }
        };

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(err) => {
                eprintln!("fake_codex invalid json: {err}");
                continue;
            }
        };

        let Some(id) = request.get("id").cloned() else {
            // Ignore unrecognized notifications without request ids.
            continue;
        };

        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));

        match method {
            "initialize" => {
                send_response(&mut stdout, &id, json!({}));
            }
            "thread/start" => {
                let thread_id = format!("thread-{}", thread_counter.fetch_add(1, Ordering::SeqCst));
                send_response(
                    &mut stdout,
                    &id,
                    json!({
                        "thread_id": thread_id,
                    }),
                );
            }
            "turn/start" => {
                let turn_id = format!("turn-{}", turn_counter.fetch_add(1, Ordering::SeqCst));
                let thread_id = params
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        params
                            .get("thread")
                            .and_then(|value| value.get("id"))
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("thread-unknown");

                send_response(
                    &mut stdout,
                    &id,
                    json!({
                        "turn_id": turn_id,
                    }),
                );

                send_notification(
                    &mut stdout,
                    "thread/status/changed",
                    json!({"thread_id": thread_id, "status": "running"}),
                );

                send_notification(
                    &mut stdout,
                    "turn/started",
                    json!({"thread_id": thread_id, "turn_id": turn_id}),
                );

                send_notification(
                    &mut stdout,
                    "turn/completed",
                    json!({"thread_id": thread_id, "turn_id": turn_id}),
                );
            }
            "turn/steer" => {
                let thread_id = params
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .unwrap_or("thread-unknown");
                let expected_turn_id = params
                    .get("expected_turn_id")
                    .and_then(Value::as_str)
                    .unwrap_or("turn-unknown");
                let turn_id = format!("turn-{}", turn_counter.fetch_add(1, Ordering::SeqCst));

                send_response(
                    &mut stdout,
                    &id,
                    json!({"turn_id": turn_id, "expected_turn_id": expected_turn_id}),
                );

                send_notification(
                    &mut stdout,
                    "turn/started",
                    json!({"thread_id": thread_id, "turn_id": turn_id}),
                );

                send_notification(
                    &mut stdout,
                    "turn/completed",
                    json!({"thread_id": thread_id, "turn_id": turn_id}),
                );
            }
            "turn/interrupt" => {
                let thread_id = params
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .unwrap_or("thread-unknown");
                let turn_id = params
                    .get("turn_id")
                    .and_then(Value::as_str)
                    .unwrap_or("turn-unknown");

                send_response(&mut stdout, &id, json!({}));
                send_notification(
                    &mut stdout,
                    "turn/aborted",
                    json!({"thread_id": thread_id, "turn_id": turn_id}),
                );
            }
            _ => {
                send_error(
                    &mut stdout,
                    &id,
                    -32601,
                    format!("unsupported method: {method}"),
                );
            }
        }
    }
}

fn send_response(stdout: &mut dyn Write, id: &Value, result: Value) {
    send_message(
        stdout,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }),
    )
}

fn send_error(stdout: &mut dyn Write, id: &Value, code: i64, message: String) {
    send_message(
        stdout,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": code,
                "message": message,
            }
        }),
    )
}

fn send_notification(stdout: &mut dyn Write, method: &str, params: Value) {
    send_message(
        stdout,
        json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }),
    )
}

fn send_message(stdout: &mut dyn Write, payload: Value) {
    let mut line = payload.to_string();
    if !line.ends_with('\n') {
        line.push('\n');
    }
    if stdout.write_all(line.as_bytes()).is_err() {
        return;
    }
    let _ = stdout.flush();
}
