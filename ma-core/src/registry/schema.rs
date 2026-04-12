// /Memory-Archive/ma-core/src/registry/schema.rs

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// Enums

/// All possible states a session can be in.
/// Stored as a lowercase string in Redis, e.g. "active", "incomplete".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    PendingAnnotation,
    PendingHumanAnnotation,
    Annotating,
    PendingCompilation,
    ReasoningDegraded,
    Complete,
    Incomplete,
}

impl SessionStatus {
    /// TTL in seconds to apply when transitioning to this status.
    /// Returns None for statuses that should have no TTL set.
    pub fn ttl_seconds(&self) -> Option<u64> {
        match self {
            SessionStatus::Active => None,
            SessionStatus::Annotating => Some(7 * 24 * 60 * 60),   // 7 days
            SessionStatus::PendingCompilation => None,
            SessionStatus::PendingAnnotation => Some(7 * 24 * 60 * 60),   // 7 days
            SessionStatus::PendingHumanAnnotation => None,
            SessionStatus::ReasoningDegraded => Some(7 * 24 * 60 * 60),   // 7 days
            SessionStatus::Incomplete => Some(30 * 24 * 60 * 60),         // 30 days
            SessionStatus::Complete => Some(90 * 24 * 60 * 60),           // 90 days
        }
    }

    /// The Redis index set this status belongs to.
    /// Used to maintain lookup sets alongside the Hash.
    pub fn index_set(&self) -> Option<&'static str> {
        match self {
            SessionStatus::Active => Some("sessions:active"),
            SessionStatus::PendingAnnotation => Some("sessions:pending"),
            SessionStatus::PendingHumanAnnotation => Some("sessions:pending_human_annotation"),
            SessionStatus::Annotating => Some("sessions:annotating"),
            SessionStatus::PendingCompilation => Some("sessions:pending_compilation"),
            SessionStatus::ReasoningDegraded => Some("sessions:reasoning_degraded"),
            _ => None,
        }
    }
}

// Implement Display and FromStr to convert to/from lowercase strings for Redis storage
impl std::fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| format!("{self:?}").to_lowercase());
        write!(f, "{s}")
    }
}

// Deserialize from a lowercase string, returning an error for unknown values
impl std::str::FromStr for SessionStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::from_value(serde_json::Value::String(s.to_string()))
            .map_err(|e| anyhow::anyhow!("Unknown session status '{s}': {e}"))
    }
}

/// Session operating mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// Human annotator provides reasoning post-session.
    Manual,
    /// Reasoning model provides reasoning in real time.
    Automated,
}

// Implement Display and FromStr to convert to/from lowercase strings for Redis storage
impl std::fmt::Display for SessionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionMode::Manual => write!(f, "manual"),
            SessionMode::Automated => write!(f, "automated"),
        }
    }
}

// Deserialize from a lowercase string, returning an error for unknown values
impl std::str::FromStr for SessionMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "manual" => Ok(SessionMode::Manual),
            "automated" => Ok(SessionMode::Automated),
            _ => anyhow::bail!("Unknown session mode '{s}' — expected 'manual' or 'automated'"),
        }
    }
}

// SessionRecord

/// Full session record — maps 1:1 to a Redis Hash.
///
/// Every field corresponds to one Hash field in Redis.
/// None fields are skipped on write and treated as absent on read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub mode: SessionMode,
    pub status: SessionStatus,

    // OS environment
    pub os_type: String,         // "LINUX" | "WINDOWS" | "MACOS"
    pub os_version: String,      // e.g. "Ubuntu 24.04 LTS"
    pub os_architecture: String, // e.g. "x86_64"
    pub os_environment_id: String,

    // Tool server IDs
    pub capture_server_id: String,
    pub actuation_server_id: String,
    pub reasoning_model_id: Option<String>, // None in manual mode

    // Memory info
    pub memory_name: String,
    pub memory_path: String,
    pub ma_core_addr: String,

    // Timestamps (ISO 8601)
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,

    // Step counters
    pub total_steps: u32,
    pub annotated_steps: u32,
    pub skipped_steps: u32,

    // Automated mode — tenant and VLM session config.
    // model_api_key_ref is a secrets store reference, not a credential value.
    // It is never logged and is filtered from annotator GetSessionStatus responses.
    pub tenant_id: String,
    pub model_provider: String,
    pub model_endpoint: String,
    pub model_api_key_ref: String,
    pub context_window_steps: u32,
    pub fallback_model_provider: String,
    pub fallback_model_endpoint: String,
    pub fallback_api_key_ref: String,
    pub storage_backend: String,
    // per-session CC and Eyes server addresses.
    // Passed at RegisterSession by the orchestration layer.
    // Empty strings mean run_watch_loop falls back to global config addresses.
    pub capture_server_addr: String,
    pub the_eyes_addr: String,
}

