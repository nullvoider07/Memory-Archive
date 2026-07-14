// /Memory-Archive/ma-core/src/capture/disconnect.rs

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::capture::DisconnectReason;
use crate::ipc::messages::OutboundMessage;
use crate::registry::{schema::SessionStatus, SessionRegistry};
use crate::session;

pub struct DisconnectHandler {
    session_id: String,
    memory_dir: PathBuf,
    registry: SessionRegistry,
    ipc_tx: Option<mpsc::Sender<OutboundMessage>>,
    is_cloud_primary: bool,
}

impl DisconnectHandler {
    pub fn new(
        session_id: String,
        memory_dir: PathBuf,
        registry: SessionRegistry,
        ipc_tx: Option<mpsc::Sender<OutboundMessage>>,
        is_cloud_primary: bool,
    ) -> Self {
        Self {
            session_id,
            memory_dir,
            registry,
            ipc_tx,
            is_cloud_primary,
        }
    }

    pub async fn handle(&mut self, reason: &DisconnectReason) {
        let reason_str = disconnect_reason_str(reason);

        tracing::warn!(
            session_id = %self.session_id,
            reason = %reason_str,
            "Session disconnected"
        );

        // 1. Update Redis status to Incomplete.
        if let Err(e) = self
            .registry
            .update_status(&self.session_id, SessionStatus::Incomplete)
            .await
        {
            tracing::error!(
                session_id = %self.session_id,
                "Failed to update Redis status to incomplete: {e}"
            );
        }

        if self.is_cloud_primary {
            // Cloud mode: metadata.json status is already set to "incomplete" and
            // flushed to cloud by the caller (capture/mod.rs) before handle() is
            // invoked. Directory rename has no meaning in cloud storage.
        } else {
            // Local mode: update metadata.json status on disk before renaming the
            // directory, so the file inside reflects the correct status.
            if let Err(e) = session::metadata::mark_incomplete_status(&self.memory_dir) {
                tracing::error!(
                    session_id = %self.session_id,
                    "Failed to update metadata.json status to incomplete: {e}"
                );
            }
            match session::mark_incomplete(&self.memory_dir) {
                Ok(new_dir) => {
                    // Keep the Redis record's memory_path in sync with the rename
                    // so later lookups resolve the "(incomplete)" directory.
                    if let Err(e) = self
                        .registry
                        .update_memory_path(&self.session_id, &new_dir.to_string_lossy())
                        .await
                    {
                        tracing::error!(
                            session_id = %self.session_id,
                            "Failed to update memory_path after incomplete rename: {e}"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!(
                        session_id = %self.session_id,
                        memory_dir = %self.memory_dir.display(),
                        "Failed to mark memory directory as incomplete: {e}"
                    );
                }
            }
        }

        // 2. Notify Python via IPC so the CLI can surface the disconnect.
        if let Some(ref tx) = self.ipc_tx {
            let msg = OutboundMessage::SessionDisconnected {
                session_id: self.session_id.clone(),
                reason: reason_str,
            };
            if tx.send(msg).await.is_err() {
                tracing::debug!(
                    session_id = %self.session_id,
                    "IPC disconnect notification not delivered — client already disconnected"
                );
            }
        }
    }
}

fn disconnect_reason_str(reason: &DisconnectReason) -> String {
    match reason {
        DisconnectReason::AgentDisconnected => "agent disconnected".to_string(),
        DisconnectReason::SilenceTimeout => "silence timeout".to_string(),
        DisconnectReason::TransportError(msg) => format!("transport error: {msg}"),
    }
}