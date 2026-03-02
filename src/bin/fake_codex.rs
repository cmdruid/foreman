use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Write};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Real usage in foreman: `<binary> app-server ...`
    // The test shim requires the invocation to be `app-server` and ignores flags after it.
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
    let mut thread_stall_state = HashMap::new();

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
            "model/list" => {
                send_response(
                    &mut stdout,
                    &id,
                    json!({
                        "data": [
                            {"model": "gpt-5.3-codex-spark", "provider": "mock"},
                        ]
                    }),
                );
            }
            "thread/start" => {
                let thread_id = parse_string_param(&params, "thread")
                    .unwrap_or_else(|| format!("thread-{}", thread_counter.fetch_add(1, Ordering::SeqCst)));
                send_response(
                    &mut stdout,
                    &id,
                    json!({
                        "thread_id": thread_id,
                    }),
                );
                send_notification(
                    &mut stdout,
                    "thread/status/changed",
                    json!({"thread_id": thread_id, "status": "running"}),
                );
            }
            "turn/start" => {
                let turn_id = format!("turn-{}", turn_counter.fetch_add(1, Ordering::SeqCst));
                let thread_id = parse_string_param(&params, "thread")
                    .unwrap_or_else(|| "thread-unknown".to_string());
                let input = params.get("input").cloned().unwrap_or_else(|| json!([]));
                let text = extract_prompt_text_from_input(&input);
                let should_stall = if has_stall_marker(&text) && !thread_stall_state.contains_key(&thread_id)
                {
                    thread_stall_state.insert(thread_id.clone(), true);
                    true
                } else {
                    false
                };

                send_response(&mut stdout, &id, json!({"turn_id": turn_id}));

                send_notification(
                    &mut stdout,
                    "thread/status/changed",
                    json!({"thread_id": thread_id, "status": "running"}),
                );

                send_notification(
                    &mut stdout,
                    "turn/started",
                    json!({"thread_id": thread_id, "turn_id": turn_id, "status": "running"}),
                );

                send_notification(
                    &mut stdout,
                    "item/assistantMessage",
                    json!({
                        "thread_id": thread_id,
                        "turn_id": turn_id,
                        "text": "Worker acknowledged instructions",
                        "type": "agentMessage"
                    }),
                );

                if !should_stall {
                    send_notification(
                        &mut stdout,
                        "codex/event/exec_command_begin",
                        json!({"thread_id": thread_id, "turn_id": turn_id, "command": "mock-cmd"}),
                    );
                    send_notification(
                        &mut stdout,
                        "item/commandExecution",
                        json!({
                            "thread_id": thread_id,
                            "turn_id": turn_id,
                            "status": "start",
                            "command": "mock-cmd",
                            "type": "commandExecution",
                        }),
                    );
                    send_notification(
                        &mut stdout,
                        "item/assistantMessage/delta",
                        json!({
                            "thread_id": thread_id,
                            "turn_id": turn_id,
                            "text": "Mock command executed",
                            "type": "agentMessage"
                        }),
                    );
                    send_notification(
                        &mut stdout,
                        "codex/event/exec_command_end",
                        json!({"thread_id": thread_id, "turn_id": turn_id, "command": "mock-cmd"}),
                    );
                    send_notification(
                        &mut stdout,
                        "turn/completed",
                        json!({"thread_id": thread_id, "turn_id": turn_id, "status": "completed", "items": [{"type": "text", "text": "worker complete"}]}),
                    );
                }
            }
            "turn/steer" => {
                let thread_id = parse_string_param(&params, "thread")
                    .unwrap_or_else(|| "thread-unknown".to_string());
                let expected_turn_id = parse_string_param(&params, "expected_turn")
                    .or_else(|| parse_string_param(&params, "expectedTurnId"))
                    .unwrap_or_else(|| "turn-unknown".to_string());
                let turn_id = format!("turn-{}", turn_counter.fetch_add(1, Ordering::SeqCst));

                send_response(
                    &mut stdout,
                    &id,
                    json!({"turn_id": turn_id, "expected_turn_id": expected_turn_id}),
                );

                send_notification(
                    &mut stdout,
                    "turn/started",
                    json!({"thread_id": thread_id, "turn_id": turn_id, "status": "running"}),
                );

                send_notification(
                    &mut stdout,
                    "item/agentMessage",
                    json!({
                        "thread_id": thread_id,
                        "turn_id": turn_id,
                        "text": "Steering request accepted",
                        "type": "agentMessage"
                    }),
                );
                send_notification(
                    &mut stdout,
                    "turn/completed",
                    json!({"thread_id": thread_id, "turn_id": turn_id, "status": "completed", "items": [{"type": "text", "text": "steered"}]}),
                );
            }
            "turn/interrupt" => {
                let thread_id = parse_string_param(&params, "thread")
                    .unwrap_or_else(|| "thread-unknown".to_string());
                let turn_id = parse_string_param(&params, "turn")
                    .unwrap_or_else(|| "turn-unknown".to_string());
                thread_stall_state.remove(&thread_id);

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

fn parse_id_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn parse_string_param(params: &Value, stem: &str) -> Option<String> {
    let candidates: &[&str] = match stem {
        "thread" | "thread_id" => &["threadId", "thread_id"],
        "turn" | "turn_id" => &["turnId", "turn_id"],
        "expected_turn" => &["expectedTurnId", "expected_turn_id", "expectedTurn", "expected_turn"],
        "expectedTurnId" => &["expectedTurnId", "expected_turn_id", "expectedTurn", "expected_turn"],
        _ => &[stem],
    };

    for field in candidates {
        if let Some(value) = params.get(*field).and_then(parse_id_value) {
            return Some(value);
        }
    }

    None
}

fn extract_prompt_text_from_input(input: &Value) -> String {
    let mut parts = Vec::new();
    let Some(values) = input.as_array() else {
        return String::new();
    };

    for value in values {
        if let Some(text) = value.get("text").and_then(Value::as_str) {
            parts.push(text.to_string());
        } else if let Some(content) = value.get("content").and_then(Value::as_array) {
            for segment in content {
                if let Some(text) = segment.get("text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                }
            }
        }
    }

    parts.join(" ")
}

fn has_stall_marker(text: &str) -> bool {
    text.contains("STALL_ONCE_THEN_COMPLETE")
}
