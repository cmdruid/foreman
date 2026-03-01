mod app_server_client;
mod foreman;
mod models;
mod protocol;

use std::{net::SocketAddr, sync::Arc};

use anyhow::Context;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
};
use clap::Parser;
use tokio::sync::broadcast;
use tracing::Level;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

use app_server_client::AppServerClient;
use foreman::Foreman;
use models::{
    InterruptInput, SendAgentInput, SpawnAgentRequest, SpawnAgentResponse, SteerAgentInput,
};

#[derive(Parser, Debug)]
#[command(author, version, about = "codex-foreman")]
struct Args {
    #[arg(long, default_value = "0.0.0.0:8787")]
    bind: String,
    #[arg(long, default_value = "codex")]
    codex_binary: String,
    #[arg(long)]
    config: Vec<String>,
}

#[derive(Clone)]
struct AppState {
    foreman: Arc<Foreman>,
}

#[derive(Debug, serde::Serialize)]
struct ErrorBody {
    error: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(
            tracing_subscriber::EnvFilter::from_default_env().add_directive(
                "codex_foreman=debug"
                    .parse::<tracing::metadata::LevelFilter>()
                    .unwrap_or_else(|_| Level::INFO.into()),
            ),
        )
        .init();

    let args = Args::parse();

    let (event_tx, event_rx) = broadcast::channel(256);
    let client = AppServerClient::connect(&args.codex_binary, &args.config, event_tx).await?;
    let foreman = Foreman::new(client, event_rx).await;
    let state = AppState { foreman };

    let app = Router::new()
        .route("/health", get(health))
        .route("/agents", post(spawn_agent).get(list_agents))
        .route("/agents/:id", get(get_agent))
        .route("/agents/:id/send", post(send_turn))
        .route("/agents/:id/steer", post(steer_agent))
        .route("/agents/:id/interrupt", post(interrupt_agent))
        .route("/agents/:id", delete(close_agent))
        .with_state(state);

    let addr: SocketAddr = args
        .bind
        .parse()
        .context("failed to parse --bind address")?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!(%addr, "codex-foreman listening");
    axum::serve(listener, app).await?;

    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

async fn spawn_agent(
    State(state): State<AppState>,
    Json(request): Json<SpawnAgentRequest>,
) -> impl IntoResponse {
    match state.foreman.spawn_agent(request).await {
        Ok(resp) => (StatusCode::CREATED, Json(resp)).into_response(),
        Err(err) => error_response(StatusCode::BAD_REQUEST, err),
    }
}

async fn list_agents(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.foreman.list_agents().await).into_response()
}

async fn get_agent(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match parse_agent_id(&id) {
        Some(agent_id) => match state.foreman.get_agent(agent_id).await {
            Ok(resp) => Json(resp).into_response(),
            Err(err) => error_response(StatusCode::NOT_FOUND, err),
        },
        None => error_response(StatusCode::BAD_REQUEST, anyhow::anyhow!("invalid agent id")),
    }
}

async fn send_turn(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendAgentInput>,
) -> impl IntoResponse {
    match parse_agent_id(&id) {
        Some(agent_id) => match state.foreman.send_turn(agent_id, req).await {
            Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
            Err(err) => error_response(StatusCode::BAD_REQUEST, err),
        },
        None => error_response(StatusCode::BAD_REQUEST, anyhow::anyhow!("invalid agent id")),
    }
}

async fn steer_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SteerAgentInput>,
) -> impl IntoResponse {
    match parse_agent_id(&id) {
        Some(agent_id) => match state.foreman.steer(agent_id, req.prompt).await {
            Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
            Err(err) => error_response(StatusCode::BAD_REQUEST, err),
        },
        None => error_response(StatusCode::BAD_REQUEST, anyhow::anyhow!("invalid agent id")),
    }
}

async fn interrupt_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<InterruptInput>,
) -> impl IntoResponse {
    match parse_agent_id(&id) {
        Some(agent_id) => match state.foreman.interrupt(agent_id, req.turn_id).await {
            Ok(()) => StatusCode::OK,
            Err(err) => error_response(StatusCode::BAD_REQUEST, err),
        }
        .into_response(),
        None => error_response(StatusCode::BAD_REQUEST, anyhow::anyhow!("invalid agent id")),
    }
}

async fn close_agent(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match parse_agent_id(&id) {
        Some(agent_id) => match state.foreman.close_agent(agent_id).await {
            Ok(()) => StatusCode::NO_CONTENT,
            Err(err) => error_response(StatusCode::BAD_REQUEST, err),
        }
        .into_response(),
        None => error_response(StatusCode::BAD_REQUEST, anyhow::anyhow!("invalid agent id")),
    }
}

fn parse_agent_id(id: &str) -> Option<Uuid> {
    Uuid::parse_str(id).ok()
}

fn error_response(code: StatusCode, err: anyhow::Error) -> axum::response::Response {
    (
        code,
        Json(ErrorBody {
            error: err.to_string(),
        }),
    )
        .into_response()
}
