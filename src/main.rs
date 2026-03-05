mod config;
mod constants;
mod events;
mod foreman;
mod models;
mod project;
mod state;

use std::{
    convert::Infallible,
    env, fs,
    fs::OpenOptions,
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
    http::{HeaderMap, HeaderName, StatusCode},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Sse, sse::Event},
    routing::{get, post},
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::{StreamExt, wrappers::BroadcastStream};
use tracing::warn;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

use codex_api::AppServerClient;
use config::{CallbackProfile, RuntimeAuthConfig, ServiceConfig};
use constants as consts;
use foreman::Foreman;
use models::{
    CompactProjectRequest, CreateProjectJobsRequest, InterruptInput, SendAgentInput,
    SpawnAgentRequest, SpawnProjectRequest, SpawnProjectWorkerRequest, SteerAgentInput,
};
use project::{ProjectConfig, ProjectLintIssue, lint_project_toml};
use state::PersistedState;

#[derive(Parser, Debug)]
#[command(name = consts::APP_NAME, author, version, about = consts::APP_DESCRIPTION)]
struct Args {
    #[arg(long)]
    socket_path: Option<String>,
    #[arg(long, default_value_t = consts::DEFAULT_CODEX_BINARY.to_string())]
    codex_binary: String,
    #[arg(long)]
    config: Vec<String>,
    #[arg(long, value_name = "PATH")]
    init_project: Option<String>,
    #[arg(long)]
    init_project_overwrite: bool,
    #[arg(long)]
    with_manual: bool,
    #[arg(long, value_name = "PATH")]
    template_dir: Option<String>,
    #[arg(long = "project", value_name = "PATH")]
    project: Option<PathBuf>,
    #[arg(long, default_value_t = default_service_config_path())]
    service_config: String,
    #[arg(long)]
    validate_config: bool,
    #[arg(long)]
    project_lint: bool,
    #[arg(long)]
    fix: bool,
    #[arg(long)]
    doctor: bool,
    #[arg(long)]
    config_show_resolved: bool,
    #[arg(long)]
    state_path: Option<PathBuf>,
    #[arg(long)]
    force: bool,
}

