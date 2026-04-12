// /Memory-Archive/ma-core/src/capture/writer.rs

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use chrono::Utc;
use ma_proto::control_center::CommandEvent;
use serde::Serialize;

use crate::storage::StorageBackend;

pub struct CommandWriter {
    commands_dir: PathBuf,
    step_counter: u32,
    is_cloud_primary: bool,
    session_id: String,
    memory_name: String,
    storage: Arc<dyn StorageBackend>,

    // In-memory buffers used only in cloud_primary mode.
    raw_input_buf: String,
    converted_input_buf: String,
    actuation_buf: String,
    cc_buf: String,

    raw_header_written: bool,
    converted_header_written: bool,
}

impl CommandWriter {
    pub fn new(
        memory_dir: &Path,
        is_cloud_primary: bool,
        session_id: String,
        storage: Arc<dyn StorageBackend>,
    ) -> Self {
        let memory_name = memory_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        Self {
            commands_dir: memory_dir.join("commands"),
            step_counter: 0,
            is_cloud_primary,
            session_id,
            memory_name,
            storage,
            raw_input_buf: String::new(),
            converted_input_buf: String::new(),
            actuation_buf: String::new(),
            cc_buf: String::new(),
            raw_header_written: false,
            converted_header_written: false,
        }
    }

    fn cloud_path(&self, relative_path: &str) -> String {
        format!("{}/{}", self.memory_name, relative_path)
    }

    /// Write one event to all four command files (local) or in-memory buffers
    /// (cloud_primary). Returns the step number assigned to this event.
    pub fn write_event(
        &mut self,
        event: &CommandEvent,
        converted: &str,
    ) -> anyhow::Result<u32> {
        self.step_counter += 1;
        let step = self.step_counter;

        if self.is_cloud_primary {
            self.buffer_raw(step, event);
            self.buffer_converted(step, event, converted);
            self.buffer_jsonl(event)?;
            self.buffer_cc_command(step, event)?;
        } else {
            self.append_raw(step, event)?;
            self.append_converted(step, event, converted)?;
            self.append_jsonl(event)?;
            self.append_cc_command(step, event)?;
        }

        tracing::debug!(
            step = step,
            action_type = %event.action_type,
            action_subtype = %event.action_subtype,
            success = event.success,
            "Command written"
        );

        Ok(step)
    }

    /// Flush in-memory command file buffers to cloud storage.
    /// No-op in local mode. Called at the same interval as metadata flush.
    pub async fn flush_to_cloud(&self) -> anyhow::Result<()> {
        if !self.is_cloud_primary {
            return Ok(());
        }

        self.storage.put(
            &self.session_id,
            &self.cloud_path("commands/raw_input.md"),
            self.raw_input_buf.as_bytes().to_vec(),
            "text/markdown",
        ).await.context("Failed to flush raw_input.md to cloud")?;

        self.storage.put(
            &self.session_id,
            &self.cloud_path("commands/converted_input.md"),
            self.converted_input_buf.as_bytes().to_vec(),
            "text/markdown",
        ).await.context("Failed to flush converted_input.md to cloud")?;

        self.storage.put(
            &self.session_id,
            &self.cloud_path("commands/actuation_commands.json"),
            self.actuation_buf.as_bytes().to_vec(),
            "application/json",
        ).await.context("Failed to flush actuation_commands.json to cloud")?;

        self.storage.put(
            &self.session_id,
            &self.cloud_path("commands/cc_commands.json"),
            self.cc_buf.as_bytes().to_vec(),
            "application/json",
        ).await.context("Failed to flush cc_commands.json to cloud")?;

        Ok(())
    }

    /// Finalise all JSONL files — convert from one-object-per-line to
    /// pretty-printed JSON arrays.
    ///
    /// In local mode: atomic disk write (temp + rename).
    /// In cloud_primary mode: finalise in-memory buffers and flush to cloud.
    ///
    /// Called once by the watch loop after `memory-archive done` is received.
    pub async fn finalise(&mut self) -> anyhow::Result<()> {
        if self.is_cloud_primary {
            self.finalise_jsonl_buf_cloud("actuation_commands.json", &self.actuation_buf.clone()).await?;
            self.finalise_jsonl_buf_cloud("cc_commands.json", &self.cc_buf.clone()).await?;
            self.flush_to_cloud().await?;
        } else {
            self.finalise_jsonl_file("actuation_commands.json")?;
            self.finalise_jsonl_file("cc_commands.json")?;
        }
        Ok(())
    }

