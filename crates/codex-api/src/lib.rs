//! `codex-api` is the low-level JSON-RPC client boundary for the local `codex` app-server.
//!
//! This crate owns wire-format protocol types and the transport-level `AppServerClient`
//! used by `codex-foreman`.
//!
//! The supported runtime methods are intentionally narrow:
//! - `initialize`
//! - `thread/start`
//! - `thread/interrupt`
//! - `model/list`
//! - `turn/start`
//! - `turn/steer`
//! - `turn/interrupt`
//!
//! Notifications are exposed through `RawNotification`, while server-initiated JSON-RPC
//! requests are auto-approved only for the known interactive prompts listed below.
//! Any other server request receives a JSON-RPC `-32601` error:
//!
//! - `item/commandExecution/requestApproval`
//! - `item/fileChange/requestApproval`
//! - `item/requestUserInput`
//! - `item/tool/requestUserInput`
//! - `item/tool/call`
pub mod client;
pub mod protocol;

pub use client::{
    AppServerClient, EmptyResponse, RawNotification, TextPayload, ThreadStartRequest,
    ModelListEntry, ModelListRequest, ModelListResponse, ThreadInterruptRequest, ThreadStartResponse,
    TurnInterruptRequest, TurnStartRequest, TurnStartResponse, TurnSteerRequest, TurnSteerResponse,
};
pub use protocol::{
    JSONRPCError, JSONRPCErrorError, JSONRPCMessage, JSONRPCNotification, JSONRPCRequest,
    JSONRPCResponse, RequestId, parse_thread_id, parse_turn_id,
};
