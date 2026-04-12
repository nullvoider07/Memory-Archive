// /Memory-Archive/ma-core/src/capture/stream.rs

use std::time::Duration;

use anyhow::Context;
use ma_proto::control_center::{
    control_service_client::ControlServiceClient,
    CommandEvent,
    WatchRequest,
};
use tokio::time::timeout;
use tonic::Streaming;

// DisconnectReason
#[derive(Debug, Clone)]
pub enum DisconnectReason {
    AgentDisconnected,
    SilenceTimeout,
    TransportError(String),
}

// WatchStream
pub struct WatchStream {
    stream: Streaming<CommandEvent>,
    silence_timeout: Duration,
    disconnect_reason: Option<DisconnectReason>,
}

// WatchStream implementation
impl WatchStream {
    pub async fn connect(addr: String, silence_timeout: Duration) -> anyhow::Result<Self> {
        const MAX_ATTEMPTS: u32 = 10;
        const BASE_DELAY_MS: u64 = 500;
        const MAX_DELAY_MS: u64 = 30_000;

        let mut attempt = 0;

        loop {
            attempt += 1;
            match Self::try_connect(&addr).await {
                Ok(stream) => {
                    tracing::info!(attempt, "Connected to Control-Center");
                    tracing::debug!(addr = %addr, "Control-Center address");
                    return Ok(Self {
                        stream,
                        silence_timeout,
                        disconnect_reason: None,
                    });
                }
                Err(e) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Err(e).with_context(|| {
                            format!(
                                "Failed to connect to Control-Center at {addr} \
                                 after {MAX_ATTEMPTS} attempts"
                            )
                        });
                    }

                    // Exponential backoff: 500ms, 1s, 2s, 4s ... capped at 30s.
                    let delay_ms = (BASE_DELAY_MS * (1 << (attempt - 1))).min(MAX_DELAY_MS);
                    tracing::warn!(
                        addr = %addr,
                        attempt,
                        next_retry_ms = delay_ms,
                        "Control-Center not ready — retrying: {e}"
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }
        }
    }

    /// Single connection attempt — separated so the retry loop stays clean.
    async fn try_connect(addr: &str) -> anyhow::Result<Streaming<CommandEvent>> {
        let mut client = ControlServiceClient::connect(addr.to_string())
            .await
            .with_context(|| format!("Failed to connect to Control-Center at {addr}"))?;

        let response = client
            .watch_commands(WatchRequest {})
            .await
            .context("Failed to initiate WatchCommands stream")?;

        Ok(response.into_inner())
    }

    pub async fn next_event(&mut self) -> Option<CommandEvent> {
        loop {
            let result = timeout(self.silence_timeout, self.stream.message()).await;

            match result {
                Err(_elapsed) => {
                    self.disconnect_reason = Some(DisconnectReason::SilenceTimeout);
                    return None;
                }
                Ok(Err(status)) => {
                    self.disconnect_reason =
                        Some(DisconnectReason::TransportError(status.to_string()));
                    return None;
                }
                Ok(Ok(None)) => {
                    self.disconnect_reason = Some(DisconnectReason::AgentDisconnected);
                    return None;
                }
                Ok(Ok(Some(event))) => {
                    if event.is_heartbeat {
                        continue;
                    }
                    return Some(event);
                }
            }
        }
    }

    pub fn disconnect_reason(&self) -> Option<DisconnectReason> {
        self.disconnect_reason.clone()
    }
}