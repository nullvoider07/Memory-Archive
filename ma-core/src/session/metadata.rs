// /Memory-Archive/ma-core/src/session/metadata.rs

use std::path::Path;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// Schema

/// Per-provider token counts stored in token_costs_by_provider.
/// Serialises to {"input_tokens": N, "output_tokens": N} in metadata.json.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderTokenCounts {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub memory_name: String,
    pub memory_description: String, // Filled in by user during memory.md compilation
    pub session_id: String,
    pub mode: String,               // "manual" | "automated"
    pub status: String,             // mirrors SessionStatus

    pub os: OsInfo,

    // Tool server IDs
    pub capture_server_id: String,
    pub actuation_server_id: String,
    pub reasoning_model_id: Option<String>,
    #[serde(default)]
    pub ma_core_addr: String,

    // Timestamps
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,

    // Step counters — updated incrementally as the session runs
    pub total_steps: u32,
    pub annotated_steps: u32,
    pub skipped_steps: u32,

    // Per-step manifest — populated by vision pipeline
    // Each entry maps a step to its image path and fetch status.
    #[serde(default)]
    pub steps: Vec<StepEntry>,

    // Image fetches skipped due to failed commands — populated by vision pipeline
    #[serde(default)]
    pub skipped_image_fetches: Vec<SkippedFetch>,

    /// Final screenshot captured when the session ends cleanly via `done`.
    /// Relative path inside the memory directory, e.g. "vision/closing_state.png".
    /// None if vision was disabled or the fetch failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closing_image_path: Option<String>,

    /// Tenant identifier for cost attribution and future multi-tenant routing.
    #[serde(default)]
    pub tenant_id: String,

    /// Aggregate VLM token counts updated as reasoning entries arrive.
    /// Zero in manual mode. Populated during automated mode (T8.6+).
    #[serde(default)]
    pub total_input_tokens: u64,

    #[serde(default)]
    pub total_output_tokens: u64,

    /// Per-provider token count breakdown. Updated incrementally alongside
    /// total_input_tokens / total_output_tokens on every ReasoningResult.
    /// Keyed by provider name (matching the `provider` field in reasoning.jsonl).
    /// Empty in manual mode. Enables the `cost` command to produce per-provider
    /// cost breakdown from metadata.json alone — no reasoning.jsonl scan needed.
    #[serde(default)]
    pub token_costs_by_provider: std::collections::HashMap<String, ProviderTokenCounts>,

    /// Tracks whether a capture session is actively running.
    /// Set to "capturing" when the watch loop starts.
    /// Set to "interrupted" by the signal handler on clean shutdown.
    /// Cleared (null) on successful done.
    /// Used by the startup sweep to detect orphaned sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_progress: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kafka_partition: Option<i32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kafka_offset: Option<i64>,

    /// Per-session primary VLM provider name. Empty in manual mode.
    #[serde(default)]
    pub model_provider: String,

    /// Per-session fallback VLM provider name. Empty if no fallback configured.
    #[serde(default)]
    pub fallback_model_provider: String,

    /// Per-session fallback VLM endpoint. Empty if no fallback configured.
    #[serde(default)]
    pub fallback_model_endpoint: String,

    // Per-session CC and Eyes server addresses.
    // Set at registration time from RegisterSession IPC. Empty strings mean
    // run_watch_loop falls back to global config.control_center_addr / config.the_eyes_addr.
    /// HTTP address of the Control-Center server assigned to this session.
    /// e.g. "http://10.18.44.23:8080". Empty = use global config fallback.
    #[serde(default)]
    pub capture_server_addr: String,

    /// HTTP address of The-Eyes server assigned to this session.
    /// e.g. "http://10.18.44.23:8081". Empty = use global config fallback.
    #[serde(default)]
    pub the_eyes_addr: String,
}

/// Set the in_progress field — called when the watch loop starts ("capturing")
/// or when the signal handler fires ("interrupted").
pub fn set_in_progress(memory_dir: &Path, value: &str) -> anyhow::Result<()> {
    let mut metadata = read(memory_dir)?;
    metadata.in_progress = Some(value.to_string());
    write(memory_dir, &metadata)
}

/// Clear the in_progress field — called on clean done.
pub fn clear_in_progress(memory_dir: &Path) -> anyhow::Result<()> {
    let mut metadata = read(memory_dir)?;
    metadata.in_progress = None;
    write(memory_dir, &metadata)
}

/// Record the closing state image path after a clean done.
/// Called after the final screenshot is saved to vision/closing_state.png.
pub fn set_closing_image(memory_dir: &Path, path: &str) -> anyhow::Result<()> {
    let mut metadata = read(memory_dir)?;
    metadata.closing_image_path = Some(path.to_string());
    write(memory_dir, &metadata)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OsInfo {
    pub os_type: String,         // "LINUX" | "WINDOWS" | "MACOS"
    pub os_version: String,      // e.g. "Ubuntu 24.04 LTS"
    pub os_architecture: String, // e.g. "x86_64"
    pub os_environment_id: String,
}

/// One entry per actuation step — added by vision pipeline as images are fetched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepEntry {
    pub step_id: u32,
    pub timestamp: String,
    pub action_type: String,
    pub action_subtype: String,
    pub image_path: Option<String>,
    pub image_fetched: bool,
    pub marked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_image_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_image_path: Option<String>,
    #[serde(default)]
    pub raw_command: String,
    #[serde(default)]
    pub converted_command: String,
}

