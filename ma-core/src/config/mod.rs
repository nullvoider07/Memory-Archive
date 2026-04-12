// /Memory-Archive/ma-core/src/config/mod.rs

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "defaults::redis_url")]
    pub redis_url: String,

    #[serde(default = "defaults::ipc_socket_path")]
    pub ipc_socket_path: String,

    #[serde(default = "defaults::storage_path")]
    pub storage_path: String,

    #[serde(default)]
    pub control_center_addr: String,

    #[serde(default)]
    pub the_eyes_addr: String,

    #[serde(default = "defaults::the_eyes_poll_interval_seconds")]
    pub the_eyes_poll_interval_seconds: u64,

    #[serde(default = "defaults::silence_timeout_seconds")]
    pub silence_timeout_seconds: u64,

    /// Milliseconds to wait after a keyboard/press event before fetching the
    /// screenshot. Allows the screen to reflect the result of the keypress.
    #[serde(default = "defaults::press_fetch_delay_ms")]
    pub press_fetch_delay_ms: u64,

    /// Milliseconds to wait after a keyboard/type event before fetching the
    /// screenshot. Allows typed text to appear on screen.
    #[serde(default = "defaults::type_fetch_delay_ms")]
    pub type_fetch_delay_ms: u64,

    /// Milliseconds before an action timestamp to fetch the before-frame.
    /// Should roughly match The-Eyes capture interval so we get the frame
    /// captured just before the action fired.
    #[serde(default = "defaults::before_fetch_offset_ms")]
    pub before_fetch_offset_ms: u64,

    /// Milliseconds after the action frame timestamp to fetch the after-frame.
    /// Gives the interface time to reflect the result of the action.
    #[serde(default = "defaults::after_fetch_delay_ms")]
    pub after_fetch_delay_ms: u64,

    #[serde(default)]
    pub kafka_broker: String,

    #[serde(default = "defaults::kafka_channel_capacity")]
    pub kafka_channel_capacity: usize,

    #[serde(default = "defaults::metadata_flush_interval")]
    pub metadata_flush_interval: u32,

    /// TCP port for remote ma-app connections. None = TCP disabled (local Unix socket only).
    #[serde(default)]
    pub ipc_port: Option<u16>,

    /// Bind address for the TCP IPC listener. Defaults to 0.0.0.0.
    #[serde(default = "defaults::ipc_bind_addr")]
    pub ipc_bind_addr: String,

    /// Port for the annotator management REST API. None = disabled.
    /// When set, the server requires the MA_ANNOTATOR_MGMT_TOKEN env var.
    /// Default when explicitly configured: 9002.
    #[serde(default)]
    pub annotator_mgmt_port: Option<u16>,

    #[serde(default = "defaults::storage_mode")]
    pub storage_mode: String,

    #[serde(default)]
    pub cloud: CloudConfig,

    #[serde(default)]
    pub observability: ObservabilityConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            redis_url: defaults::redis_url(),
            ipc_socket_path: defaults::ipc_socket_path(),
            storage_path: defaults::storage_path(),
            control_center_addr: String::new(),
            the_eyes_addr: String::new(),
            the_eyes_poll_interval_seconds: defaults::the_eyes_poll_interval_seconds(),
            silence_timeout_seconds: defaults::silence_timeout_seconds(),
            press_fetch_delay_ms: defaults::press_fetch_delay_ms(),
            type_fetch_delay_ms: defaults::type_fetch_delay_ms(),
            before_fetch_offset_ms: defaults::before_fetch_offset_ms(),
            after_fetch_delay_ms: defaults::after_fetch_delay_ms(),
            kafka_broker: String::new(),
            kafka_channel_capacity: defaults::kafka_channel_capacity(),
            metadata_flush_interval: defaults::metadata_flush_interval(),
            ipc_port: None,
            ipc_bind_addr: defaults::ipc_bind_addr(),
            annotator_mgmt_port: None,
            storage_mode: defaults::storage_mode(),
            cloud: CloudConfig::default(),
            observability: ObservabilityConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    #[serde(default = "defaults::metrics_port")]
    pub metrics_port: u16,

    #[serde(default)]
    pub metrics_token: String,

    #[serde(default = "defaults::log_level")]
    pub log_level: String,

    #[serde(default = "defaults::log_output")]
    pub log_output: String,

    #[serde(default)]
    pub log_file_path: String,

    #[serde(default)]
    pub log_forward_url: String,

    #[serde(default)]
    pub alert_webhook_url: String,

    #[serde(default = "defaults::memory_warn_mb")]
    pub memory_warn_mb: u64,

    #[serde(default = "defaults::kafka_lag_warn")]
    pub kafka_lag_warn: i64,

    #[serde(default = "defaults::upload_queue_warn")]
    pub upload_queue_warn: u64,

    #[serde(default = "defaults::ipc_push_queue_warn")]
    pub ipc_push_queue_warn: u64,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            metrics_port: defaults::metrics_port(),
            metrics_token: String::new(),
            log_level: defaults::log_level(),
            log_output: defaults::log_output(),
            log_file_path: String::new(),
            log_forward_url: String::new(),
            alert_webhook_url: String::new(),
            memory_warn_mb: defaults::memory_warn_mb(),
            kafka_lag_warn: defaults::kafka_lag_warn(),
            upload_queue_warn: defaults::upload_queue_warn(),
            ipc_push_queue_warn: defaults::ipc_push_queue_warn(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CloudConfig {
    #[serde(default)]
    pub provider: String,

    #[serde(default)]
    pub aws: AwsConfig,

    #[serde(default)]
    pub azure: AzureConfig,

    #[serde(default)]
    pub gcp: GcpConfig,

    #[serde(default)]
    pub backends: Vec<NamedBackendConfig>,

    #[serde(default)]
    pub routing_rules: Vec<RoutingRuleConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AwsConfig {
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub region: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AzureConfig {
    #[serde(default)]
    pub container: String,
    #[serde(default)]
    pub account: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GcpConfig {
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub project: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamedBackendConfig {
    pub name: String,
    pub provider: String,
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub container: String,
    #[serde(default)]
    pub project: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRuleConfig {
    #[serde(default)]
    pub match_tenant_prefix: String,
    #[serde(default)]
    pub match_default: bool,
    pub backend: String,
}

mod defaults {
    use std::path::PathBuf;

    pub fn redis_url() -> String {
        "redis://127.0.0.1:6379".to_string()
    }

    pub fn ipc_socket_path() -> String {
        home_dir()
            .join(".memory-archive")
            .join("ma.sock")
            .to_string_lossy()
            .to_string()
    }

    pub fn storage_path() -> String {
        home_dir()
            .join(".memory-archive")
            .join("memories")
            .to_string_lossy()
            .to_string()
    }

    pub fn silence_timeout_seconds() -> u64 { 30 }
    pub fn the_eyes_poll_interval_seconds() -> u64 { 10 }
    pub fn press_fetch_delay_ms() -> u64 { 1000 }
    pub fn type_fetch_delay_ms() -> u64 { 1000 }
    pub fn before_fetch_offset_ms() -> u64 { 1000 }
    pub fn after_fetch_delay_ms() -> u64 { 1000 }
    pub fn ipc_bind_addr() -> String { "0.0.0.0".to_string() }
    pub fn storage_mode() -> String { "local".to_string() }
    pub fn kafka_channel_capacity() -> usize { 1000 }
    pub fn kafka_lag_warn() -> i64 { 10_000 }
    pub fn metadata_flush_interval() -> u32 { 10 }
    pub fn metrics_port() -> u16 { 9091 }
    pub fn log_level() -> String { "info".to_string() }
    pub fn log_output() -> String { "stdout".to_string() }
    pub fn memory_warn_mb() -> u64 { 4096 }
    pub fn upload_queue_warn() -> u64 { 1000 }
    pub fn ipc_push_queue_warn() -> u64 { 500 }

    fn home_dir() -> PathBuf {
        std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir())
    }
}

pub fn config_path() -> PathBuf {
    if let Ok(path) = std::env::var("MEMORY_ARCHIVE_CONFIG") {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| "/tmp".to_string());
    Path::new(&home).join(".memory-archive").join("config.json")
}

pub fn load() -> anyhow::Result<Config> {
    let path = config_path();

    if !path.exists() {
        tracing::info!("No config file found at {} — using defaults", path.display());
        return Ok(Config::default());
    }

    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read config: {}", path.display()))?;

    serde_json::from_str(&raw)
        .with_context(|| format!("Failed to parse config: {}", path.display()))
}

#[allow(dead_code)]
pub fn save(config: &Config) -> anyhow::Result<()> {
    let path = config_path();

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
    }

    let json = serde_json::to_string_pretty(config)
        .context("Failed to serialise config")?;

    std::fs::write(&path, json)
        .with_context(|| format!("Failed to write config: {}", path.display()))?;

    tracing::info!("Config saved to: {}", path.display());
    Ok(())
}