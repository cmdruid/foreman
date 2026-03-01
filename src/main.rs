mod config;
mod events;
mod foreman;
mod models;
mod project;
mod state;

use std::{
    fs,
    net::SocketAddr,
    path::{Path as StdPath, PathBuf},
    process::Command as StdCommand,
    sync::Arc,
};

use anyhow::{Context, anyhow};
use axum::http::Request;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderName, StatusCode},
    middleware::{Next, from_fn_with_state},
    response::IntoResponse,
    routing::{get, post},
};
use clap::Parser;
use serde::Deserialize;
use tokio::sync::broadcast;
use tracing::warn;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

use codex_api::AppServerClient;
use config::{CallbackProfile, RuntimeAuthConfig, ServiceConfig};
use foreman::Foreman;
use models::{
    CompactProjectRequest, CreateProjectJobsRequest, InterruptInput, SendAgentInput,
    SpawnAgentRequest, SpawnProjectRequest, SpawnProjectWorkerRequest, SteerAgentInput,
};
use project::PROJECT_CONFIG_FILE;
use state::PersistedState;

#[derive(Parser, Debug)]
#[command(author, version, about = "codex-foreman")]
struct Args {
    #[arg(long, default_value = "0.0.0.0:8787")]
    bind: String,
    #[arg(long, default_value = "codex")]
    codex_binary: String,
    #[arg(long)]
    config: Vec<String>,
    #[arg(long, value_name = "PATH")]
    init_project: Option<String>,
    #[arg(long)]
    init_project_overwrite: bool,
    #[arg(long)]
    init_project_manual: bool,
    #[arg(long, value_name = "PATH")]
    template_dir: Option<String>,
    #[arg(long, default_value = "/etc/codex-foreman/config.toml")]
    service_config: String,
    #[arg(long)]
    validate_config: bool,
    #[arg(long, default_value = "/var/lib/codex-foreman/foreman-state.json")]
    state_path: PathBuf,
}

#[derive(Clone)]
struct AppState {
    foreman: Arc<Foreman>,
    security: Option<RuntimeAuthConfig>,
    bind: String,
    codex_binary: String,
    state_path: String,
}

#[derive(Debug, serde::Serialize)]
struct ErrorBody {
    error: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let env_filter = tracing_subscriber::EnvFilter::from_default_env().add_directive(
        "codex_foreman=debug"
            .parse::<tracing_subscriber::filter::Directive>()
            .context("failed to parse built-in codex_foreman log directive")?,
    );

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(env_filter)
        .init();

    let args = Args::parse();

    if let Some(project_path) = args.init_project.as_deref() {
        let template_dir = resolve_template_dir(args.template_dir.as_deref())?;
        let path = StdPath::new(project_path);
        init_project(
            path,
            args.init_project_overwrite,
            args.init_project_manual,
            template_dir.as_path(),
        )?;
        return Ok(());
    }

    let service_config = ServiceConfig::load(StdPath::new(&args.service_config))
        .with_context(|| format!("failed to load service config '{}'", args.service_config))?;
    verify_codex_binary_version(&args.codex_binary, service_config.expected_codex_version())?;

    let warnings = service_config.validate()?;
    if args.validate_config {
        println!(
            "codex-foreman config validated: {} callback profile(s) loaded",
            service_config.callbacks.profiles.len()
        );
        for (name, profile) in &service_config.callbacks.profiles {
            match profile {
                CallbackProfile::Webhook(webhook) => {
                    println!(
                        " - {}: webhook -> {} (events: {:?})",
                        name, webhook.url, webhook.events
                    );
                }
                CallbackProfile::Command(command) => {
                    println!(
                        " - {}: command -> {} {} (events: {:?})",
                        name,
                        command.program,
                        if command.args.is_empty() {
                            String::new()
                        } else {
                            format!("args:{:?}", command.args)
                        },
                        command.events
                    );
                }
            }
        }

        if !warnings.is_empty() {
            for warning in warnings {
                println!("warning: {warning}");
            }
        } else {
            println!("warning: none");
        }

        return Ok(());
    }

