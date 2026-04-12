// /Memory-Archive/ma-core/src/session/mod.rs

pub mod metadata;
pub mod reasoning;

use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::Utc;

use crate::registry::schema::SessionRecord;
use metadata::{OsInfo, SessionMetadata};

// Directory initialiser

/// Create the full memory directory tree for a new session.
///
/// Returns the absolute path to the created memory directory.
///
/// Errors if the directory already exists (duplicate memory_name within the
/// same storage_path) or if any subdirectory cannot be created.
pub fn initialise(record: &SessionRecord, storage_path: &str, is_cloud_primary: bool) -> anyhow::Result<PathBuf> {
    let memory_dir = Path::new(storage_path).join(&record.memory_name);

    if is_cloud_primary {
        // In cloud_primary mode the local directory is not the primary store.
        // We only need it as a scratch space for the command writer buffers.
        // Create it fresh every time — removing any stale directory from a
        // previous session with the same memory_name so the watch loop always
        // starts with a clean local state.
        if memory_dir.exists() {
            std::fs::remove_dir_all(&memory_dir)
                .with_context(|| format!("Failed to remove stale memory directory: {}", memory_dir.display()))?;
        }
        std::fs::create_dir_all(memory_dir.join("commands"))
            .with_context(|| format!("Failed to create commands directory: {}", memory_dir.display()))?;
    } else {
        if memory_dir.exists() {
            anyhow::bail!(
                "Memory directory already exists: {} — choose a different memory_name",
                memory_dir.display()
            );
        }
        let subdirs = ["commands", "vision/frames", "reasoning"];
        for subdir in &subdirs {
            let path = memory_dir.join(subdir);
            std::fs::create_dir_all(&path)
                .with_context(|| format!("Failed to create directory: {}", path.display()))?;
        }
    }

    tracing::info!(session_id = %record.session_id, "Memory directory initialised");

    let metadata = build_initial_metadata(record);
    metadata::write(&memory_dir, &metadata)?;

    Ok(memory_dir)
}

/// Rename an active memory directory to flag it as incomplete.
pub fn mark_incomplete(memory_dir: &Path) -> anyhow::Result<()> {
    let parent = memory_dir
        .parent()
        .context("Memory directory has no parent")?;

    let current_name = memory_dir
        .file_name()
        .context("Memory directory has no name")?
        .to_string_lossy();

    // Avoid double-flagging if already marked incomplete.
    if current_name.ends_with(" (incomplete)") {
        return Ok(());
    }

    let incomplete_name = format!("{current_name} (incomplete)");
    let incomplete_dir = parent.join(&incomplete_name);

    std::fs::rename(memory_dir, &incomplete_dir).with_context(|| {
        format!(
            "Failed to rename {} → {}",
            memory_dir.display(),
            incomplete_dir.display()
        )
    })?;

    tracing::warn!(
        "Memory directory marked incomplete: {}",
        incomplete_dir.display()
    );

    Ok(())
}

// Internal helper to build the initial SessionMetadata from the SessionRecord
fn build_initial_metadata(record: &SessionRecord) -> SessionMetadata {
    SessionMetadata {
        memory_name: record.memory_name.clone(),
        memory_description: String::new(),
        session_id: record.session_id.clone(),
        mode: record.mode.to_string(),
        status: record.status.to_string(),
        os: OsInfo {
            os_type: record.os_type.clone(),
            os_version: record.os_version.clone(),
            os_architecture: record.os_architecture.clone(),
            os_environment_id: record.os_environment_id.clone(),
        },
        capture_server_id: record.capture_server_id.clone(),
        actuation_server_id: record.actuation_server_id.clone(),
        reasoning_model_id: record.reasoning_model_id.clone(),
        ma_core_addr: record.ma_core_addr.clone(),
        created_at: Utc::now(),
        completed_at: None,
        total_steps: 0,
        annotated_steps: 0,
        skipped_steps: 0,
        tenant_id: record.tenant_id.clone(),
        total_input_tokens: 0,
        total_output_tokens: 0,
        token_costs_by_provider: std::collections::HashMap::new(),
        steps: Vec::new(),
        skipped_image_fetches: Vec::new(),
        closing_image_path: None,
        in_progress: None,
        kafka_partition: None,
        kafka_offset: None,
        model_provider: record.model_provider.clone(),
        fallback_model_provider: record.fallback_model_provider.clone(),
        fallback_model_endpoint: record.fallback_model_endpoint.clone(),
        capture_server_addr: record.capture_server_addr.clone(),
        the_eyes_addr: record.the_eyes_addr.clone(),
    }
}