fn default_service_config_path() -> String {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| {
            home.join(consts::FOREMAN_RUNTIME_DIR)
                .join(consts::DEFAULT_SERVICE_CONFIG_FILENAME)
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(|| consts::DEFAULT_SERVICE_CONFIG_PATH.to_string())
}

#[derive(Clone)]
struct AppState {
    foreman: Arc<Foreman>,
    security: Option<RuntimeAuthConfig>,
    socket_path: String,
    codex_binary: String,
    state_path: String,
}

#[derive(Debug, serde::Serialize)]
struct ErrorBody {
    error: String,
}

#[derive(Debug)]
struct PidLockGuard {
    path: PathBuf,
    pid: u32,
}

impl Drop for PidLockGuard {
    fn drop(&mut self) {
        if fs::read_to_string(&self.path)
            .ok()
            .and_then(|contents| contents.trim().parse::<u32>().ok())
            .is_some_and(|pid| pid == self.pid)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("foreman: error: {err}");
        if let Some(hint) = error_hint(&err) {
            eprintln!("foreman: hint: {hint}");
        }
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let env_filter = tracing_subscriber::EnvFilter::from_default_env().add_directive(
        consts::DEFAULT_LOG_FILTER
            .parse::<tracing_subscriber::filter::Directive>()
            .context("failed to parse built-in foreman log directive")?,
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
            args.with_manual,
            template_dir.as_path(),
        )?;
        return Ok(());
    }

    let service_config = ServiceConfig::load(StdPath::new(&args.service_config))
        .with_context(|| format!("failed to load service config '{}'", args.service_config))?;

    let warnings = service_config.validate()?;
    if args.validate_config {
        println!(
            "foreman config validated: {} callback profile(s) loaded",
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
                CallbackProfile::UnixSocket(unix_socket) => {
                    println!(
                        " - {}: unix-socket -> {} (events: {:?})",
                        name,
                        unix_socket.socket.display(),
                        unix_socket.events
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

    if args.project_lint {
        let project_toml_path = resolve_required_project_toml_path(args.project.as_ref())?;
        run_project_lint_command(
            &project_toml_path,
            &service_config,
            args.fix,
            project_toml_path
                .parent()
                .unwrap_or_else(|| StdPath::new(".")),
        )?;
        return Ok(());
    }

    if args.doctor {
        let project_toml_path = resolve_required_project_toml_path(args.project.as_ref())?;
        run_doctor_command(&args, &service_config, &project_toml_path)?;
        return Ok(());
    }

    if args.config_show_resolved {
        let project_toml_path = resolve_required_project_toml_path(args.project.as_ref())?;
        run_config_show_resolved(&args, &service_config, &project_toml_path)?;
        return Ok(());
    }

    verify_codex_binary_version(&args.codex_binary, service_config.expected_codex_version())?;

    let project_path = if args.init_project.is_none()
        && !args.validate_config
        && !args.project_lint
        && !args.doctor
        && !args.config_show_resolved
    {
        resolve_required_project_toml_path(args.project.as_ref())?
    } else {
        PathBuf::new()
    };

    let project_state_path = args.state_path.unwrap_or_else(|| {
        let project_directory = project_path.parent().unwrap_or_else(|| StdPath::new("."));
        project_directory
            .join(consts::FOREMAN_RUNTIME_DIR)
            .join(consts::DEFAULT_STATE_FILENAME)
    });

    let persisted_state = PersistedState::load(&project_state_path)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(%err, "failed to load persisted state, starting with empty state");
            PersistedState::default()
        });
    let socket_path = resolve_socket_path(&args.socket_path, &project_state_path)?;
    let _pid_lock = acquire_pid_lockfile(&socket_path, args.force)?;
    cleanup_socket_path(&socket_path)?;

    let (event_tx, event_rx) = broadcast::channel(consts::BROADCAST_CHANNEL_CAPACITY);
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
        project_state_path.clone(),
    );
    foreman
        .recover_state()
        .await
        .inspect_err(|err| tracing::warn!(%err, "recovery from persisted state failed"))?;
    let state = AppState {
        foreman,
        security,
        socket_path: socket_path.to_string_lossy().to_string(),
        codex_binary: args.codex_binary.clone(),
        state_path: project_state_path.to_string_lossy().to_string(),
    };

    #[cfg(not(unix))]
    return Err(anyhow::anyhow!(
        "foreman now requires unix sockets, which are unsupported on this platform"
    ));

    #[cfg(unix)]
    {
        let app = Router::new()
            .route(consts::HEALTH_ROUTE, get(health))
            .route(consts::ROUTE_AGENTS, post(spawn_agent).get(list_agents))
            .route(consts::ROUTE_AGENT_ID, get(get_agent).delete(close_agent))
            .route(consts::ROUTE_AGENT_RESULT, get(get_agent_result))
            .route(consts::ROUTE_AGENT_WAIT, get(wait_agent_result))
            .route(consts::ROUTE_AGENT_EVENTS, get(get_agent_events))
            .route(consts::ROUTE_AGENT_LOGS, get(get_agent_logs))
            .route(consts::ROUTE_AGENT_SEND, post(send_turn))
            .route(consts::ROUTE_AGENT_STEER, post(steer_agent))
            .route(consts::ROUTE_AGENT_INTERRUPT, post(interrupt_agent))
            .route(
                consts::ROUTE_PROJECTS,
                post(create_project).get(list_projects),
            )
            .route(
                consts::ROUTE_PROJECT_ID,
                get(get_project).delete(close_project),
            )
            .route(
                consts::ROUTE_PROJECT_CALLBACK_STATUS,
                get(get_project_callback_status),
            )
            .route(consts::ROUTE_PROJECT_WORKERS, post(spawn_project_worker))
            .route(
                consts::ROUTE_PROJECT_FOREMAN_SEND,
                post(send_project_foreman_turn),
            )
            .route(
                consts::ROUTE_PROJECT_FOREMAN_STEER,
                post(steer_project_foreman),
            )
            .route(consts::ROUTE_PROJECT_COMPACT, post(compact_project))
            .route(consts::ROUTE_PROJECT_JOBS, post(create_project_jobs))
            .route(consts::ROUTE_JOBS, get(list_jobs))
            .route(consts::ROUTE_JOB_ID, get(get_job))
            .route(consts::ROUTE_JOB_RESULT, get(get_job_result))
            .route(consts::ROUTE_JOB_WAIT, get(wait_job_result))
            .route(consts::ROUTE_JOB_EVENTS, get(get_job_events_stream))
            .route(consts::STATUS_ROUTE, get(get_status))
            .with_state(state.clone())
            .layer(from_fn_with_state(state.clone(), require_api_auth));

        let listener = tokio::net::UnixListener::bind(&socket_path)
            .with_context(|| format!("failed to bind unix socket '{}'", socket_path.display()))?;
        set_socket_permissions(&socket_path)?;
        tracing::info!(socket = ?socket_path, "foreman listening on Unix socket");
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let ctrl_c = async {
                    tokio::signal::ctrl_c()
                        .await
                        .expect("failed to install ctrl_c handler");
                };

                let terminate = async {
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("failed to install SIGTERM handler")
                        .recv()
                        .await;
                };

                tokio::select! {
                    _ = ctrl_c => {},
                    _ = terminate => {},
                }
                tracing::info!("shutdown signal received");
            })
            .await?;
        state.foreman.reconcile_agent_statuses().await;
        state
            .foreman
            .persist_state_now()
            .await
            .context("failed to persist reconciled foreman state during shutdown")?;
    }
    Ok(())
}