    let persisted_state = PersistedState::load(&args.state_path)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(%err, "failed to load persisted state, starting with empty state");
            PersistedState::default()
        });

    let (event_tx, event_rx) = broadcast::channel(256);
    let security = service_config.resolve_auth_config()?;
    let (initialize_timeout_ms, request_timeout_ms) = service_config.app_server_timeouts();
    let client = AppServerClient::connect_with_timeouts(
        &args.codex_binary,
        &args.config,
        event_tx,
        initialize_timeout_ms,
        request_timeout_ms,
    )
    .await?;
    let foreman = Foreman::new(
        client,
        event_rx,
        service_config.clone(),
        persisted_state,
        args.state_path.clone(),
    );
    foreman
        .recover_state()
        .await
        .inspect_err(|err| tracing::warn!(%err, "recovery from persisted state failed"))?;
    let state = AppState {
        foreman,
        security,
        bind: args.bind.clone(),
        codex_binary: args.codex_binary.clone(),
        state_path: args.state_path.to_string_lossy().to_string(),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/agents", post(spawn_agent).get(list_agents))
        .route("/agents/{id}", get(get_agent).delete(close_agent))
        .route("/agents/{id}/result", get(get_agent_result))
        .route("/agents/{id}/wait", get(wait_agent_result))
        .route("/agents/{id}/events", get(get_agent_events))
        .route("/agents/{id}/send", post(send_turn))
        .route("/agents/{id}/steer", post(steer_agent))
        .route("/agents/{id}/interrupt", post(interrupt_agent))
        .route("/projects", post(create_project).get(list_projects))
        .route("/projects/{id}", get(get_project).delete(close_project))
        .route("/projects/{id}/workers", post(spawn_project_worker))
        .route(
            "/projects/{id}/foreman/send",
            post(send_project_foreman_turn),
        )
        .route("/projects/{id}/foreman/steer", post(steer_project_foreman))
        .route("/projects/{id}/compact", post(compact_project))
        .route("/projects/{id}/jobs", post(create_project_jobs))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}", get(get_job))
        .route("/jobs/{id}/result", get(get_job_result))
        .route("/jobs/{id}/wait", get(wait_job_result))
        .route("/status", get(get_status))
        .with_state(state.clone())
        .layer(from_fn_with_state(state.clone(), require_api_auth));

    let addr: SocketAddr = args
        .bind
        .parse()
        .context("failed to parse --bind address")?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!(%addr, "codex-foreman listening");
    axum::serve(listener, app).await?;

    Ok(())
}

fn extract_codex_version(text: &str) -> Option<String> {
    text.split_whitespace().find_map(|token| {
        let token = token.trim();
        if token.chars().all(|ch| ch.is_ascii_digit() || ch == '.') && !token.is_empty() {
            Some(token.to_string())
        } else {
            None
        }
    })
}