    pub fn step_count(&self) -> u32 {
        self.step_counter
    }

    // In-memory buffer writers (cloud_primary mode)

    fn buffer_raw(&mut self, step: u32, event: &CommandEvent) {
        if !self.raw_header_written {
            self.raw_input_buf.push_str(&format!(
                "# Raw Input\n\n_Generated by Memory Archive — {}_\n\n\
                 | Step | Timestamp | Command |\n\
                 |------|-----------|----------|\n",
                Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
            ));
            self.raw_header_written = true;
        }
        let failed_prefix = if !event.success { "[FAILED] " } else { "" };
        self.raw_input_buf.push_str(&format!(
            "| {:>4} | {} | {}{} |\n",
            step, event.timestamp, failed_prefix, event.raw_command
        ));
    }

    fn buffer_converted(&mut self, step: u32, event: &CommandEvent, converted: &str) {
        if !self.converted_header_written {
            self.converted_input_buf.push_str(&format!(
                "# Converted Input\n\n_Generated by Memory Archive — {}_\n\n\
                 | Step | Timestamp | Action |\n\
                 |------|-----------|--------|\n",
                Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
            ));
            self.converted_header_written = true;
        }
        let failed_prefix = if !event.success { "[FAILED] " } else { "" };
        self.converted_input_buf.push_str(&format!(
            "| {:>4} | {} | {}{} |\n",
            step, event.timestamp, failed_prefix, converted
        ));
    }

    fn buffer_jsonl(&mut self, event: &CommandEvent) -> anyhow::Result<()> {
        let json = serde_json::to_string(event)
            .context("Failed to serialise CommandEvent to JSON")?;
        self.actuation_buf.push_str(&json);
        self.actuation_buf.push('\n');
        Ok(())
    }

