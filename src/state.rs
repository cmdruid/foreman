use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

use crate::constants;
use crate::models::{AgentEventDto, AgentResult};
use crate::project::ProjectConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedState {
    pub version: u32,
    pub generated_at: u64,
    pub agents: Vec<PersistedAgentRecord>,
    pub projects: Vec<PersistedProjectRecord>,
    pub jobs: Vec<PersistedJobRecord>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            version: constants::PERSISTED_STATE_VERSION,
            generated_at: now_ts(),
            agents: Vec::new(),
            projects: Vec::new(),
            jobs: Vec::new(),
        }
    }
}

impl PersistedState {
    pub async fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let bytes = fs::read(path)
            .await
            .with_context(|| format!("read '{}'", path.display()))?;
        let mut state: Self =
            serde_json::from_slice(&bytes).context("invalid persisted state JSON")?;
        if state.version == 0 {
            state.version = constants::PERSISTED_STATE_VERSION;
        }
        state.generated_at = now_ts();
        Ok(state)
    }

    pub async fn save(&self, path: &Path) -> Result<()> {
        let mut state = self.clone();
        state.generated_at = now_ts();
        let data = serde_json::to_vec_pretty(&state).context("serialize persisted state")?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create state dir '{}'", parent.display()))?;
        }

        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(constants::DEFAULT_STATE_FILENAME);
        let tmp = path.with_file_name(format!("{file_name}.{}.tmp", Uuid::new_v4()));
        let mut file = fs::File::create(&tmp)
            .await
            .with_context(|| format!("create temporary state file '{}'", tmp.display()))?;
        file.write_all(&data)
            .await
            .with_context(|| format!("write temporary state file '{}'", tmp.display()))?;
        file.flush()
            .await
            .with_context(|| format!("flush temporary state file '{}'", tmp.display()))?;
        fs::rename(&tmp, path)
            .await
            .with_context(|| format!("rename temporary state file to '{}'", path.display()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PersistedWorkerCallback {
    None,
    Webhook {
        url: String,
        secret: Option<String>,
        #[serde(default)]
        timeout_ms: Option<u64>,
        events: Option<Vec<String>>,
        vars: std::collections::HashMap<String, String>,
    },
    Profile {
        profile: String,
        prompt_prefix: Option<String>,
        command_args: Option<Vec<String>>,
        events: Option<Vec<String>>,
        vars: std::collections::HashMap<String, String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedAgentRecord {
    pub id: String,
    pub thread_id: String,
    pub active_turn_id: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub restart_attempts: u32,
    pub status: String,
    pub callback: PersistedWorkerCallback,
    pub role: String,
    pub project_id: Option<String>,
    pub foreman_id: Option<String>,
    pub error: Option<String>,
    pub result: Option<AgentResult>,
    #[serde(default)]
    pub job_id: Option<String>,
    pub updated_at: u64,
    pub events: Vec<AgentEventDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedRuntimeFiles {
    pub foreman_prompt: String,
    pub worker_prompt: String,
    pub runbook: String,
    pub handoff: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedProjectRecord {
    pub id: String,
    pub path: String,
    pub name: String,
    pub status: String,
    pub foreman_agent_id: Option<String>,
    pub worker_ids: Vec<String>,
    pub completed_worker_turns: u64,
    pub config: ProjectConfig,
    pub runtime: PersistedRuntimeFiles,
    #[serde(default)]
    pub lifecycle_callback_status: std::collections::HashMap<String, String>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedJobRecord {
    pub id: String,
    pub project_id: Option<String>,
    pub status: String,
    pub worker_ids: Vec<String>,
    pub worker_labels: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    pub created_at: u64,
    pub completed_at: Option<u64>,
    pub updated_at: u64,
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}