fn verify_codex_binary_version(
    codex_binary: &str,
    expected_version: Option<&str>,
) -> anyhow::Result<()> {
    let Some(expected_version) = expected_version else {
        return Ok(());
    };

    let output = StdCommand::new(codex_binary)
        .arg("--version")
        .output()
        .context("failed to execute codex binary for version check")?;
    if !output.status.success() {
        return Err(anyhow!(
            "codex --version failed with exit code: {}",
            output.status
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let detected = extract_codex_version(&stdout)
        .or_else(|| extract_codex_version(String::from_utf8_lossy(&output.stderr).as_ref()));
    match detected {
        Some(version) if version == expected_version => Ok(()),
        Some(version) => Err(anyhow!(
            "unexpected codex version: expected={expected_version}, detected={version}"
        )),
        None => Err(anyhow!(
            "failed to parse codex version from output: '{}'",
            stdout.trim()
        )),
    }
}

async fn health() -> &'static str {
    "ok"
}

async fn require_api_auth(
    State(app_state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> impl IntoResponse {
    let Some(auth) = app_state.security.as_ref() else {
        return next.run(request).await;
    };

    let path = request.uri().path();
    if auth
        .skip_paths
        .iter()
        .any(|skip_path| path == skip_path || path.starts_with(&format!("{skip_path}/")))
    {
        return next.run(request).await;
    }

    let header_name = match HeaderName::from_bytes(auth.header_name.as_bytes()) {
        Ok(name) => name,
        Err(err) => {
            warn!(%err, "invalid API auth header name");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                anyhow::anyhow!("invalid API auth configuration"),
            );
        }
    };

    let token = match request.headers().get(&header_name) {
        Some(header) => match header.to_str() {
            Ok(value) => value.to_string(),
            Err(_) => {
                return error_response(
                    StatusCode::UNAUTHORIZED,
                    anyhow::anyhow!("invalid API auth header"),
                );
            }
        },
        None => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                anyhow::anyhow!("missing API authentication header"),
            );
        }
    };

    let candidate = if let Some(scheme) = &auth.header_scheme {
        let expected = scheme.trim().to_lowercase();
        let Some((token_scheme, provided_token)) = token.split_once(' ') else {
            return error_response(
                StatusCode::UNAUTHORIZED,
                anyhow::anyhow!("invalid API auth header format"),
            );
        };
        if token_scheme.to_lowercase() != expected {
            return error_response(
                StatusCode::UNAUTHORIZED,
                anyhow::anyhow!("invalid API auth scheme"),
            );
        }
        provided_token
    } else {
        token.as_str()
    };

    if candidate != auth.token {
        return error_response(
            StatusCode::UNAUTHORIZED,
            anyhow::anyhow!("invalid API token"),
        );
    }

    next.run(request).await
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

async fn get_agent(State(state): State<AppState>, Path(agent_id): Path<Uuid>) -> impl IntoResponse {
    match state.foreman.get_agent(agent_id).await {
        Ok(resp) => Json(resp).into_response(),
        Err(err) => error_response(StatusCode::NOT_FOUND, err),
    }
}

#[derive(Debug, Deserialize)]
struct WaitAgentQuery {
    timeout_ms: Option<u64>,
    poll_ms: Option<u64>,
    include_events: Option<bool>,
}

async fn get_agent_result(
    State(state): State<AppState>,
    Path(agent_id): Path<Uuid>,
) -> impl IntoResponse {
    match state.foreman.get_agent_result(agent_id).await {
        Ok(resp) => Json(resp).into_response(),
        Err(err) => error_response(StatusCode::NOT_FOUND, err),
    }
}

async fn wait_agent_result(
    State(state): State<AppState>,
    Path(agent_id): Path<Uuid>,
    Query(query): Query<WaitAgentQuery>,
) -> impl IntoResponse {
    match state
        .foreman
        .wait_for_agent_result(
            agent_id,
            query.timeout_ms,
            query.poll_ms,
            query.include_events.unwrap_or(false),
        )
        .await
    {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(err) => error_response(StatusCode::NOT_FOUND, err),
    }
}

#[derive(Debug, Deserialize)]
struct AgentEventsQuery {
    tail: Option<usize>,
}

async fn get_agent_events(
    State(state): State<AppState>,
    Path(agent_id): Path<Uuid>,
    Query(query): Query<AgentEventsQuery>,
) -> impl IntoResponse {
    match state.foreman.get_agent_events(agent_id, query.tail).await {
        Ok(resp) => Json(resp).into_response(),
        Err(err) => error_response(StatusCode::NOT_FOUND, err),
    }
}

#[derive(Debug, serde::Serialize)]
struct StatusResponse {
    status: &'static str,
    foreman_pid: u32,
    bind: String,
    version: &'static str,
    codex_binary: String,
    state_path: String,
    app_server_pid: Option<u32>,
    callback_profiles: usize,
    default_callback_profile: Option<String>,
    agent_count: usize,
    project_count: usize,
    started_at: u64,
    uptime_seconds: u64,
}

async fn get_status(State(state): State<AppState>) -> impl IntoResponse {
    let (agent_count, project_count, started_at, uptime_seconds) =
        state.foreman.status_summary().await;
    let app_server_pid = state.foreman.app_server_pid().await;

    Json(StatusResponse {
        status: "ready",
        foreman_pid: std::process::id(),
        bind: state.bind,
        version: env!("CARGO_PKG_VERSION"),
        codex_binary: state.codex_binary,
        state_path: state.state_path,
        app_server_pid,
        callback_profiles: state.foreman.configured_callback_profile_count(),
        default_callback_profile: state.foreman.default_callback_profile(),
        agent_count,
        project_count,
        started_at,
        uptime_seconds,
    })
    .into_response()
}

async fn send_turn(
    State(state): State<AppState>,
    Path(agent_id): Path<Uuid>,
    Json(req): Json<SendAgentInput>,
) -> impl IntoResponse {
    match state.foreman.send_turn(agent_id, req).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(err) => error_response(StatusCode::BAD_REQUEST, err),
    }
}

async fn steer_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<Uuid>,
    Json(req): Json<SteerAgentInput>,
) -> impl IntoResponse {
    match state.foreman.steer(agent_id, req.prompt).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(err) => error_response(StatusCode::BAD_REQUEST, err),
    }
}

