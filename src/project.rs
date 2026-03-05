use std::{collections::HashMap, fs, path::Path, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    config::{CallbackFilter, CallbackProfile},
    constants,
};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectConfig {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub prompts: ProjectPrompts,
    #[serde(default)]
    pub callbacks: ProjectCallbacks,
    #[serde(default)]
    pub policy: ProjectPolicy,
    #[serde(default)]
    pub jobs: ProjectJobsConfig,
    #[serde(default)]
    pub validation: Option<ValidationConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectPrompts {
    #[serde(default = "default_foreman_prompt_file")]
    pub foreman_file: String,
    #[serde(default = "default_worker_prompt_file")]
    pub worker_file: String,
    #[serde(default = "default_runbook_file")]
    pub runbook_file: String,
    #[serde(default)]
    pub handoff_file: Option<String>,
}

impl Default for ProjectPrompts {
    fn default() -> Self {
        Self {
            foreman_file: default_foreman_prompt_file(),
            worker_file: default_worker_prompt_file(),
            runbook_file: default_runbook_file(),
            handoff_file: Some(default_handoff_file()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectCallbacks {
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub profiles: HashMap<String, CallbackProfile>,
    #[serde(default)]
    pub filters: Vec<CallbackFilter>,
    #[serde(default)]
    pub worker: CallbackSpec,
    #[serde(default)]
    pub foreman: CallbackSpec,
    #[serde(default)]
    pub bubble_up: CallbackSpec,
    #[serde(default)]
    pub lifecycle: ProjectLifecycleCallbacks,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CallbackSpec {
    #[serde(default)]
    pub callback_profile: Option<String>,
    #[serde(default)]
    pub callback_prompt_prefix: Option<String>,
    #[serde(default)]
    pub callback_args: Option<Vec<String>>,
    #[serde(default)]
    pub callback_events: Option<Vec<String>>,
    #[serde(default)]
    pub events: Option<Vec<String>>,
    #[serde(default)]
    pub callback_vars: Option<HashMap<String, String>>,
    #[serde(default)]
    pub socket: Option<PathBuf>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectLifecycleCallbacks {
    #[serde(default)]
    pub start: CallbackSpec,
    #[serde(default)]
    pub compact: CallbackSpec,
    #[serde(default)]
    pub stop: CallbackSpec,
    #[serde(default)]
    pub worker_completed: CallbackSpec,
    #[serde(default)]
    pub worker_aborted: CallbackSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectPolicy {
    #[serde(default)]
    pub bubble_up_events: Option<Vec<String>>,
    #[serde(default)]
    pub compact_after_turns: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectJobsConfig {
    #[serde(default)]
    pub defaults: JobDefaultsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct JobDefaultsConfig {
    #[serde(default)]
    pub min_turns: Option<u32>,
    #[serde(default)]
    pub strategy: Option<String>,
    #[serde(default = "default_worktree_mode")]
    pub worktree_mode: String,
    #[serde(default = "default_merge_strategy")]
    pub merge_strategy: String,
    #[serde(default)]
    pub base_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationConfig {
    #[serde(default)]
    pub on_turn: Vec<String>,
    #[serde(default)]
    pub on_complete: Vec<String>,
    #[serde(default = "default_validation_fail_action")]
    pub fail_action: String,
    #[serde(default = "default_validation_max_retries")]
    pub max_retries: u32,
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            on_turn: Vec::new(),
            on_complete: Vec::new(),
            fail_action: default_validation_fail_action(),
            max_retries: default_validation_max_retries(),
        }
    }
}

impl Default for ProjectPolicy {
    fn default() -> Self {
        Self {
            bubble_up_events: Some(
                constants::DEFAULT_BUBBLE_UP_EVENTS
                    .iter()
                    .map(|value| value.to_string())
                    .collect(),
            ),
            compact_after_turns: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProjectRuntimeFiles {
    pub path: PathBuf,
    pub foreman_prompt: String,
    pub worker_prompt: String,
    pub runbook: String,
    pub handoff: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectLintIssue {
    pub code: &'static str,
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ProjectLintReport {
    pub issues: Vec<ProjectLintIssue>,
    pub fixed: bool,
}

impl ProjectConfig {
    pub fn load(project_path: &Path) -> Result<Self> {
        let config_path = project_path.join(constants::PROJECT_CONFIG_FILE);
        if !config_path.exists() {
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read project config {}", config_path.display()))?;
        let manifest: toml::Value = toml::from_str(&raw).with_context(|| {
            format!(
                "{} invalid project config format",
                constants::ERROR_CODE_CFG_PROJECT_PARSE
            )
        })?;
        if manifest.get("hooks").is_some() {
            anyhow::bail!(
                "{} project config uses unsupported [hooks]; replace with [callbacks.lifecycle]",
                constants::ERROR_CODE_CFG_PROJECT_PARSE
            );
        }
        let mut lint_report = ProjectLintReport::default();
        validate_unknown_keys(&manifest, &mut lint_report);
        if let Some(issue) = lint_report.issues.first() {
            anyhow::bail!("{} {}: {}", issue.code, issue.path, issue.message);
        }
        let config: Self = toml::from_str(&raw).with_context(|| {
            format!(
                "{} invalid project config format",
                constants::ERROR_CODE_CFG_PROJECT_PARSE
            )
        })?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        self.prompts.validate().with_context(|| {
            format!(
                "{} invalid project prompt configuration",
                constants::ERROR_CODE_CFG_PROJECT_VALIDATION
            )
        })?;
        self.jobs.validate().with_context(|| {
            format!(
                "{} invalid project jobs configuration",
                constants::ERROR_CODE_CFG_PROJECT_VALIDATION
            )
        })?;
        if let Some(validation) = &self.validation {
            validation.validate().with_context(|| {
                format!(
                    "{} invalid project validation configuration",
                    constants::ERROR_CODE_CFG_PROJECT_VALIDATION
                )
            })?;
        }
        Ok(())
    }

    pub fn load_runtime_files(&self, path: &Path) -> Result<ProjectRuntimeFiles> {
        let root = path.to_path_buf();

        let foreman_prompt = read_text_required(&root.join(&self.prompts.foreman_file))?;
        let worker_prompt = read_text_required(&root.join(&self.prompts.worker_file))?;
        let runbook = read_text_required(&root.join(&self.prompts.runbook_file))?;
        let handoff = if let Some(handoff_file) = &self.prompts.handoff_file {
            let candidate = root.join(handoff_file);
            if candidate.exists() {
                Some(read_text(&candidate)?)
            } else {
                None
            }
        } else {
            None
        };

        Ok(ProjectRuntimeFiles {
            path: root,
            foreman_prompt,
            worker_prompt,
            runbook,
            handoff,
        })
    }
}

pub fn lint_project_toml(
    project_toml_path: &Path,
    service_callback_profiles: &HashMap<String, CallbackProfile>,
    fix: bool,
) -> Result<ProjectLintReport> {
    let mut report = ProjectLintReport::default();
    let raw = fs::read_to_string(project_toml_path).with_context(|| {
        format!(
            "{} failed to read project config {}",
            constants::ERROR_CODE_CFG_PROJECT_PATH_INVALID,
            project_toml_path.display()
        )
    })?;

    let manifest: toml::Value = toml::from_str(&raw).with_context(|| {
        format!(
            "{} invalid project config format",
            constants::ERROR_CODE_CFG_PROJECT_PARSE
        )
    })?;

    if manifest.get("hooks").is_some() {
        report.issues.push(ProjectLintIssue {
            code: constants::ERROR_CODE_CFG_PROJECT_PARSE,
            path: "hooks".to_string(),
            message: "unsupported [hooks]; use [callbacks.lifecycle]".to_string(),
        });
    }

    validate_unknown_keys(&manifest, &mut report);

    let project_config: ProjectConfig = toml::from_str(&raw).with_context(|| {
        format!(
            "{} invalid project config format",
            constants::ERROR_CODE_CFG_PROJECT_PARSE
        )
    })?;
    project_config.validate()?;

    validate_callback_profile_references(&project_config, service_callback_profiles, &mut report);

    if fix {
        report.fixed = false;
    }

    Ok(report)
}

impl ProjectPrompts {
    fn validate(&self) -> Result<()> {
        if self.foreman_file.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "{} prompts.foreman_file cannot be empty",
                constants::ERROR_CODE_CFG_PROJECT_VALIDATION
            ));
        }
        if self.worker_file.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "{} prompts.worker_file cannot be empty",
                constants::ERROR_CODE_CFG_PROJECT_VALIDATION
            ));
        }
        if self.runbook_file.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "{} prompts.runbook_file cannot be empty",
                constants::ERROR_CODE_CFG_PROJECT_VALIDATION
            ));
        }
        Ok(())
    }
}

impl ProjectJobsConfig {
    fn validate(&self) -> Result<()> {
        self.defaults.validate()
    }
}

impl JobDefaultsConfig {
    fn validate(&self) -> Result<()> {
        if let Some(strategy) = &self.strategy
            && strategy != "single"
            && strategy != "explore-plan-execute"
        {
            anyhow::bail!("jobs.defaults.strategy must be 'single' or 'explore-plan-execute'");
        }

        if self.worktree_mode != "auto"
            && self.worktree_mode != "always"
            && self.worktree_mode != "never"
        {
            anyhow::bail!("jobs.defaults.worktree_mode must be 'auto', 'always', or 'never'");
        }

        if self.merge_strategy != "sequential"
            && self.merge_strategy != "rebase"
            && self.merge_strategy != "manual"
        {
            anyhow::bail!(
                "jobs.defaults.merge_strategy must be 'sequential', 'rebase', or 'manual'"
            );
        }

        Ok(())
    }
}

impl ValidationConfig {
    fn validate(&self) -> Result<()> {
        if self.fail_action != "retry" && self.fail_action != "abort" && self.fail_action != "warn"
        {
            anyhow::bail!("validation.fail_action must be 'retry', 'abort', or 'warn'");
        }
        Ok(())
    }
}

fn validate_unknown_keys(manifest: &toml::Value, report: &mut ProjectLintReport) {
    let Some(root) = manifest.as_table() else {
        report.issues.push(ProjectLintIssue {
            code: constants::ERROR_CODE_CFG_PROJECT_PARSE,
            path: "root".to_string(),
            message: "project config root must be a TOML table".to_string(),
        });
        return;
    };

    let root_allowed = [
        "name",
        "prompts",
        "callbacks",
        "policy",
        "jobs",
        "validation",
    ];
    push_unknown_table_keys(root, "", &root_allowed, report);

    if let Some(prompts) = root.get("prompts").and_then(toml::Value::as_table) {
        let prompts_allowed = [
            "foreman_file",
            "worker_file",
            "runbook_file",
            "handoff_file",
        ];
        push_unknown_table_keys(prompts, "prompts", &prompts_allowed, report);
    }

    if let Some(callbacks) = root.get("callbacks").and_then(toml::Value::as_table) {
        let callbacks_allowed = [
            "env",
            "profiles",
            "filters",
            "worker",
            "foreman",
            "bubble_up",
            "lifecycle",
        ];
        push_unknown_table_keys(callbacks, "callbacks", &callbacks_allowed, report);

        if let Some(filters) = callbacks.get("filters").and_then(toml::Value::as_array) {
            for (index, filter) in filters.iter().enumerate() {
                if let Some(filter) = filter.as_table() {
                    push_unknown_table_keys(
                        filter,
                        &format!("callbacks.filters[{index}]"),
                        &[
                            "name",
                            "callback_profile",
                            "tool",
                            "command_pattern",
                            "debounce_ms",
                        ],
                        report,
                    );
                }
            }
        }

        for spec_name in ["worker", "foreman", "bubble_up"] {
            if let Some(spec) = callbacks.get(spec_name).and_then(toml::Value::as_table) {
                push_unknown_table_keys(
                    spec,
                    &format!("callbacks.{spec_name}"),
                    &[
                        "callback_profile",
                        "callback_prompt_prefix",
                        "callback_args",
                        "callback_events",
                        "events",
                        "callback_vars",
                        "socket",
                        "timeout_ms",
                    ],
                    report,
                );
            }
        }

        if let Some(lifecycle) = callbacks.get("lifecycle").and_then(toml::Value::as_table) {
            let lifecycle_allowed = [
                "start",
                "compact",
                "stop",
                "worker_completed",
                "worker_aborted",
            ];
            push_unknown_table_keys(lifecycle, "callbacks.lifecycle", &lifecycle_allowed, report);
            for lifecycle_key in lifecycle_allowed {
                if let Some(spec) = lifecycle.get(lifecycle_key).and_then(toml::Value::as_table) {
                    push_unknown_table_keys(
                        spec,
                        &format!("callbacks.lifecycle.{lifecycle_key}"),
                        &[
                            "callback_profile",
                            "callback_prompt_prefix",
                            "callback_args",
                            "callback_events",
                            "events",
                            "callback_vars",
                            "socket",
                            "timeout_ms",
                        ],
                        report,
                    );
                }
            }
        }
    }

    if let Some(policy) = root.get("policy").and_then(toml::Value::as_table) {
        let policy_allowed = ["bubble_up_events", "compact_after_turns"];
        push_unknown_table_keys(policy, "policy", &policy_allowed, report);
    }

    if let Some(jobs) = root.get("jobs").and_then(toml::Value::as_table) {
        let jobs_allowed = ["defaults"];
        push_unknown_table_keys(jobs, "jobs", &jobs_allowed, report);
        if let Some(defaults) = jobs.get("defaults").and_then(toml::Value::as_table) {
            let defaults_allowed = [
                "min_turns",
                "strategy",
                "worktree_mode",
                "merge_strategy",
                "base_branch",
            ];
            push_unknown_table_keys(defaults, "jobs.defaults", &defaults_allowed, report);
        }
    }

    if let Some(validation) = root.get("validation").and_then(toml::Value::as_table) {
        let validation_allowed = ["on_turn", "on_complete", "fail_action", "max_retries"];
        push_unknown_table_keys(validation, "validation", &validation_allowed, report);
    }
}

fn push_unknown_table_keys(
    table: &toml::map::Map<String, toml::Value>,
    prefix: &str,
    allowed: &[&str],
    report: &mut ProjectLintReport,
) {
    for key in table.keys() {
        if allowed.iter().any(|allowed_key| allowed_key == key) {
            continue;
        }
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        report.issues.push(ProjectLintIssue {
            code: constants::ERROR_CODE_CFG_PROJECT_UNKNOWN_KEY,
            path,
            message: "unknown key".to_string(),
        });
    }
}

fn validate_callback_profile_references(
    config: &ProjectConfig,
    service_callback_profiles: &HashMap<String, CallbackProfile>,
    report: &mut ProjectLintReport,
) {
    let mut specs: Vec<(&str, &CallbackSpec)> = vec![
        ("callbacks.worker", &config.callbacks.worker),
        ("callbacks.foreman", &config.callbacks.foreman),
        ("callbacks.bubble_up", &config.callbacks.bubble_up),
        (
            "callbacks.lifecycle.start",
            &config.callbacks.lifecycle.start,
        ),
        (
            "callbacks.lifecycle.compact",
            &config.callbacks.lifecycle.compact,
        ),
        ("callbacks.lifecycle.stop", &config.callbacks.lifecycle.stop),
        (
            "callbacks.lifecycle.worker_completed",
            &config.callbacks.lifecycle.worker_completed,
        ),
        (
            "callbacks.lifecycle.worker_aborted",
            &config.callbacks.lifecycle.worker_aborted,
        ),
    ];

    specs.retain(|(_, spec)| spec.callback_profile.is_some());

    for (spec_path, spec) in specs {
        if let Some(profile_name) = spec.callback_profile.as_deref()
            && !config.callbacks.profiles.contains_key(profile_name)
            && !service_callback_profiles.contains_key(profile_name)
        {
            report.issues.push(ProjectLintIssue {
                code: constants::ERROR_CODE_CFG_CALLBACK_PROFILE_MISSING,
                path: format!("{spec_path}.callback_profile"),
                message: format!("unknown callback profile '{profile_name}'"),
            });
        }
    }

    for (index, filter) in config.callbacks.filters.iter().enumerate() {
        if !config
            .callbacks
            .profiles
            .contains_key(filter.callback_profile.as_str())
            && !service_callback_profiles.contains_key(filter.callback_profile.as_str())
        {
            report.issues.push(ProjectLintIssue {
                code: constants::ERROR_CODE_CFG_CALLBACK_PROFILE_MISSING,
                path: format!("callbacks.filters[{index}].callback_profile"),
                message: format!("unknown callback profile '{}'", filter.callback_profile),
            });
        }
    }
}

fn read_text(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))
}

fn read_text_required(path: &Path) -> Result<String> {
    let data = read_text(path)?;
    if data.trim().is_empty() {
        anyhow::bail!("{} is empty", path.display());
    }
    Ok(data)
}

fn default_foreman_prompt_file() -> String {
    constants::DEFAULT_FOREMAN_PROMPT_FILE.to_string()
}

fn default_worker_prompt_file() -> String {
    constants::DEFAULT_WORKER_PROMPT_FILE.to_string()
}

fn default_runbook_file() -> String {
    constants::DEFAULT_RUNBOOK_FILE.to_string()
}

fn default_handoff_file() -> String {
    constants::DEFAULT_HANDOFF_FILE.to_string()
}

fn default_validation_fail_action() -> String {
    "retry".to_string()
}

fn default_validation_max_retries() -> u32 {
    2
}

fn default_worktree_mode() -> String {
    "never".to_string()
}

fn default_merge_strategy() -> String {
    "manual".to_string()
}