fn resolve_socket_path(
    socket_path: &Option<String>,
    state_path: &StdPath,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = socket_path {
        return Ok(StdPath::new(path).to_path_buf());
    }

    let state_dir = state_path
        .parent()
        .unwrap_or_else(|| StdPath::new(consts::DEFAULT_WORKING_DIRECTORY));
    if !state_dir.exists() {
        fs::create_dir_all(state_dir).with_context(|| {
            format!(
                "create state directory '{}' for derived socket path",
                state_dir.display()
            )
        })?;
    }

    Ok(state_dir.join(consts::FOREMAN_SOCKET_FILENAME))
}

fn error_hint(err: &anyhow::Error) -> Option<&'static str> {
    let text = format!("{err:#}").to_lowercase();
    if text.contains("address already in use") {
        return Some("socket is already in use; remove the stale socket file or choose a different --socket-path");
    }
    if text.contains("active pid") {
        return Some("another foreman process is running; stop it first or retry with --force for crash recovery");
    }
    if text.contains("failed to bind") {
        return Some("failed to bind socket; check parent directory permissions and whether the path is writable");
    }
    None
}

#[cfg(unix)]
fn cleanup_socket_path(path: &StdPath) -> anyhow::Result<()> {
    use std::os::unix::fs::FileTypeExt;

    if path.exists() {
        let file_type = fs::symlink_metadata(path)
            .with_context(|| format!("read metadata for '{}'", path.display()))?
            .file_type();
        if !file_type.is_file() && !file_type.is_socket() {
            return Err(anyhow::anyhow!(
                "socket path '{}' exists and is not a regular file or socket",
                path.display()
            ));
        }
        fs::remove_file(path)
            .with_context(|| format!("remove existing socket path '{}'", path.display()))?;
    }

    Ok(())
}

