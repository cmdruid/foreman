# codex-foreman

A minimal Rust control-plane for orchestrating multiple long-lived Codex `app-server` agents from a single process.

`codex-foreman` exposes a tiny HTTP API to:
- spawn and track agents (thread-backed)
- send turns and steering commands
- interrupt running turns
- receive callback events
- close/remove agent entries

## Build

```bash
cd ~/repos/codex-foreman
cargo build
```

## Run

```bash
cargo run -- --bind 0.0.0.0:8787 --codex-binary /usr/local/bin/codex
```

By default it assumes `codex` is on `PATH`.

## API (MVP)

- `GET /health`
- `POST /agents` → spawn agent
- `GET /agents` → list agents
- `GET /agents/:id` → get agent state
- `POST /agents/:id/send` → start a new turn
- `POST /agents/:id/steer` → steer active turn
- `POST /agents/:id/interrupt` → interrupt a turn
- `DELETE /agents/:id` → remove local tracking for an agent

### Webhook payload

`callback_url` can be set on spawn and send.

Codex events are posted as:

```json
{
  "agentId": "uuid",
  "ts": 1710000000,
  "method": "turn/completed",
  "params": {"threadId":"...", "turnId":"..."}
}
```

## Design Notes

- `codex-foreman` is the control-plane runtime; it launches one `codex app-server` process and multiplexes JSON-RPC messages.
- Agents are keyed by local UUID and mapped to codex `threadId`s.
- Notifications from Codex are also retained as recent event history and forwarded to the callback URL when configured.

This is intentionally minimal. For production, add persistence, auth, idempotency, retries and stronger request validation.
