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
///
/// Returns the directory's path after the rename so callers can propagate it to
/// the session registry — the Redis record's `memory_path` must track the rename
/// or later lookups (annotate, compile, delete) resolve a path that no longer
/// exists.
pub fn mark_incomplete(memory_dir: &Path) -> anyhow::Result<PathBuf> {
    let parent = memory_dir
        .parent()
        .context("Memory directory has no parent")?;

    let current_name = memory_dir
        .file_name()
        .context("Memory directory has no name")?
        .to_string_lossy();

    // Avoid double-flagging if already marked incomplete.
    if current_name.ends_with(" (incomplete)") {
        return Ok(memory_dir.to_path_buf());
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

    Ok(incomplete_dir)
}

/// Permanently remove a session's local memory directory and its
/// "(incomplete)"-suffixed sibling (produced by `mark_incomplete`).
///
/// Used by session deletion. In local mode captures live at
/// `storage_path/{memory_name}` (the record's `memory_path`), which is distinct
/// from the `storage_path/{session_id}` proxy tree handled by `LocalBackend`.
///
/// Each directory is removed only when its `metadata.json` carries `session_id`.
/// A `memory_path` does not uniquely identify a session: when a scrapped take is
/// re-recorded under the same `memory_name`, the replacement occupies the same
/// path while the abandoned record keeps pointing at it, so purging by path alone
/// deletes the successful recording instead of the discarded one. A directory
/// whose metadata is missing, unreadable, or names a different session is left in
/// place and reported — retention and deletion must not destroy a recording they
/// cannot prove they own.
///
/// Returns `true` if at least one directory was removed. A missing directory is
/// not an error.
/// Read just the `session_id` out of a directory's metadata.json.
///
/// Parsed as untyped JSON rather than through `metadata::read` so that ownership
/// stays establishable for sessions written before a field was added to
/// `SessionMetadata`. A strict parse would fail on those and make deletion refuse
/// to remove directories it does in fact own.
fn read_owner_session_id(memory_dir: &Path) -> anyhow::Result<String> {
    let path = memory_dir.join("metadata.json");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read metadata.json: {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("Failed to parse metadata.json: {}", path.display()))?;
    value
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .with_context(|| format!("metadata.json has no session_id: {}", path.display()))
}

pub fn purge_memory_dir(memory_path: &str, session_id: &str) -> anyhow::Result<bool> {
    let mut removed = false;

    for candidate in [
        PathBuf::from(memory_path),
        PathBuf::from(format!("{memory_path} (incomplete)")),
    ] {
        if !candidate.exists() {
            continue;
        }

        match read_owner_session_id(&candidate) {
            Ok(owner) if owner == session_id => {
                std::fs::remove_dir_all(&candidate).with_context(|| {
                    format!("Failed to remove memory directory: {}", candidate.display())
                })?;
                removed = true;
            }
            Ok(owner) => {
                tracing::warn!(
                    session_id = %session_id,
                    owner_session_id = %owner,
                    path = %candidate.display(),
                    "Refusing to purge a directory owned by another session"
                );
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    path = %candidate.display(),
                    "Refusing to purge a directory whose owner could not be established: {e}"
                );
            }
        }
    }

    Ok(removed)
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
        // Both are learned from the live connection, not the registration record:
        // the transport when the stream is negotiated, the agent version from the
        // first command event.
        actuation_agent_version: String::new(),
        actuation_server_version: String::new(),
        actuation_transport: String::new(),
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

#[cfg(test)]
mod purge_tests {
    use super::purge_memory_dir;

    /// Minimal metadata.json carrying just the ownership field purge checks.
    fn write_meta(dir: &std::path::Path, session_id: &str) {
        std::fs::write(
            dir.join("metadata.json"),
            serde_json::json!({
                "memory_name": "my-memory",
                "session_id": session_id,
                "mode": "manual",
                "status": "complete",
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn purges_primary_and_incomplete_siblings() {
        let owner = uuid::Uuid::new_v4().to_string();
        let base = std::env::temp_dir().join(format!("ma-mem-{}", uuid::Uuid::new_v4()));
        let primary = base.join("my-memory");
        let incomplete = base.join("my-memory (incomplete)");
        std::fs::create_dir_all(primary.join("vision/frames")).unwrap();
        write_meta(&primary, &owner);
        std::fs::create_dir_all(&incomplete).unwrap();
        write_meta(&incomplete, &owner);

        let removed = purge_memory_dir(&primary.to_string_lossy(), &owner).unwrap();
        assert!(removed, "should report a directory was removed");
        assert!(!primary.exists(), "primary memory dir should be gone");
        assert!(!incomplete.exists(), "(incomplete) sibling should be gone");

        // No-op when nothing is present.
        let removed_again = purge_memory_dir(&primary.to_string_lossy(), &owner).unwrap();
        assert!(!removed_again);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A re-recorded take occupies the same memory_path as the abandoned session
    /// that preceded it. Purging the old session must not delete the new one.
    #[test]
    fn refuses_to_purge_a_directory_owned_by_another_session() {
        let abandoned = uuid::Uuid::new_v4().to_string();
        let replacement = uuid::Uuid::new_v4().to_string();
        let base = std::env::temp_dir().join(format!("ma-mem-{}", uuid::Uuid::new_v4()));
        let primary = base.join("my-memory");
        std::fs::create_dir_all(&primary).unwrap();
        write_meta(&primary, &replacement);

        let removed = purge_memory_dir(&primary.to_string_lossy(), &abandoned).unwrap();
        assert!(!removed, "must not report a removal it refused");
        assert!(primary.exists(), "the replacement recording must survive");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Ownership cannot be established without readable metadata, so the
    /// directory is left in place rather than deleted on assumption.
    #[test]
    fn refuses_to_purge_a_directory_with_unreadable_metadata() {
        let owner = uuid::Uuid::new_v4().to_string();
        let base = std::env::temp_dir().join(format!("ma-mem-{}", uuid::Uuid::new_v4()));
        let primary = base.join("my-memory");
        std::fs::create_dir_all(&primary).unwrap();
        std::fs::write(primary.join("metadata.json"), b"not json").unwrap();

        let removed = purge_memory_dir(&primary.to_string_lossy(), &owner).unwrap();
        assert!(!removed);
        assert!(primary.exists());

        let _ = std::fs::remove_dir_all(&base);
    }
}