#[cfg(not(unix))]
fn cleanup_socket_path(_path: &StdPath) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn acquire_pid_lockfile(socket_path: &StdPath, force: bool) -> anyhow::Result<PidLockGuard> {
    let lock_file_name = socket_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| format!("{name}.lock"))
        .unwrap_or_else(|| consts::FOREMAN_LOCK_FILENAME.to_string());
    let lock_path = socket_path
        .parent()
        .unwrap_or_else(|| StdPath::new(consts::DEFAULT_WORKING_DIRECTORY))
        .join(lock_file_name);

    let pid = std::process::id();
    let pid_contents = format!("{pid}\n");
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(pid_contents.as_bytes())
                .with_context(|| format!("write lockfile '{}'", lock_path.display()))?;
            Ok(PidLockGuard {
                path: lock_path,
                pid,
            })
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = fs::read_to_string(&lock_path)
                .with_context(|| format!("read lockfile '{}'", lock_path.display()))?;
            if let Ok(existing_pid) = existing.trim().parse::<u32>()
                && pid_is_alive(existing_pid)
                && !force
            {
                let message = format!(
                    "another foreman instance is running for socket '{}'; lockfile '{}' has active PID {}",
                    socket_path.display(),
                    lock_path.display(),
                    existing_pid
                );
                eprintln!("{message}");
                return Err(anyhow!("{message}"));
            }

            fs::write(&lock_path, pid_contents)
                .with_context(|| format!("overwrite stale lockfile '{}'", lock_path.display()))?;
            Ok(PidLockGuard {
                path: lock_path,
                pid,
            })
        }
        Err(err) => Err(err).with_context(|| format!("create lockfile '{}'", lock_path.display())),
    }
}

