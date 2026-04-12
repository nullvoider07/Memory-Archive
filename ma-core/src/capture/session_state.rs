// /Memory-Archive/ma-core/src/capture/session_state.rs

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;

use crate::session::metadata::{self, SessionMetadata, SkippedFetch, StepEntry};
use crate::storage::StorageBackend;

pub struct CaptureState {
    metadata: SessionMetadata,
    pub session_id: String,
    pub memory_dir: PathBuf,
    pub is_cloud_primary: bool,
    storage: Arc<dyn StorageBackend>,
    flush_interval: u32,
    steps_since_flush: u32,
    reasoning_maps: crate::capture::ReasoningMapsRef,
}

impl CaptureState {
    pub fn new(
        metadata: SessionMetadata,
        session_id: String,
        memory_dir: PathBuf,
        storage: Arc<dyn StorageBackend>,
        flush_interval: u32,
        is_cloud_primary: bool,
        reasoning_maps: crate::capture::ReasoningMapsRef,
    ) -> Self {
        Self {
            metadata,
            session_id,
            memory_dir,
            storage,
            flush_interval: flush_interval.max(1),
            steps_since_flush: 0,
            is_cloud_primary,
            reasoning_maps,
        }
    }

    /// Build a cloud-namespaced path: {memory_name}/{relative_path}.
    /// This ensures all objects for a session land under sessions/{session_id}/{memory_name}/
    /// in cloud storage, matching the convention used by the Python read-back path.
    fn cloud_path(&self, relative_path: &str) -> String {
        format!("{}/{}", self.metadata.memory_name, relative_path)
    }

    /// Flush in-memory metadata and command files to storage.
    /// Local mode: atomic disk write of metadata only.
    /// Cloud mode: upload metadata.json and all command files.
    pub async fn flush(&mut self) -> Result<()> {
        let (input_tokens, output_tokens) = self.reasoning_maps
            .drain_tokens(&self.session_id)
            .await;
        if input_tokens > 0 || output_tokens > 0 {
            self.metadata.total_input_tokens += input_tokens;
            self.metadata.total_output_tokens += output_tokens;
        }

        let provider_tokens = self.reasoning_maps
            .drain_provider_tokens(&self.session_id)
            .await;
        for (provider, (pin, pout)) in provider_tokens {
            let entry = self.metadata.token_costs_by_provider
                .entry(provider)
                .or_insert_with(crate::session::metadata::ProviderTokenCounts::default);
            entry.input_tokens += pin;
            entry.output_tokens += pout;
        }

        if self.is_cloud_primary {
            let bytes = serde_json::to_vec_pretty(&self.metadata)
                .context("Failed to serialize metadata for flush")?;
            self.storage
                .put(&self.session_id, &self.cloud_path("metadata.json"), bytes, "application/json")
                .await
                .context("StorageBackend::put failed for metadata.json")?;
            self.flush_command_files().await?;
        } else {
            metadata::write(&self.memory_dir, &self.metadata)
                .context("metadata::write failed")?;
        }
        Ok(())
    }