async fn interrupt_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<Uuid>,
    Json(req): Json<InterruptInput>,
) -> impl IntoResponse {
    match state.foreman.interrupt(agent_id, req.turn_id).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => error_response(StatusCode::BAD_REQUEST, err),
    }
    .into_response()
}

async fn close_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<Uuid>,
) -> impl IntoResponse {
    match state.foreman.close_agent(agent_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => error_response(StatusCode::BAD_REQUEST, err),
    }
    .into_response()
}

async fn list_projects(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.foreman.list_projects().await).into_response()
}

async fn create_project(
    State(state): State<AppState>,
    Json(request): Json<SpawnProjectRequest>,
) -> impl IntoResponse {
    match state.foreman.create_project(request).await {
        Ok(resp) => (StatusCode::CREATED, Json(resp)).into_response(),
        Err(err) => error_response(StatusCode::BAD_REQUEST, err),
    }
}

async fn get_project(
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
) -> impl IntoResponse {
    match state.foreman.get_project(project_id).await {
        Ok(resp) => Json(resp).into_response(),
        Err(err) => error_response(StatusCode::NOT_FOUND, err),
    }
}

async fn spawn_project_worker(
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
    Json(request): Json<SpawnProjectWorkerRequest>,
) -> impl IntoResponse {
    match state
        .foreman
        .spawn_project_worker(project_id, request)
        .await
    {
        Ok(resp) => (StatusCode::CREATED, Json(resp)).into_response(),
        Err(err) => error_response(StatusCode::BAD_REQUEST, err),
    }
}

async fn send_project_foreman_turn(
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
    Json(req): Json<SendAgentInput>,
) -> impl IntoResponse {
    match state.foreman.send_to_project_foreman(project_id, req).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(err) => error_response(StatusCode::BAD_REQUEST, err),
    }
}

async fn steer_project_foreman(
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
    Json(req): Json<SteerAgentInput>,
) -> impl IntoResponse {
    match state.foreman.steer_project_foreman(project_id, req).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(err) => error_response(StatusCode::BAD_REQUEST, err),
    }
}

async fn compact_project(
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
    Json(req): Json<CompactProjectRequest>,
) -> impl IntoResponse {
    match state.foreman.compact_project(project_id, req).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(err) => error_response(StatusCode::BAD_REQUEST, err),
    }
}

async fn close_project(
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
) -> impl IntoResponse {
    match state.foreman.close_project(project_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => error_response(StatusCode::BAD_REQUEST, err),
    }
    .into_response()
}