#[cfg(not(unix))]
fn acquire_pid_lockfile(_socket_path: &StdPath, _force: bool) -> anyhow::Result<PidLockGuard> {
    Ok(PidLockGuard {
        path: PathBuf::new(),
        pid: std::process::id(),
    })
}

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    StdPath::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn set_socket_permissions(path: &StdPath) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(
        path,
        fs::Permissions::from_mode(consts::FOREMAN_SOCKET_PERMISSIONS),
    )
    .with_context(|| format!("set socket permissions for '{}'", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_socket_permissions(_path: &StdPath) -> anyhow::Result<()> {
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

fn constant_time_token_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let max_len = left.len().max(right.len());
    let mut diff = left.len() ^ right.len();

    for idx in 0..max_len {
        let left_byte = *left.get(idx).unwrap_or(&0);
        let right_byte = *right.get(idx).unwrap_or(&0);
        diff |= (left_byte ^ right_byte) as usize;
    }

    diff == 0
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

fn coded_error(code: &'static str, message: impl AsRef<str>) -> anyhow::Error {
    anyhow!("[{code}] {}", message.as_ref())
}

fn resolve_required_project_toml_path(project_arg: Option<&PathBuf>) -> anyhow::Result<PathBuf> {
    let path = project_arg.ok_or_else(|| {
        coded_error(
            consts::ERROR_CODE_CFG_PROJECT_ARG_REQUIRED,
            "--project <path-to-project.toml> is required",
        )
    })?;

    if !path.exists() || !path.is_file() {
        return Err(coded_error(
            consts::ERROR_CODE_CFG_PROJECT_PATH_INVALID,
            format!("project path '{}' must point to a file", path.display()),
        ));
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if file_name != consts::PROJECT_CONFIG_FILE {
        return Err(coded_error(
            consts::ERROR_CODE_CFG_PROJECT_PATH_INVALID,
            format!(
                "project path '{}' must target '{}'",
                path.display(),
                consts::PROJECT_CONFIG_FILE
            ),
        ));
    }

    Ok(path.clone())
}

fn run_project_lint_command(
    project_toml_path: &StdPath,
    service_config: &ServiceConfig,
    fix: bool,
    project_root: &StdPath,
) -> anyhow::Result<()> {
    let mut report = lint_project_toml(project_toml_path, &service_config.callbacks.profiles, fix)?;
    let config = ProjectConfig::load(project_root)?;
    if let Err(err) = config.load_runtime_files(project_root) {
        report.issues.push(ProjectLintIssue {
            code: consts::ERROR_CODE_CFG_RUNTIME_IO,
            path: "prompts".to_string(),
            message: err.to_string(),
        });
    }

    if report.issues.is_empty() {
        println!(
            "project lint passed: {}",
            project_toml_path.to_string_lossy()
        );
        if fix {
            println!("autofix: no safe rewrites required");
        }
        return Ok(());
    }

    println!(
        "project lint found {} issue(s): {}",
        report.issues.len(),
        project_toml_path.to_string_lossy()
    );
    for issue in &report.issues {
        println!("[{}] {}: {}", issue.code, issue.path, issue.message);
    }

    Err(coded_error(
        consts::ERROR_CODE_CFG_PROJECT_VALIDATION,
        format!("project lint failed with {} issue(s)", report.issues.len()),
    ))
}

#[derive(Debug, Clone, Serialize)]
struct DoctorCheck {
    id: &'static str,
    status: &'static str,
    code: Option<&'static str>,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorReport {
    project: String,
    checks: Vec<DoctorCheck>,
}

fn run_doctor_command(
    args: &Args,
    service_config: &ServiceConfig,
    project_toml_path: &StdPath,
) -> anyhow::Result<()> {
    const PASS: &str = "pass";
    const WARN: &str = "warn";
    const FAIL: &str = "fail";

    let mut checks = Vec::new();
    let project_root = project_toml_path
        .parent()
        .unwrap_or_else(|| StdPath::new("."));

    let codex_check = StdCommand::new(&args.codex_binary)
        .arg("--version")
        .output();
    match codex_check {
        Ok(output) if output.status.success() => checks.push(DoctorCheck {
            id: "codex_binary",
            status: PASS,
            code: None,
            message: format!("'{} --version' succeeded", args.codex_binary),
        }),
        Ok(output) => checks.push(DoctorCheck {
            id: "codex_binary",
            status: FAIL,
            code: Some(consts::ERROR_CODE_CFG_RUNTIME_IO),
            message: format!(
                "'{} --version' failed with exit code {}",
                args.codex_binary, output.status
            ),
        }),
        Err(err) => checks.push(DoctorCheck {
            id: "codex_binary",
            status: FAIL,
            code: Some(consts::ERROR_CODE_CFG_RUNTIME_IO),
            message: format!("failed to execute '{} --version': {err}", args.codex_binary),
        }),
    }

    match service_config.validate() {
        Ok(warnings) => {
            checks.push(DoctorCheck {
                id: "service_config",
                status: PASS,
                code: None,
                message: "service config validated".to_string(),
            });
            for warning in warnings {
                checks.push(DoctorCheck {
                    id: "service_config_warning",
                    status: WARN,
                    code: Some(consts::ERROR_CODE_CFG_SERVICE_INVALID),
                    message: warning,
                });
            }
        }
        Err(err) => checks.push(DoctorCheck {
            id: "service_config",
            status: FAIL,
            code: Some(consts::ERROR_CODE_CFG_SERVICE_INVALID),
            message: err.to_string(),
        }),
    }

    match ProjectConfig::load(project_root).and_then(|config| config.validate().map(|_| config)) {
        Ok(config) => {
            checks.push(DoctorCheck {
                id: "project_config",
                status: PASS,
                code: None,
                message: "project config validated".to_string(),
            });

            match config.load_runtime_files(project_root) {
                Ok(_) => checks.push(DoctorCheck {
                    id: "project_prompts",
                    status: PASS,
                    code: None,
                    message: "all prompt files are readable and non-empty".to_string(),
                }),
                Err(err) => checks.push(DoctorCheck {
                    id: "project_prompts",
                    status: FAIL,
                    code: Some(consts::ERROR_CODE_CFG_RUNTIME_IO),
                    message: err.to_string(),
                }),
            }

            for (profile_name, profile) in &config.callbacks.profiles {
                if let CallbackProfile::Command(command) = profile {
                    let path = StdPath::new(&command.program);
                    if !path.exists() {
                        checks.push(DoctorCheck {
                            id: "project_callback_command",
                            status: FAIL,
                            code: Some(consts::ERROR_CODE_CB_COMMAND_INVALID),
                            message: format!(
                                "project callback profile '{profile_name}' command '{}' does not exist",
                                command.program
                            ),
                        });
                    }
                }
            }
        }
        Err(err) => checks.push(DoctorCheck {
            id: "project_config",
            status: FAIL,
            code: Some(consts::ERROR_CODE_CFG_PROJECT_VALIDATION),
            message: err.to_string(),
        }),
    }

    let runtime_dir = project_root.join(consts::FOREMAN_RUNTIME_DIR);
    match fs::create_dir_all(&runtime_dir) {
        Ok(_) => {
            let probe = runtime_dir.join(".doctor-write-probe");
            match fs::write(&probe, "ok") {
                Ok(_) => {
                    let _ = fs::remove_file(&probe);
                    checks.push(DoctorCheck {
                        id: "runtime_dir",
                        status: PASS,
                        code: None,
                        message: format!(
                            "runtime directory is writable: {}",
                            runtime_dir.display()
                        ),
                    });
                }
                Err(err) => checks.push(DoctorCheck {
                    id: "runtime_dir",
                    status: FAIL,
                    code: Some(consts::ERROR_CODE_CFG_RUNTIME_IO),
                    message: format!(
                        "runtime directory is not writable '{}': {err}",
                        runtime_dir.display()
                    ),
                }),
            }
        }
        Err(err) => checks.push(DoctorCheck {
            id: "runtime_dir",
            status: FAIL,
            code: Some(consts::ERROR_CODE_CFG_RUNTIME_IO),
            message: format!(
                "failed to create runtime directory '{}': {err}",
                runtime_dir.display()
            ),
        }),
    }

    let git_dir = project_root.join(".git");
    if git_dir.exists() {
        checks.push(DoctorCheck {
            id: "git_access",
            status: PASS,
            code: None,
            message: format!("git metadata present: {}", git_dir.display()),
        });
    } else {
        checks.push(DoctorCheck {
            id: "git_access",
            status: WARN,
            code: Some(consts::ERROR_CODE_CFG_RUNTIME_IO),
            message: "no .git directory found (worktree operations may fail)".to_string(),
        });
    }

    for (profile_name, profile) in &service_config.callbacks.profiles {
        if let CallbackProfile::Command(command) = profile {
            let command_path = StdPath::new(&command.program);
            if command_path.exists() {
                checks.push(DoctorCheck {
                    id: "service_callback_command",
                    status: PASS,
                    code: None,
                    message: format!(
                        "service callback profile '{profile_name}' command exists: {}",
                        command.program
                    ),
                });
            } else {
                checks.push(DoctorCheck {
                    id: "service_callback_command",
                    status: FAIL,
                    code: Some(consts::ERROR_CODE_CB_COMMAND_INVALID),
                    message: format!(
                        "service callback profile '{profile_name}' command '{}' does not exist",
                        command.program
                    ),
                });
            }
        }
    }

    let report = DoctorReport {
        project: project_toml_path.to_string_lossy().to_string(),
        checks,
    };
    let failures = report
        .checks
        .iter()
        .filter(|check| check.status == FAIL)
        .count();
    let warnings = report
        .checks
        .iter()
        .filter(|check| check.status == WARN)
        .count();

    println!(
        "{}",
        serde_json::to_string_pretty(&report).context("serialize doctor report")?
    );
    println!("doctor summary: failures={failures} warnings={warnings}");

    if failures > 0 {
        return Err(coded_error(
            consts::ERROR_CODE_WK_DOCTOR_FAILED,
            format!("doctor found {failures} failure(s)"),
        ));
    }

    Ok(())
}

#[derive(Debug, Clone, Serialize)]
struct ResolvedConfigOutput {
    precedence: Vec<&'static str>,
    cli: ResolvedCliOutput,
    service: ServiceConfig,
    project: ProjectConfig,
    effective_callback_profiles: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ResolvedCliOutput {
    project_toml: String,
    service_config: String,
    codex_binary: String,
    socket_path: Option<String>,
    state_path: Option<String>,
}

fn run_config_show_resolved(
    args: &Args,
    service_config: &ServiceConfig,
    project_toml_path: &StdPath,
) -> anyhow::Result<()> {
    let project_root = project_toml_path
        .parent()
        .unwrap_or_else(|| StdPath::new("."));
    let project_config = ProjectConfig::load(project_root)?;
    project_config.validate()?;

    let mut effective_profile_names: Vec<String> =
        service_config.callbacks.profiles.keys().cloned().collect();
    for profile_name in project_config.callbacks.profiles.keys() {
        if !effective_profile_names
            .iter()
            .any(|name| name == profile_name)
        {
            effective_profile_names.push(profile_name.clone());
        }
    }
    effective_profile_names.sort_unstable();

    let resolved = ResolvedConfigOutput {
        precedence: vec![
            "project.toml overrides service callback profiles/settings when names overlap",
            "service config provides global defaults and fallback callback profiles",
            "CLI flags override runtime paths and process settings",
        ],
        cli: ResolvedCliOutput {
            project_toml: project_toml_path.to_string_lossy().to_string(),
            service_config: args.service_config.clone(),
            codex_binary: args.codex_binary.clone(),
            socket_path: args.socket_path.clone(),
            state_path: args
                .state_path
                .as_ref()
                .map(|path| path.to_string_lossy().to_string()),
        },
        service: service_config.clone(),
        project: project_config,
        effective_callback_profiles: effective_profile_names,
    };

    println!(
        "{}",
        toml::to_string_pretty(&resolved).context("serialize resolved config")?
    );
    Ok(())
}

async fn health() -> &'static str {
    consts::STATUS_OK
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

    if !constant_time_token_eq(candidate, &auth.token) {
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

#[derive(Debug, Deserialize)]
struct AgentLogsQuery {
    tail: Option<usize>,
    turn: Option<u32>,
}

async fn get_agent_logs(
    State(state): State<AppState>,
    Path(agent_id): Path<Uuid>,
    Query(query): Query<AgentLogsQuery>,
) -> impl IntoResponse {
    match state
        .foreman
        .get_agent_logs(agent_id, query.tail, query.turn)
        .await
    {
        Ok(resp) => Json(resp).into_response(),
        Err(err) => error_response(StatusCode::NOT_FOUND, err),
    }
}

#[derive(Debug, serde::Serialize)]
struct StatusResponse {
    status: &'static str,
    foreman_pid: u32,
    socket_path: String,
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
        status: consts::FOREMAN_STATUS_READY,
        foreman_pid: std::process::id(),
        socket_path: state.socket_path,
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

async fn get_project_callback_status(
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
) -> impl IntoResponse {
    match state.foreman.get_project_callback_status(project_id).await {
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

fn parse_last_event_id(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

fn sse_event(method: &str, ts: u64, payload: serde_json::Value) -> Event {
    let data = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    Event::default().event(method).id(ts.to_string()).data(data)
}

async fn get_job_events_stream(
    State(state): State<AppState>,
    Path(job_id): Path<Uuid>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(err) = state.foreman.get_job(job_id).await {
        return error_response(StatusCode::NOT_FOUND, err);
    }

    let replay_from = parse_last_event_id(&headers);
    let replay = match state.foreman.get_job_event_backlog(job_id, replay_from).await {
        Ok(events) => events,
        Err(err) => return error_response(StatusCode::NOT_FOUND, err),
    };
    let replay_events = replay
        .into_iter()
        .map(|event| {
            Ok::<Event, Infallible>(sse_event(
                &event.method,
                event.ts,
                serde_json::json!({
                    "job_id": event.job_id,
                    "agent_id": event.agent_id,
                    "ts": event.ts,
                    "method": event.method,
                    "params": event.params,
                }),
            ))
        })
        .collect::<Vec<_>>();

    let live = BroadcastStream::new(state.foreman.subscribe_job_events())
        .filter_map(move |result| match result {
            Ok(envelope) if envelope.job_id == job_id => Some(Ok::<Event, Infallible>(sse_event(
                &envelope.method,
                envelope.ts,
                serde_json::json!({
                    "job_id": envelope.job_id,
                    "agent_id": envelope.agent_id,
                    "ts": envelope.ts,
                    "method": envelope.method,
                    "params": envelope.params,
                }),
            ))),
            _ => None,
        });

    let stream = tokio_stream::iter(replay_events).chain(live);
    Sse::new(stream).into_response()
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
        path.join(consts::PROJECT_CONFIG_FILE),
        &read_template_file(template_dir, consts::PROJECT_CONFIG_FILE)?,
        overwrite,
    )?;
    created_files.push(path.join(consts::PROJECT_CONFIG_FILE).display().to_string());

    write_template_file(
        path.join(consts::DEFAULT_FOREMAN_PROMPT_FILE),
        &read_template_file(template_dir, consts::DEFAULT_FOREMAN_PROMPT_FILE)?,
        overwrite,
    )?;
    created_files.push(
        path.join(consts::DEFAULT_FOREMAN_PROMPT_FILE)
            .display()
            .to_string(),
    );

    write_template_file(
        path.join(consts::DEFAULT_WORKER_PROMPT_FILE),
        &read_template_file(template_dir, consts::DEFAULT_WORKER_PROMPT_FILE)?,
        overwrite,
    )?;
    created_files.push(
        path.join(consts::DEFAULT_WORKER_PROMPT_FILE)
            .display()
            .to_string(),
    );

    write_template_file(
        path.join(consts::DEFAULT_RUNBOOK_FILE),
        &read_template_file(template_dir, consts::DEFAULT_RUNBOOK_FILE)?,
        overwrite,
    )?;
    created_files.push(
        path.join(consts::DEFAULT_RUNBOOK_FILE)
            .display()
            .to_string(),
    );

    write_template_file(
        path.join(consts::DEFAULT_HANDOFF_FILE),
        &read_template_file(template_dir, consts::DEFAULT_HANDOFF_FILE)?,
        overwrite,
    )?;
    created_files.push(
        path.join(consts::DEFAULT_HANDOFF_FILE)
            .display()
            .to_string(),
    );

    if include_manual {
        write_template_file(
            path.join(consts::DEFAULT_MANUAL_FILE),
            &read_template_file(template_dir, consts::DEFAULT_MANUAL_FILE)?,
            overwrite,
        )?;
        created_files.push(path.join(consts::DEFAULT_MANUAL_FILE).display().to_string());
    }

    println!(
        "Initialized foreman project scaffold at: {}",
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
        std::env::var(consts::TEMPLATE_DIR_ENV)
            .ok()
            .filter(|_env_dir| cli_template_dir.is_none())
            .map(PathBuf::from),
        Some(StdPath::new(env!("CARGO_MANIFEST_DIR")).join("templates")),
        std::env::current_exe()
            .ok()
            .and_then(|exe_dir| exe_dir.parent().map(|parent| parent.join("templates"))),
        Some(consts::TEMPLATE_DIR_SHARE.into()),
        Some(consts::TEMPLATE_DIR_ETC.into()),
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
        let secondary_template_dir = tempfile::tempdir().expect("temp dir");
        let backup = std::env::var_os(super::constants::TEMPLATE_DIR_ENV);

        let preferred_path = preferred.path().to_string_lossy().to_string();
        let secondary_path = secondary_template_dir.path().to_path_buf();

        // ensure env is set to a different valid directory so precedence is testable.
        // SAFETY: Tests are single-threaded and manipulate process environment for assertion setup.
        unsafe {
            std::env::set_var(super::constants::TEMPLATE_DIR_ENV, &secondary_path);
        }
        let selected = resolve_template_dir(Some(&preferred_path)).expect("template dir resolves");
        assert_eq!(selected, preferred.path());

        if let Some(old) = backup {
            // SAFETY: Tests are single-threaded and restore environment only for test scope.
            unsafe {
                std::env::set_var(super::constants::TEMPLATE_DIR_ENV, old);
            }
        } else {
            // SAFETY: Tests are single-threaded and restore environment only for test scope.
            unsafe {
                std::env::remove_var(super::constants::TEMPLATE_DIR_ENV);
            }
        }
    }

    #[test]
    fn resolve_template_dir_uses_manifest_templates_when_unset() {
        let _guard = with_template_env_lock();
        let backup = std::env::var_os(super::constants::TEMPLATE_DIR_ENV);
        // SAFETY: Tests are single-threaded and manage process environment temporarily.
        unsafe {
            std::env::remove_var(super::constants::TEMPLATE_DIR_ENV);
        }

        let resolved = resolve_template_dir(None).expect("manifest template directory");
        let expected = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
        assert_eq!(resolved, expected);

        if let Some(old) = backup {
            // SAFETY: Tests are single-threaded and restore environment only for test scope.
            unsafe {
                std::env::set_var(super::constants::TEMPLATE_DIR_ENV, old);
            }
        }
    }
}