    /// Upload all four command files from local disk to cloud storage.
    /// Files that do not yet exist are silently skipped.
    /// Called on every metadata flush so cloud state stays in sync with local writes.
    async fn flush_command_files(&self) -> Result<()> {
        let files = [
            ("commands/raw_input.md", "text/markdown"),
            ("commands/converted_input.md", "text/markdown"),
            ("commands/actuation_commands.json", "application/json"),
            ("commands/cc_commands.json", "application/json"),
        ];
        for (rel, content_type) in &files {
            let abs = self.memory_dir.join(rel);
            if !abs.exists() {
                continue;
            }
            match tokio::fs::read(&abs).await {
                Ok(bytes) => {
                    if let Err(e) = self.storage
                        .put(&self.session_id, &self.cloud_path(rel), bytes, content_type)
                        .await
                    {
                        crate::observability::metrics().cloud_upload_errors_total.inc();
                        tracing::warn!(
                            session_id = %self.session_id,
                            path = rel,
                            "Failed to upload command file: {e}"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %self.session_id,
                        path = rel,
                        "Failed to read command file for cloud upload: {e}"
                    );
                }
            }
        }
        Ok(())
    }

    /// Flush unconditionally — called on done and SIGTERM.
    pub async fn flush_now(&mut self) -> Result<()> {
        self.steps_since_flush = 0;
        self.flush().await
    }

    /// Increment step counter and flush if the interval is reached.
    /// In local mode, always flushes (write-through behavior preserved).
    pub async fn flush_if_due(&mut self) -> Result<()> {
        self.steps_since_flush += 1;
        if !self.is_cloud_primary || self.steps_since_flush >= self.flush_interval {
            self.steps_since_flush = 0;
            self.flush().await?;
        }
        Ok(())
    }

    /// Write a file (image etc.) to storage.
    /// Local mode: atomic disk write. Cloud mode: StorageBackend::put under memory_name prefix.
    pub async fn put_file(
        &self,
        relative_path: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<()> {
        if self.is_cloud_primary {
            crate::observability::metrics().cloud_upload_queue_depth.inc();
            let result = self.storage
                .put(&self.session_id, &self.cloud_path(relative_path), bytes, content_type)
                .await;
            crate::observability::metrics().cloud_upload_queue_depth.dec();
            if let Err(ref e) = result {
                crate::observability::metrics().cloud_upload_errors_total.inc();
                tracing::error!(
                    session_id = %self.session_id,
                    path = %relative_path,
                    "Cloud upload failed: {e}"
                );
            }
            result.with_context(|| format!("StorageBackend::put failed for {relative_path}"))?;
        } else {
            let abs_path = self.memory_dir.join(relative_path);
            if let Some(parent) = abs_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let tmp = abs_path.with_extension("tmp");
            tokio::fs::write(&tmp, &bytes).await?;
            tokio::fs::rename(&tmp, &abs_path).await?;
        }
        Ok(())
    }

    pub fn set_in_progress(&mut self, value: &str) {
        self.metadata.in_progress = Some(value.to_string());
    }

    pub fn clear_in_progress(&mut self) {
        self.metadata.in_progress = None;
    }

    pub fn set_closing_image(&mut self, path: &str) {
        self.metadata.closing_image_path = Some(path.to_string());
    }

    pub fn mark_complete(&mut self) {
        self.metadata.status = "complete".to_string();
        self.metadata.completed_at = Some(Utc::now());
    }

    /// Mark the session as incomplete in the in-memory metadata.
    /// Should be called before flush_now() when a session disconnects uncleanly,
    /// so the cloud metadata reflects the correct status.
    pub fn mark_incomplete(&mut self) {
        self.metadata.status = "incomplete".to_string();
    }

    pub fn append_step(&mut self, step: StepEntry) {
        self.metadata.total_steps += 1;
        self.metadata.steps.push(step);
    }

    pub fn update_step_image(
        &mut self,
        step_id: u32,
        image_path: String,
        marked: bool,
        before_image_path: Option<String>,
        after_image_path: Option<String>,
    ) {
        if let Some(entry) = self.metadata.steps.iter_mut().find(|s| s.step_id == step_id) {
            entry.image_path = Some(image_path);
            entry.image_fetched = true;
            entry.marked = marked;
            entry.before_image_path = before_image_path;
            entry.after_image_path = after_image_path;
        }
    }

    pub fn append_skipped_fetch(&mut self, step_id: u32, reason: &str) {
        self.metadata.skipped_image_fetches.push(SkippedFetch {
            step_id,
            reason: reason.to_string(),
        });
    }

    pub fn update_kafka_position(&mut self, partition: i32, offset: i64) {
        self.metadata.kafka_partition = Some(partition);
        self.metadata.kafka_offset = Some(offset);
    }

    pub fn storage(&self) -> Arc<dyn StorageBackend> {
        self.storage.clone()
    }

    /// Read-only access to total_steps — used by crash recovery to assign
    /// step IDs when replaying Kafka events.
    pub fn total_steps(&self) -> u32 {
        self.metadata.total_steps
    }

    pub fn all_steps(&self) -> &[crate::session::metadata::StepEntry] {
        &self.metadata.steps
    }

    /// Transition metadata status to pending_annotation and clear in_progress.
    /// Used by crash recovery after Kafka replay completes successfully.
    pub fn mark_recovered(&mut self) {
        self.metadata.status = "pending_annotation".to_string();
        self.metadata.in_progress = None;
    }

    /// Return the last `n` completed steps as context for StepReadyForReasoning.
    /// Reads from in-memory step list — no I/O. Reasoning text is empty until T8.6
    /// writes it back; the context will be populated correctly once the pipeline
    /// is fully wired. Steps are returned oldest-first.
    pub fn recent_steps_context(&self, n: u32) -> Vec<crate::ipc::messages::ContextStep> {
        let steps = &self.metadata.steps;
        let take = n as usize;
        let skip = steps.len().saturating_sub(take);
        steps[skip..]
            .iter()
            .map(|s| crate::ipc::messages::ContextStep {
                step_id: s.step_id,
                converted_command: s.action_type.clone() + "/" + &s.action_subtype,
                reasoning: String::new(),
            })
            .collect()
    }
}