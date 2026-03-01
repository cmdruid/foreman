use std::{collections::HashMap, fs, path::Path, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const PROJECT_CONFIG_FILE: &str = "project.toml";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectConfig {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub prompts: ProjectPrompts,
    #[serde(default)]
    pub callbacks: ProjectCallbacks,
    #[serde(default)]
    pub hooks: ProjectHooks,
    #[serde(default)]
    pub policy: ProjectPolicy,
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
    pub worker: CallbackSpec,
    #[serde(default)]
    pub foreman: CallbackSpec,
    #[serde(default)]
    pub bubble_up: CallbackSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CallbackSpec {
    pub callback_profile: Option<String>,
    pub callback_prompt_prefix: Option<String>,
    pub callback_args: Option<Vec<String>>,
    pub callback_events: Option<Vec<String>>,
    pub callback_vars: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectHooks {
    #[serde(default)]
    pub on_project_start: Option<String>,
    #[serde(default)]
    pub on_worker_completed: Option<String>,
    #[serde(default)]
    pub on_worker_aborted: Option<String>,
    #[serde(default)]
    pub on_project_compaction: Option<String>,
    #[serde(default)]
    pub on_project_stop: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectPolicy {
    #[serde(default)]
    pub bubble_up_events: Option<Vec<String>>,
    #[serde(default)]
    pub compact_after_turns: Option<u64>,
}

impl Default for ProjectPolicy {
    fn default() -> Self {
        Self {
            bubble_up_events: Some(vec![
                "turn/completed".to_string(),
                "turn/aborted".to_string(),
            ]),
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

impl ProjectConfig {
    pub fn load(project_path: &Path) -> Result<Self> {
        let config_path = project_path.join(PROJECT_CONFIG_FILE);
        if !config_path.exists() {
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read project config {}", config_path.display()))?;
        let config: Self = toml::from_str(&raw).context("invalid project config format")?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        self.prompts
            .validate()
            .context("invalid project prompt configuration")?;
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

impl ProjectPrompts {
    fn validate(&self) -> Result<()> {
        if self.foreman_file.trim().is_empty() {
            return Err(anyhow::anyhow!("foreman_file cannot be empty"));
        }
        if self.worker_file.trim().is_empty() {
            return Err(anyhow::anyhow!("worker_file cannot be empty"));
        }
        if self.runbook_file.trim().is_empty() {
            return Err(anyhow::anyhow!("runbook_file cannot be empty"));
        }
        Ok(())
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
    "FOREMAN.md".to_string()
}

fn default_worker_prompt_file() -> String {
    "WORKER.md".to_string()
}

fn default_runbook_file() -> String {
    "RUNBOOK.md".to_string()
}

fn default_handoff_file() -> String {
    "HANDOFF.md".to_string()
}