    fn buffer_cc_command(&mut self, step: u32, event: &CommandEvent) -> anyhow::Result<()> {
        #[derive(Serialize)]
        struct CcCommandEntry<'a> {
            step_id: u32,
            os: &'a str,
            command: String,
        }
        let entry = CcCommandEntry {
            step_id: step,
            os: &event.os_type,
            command: crate::convert::to_cc_command(event),
        };
        let json = serde_json::to_string(&entry)
            .context("Failed to serialise CcCommandEntry")?;
        self.cc_buf.push_str(&json);
        self.cc_buf.push('\n');
        Ok(())
    }

    async fn finalise_jsonl_buf_cloud(
        &self,
        filename: &str,
        buf: &str,
    ) -> anyhow::Result<()> {
        let entries: Vec<serde_json::Value> = if buf.trim().is_empty() {
            vec![]
        } else {
            buf.lines()
                .filter(|l| !l.trim().is_empty())
                .map(|line| {
                    serde_json::from_str(line)
                        .with_context(|| format!("Failed to parse JSONL line in {filename}: {line}"))
                })
                .collect::<anyhow::Result<_>>()?
        };

        let json = serde_json::to_string_pretty(&entries)
            .with_context(|| format!("Failed to serialise {filename} array"))?;

        self.storage.put(
            &self.session_id,
            &self.cloud_path(&format!("commands/{filename}")),
            json.into_bytes(),
            "application/json",
        ).await.with_context(|| format!("Failed to upload finalised {filename} to cloud"))?;

        tracing::info!(total_entries = entries.len(), "{filename} finalised");
        Ok(())
    }

    // Local disk writers (local mode)

    fn append_raw(&self, step: u32, event: &CommandEvent) -> anyhow::Result<()> {
        let path = self.commands_dir.join("raw_input.md");
        let failed_prefix = if !event.success { "[FAILED] " } else { "" };
        let line = format!(
            "| {:>4} | {} | {}{} |\n",
            step, event.timestamp, failed_prefix, event.raw_command
        );
        self.append_line(&path, &line)
            .context("Failed to append to raw_input.md")
    }

    fn append_converted(
        &self,
        step: u32,
        event: &CommandEvent,
        converted: &str,
    ) -> anyhow::Result<()> {
        let path = self.commands_dir.join("converted_input.md");
        let failed_prefix = if !event.success { "[FAILED] " } else { "" };
        let line = format!(
            "| {:>4} | {} | {}{} |\n",
            step, event.timestamp, failed_prefix, converted
        );
        self.append_line(&path, &line)
            .context("Failed to append to converted_input.md")
    }

    fn append_jsonl(&self, event: &CommandEvent) -> anyhow::Result<()> {
        let path = self.commands_dir.join("actuation_commands.json");
        let json = serde_json::to_string(event)
            .context("Failed to serialise CommandEvent to JSON")?;
        let line = format!("{json}\n");
        self.append_line(&path, &line)
            .context("Failed to append to actuation_commands.json")
    }

    fn append_cc_command(&self, step: u32, event: &CommandEvent) -> anyhow::Result<()> {
        #[derive(Serialize)]
        struct CcCommandEntry<'a> {
            step_id: u32,
            os: &'a str,
            command: String,
        }

        let path = self.commands_dir.join("cc_commands.json");
        let entry = CcCommandEntry {
            step_id: step,
            os: &event.os_type,
            command: crate::convert::to_cc_command(event),
        };
        let json = serde_json::to_string(&entry)
            .context("Failed to serialise CcCommandEntry")?;
        let line = format!("{json}\n");
        self.append_line(&path, &line)
            .context("Failed to append to cc_commands.json")
    }

    fn finalise_jsonl_file(&self, filename: &str) -> anyhow::Result<()> {
        let jsonl_path = self.commands_dir.join(filename);

        if !jsonl_path.exists() {
            let tmp = self.commands_dir.join(format!("{filename}.tmp"));
            std::fs::write(&tmp, "[]\n")
                .with_context(|| format!("Failed to write empty {filename}"))?;
            std::fs::rename(&tmp, &jsonl_path)
                .with_context(|| format!("Failed to rename {filename}"))?;
            return Ok(());
        }

        let raw = std::fs::read_to_string(&jsonl_path)
            .with_context(|| format!("Failed to read {filename} for finalisation"))?;

        let entries: Vec<serde_json::Value> = raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line)
                    .with_context(|| format!("Failed to parse JSONL line in {filename}: {line}"))
            })
            .collect::<anyhow::Result<_>>()?;

        let json = serde_json::to_string_pretty(&entries)
            .with_context(|| format!("Failed to serialise {filename} array"))?;

        let tmp = self.commands_dir.join(format!("{filename}.tmp"));
        std::fs::write(&tmp, &json)
            .with_context(|| format!("Failed to write {filename}.tmp"))?;
        std::fs::rename(&tmp, &jsonl_path)
            .with_context(|| format!("Failed to rename {filename}"))?;

        tracing::info!(total_entries = entries.len(), "{filename} finalised");
        Ok(())
    }

    fn append_line(&self, path: &Path, line: &str) -> anyhow::Result<()> {
        let needs_header = !path.exists();

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("Failed to open: {}", path.display()))?;

        if needs_header {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                let header = match name {
                    "raw_input.md" => {
                        format!(
                            "# Raw Input\n\n_Generated by Memory Archive — {}_\n\n\
                             | Step | Timestamp | Command |\n\
                             |------|-----------|----------|\n",
                            Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
                        )
                    }
                    "converted_input.md" => {
                        format!(
                            "# Converted Input\n\n_Generated by Memory Archive — {}_\n\n\
                             | Step | Timestamp | Action |\n\
                             |------|-----------|--------|\n",
                            Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
                        )
                    }
                    _ => String::new(),
                };
                if !header.is_empty() {
                    file.write_all(header.as_bytes())
                        .context("Failed to write file header")?;
                }
            }
        }

        file.write_all(line.as_bytes())
            .with_context(|| format!("Failed to write line to: {}", path.display()))
    }
}