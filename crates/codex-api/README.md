# `codex-api`

`codex-api` is the protocol boundary crate used by `foreman` to talk to the
local `codex` app-server process.

## Scope

- owns JSON-RPC client transport for the `app-server` subprocess
- serializes outgoing JSON-RPC requests and notifications
- parses inbound JSON-RPC responses and notifications
- auto-approves known interactive prompts required by the tool runtime

## Supported RPC methods

`foreman` intentionally relies only on this small set for production control
flow:

- `initialize`
- `thread/start`
- `turn/start`
- `turn/steer`
- `turn/interrupt`

### Notifications forwarded to `foreman`

All inbound notifications are parsed and exposed to the caller through the
`RawNotification` channel with:

- `thread_id`
- parsed `turn_id` helpers for lifecycle status and event correlation where present

## Auto-approved server requests

When the server sends the following requests, `foreman` responds automatically:

- `item/commandExecution/requestApproval`
- `item/fileChange/requestApproval`
- `item/requestUserInput`
- `item/tool/requestUserInput`
- `item/tool/call` (returns `{ "success": true, "content_items": [] }`)

All other server-initiated requests are answered with JSON-RPC `-32601` errors.

## Error behavior

- connection handshake requires successful `initialize` within configured timeout
- any startup/connect failure is retried with bounded backoff
- request timeouts are propagated as `app-server request timed out after {ms}ms`
- malformed server responses and closed pipes fail outstanding in-flight requests

## Quick example

```rust
use codex_api::AppServerClient;
use tokio::sync::broadcast;

let (event_tx, _event_rx) = broadcast::channel(16);
let client = AppServerClient::connect("codex", &[], event_tx).await?;
let thread = client.request::<_, serde_json::Value>(
    "thread/start",
    &serde_json::json!({ "cwd": null, "model": null, "model_provider": null, "sandbox": null })
).await?;
```

## Smoke test

Run the live app-server smoke test from this crate:

```bash
CODEX_BIN=/usr/local/bin/codex \\
CODEX_API_SMOKE_TEST=1 \\
cargo test -p codex-api --test smoke -- --ignored codex_api_smoke_app_server_creates_real_file
```

Expected result:

- App-server launches successfully
- `thread/start` then `turn/start` are exercised
- the temporary workspace receives `.audit-generated/smoke-app-server.txt` with test content
- command/turn completion events are observed