/// Skipped image fetch record — failed commands produce no image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedFetch {
    pub step_id: u32,
    pub reason: String, // e.g. "command failed"
}

// Read / write
//
/// Append a new StepEntry to metadata.json and increment total_steps.
#[allow(dead_code)]
pub fn append_step(memory_dir: &Path, step: StepEntry) -> anyhow::Result<()> {
    let mut metadata = read(memory_dir)?;

    metadata.total_steps += 1;
    metadata.steps.push(step);

    write(memory_dir, &metadata)
}

/// Update the annotated and skipped step counters in metadata.json.
#[allow(dead_code)]
pub fn update_annotation_counters(
    memory_dir: &Path,
    annotated: u32,
    skipped: u32,
) -> anyhow::Result<()> {
    let mut metadata = read(memory_dir)?;

    metadata.annotated_steps = annotated;
    metadata.skipped_steps = skipped;

    write(memory_dir, &metadata)
}

/// Update a step's image fields after the vision pipeline completes.
#[allow(dead_code)]
pub fn update_step_image(
    memory_dir: &Path,
    step_id: u32,
    image_path: String,
    marked: bool,
    before_image_path: Option<String>,
    after_image_path: Option<String>,
) -> anyhow::Result<()> {
    let mut metadata = read(memory_dir)?;

    if let Some(entry) = metadata.steps.iter_mut().find(|s| s.step_id == step_id) {
        entry.image_path = Some(image_path);
        entry.image_fetched = true;
        entry.marked = marked;
        entry.before_image_path = before_image_path;
        entry.after_image_path = after_image_path;
    }

    write(memory_dir, &metadata)
}

/// Record a skipped image fetch (failed command or non-triggering action).
#[allow(dead_code)]
pub fn append_skipped_fetch(
    memory_dir: &Path,
    step_id: u32,
    reason: &str,
) -> anyhow::Result<()> {
    let mut metadata = read(memory_dir)?;

    metadata.skipped_image_fetches.push(SkippedFetch {
        step_id,
        reason: reason.to_string(),
    });

    write(memory_dir, &metadata)
}

/// Update kafka_partition and kafka_offset after each event is processed.
#[allow(dead_code)]
pub fn update_kafka_position(
    memory_dir: &Path,
    partition: i32,
    offset: i64,
) -> anyhow::Result<()> {
    let mut metadata = read(memory_dir)?;
    metadata.kafka_partition = Some(partition);
    metadata.kafka_offset = Some(offset);
    write(memory_dir, &metadata)
}

/// Update metadata.json status to "incomplete" without renaming the directory.
/// Called in local mode disconnect before the directory rename so the file
/// inside always reflects the true session status.
pub fn mark_incomplete_status(memory_dir: &Path) -> anyhow::Result<()> {
    let mut metadata = read(memory_dir)?;
    metadata.status = "incomplete".to_string();
    write(memory_dir, &metadata)
}

/// Mark the session as complete and set completed_at timestamp.
pub fn mark_complete(memory_dir: &Path) -> anyhow::Result<()> {
    let mut metadata = read(memory_dir)?;

    metadata.status = "complete".to_string();
    metadata.completed_at = Some(chrono::Utc::now());

    write(memory_dir, &metadata)
}

/// Uses atomic write (temp file + rename) to prevent partial writes.
pub fn write(memory_dir: &Path, metadata: &SessionMetadata) -> anyhow::Result<()> {
    let path = memory_dir.join("metadata.json");
    let tmp_path = memory_dir.join("metadata.json.tmp");

    let json = serde_json::to_string_pretty(metadata)
        .context("Failed to serialise metadata")?;

    std::fs::write(&tmp_path, json)
        .with_context(|| format!("Failed to write temp metadata: {}", tmp_path.display()))?;

    std::fs::rename(&tmp_path, &path)
        .with_context(|| format!("Failed to rename metadata into place: {}", path.display()))?;

    tracing::debug!("metadata.json written: {}", path.display());
    Ok(())
}

/// Parse a SessionMetadata from raw bytes — used by crash recovery in cloud_primary mode
/// where there is no local file path to read from.
pub fn from_bytes(bytes: &[u8]) -> anyhow::Result<SessionMetadata> {
    serde_json::from_slice(bytes)
        .context("Failed to parse SessionMetadata from bytes")
}

/// Read metadata.json from the memory directory.
pub fn read(memory_dir: &Path) -> anyhow::Result<SessionMetadata> {
    let path = memory_dir.join("metadata.json");

    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read metadata.json: {}", path.display()))?;

    serde_json::from_str(&raw)
        .with_context(|| format!("Failed to parse metadata.json: {}", path.display()))
}