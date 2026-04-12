// /Memory-Archive/ma-core/src/session/reasoning.rs

use std::path::Path;

use anyhow::Context;
use chrono::Utc;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningEntry {
    pub step_id: u32,
    pub timestamp_action: String,
    pub timestamp_annotated: String,
    pub action_type: String,
    pub action_subtype: String,
    pub raw_command: String,
    pub converted_command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_path: Option<String>,
    pub reasoning: String,
    pub skipped: bool,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_intent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyboard_visual_annotation: Option<serde_json::Value>,
}

/// Build a reasoning entry for an automated-mode step from a ReasoningResult IPC message.
pub fn build_automated_entry(
    step: &crate::session::metadata::StepEntry,
    reasoning: String,
    source: String,
    provider: Option<String>,
    model_id: Option<String>,
    api_version: Option<String>,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    latency_ms: Option<u32>,
    action_intent: Option<String>,
    confidence: Option<f32>,
    keyboard_visual_annotation: Option<serde_json::Value>,
) -> ReasoningEntry {
    ReasoningEntry {
        step_id: step.step_id,
        timestamp_action: step.timestamp.clone(),
        timestamp_annotated: Utc::now().to_rfc3339(),
        action_type: step.action_type.clone(),
        action_subtype: step.action_subtype.clone(),
        raw_command: step.raw_command.clone(),
        converted_command: step.converted_command.clone(),
        image_path: step.image_path.clone(),
        reasoning,
        skipped: false,
        source,
        provider,
        model_id,
        api_version,
        input_tokens,
        output_tokens,
        latency_ms,
        action_intent,
        confidence,
        keyboard_visual_annotation,
    }
}

/// Atomically upsert one entry in reasoning.jsonl.
///
/// Reads all existing entries into a map keyed by step_id, inserts or replaces
/// the new entry, writes sorted output to a .tmp file, then renames atomically.
///
/// Cloud-primary mode: reads/writes from the local scratch memory_dir.
/// The caller is responsible for uploading the updated file to cloud storage
/// after this function returns.
pub fn upsert_entry(memory_dir: &Path, entry: &ReasoningEntry) -> anyhow::Result<()> {
    let dir = memory_dir.join("reasoning");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create reasoning dir: {}", dir.display()))?;

    let path = dir.join("reasoning.jsonl");
    let tmp = dir.join("reasoning.jsonl.tmp");

    let mut entries: std::collections::HashMap<u32, ReasoningEntry> =
        std::collections::HashMap::new();

    if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read reasoning.jsonl: {}", path.display()))?;

        for (lineno, line) in raw.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<ReasoningEntry>(line) {
                Ok(e) => {
                    entries.insert(e.step_id, e);
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        lineno = lineno + 1,
                        "reasoning.jsonl: skipping malformed line: {e}"
                    );
                }
            }
        }
    }

    entries.insert(entry.step_id, entry.clone());

    let mut sorted: Vec<&ReasoningEntry> = entries.values().collect();
    sorted.sort_by_key(|e| e.step_id);

    let content: String = sorted
        .iter()
        .map(|e| {
            serde_json::to_string(e).unwrap_or_else(|_| String::new())
        })
        .filter(|s| !s.is_empty())
        .map(|s| s + "\n")
        .collect();

    std::fs::write(&tmp, content.as_bytes())
        .with_context(|| format!("Failed to write reasoning.jsonl.tmp: {}", tmp.display()))?;

    std::fs::rename(&tmp, &path)
        .with_context(|| format!("Failed to rename reasoning.jsonl.tmp: {}", path.display()))?;

    tracing::debug!(step_id = entry.step_id, source = %entry.source, "reasoning.jsonl entry written");
    Ok(())
}

/// Read all entries from reasoning.jsonl.
/// Returns empty Vec if the file does not exist.
/// Skips malformed lines with a warning rather than failing.
#[allow(dead_code)]
pub fn read_all(memory_dir: &Path) -> anyhow::Result<Vec<ReasoningEntry>> {
    let path = memory_dir.join("reasoning").join("reasoning.jsonl");

    if !path.exists() {
        return Ok(Vec::new());
    }

    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read reasoning.jsonl: {}", path.display()))?;

    let mut entries = Vec::new();
    for (lineno, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<ReasoningEntry>(line) {
            Ok(e) => entries.push(e),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    lineno = lineno + 1,
                    "reasoning.jsonl: skipping malformed line: {e}"
                );
            }
        }
    }
    entries.sort_by_key(|e| e.step_id);
    Ok(entries)
}