async fn create_project_jobs(
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
    Json(request): Json<CreateProjectJobsRequest>,
) -> impl IntoResponse {
    match state.foreman.create_project_jobs(project_id, request).await {
        Ok(resp) => (StatusCode::CREATED, Json(resp)).into_response(),
        Err(err) => error_response(StatusCode::BAD_REQUEST, err),
    }
}

async fn list_jobs(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.foreman.list_jobs().await).into_response()
}

async fn get_job(State(state): State<AppState>, Path(job_id): Path<Uuid>) -> impl IntoResponse {
    match state.foreman.get_job(job_id).await {
        Ok(resp) => Json(resp).into_response(),
        Err(err) => error_response(StatusCode::NOT_FOUND, err),
    }
}

async fn get_job_result(
    State(state): State<AppState>,
    Path(job_id): Path<Uuid>,
) -> impl IntoResponse {
    match state.foreman.get_job_result(job_id).await {
        Ok(resp) => Json(resp).into_response(),
        Err(err) => error_response(StatusCode::NOT_FOUND, err),
    }
}

#[derive(Debug, Deserialize)]
struct WaitJobQuery {
    timeout_ms: Option<u64>,
    poll_ms: Option<u64>,
    include_workers: Option<bool>,
}

async fn wait_job_result(
    State(state): State<AppState>,
    Path(job_id): Path<Uuid>,
    Query(query): Query<WaitJobQuery>,
) -> impl IntoResponse {
    match state
        .foreman
        .wait_for_job_result(
            job_id,
            query.timeout_ms,
            query.poll_ms,
            query.include_workers.unwrap_or(false),
        )
        .await
    {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(err) => error_response(StatusCode::NOT_FOUND, err),
    }
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

fn init_project(
    path: &StdPath,
    overwrite: bool,
    include_manual: bool,
    template_dir: &StdPath,
) -> anyhow::Result<()> {
    if path.exists() && !path.is_dir() {
        return Err(anyhow::anyhow!(
            "init path '{}' exists and is not a directory",
            path.display()
        ));
    }

    std::fs::create_dir_all(path)
        .with_context(|| format!("failed to create '{}'", path.display()))?;

    let mut created_files = Vec::new();

    write_template_file(
        path.join(PROJECT_CONFIG_FILE),
        &read_template_file(template_dir, "project.toml")?,
        overwrite,
    )?;
    created_files.push(path.join(PROJECT_CONFIG_FILE).display().to_string());

    write_template_file(
        path.join("FOREMAN.md"),
        &read_template_file(template_dir, "FOREMAN.md")?,
        overwrite,
    )?;
    created_files.push(path.join("FOREMAN.md").display().to_string());

    write_template_file(
        path.join("WORKER.md"),
        &read_template_file(template_dir, "WORKER.md")?,
        overwrite,
    )?;
    created_files.push(path.join("WORKER.md").display().to_string());

    write_template_file(
        path.join("RUNBOOK.md"),
        &read_template_file(template_dir, "RUNBOOK.md")?,
        overwrite,
    )?;
    created_files.push(path.join("RUNBOOK.md").display().to_string());

    write_template_file(
        path.join("HANDOFF.md"),
        &read_template_file(template_dir, "HANDOFF.md")?,
        overwrite,
    )?;
    created_files.push(path.join("HANDOFF.md").display().to_string());

    if include_manual {
        write_template_file(
            path.join("MANUAL.md"),
            &read_template_file(template_dir, "MANUAL.md")?,
            overwrite,
        )?;
        created_files.push(path.join("MANUAL.md").display().to_string());
    }

    println!(
        "Initialized codex-foreman project scaffold at: {}",
        path.display()
    );
    for file in created_files {
        println!("  - {}", file);
    }

    println!(
        "Use this path with POST /projects: {{\"path\":\"{}\"}}",
        path.to_string_lossy()
    );
    Ok(())
}

fn write_template_file(path: PathBuf, template: &str, overwrite: bool) -> anyhow::Result<()> {
    if path.exists() && !overwrite {
        return Err(anyhow::anyhow!(
            "file '{}' already exists, use --init-project-overwrite to replace",
            path.display()
        ));
    }

    fs::write(&path, template).with_context(|| format!("failed to write '{}'", path.display()))?;
    Ok(())
}

fn resolve_template_dir(cli_template_dir: Option<&str>) -> anyhow::Result<PathBuf> {
    let candidates = [
        cli_template_dir.map(PathBuf::from),
        std::env::var("CODEX_FOREMAN_TEMPLATE_DIR")
            .ok()
            .filter(|_env_dir| cli_template_dir.is_none())
            .map(PathBuf::from),
        Some(StdPath::new(env!("CARGO_MANIFEST_DIR")).join("templates")),
        std::env::current_exe()
            .ok()
            .and_then(|exe_dir| exe_dir.parent().map(|parent| parent.join("templates"))),
        Some("/usr/share/codex-foreman/templates".into()),
        Some("/etc/codex-foreman/templates".into()),
    ];

    let mut searched = Vec::new();
    for candidate in candidates.into_iter().flatten() {
        searched.push(candidate.clone());
        if candidate.exists() && candidate.is_dir() {
            return Ok(candidate);
        }
    }

    Err(anyhow::anyhow!(
        "template directory not found; searched: {}",
        searched
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn read_template_file(template_dir: &StdPath, filename: &str) -> anyhow::Result<String> {
    let path = template_dir.join(filename);
    fs::read_to_string(&path)
        .with_context(|| format!("failed to read template '{}'", path.display()))
}

#[cfg(test)]
mod tests {
    use super::resolve_template_dir;
    use std::{
        path::Path,
        sync::{Mutex, OnceLock},
    };

    static TEMPLATE_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn with_template_env_lock() -> std::sync::MutexGuard<'static, ()> {
        let lock = TEMPLATE_ENV_LOCK.get_or_init(|| Mutex::new(()));
        lock.lock().expect("failed to lock template env")
    }

    #[test]
    fn resolve_template_dir_prefers_cli_override() {
        let _guard = with_template_env_lock();
        let preferred = tempfile::tempdir().expect("temp dir");
        let fallback = tempfile::tempdir().expect("temp dir");
        let backup = std::env::var_os("CODEX_FOREMAN_TEMPLATE_DIR");

        let preferred_path = preferred.path().to_string_lossy().to_string();
        let fallback_path = fallback.path().to_path_buf();

        // ensure env is set to a different valid directory so precedence is testable.
        // SAFETY: Tests are single-threaded and manipulate process environment for assertion setup.
        unsafe {
            std::env::set_var("CODEX_FOREMAN_TEMPLATE_DIR", &fallback_path);
        }
        let selected = resolve_template_dir(Some(&preferred_path)).expect("template dir resolves");
        assert_eq!(selected, preferred.path());

        if let Some(old) = backup {
            // SAFETY: Tests are single-threaded and restore environment only for test scope.
            unsafe {
                std::env::set_var("CODEX_FOREMAN_TEMPLATE_DIR", old);
            }
        } else {
            // SAFETY: Tests are single-threaded and restore environment only for test scope.
            unsafe {
                std::env::remove_var("CODEX_FOREMAN_TEMPLATE_DIR");
            }
        }
    }

    #[test]
    fn resolve_template_dir_falls_back_to_manifest_templates() {
        let _guard = with_template_env_lock();
        let backup = std::env::var_os("CODEX_FOREMAN_TEMPLATE_DIR");
        // SAFETY: Tests are single-threaded and manage process environment temporarily.
        unsafe {
            std::env::remove_var("CODEX_FOREMAN_TEMPLATE_DIR");
        }

        let resolved = resolve_template_dir(None).expect("manifest fallback");
        let expected = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
        assert_eq!(resolved, expected);

        if let Some(old) = backup {
            // SAFETY: Tests are single-threaded and restore environment only for test scope.
            unsafe {
                std::env::set_var("CODEX_FOREMAN_TEMPLATE_DIR", old);
            }
        }
    }
}