impl SessionRecord {
    /// Serialise to a flat Vec of (field, value) string pairs for HSET.
    /// Redis Hash values must all be strings.
    pub fn to_redis_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("session_id".into(), self.session_id.clone()),
            ("mode".into(), self.mode.to_string()),
            ("status".into(), self.status.to_string()),
            ("os_type".into(), self.os_type.clone()),
            ("os_version".into(), self.os_version.clone()),
            ("os_architecture".into(), self.os_architecture.clone()),
            ("os_environment_id".into(), self.os_environment_id.clone()),
            ("capture_server_id".into(), self.capture_server_id.clone()),
            ("actuation_server_id".into(), self.actuation_server_id.clone()),
            ("memory_name".into(), self.memory_name.clone()),
            ("memory_path".into(), self.memory_path.clone()),
            ("ma_core_addr".into(), self.ma_core_addr.clone()),
            ("created_at".into(), self.created_at.to_rfc3339()),
            ("updated_at".into(), self.updated_at.to_rfc3339()),
            ("total_steps".into(), self.total_steps.to_string()),
            ("annotated_steps".into(), self.annotated_steps.to_string()),
            ("skipped_steps".into(), self.skipped_steps.to_string()),
            ("tenant_id".into(), self.tenant_id.clone()),
            ("model_provider".into(), self.model_provider.clone()),
            ("model_endpoint".into(), self.model_endpoint.clone()),
            ("model_api_key_ref".into(), self.model_api_key_ref.clone()),
            ("context_window_steps".into(), self.context_window_steps.to_string()),
            ("fallback_model_provider".into(), self.fallback_model_provider.clone()),
            ("fallback_model_endpoint".into(), self.fallback_model_endpoint.clone()),
            ("fallback_api_key_ref".into(), self.fallback_api_key_ref.clone()),
            ("storage_backend".into(), self.storage_backend.clone()),
            ("capture_server_addr".into(), self.capture_server_addr.clone()),
            ("the_eyes_addr".into(), self.the_eyes_addr.clone()),
        ];

        if let Some(ref model_id) = self.reasoning_model_id {
            pairs.push(("reasoning_model_id".into(), model_id.clone()));
        }

        pairs
    }

    /// Deserialise from a flat HashMap of Redis Hash field → value strings.
    pub fn from_redis_map(
        map: std::collections::HashMap<String, String>,
    ) -> anyhow::Result<Self> {
        let get = |key: &str| -> anyhow::Result<String> {
            map.get(key)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Missing Redis field: '{key}'"))
        };

        Ok(SessionRecord {
            session_id: get("session_id")?,
            mode: get("mode")?.parse()?,
            status: get("status")?.parse()?,
            os_type: get("os_type")?,
            os_version: get("os_version")?,
            os_architecture: get("os_architecture")?,
            os_environment_id: get("os_environment_id")?,
            capture_server_id: get("capture_server_id")?,
            actuation_server_id: get("actuation_server_id")?,
            reasoning_model_id: map.get("reasoning_model_id").cloned(),
            memory_name: get("memory_name")?,
            memory_path: get("memory_path")?,
            ma_core_addr: map.get("ma_core_addr").cloned().unwrap_or_default(),
            created_at: get("created_at")?.parse()?,
            updated_at: get("updated_at")?.parse()?,
            total_steps: get("total_steps")?.parse()?,
            annotated_steps: get("annotated_steps")?.parse()?,
            skipped_steps: get("skipped_steps")?.parse()?,
            tenant_id: map.get("tenant_id").cloned().unwrap_or_default(),
            model_provider: map.get("model_provider").cloned().unwrap_or_default(),
            model_endpoint: map.get("model_endpoint").cloned().unwrap_or_default(),
            model_api_key_ref: map.get("model_api_key_ref").cloned().unwrap_or_default(),
            context_window_steps: map.get("context_window_steps")
                .and_then(|s| s.parse().ok())
                .unwrap_or(5),
            fallback_model_provider: map.get("fallback_model_provider").cloned().unwrap_or_default(),
            fallback_model_endpoint: map.get("fallback_model_endpoint").cloned().unwrap_or_default(),
            fallback_api_key_ref: map.get("fallback_api_key_ref").cloned().unwrap_or_default(),
            storage_backend: map.get("storage_backend").cloned().unwrap_or_default(),
            capture_server_addr: map.get("capture_server_addr").cloned().unwrap_or_default(),
            the_eyes_addr: map.get("the_eyes_addr").cloned().unwrap_or_default(),
        })
    }
}

/// Redis key for a session Hash.
pub fn session_key(session_id: &str) -> String {
    format!("session:{session_id}")
}

/// Redis key for the OS-type index set.
pub fn os_index_key(os_type: &str) -> String {
    format!("sessions:by_os:{}", os_type.to_uppercase())
}

/// Redis key for the mode index set.
pub fn mode_index_key(mode: &SessionMode) -> String {
    format!("sessions:by_mode:{mode}")
}