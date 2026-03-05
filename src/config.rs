use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use crate::constants;
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceConfig {
    #[serde(default)]
    pub app_server: AppServerConfig,
    #[serde(default)]
    pub protocol: ProtocolConfig,
    #[serde(default)]
    pub callbacks: CallbackRegistry,
    #[serde(default)]
    pub worker_monitoring: WorkerMonitoringConfig,
    #[serde(default)]
    pub security: SecurityConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerMonitoringConfig {
    #[serde(default = "default_worker_monitoring_enabled")]
    pub enabled: bool,
    #[serde(default = "default_worker_monitoring_inactivity_timeout_ms")]
    pub inactivity_timeout_ms: u64,
    #[serde(default = "default_worker_monitoring_max_restarts")]
    pub max_restarts: u32,
    #[serde(default = "default_worker_monitoring_watch_interval_ms")]
    pub watch_interval_ms: u64,
}

impl Default for WorkerMonitoringConfig {
    fn default() -> Self {
        Self {
            enabled: default_worker_monitoring_enabled(),
            inactivity_timeout_ms: default_worker_monitoring_inactivity_timeout_ms(),
            max_restarts: default_worker_monitoring_max_restarts(),
            watch_interval_ms: default_worker_monitoring_watch_interval_ms(),
        }
    }
}

fn default_worker_monitoring_enabled() -> bool {
    constants::DEFAULT_WORKER_MONITORING_ENABLED
}

fn default_worker_monitoring_inactivity_timeout_ms() -> u64 {
    constants::DEFAULT_WORKER_MONITORING_INACTIVITY_TIMEOUT_MS
}

fn default_worker_monitoring_max_restarts() -> u32 {
    constants::DEFAULT_WORKER_MONITORING_MAX_RESTARTS
}

fn default_worker_monitoring_watch_interval_ms() -> u64 {
    constants::DEFAULT_WORKER_MONITORING_WATCH_INTERVAL_MS
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProtocolConfig {
    #[serde(default)]
    pub expected_codex_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppServerConfig {
    #[serde(default = "default_app_server_initialize_timeout_ms")]
    pub initialize_timeout_ms: u64,
    #[serde(default = "default_app_server_request_timeout_ms")]
    pub request_timeout_ms: u64,
}

impl Default for AppServerConfig {
    fn default() -> Self {
        Self {
            initialize_timeout_ms: default_app_server_initialize_timeout_ms(),
            request_timeout_ms: default_app_server_request_timeout_ms(),
        }
    }
}

fn default_app_server_initialize_timeout_ms() -> u64 {
    constants::DEFAULT_APP_SERVER_INITIALIZE_TIMEOUT_MS
}

fn default_app_server_request_timeout_ms() -> u64 {
    constants::DEFAULT_APP_SERVER_REQUEST_TIMEOUT_MS
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SecurityConfig {
    #[serde(default)]
    pub auth: Option<AuthConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub header_name: Option<String>,
    #[serde(default)]
    pub header_scheme: Option<String>,
    #[serde(default)]
    pub skip_paths: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RuntimeAuthConfig {
    pub token: String,
    pub header_name: String,
    pub header_scheme: Option<String>,
    pub skip_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CallbackRegistry {
    #[serde(default)]
    pub default_profile: Option<String>,
    #[serde(default)]
    pub profiles: HashMap<String, CallbackProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallbackFilter {
    pub name: String,
    pub callback_profile: String,
    pub tool: String,
    pub command_pattern: String,
    #[serde(default)]
    pub debounce_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum CallbackProfile {
    Webhook(WebhookCallbackProfile),
    Command(CommandCallbackProfile),
    UnixSocket(UnixSocketCallbackProfile),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookCallbackProfile {
    pub url: String,
    #[serde(default)]
    pub secret_env: Option<String>,
    #[serde(default)]
    pub events: Option<Vec<String>>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandCallbackProfile {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub prompt_prefix: Option<String>,
    #[serde(default)]
    pub events: Option<Vec<String>>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub event_prompt_variable: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnixSocketCallbackProfile {
    pub socket: PathBuf,
    #[serde(default)]
    pub events: Option<Vec<String>>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl ServiceConfig {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file: {}", path.display()))?;
        let value: Self = toml::from_str(&raw).context("invalid callback config format")?;
        Ok(value)
    }

    pub fn default_callback_profile(&self) -> Option<String> {
        self.callbacks.default_profile.clone()
    }

    pub fn get_callback_profile(&self, name: &str) -> Option<&CallbackProfile> {
        self.callbacks.profiles.get(name)
    }

    pub fn validate(&self) -> Result<Vec<String>> {
        let mut warnings = Vec::new();

        if self.app_server.initialize_timeout_ms == 0 {
            return Err(anyhow!(
                "app_server.initialize_timeout_ms must be greater than 0"
            ));
        }

        if self.app_server.request_timeout_ms == 0 {
            return Err(anyhow!(
                "app_server.request_timeout_ms must be greater than 0"
            ));
        }

        if self.worker_monitoring.enabled {
            if self.worker_monitoring.inactivity_timeout_ms == 0 {
                return Err(anyhow!(
                    "worker_monitoring.inactivity_timeout_ms must be greater than 0"
                ));
            }

            if self.worker_monitoring.watch_interval_ms == 0 {
                return Err(anyhow!(
                    "worker_monitoring.watch_interval_ms must be greater than 0"
                ));
            }

            if self.worker_monitoring.max_restarts == 0 {
                warnings.push(
                    "worker_monitoring.max_restarts is 0 (workers will fail after timeout)"
                        .to_string(),
                );
            }
        }

        if let Some(default_profile) = &self.callbacks.default_profile
            && !self.callbacks.profiles.contains_key(default_profile)
        {
            return Err(anyhow!(
                "default_profile '{default_profile}' is not defined in callbacks.profiles"
            ));
        }

        for (name, profile) in &self.callbacks.profiles {
            if profile.timeout_ms() == Some(0) {
                warnings.push(format!(
                    "callback profile '{name}' uses timeout_ms=0 (no timeout)"
                ));
            }

            match profile {
                CallbackProfile::Command(command) => {
                    if command.program.trim().is_empty() {
                        return Err(anyhow!(
                            "callback profile '{name}' has an empty command program"
                        ));
                    }
                    if command.program.contains("{{") || command.program.contains("}}") {
                        return Err(anyhow!(
                            "callback profile '{name}' must use a static command program path (templates are not allowed)"
                        ));
                    }
                    if !Path::new(command.program.trim()).is_absolute() {
                        return Err(anyhow!(
                            "callback profile '{name}' command program must be an absolute path"
                        ));
                    }
                }
                CallbackProfile::Webhook(webhook) => {
                    if webhook.url.trim().is_empty() {
                        return Err(anyhow!(
                            "callback profile '{name}' has an empty webhook url"
                        ));
                    }
                }
                CallbackProfile::UnixSocket(unix_socket) => {
                    if unix_socket
                        .socket
                        .as_os_str()
                        .to_string_lossy()
                        .trim()
                        .is_empty()
                    {
                        return Err(anyhow!(
                            "callback profile '{name}' has an empty unix socket path"
                        ));
                    }
                }
            }
        }

        if let Some(auth) = &self.security.auth
            && auth.enabled
        {
            if auth.token.is_some() && auth.token_env.is_some() {
                warnings.push(
                    "auth.token is configured and will take precedence over auth.token_env"
                        .to_string(),
                );
            }

            if auth.token.is_none() && auth.token_env.is_none() {
                return Err(anyhow!(
                    "auth is enabled but token or token_env is not configured"
                ));
            }
        }

        Ok(warnings)
    }

    pub fn resolve_auth_config(&self) -> Result<Option<RuntimeAuthConfig>> {
        let Some(auth) = &self.security.auth else {
            return Ok(None);
        };

        if !auth.enabled {
            return Ok(None);
        }

        let token = if let Some(token) = &auth.token {
            token.clone()
        } else if let Some(token_env) = &auth.token_env {
            std::env::var(token_env)
                .with_context(|| format!("failed to read API token from env var '{token_env}'"))?
        } else {
            return Err(anyhow!(
                "auth is enabled but token or token_env is not configured"
            ));
        };

        if token.trim().is_empty() {
            return Err(anyhow!("resolved API token is empty"));
        }

        let skip_paths = if auth.skip_paths.is_empty() {
            vec![constants::DEFAULT_AUTH_SKIP_PATH.to_string()]
        } else {
            auth.skip_paths.clone()
        };

        Ok(Some(RuntimeAuthConfig {
            token,
            header_name: auth
                .header_name
                .clone()
                .unwrap_or_else(|| constants::DEFAULT_AUTH_HEADER_NAME.to_string()),
            header_scheme: auth.header_scheme.clone(),
            skip_paths,
        }))
    }

    pub fn app_server_timeouts(&self) -> (u64, u64) {
        (
            self.app_server.initialize_timeout_ms,
            self.app_server.request_timeout_ms,
        )
    }

    pub fn expected_codex_version(&self) -> Option<&str> {
        self.protocol.expected_codex_version.as_deref()
    }
}

impl CallbackProfile {
    pub fn timeout_ms(&self) -> Option<u64> {
        match self {
            Self::Webhook(profile) => profile.timeout_ms,
            Self::Command(profile) => profile.timeout_ms,
            Self::UnixSocket(profile) => profile.timeout_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AppServerConfig, AuthConfig, SecurityConfig, ServiceConfig, WorkerMonitoringConfig,
    };
    use crate::constants;
    use std::{env, path::Path};

    #[test]
    fn load_missing_file_returns_default_config() {
        let config = ServiceConfig::load(Path::new("/tmp/does-not-exist-foreman.toml"))
            .expect("load missing file");
        assert_eq!(
            config.app_server.initialize_timeout_ms,
            constants::DEFAULT_APP_SERVER_INITIALIZE_TIMEOUT_MS
        );
        assert_eq!(
            config.app_server.request_timeout_ms,
            constants::DEFAULT_APP_SERVER_REQUEST_TIMEOUT_MS
        );
        assert!(config.callbacks.profiles.is_empty());
        assert!(config.security.auth.is_none());
        assert!(config.protocol.expected_codex_version.is_none());
        assert!(!config.worker_monitoring.enabled);
    }

    #[test]
    fn app_server_timeout_defaults_are_valid() {
        let config = ServiceConfig::default();
        assert_eq!(
            config.app_server.initialize_timeout_ms,
            constants::DEFAULT_APP_SERVER_INITIALIZE_TIMEOUT_MS
        );
        assert_eq!(
            config.app_server.request_timeout_ms,
            constants::DEFAULT_APP_SERVER_REQUEST_TIMEOUT_MS
        );
        assert!(!config.worker_monitoring.enabled);
        assert_eq!(
            config.worker_monitoring.inactivity_timeout_ms,
            constants::DEFAULT_WORKER_MONITORING_INACTIVITY_TIMEOUT_MS
        );
        assert_eq!(
            config.worker_monitoring.max_restarts,
            constants::DEFAULT_WORKER_MONITORING_MAX_RESTARTS
        );
        assert_eq!(
            config.worker_monitoring.watch_interval_ms,
            constants::DEFAULT_WORKER_MONITORING_WATCH_INTERVAL_MS
        );
    }

    #[test]
    fn worker_monitoring_defaults_validate() {
        let config = ServiceConfig {
            worker_monitoring: WorkerMonitoringConfig::default(),
            ..ServiceConfig::default()
        };
        assert!(!config.worker_monitoring.enabled);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn invalid_worker_monitoring_timeout_is_rejected() {
        let config = ServiceConfig {
            worker_monitoring: WorkerMonitoringConfig {
                enabled: true,
                inactivity_timeout_ms: 0,
                max_restarts: 1,
                watch_interval_ms: 500,
            },
            ..ServiceConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn zero_restart_limits_are_allowed_with_warning_when_enabled() {
        let config = ServiceConfig {
            worker_monitoring: WorkerMonitoringConfig {
                enabled: true,
                inactivity_timeout_ms: 2_000,
                max_restarts: 0,
                watch_interval_ms: 500,
            },
            ..ServiceConfig::default()
        };
        let warnings = config.validate().expect("validation should pass");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("workers will fail after timeout"));
    }

    #[test]
    fn custom_app_server_timeouts_validate() {
        let config = ServiceConfig {
            app_server: AppServerConfig {
                initialize_timeout_ms: 50,
                request_timeout_ms: 100,
            },
            ..ServiceConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn zero_timeout_app_server_settings_are_rejected() {
        let config = ServiceConfig {
            app_server: AppServerConfig {
                initialize_timeout_ms: 0,
                request_timeout_ms: 30_000,
            },
            ..ServiceConfig::default()
        };
        assert!(config.validate().is_err());

        let config = ServiceConfig {
            app_server: AppServerConfig {
                initialize_timeout_ms: 5_000,
                request_timeout_ms: 0,
            },
            ..ServiceConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_reports_unknown_default_profile() {
        let mut config = ServiceConfig::default();
        config.callbacks.default_profile = Some("missing-profile".to_string());

        let err = config
            .validate()
            .expect_err("missing default profile must fail");
        assert!(err.to_string().contains("missing-profile"));
    }

    #[test]
    fn resolve_auth_prefers_token_over_env_with_warning() {
        let config = ServiceConfig {
            security: SecurityConfig {
                auth: Some(AuthConfig {
                    enabled: true,
                    token: Some("inline-token".to_string()),
                    token_env: Some("CODEX_FOREMAN_AUTH_TEST_TOKEN".to_string()),
                    header_name: None,
                    header_scheme: None,
                    skip_paths: Vec::new(),
                }),
            },
            ..ServiceConfig::default()
        };

        let warnings = config.validate().expect("validated auth config");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("token is configured"));

        let resolved = config
            .resolve_auth_config()
            .expect("resolve auth config")
            .expect("auth enabled");
        assert_eq!(resolved.token, "inline-token");
        assert_eq!(resolved.header_name, constants::DEFAULT_AUTH_HEADER_NAME);
        assert_eq!(
            resolved.skip_paths,
            vec![constants::DEFAULT_AUTH_SKIP_PATH.to_string()]
        );
    }

    #[test]
    fn resolve_auth_reads_token_from_environment() {
        let var_name = "CODEX_FOREMAN_AUTH_TEST_TOKEN_ENV";
        unsafe {
            env::set_var(var_name, "env-token");
        }

        let resolved = {
            let config = ServiceConfig {
                security: SecurityConfig {
                    auth: Some(AuthConfig {
                        enabled: true,
                        token: None,
                        token_env: Some(var_name.to_string()),
                        header_name: Some("x-api-key".to_string()),
                        header_scheme: None,
                        skip_paths: vec!["/custom-health".to_string()],
                    }),
                },
                ..ServiceConfig::default()
            };

            config
                .resolve_auth_config()
                .expect("resolve auth config")
                .expect("auth enabled")
        };

        unsafe {
            env::remove_var(var_name);
        }

        assert_eq!(resolved.token, "env-token");
        assert_eq!(resolved.header_name, "x-api-key");
        assert_eq!(resolved.skip_paths, vec!["/custom-health".to_string()]);
    }

    #[test]
    fn resolve_auth_rejects_empty_token() {
        let config = ServiceConfig {
            security: SecurityConfig {
                auth: Some(AuthConfig {
                    enabled: true,
                    token: Some("   ".to_string()),
                    token_env: None,
                    header_name: None,
                    header_scheme: None,
                    skip_paths: Vec::new(),
                }),
            },
            ..ServiceConfig::default()
        };

        assert!(config.resolve_auth_config().is_err());
    }
}
