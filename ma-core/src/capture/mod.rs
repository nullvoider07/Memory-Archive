// /Memory-Archive/ma-core/src/capture/mod.rs

pub mod disconnect;
pub mod session_state;
pub mod stream;
pub mod writer;

pub use disconnect::DisconnectHandler;
pub use session_state::CaptureState;
pub use stream::{DisconnectReason, WatchStream};
pub use writer::CommandWriter;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ma_proto::control_center::CommandEvent;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::config::Config;
use crate::ipc::messages::OutboundMessage;
use crate::kafka::KafkaSessionMap;
use crate::registry::{schema::SessionStatus, SessionRegistry};
use crate::session::metadata::StepEntry;
use crate::storage::StorageBackend;
use crate::vision::VisionPipeline;

pub type DoneHandleMap = Arc<Mutex<HashMap<String, (oneshot::Sender<()>, oneshot::Receiver<u32>)>>>;
pub type PushHandleMap = Arc<Mutex<HashMap<String, mpsc::Sender<OutboundMessage>>>>;
pub type ReasoningMapsRef = Arc<ReasoningMaps>;

pub struct ReasoningMaps {
    tokens: tokio::sync::Mutex<HashMap<String, (u64, u64)>>,
    provider_tokens: tokio::sync::Mutex<HashMap<String, HashMap<String, (u64, u64)>>>,
    write_locks: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    degraded: tokio::sync::Mutex<std::collections::HashSet<String>>,
}

impl Default for ReasoningMaps {
    fn default() -> Self {
        Self {
            tokens: tokio::sync::Mutex::new(HashMap::new()),
            provider_tokens: tokio::sync::Mutex::new(HashMap::new()),
            write_locks: tokio::sync::Mutex::new(HashMap::new()),
            degraded: tokio::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }
}

impl ReasoningMaps {
    pub async fn add_tokens(&self, session_id: &str, input: u64, output: u64) {
        let mut map = self.tokens.lock().await;
        let entry = map.entry(session_id.to_string()).or_insert((0, 0));
        entry.0 += input;
        entry.1 += output;
    }

    pub async fn drain_tokens(&self, session_id: &str) -> (u64, u64) {
        let mut map = self.tokens.lock().await;
        map.remove(session_id).unwrap_or((0, 0))
    }

    /// Record token counts for a specific provider within a session.
    /// Called from the ReasoningResult IPC handler alongside add_tokens.
    /// Empty provider strings are ignored — manual-mode entries have no provider.
    pub async fn add_provider_tokens(
        &self,
        session_id: &str,
        provider: &str,
        input: u64,
        output: u64,
    ) {
        if provider.is_empty() {
            return;
        }
        let mut outer = self.provider_tokens.lock().await;
        let inner = outer.entry(session_id.to_string()).or_insert_with(HashMap::new);
        let entry = inner.entry(provider.to_string()).or_insert((0, 0));
        entry.0 += input;
        entry.1 += output;
    }

    /// Drain and return all per-provider token counts for a session.
    /// Called from CaptureState::flush() alongside drain_tokens().
    /// Returns an empty map if no provider tokens have been recorded.
    pub async fn drain_provider_tokens(
        &self,
        session_id: &str,
    ) -> HashMap<String, (u64, u64)> {
        let mut outer = self.provider_tokens.lock().await;
        outer.remove(session_id).unwrap_or_default()
    }

    pub async fn session_write_lock(&self, session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.write_locks.lock().await;
        map.entry(session_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    pub async fn mark_degraded(&self, session_id: &str) {
        self.degraded.lock().await.insert(session_id.to_string());
    }

    pub async fn reset_degraded(&self, session_id: &str) {
        self.degraded.lock().await.remove(session_id);
    }

    pub async fn is_degraded(&self, session_id: &str) -> bool {
        self.degraded.lock().await.contains(session_id)
    }

    pub async fn remove_session(&self, session_id: &str) {
        self.tokens.lock().await.remove(session_id);
        self.provider_tokens.lock().await.remove(session_id);
        self.write_locks.lock().await.remove(session_id);
        self.degraded.lock().await.remove(session_id);
    }
}

pub struct EventWithPosition {
    pub event: CommandEvent,
    pub kafka_partition: Option<i32>,
    pub kafka_offset: Option<i64>,
}

pub enum EventSource {
    Grpc(WatchStream),
    Kafka(mpsc::Receiver<crate::kafka::KafkaEvent>),
}

impl EventSource {
    pub async fn next_event(&mut self) -> Option<EventWithPosition> {
        match self {
            EventSource::Grpc(stream) => {
                stream.next_event().await.map(|event| EventWithPosition {
                    event,
                    kafka_partition: None,
                    kafka_offset: None,
                })
            }
            EventSource::Kafka(rx) => {
                rx.recv().await.map(|ke| EventWithPosition {
                    event: ke.event,
                    kafka_partition: Some(ke.partition),
                    kafka_offset: Some(ke.offset),
                })
            }
        }
    }

    pub fn disconnect_reason(&self) -> Option<DisconnectReason> {
        match self {
            EventSource::Grpc(stream) => stream.disconnect_reason(),
            EventSource::Kafka(_) => None,
        }
    }
}

pub async fn run_watch_loop(
    session_id: String,
    mut registry: SessionRegistry,
    config: Config,
    push_tx: mpsc::Sender<OutboundMessage>,
    done_handles: DoneHandleMap,
    kafka_session_map: KafkaSessionMap,
    storage: Arc<dyn StorageBackend>,
    reasoning_maps: ReasoningMapsRef,
) {
    let record = match registry.get(&session_id).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(session_id = %session_id, "Watch loop: failed to get session: {e}");
            return;
        }
    };

    let memory_dir = PathBuf::from(&record.memory_path);
    let is_cloud_primary = config.storage_mode == "cloud_primary";

    // resolve per-session server addresses.
    // Use the address stored in the SessionRecord if the orchestration layer
    // supplied one at RegisterSession time; fall back to the global config
    // value for single-machine or dev deployments where per-session addresses
    // are not passed. Increment the metric for each address resolved.
    let effective_the_eyes_addr: String;
    let effective_cc_addr: String;
    {
        let per_session_eyes = !record.the_eyes_addr.is_empty();
        let per_session_cc   = !record.capture_server_addr.is_empty();

        effective_the_eyes_addr = if per_session_eyes {
            record.the_eyes_addr.clone()
        } else {
            config.the_eyes_addr.clone()
        };

        effective_cc_addr = if per_session_cc {
            record.capture_server_addr.clone()
        } else {
            config.control_center_addr.clone()
        };

        // increment address source metric.
        // We track Eyes and CC separately — they may come from different sources
        // in theory, but in practice both are either per-session or both global.
        // The spec tracks one counter covering both addresses as a pair; we
        // report whichever Eyes source governs (Eyes is the binding source
        // since it is always used, whereas CC is only used in direct gRPC mode).
        if per_session_eyes {
            crate::observability::metrics()
                .session_server_address_source
                .get_or_create(&crate::observability::ServerAddressSourceLabels {
                    source: "per_session".to_string(),
                })
                .inc();
        } else {
            crate::observability::metrics()
                .session_server_address_source
                .get_or_create(&crate::observability::ServerAddressSourceLabels {
                    source: "global_config".to_string(),
                })
                .inc();
        }
    }

    let initial_metadata = if is_cloud_primary {
        let cloud_meta_path = format!("{}/metadata.json", record.memory_name);
        match storage.get(&session_id, &cloud_meta_path).await {
            Ok(bytes) => match crate::session::metadata::from_bytes(&bytes) {
                Ok(m) => m,
                Err(e) => {
                    tracing::error!(session_id = %session_id, "Watch loop: failed to parse cloud metadata: {e}");
                    return;
                }
            },
            Err(e) => {
                tracing::error!(session_id = %session_id, "Watch loop: failed to read metadata from cloud: {e}");
                return;
            }
        }
    } else {
        match crate::session::metadata::read(&memory_dir) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(session_id = %session_id, "Watch loop: failed to read metadata: {e}");
                return;
            }
        }
    };

    let mut state = CaptureState::new(
        initial_metadata,
        session_id.clone(),
        memory_dir.clone(),
        storage.clone(),
        config.metadata_flush_interval,
        is_cloud_primary,
        reasoning_maps.clone(),
    );

    let use_kafka = !config.kafka_broker.is_empty() && is_cloud_primary;

    let mut event_source = if use_kafka {
        let (tx, rx) = mpsc::channel::<crate::kafka::KafkaEvent>(config.kafka_channel_capacity);
        kafka_session_map.lock().await.insert(session_id.clone(), tx);
        tracing::info!(session_id = %session_id, "Watch loop: using Kafka event source");
        EventSource::Kafka(rx)
    } else {
        let cc_addr = effective_cc_addr.clone();
        if cc_addr.is_empty() {
            tracing::error!(
                session_id = %session_id,
                "control_center_addr not set — run: memory-archive config --control-center-addr http://<host>:<port>"
            );
            return;
        }
        let silence_timeout = Duration::from_secs(config.silence_timeout_seconds);
        match WatchStream::connect(cc_addr.clone(), silence_timeout).await {
            Ok(s) => {
                tracing::info!(
                    session_id = %session_id,
                    cc_addr = %cc_addr,
                    "Watch loop: using direct gRPC event source"
                );
                EventSource::Grpc(s)
            }
            Err(e) => {
                tracing::error!(session_id = %session_id, "Failed to connect to Control-Center at {cc_addr}: {e}");
                return;
            }
        }
    };

    let is_automated = record.mode == crate::registry::schema::SessionMode::Automated;

    let vision = VisionPipeline::new(
        &effective_the_eyes_addr,
        &memory_dir,
        &config,
        session_id.clone(),
        storage.clone(),
        is_automated,
    ).await;

    let (eyes_down_tx, mut eyes_down_rx) = oneshot::channel::<()>();

    if vision.is_some() {
        let poll_interval = Duration::from_secs(config.the_eyes_poll_interval_seconds);
        let addr = effective_the_eyes_addr.clone();
        tokio::spawn(async move {
            let client = match crate::vision::client::EyesClient::new(addr) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("Liveness monitor: failed to create client: {e}");
                    let _ = eyes_down_tx.send(());
                    return;
                }
            };
            loop {
                tokio::time::sleep(poll_interval).await;
                if !client.is_alive().await {
                    tracing::warn!("The-Eyes liveness check failed — triggering disconnect");
                    let _ = eyes_down_tx.send(());
                    return;
                }
            }
        });
    }

    let (done_tx, mut done_rx) = oneshot::channel::<()>();
    let (result_tx, result_rx) = oneshot::channel::<u32>();
    done_handles
        .lock()
        .await
        .insert(session_id.clone(), (done_tx, result_rx));

    state.set_in_progress("capturing");
    if let Err(e) = state.flush().await {
        tracing::error!(session_id = %session_id, "Failed to flush initial in_progress state: {e}");
    }

    tracing::info!(session_id = %session_id, "Watch loop started");
    crate::observability::metrics().active_sessions.inc();

    if is_automated {
        let started_msg = OutboundMessage::SessionStarted {
            session_id: session_id.clone(),
        };
        if let Err(e) = push_tx.try_send(started_msg) {
            tracing::warn!(session_id = %session_id, "SessionStarted push dropped: {e}");
        }
    }

    let mut writer = CommandWriter::new(
        &memory_dir,
        is_cloud_primary,
        session_id.clone(),
        storage.clone(),
    );
    let mut last_click: Option<(i32, i32)> = None;
    let sync_tx = push_tx.clone();
    let mut disconnect_handler = DisconnectHandler::new(
        session_id.clone(),
        memory_dir.clone(),
        registry.clone(),
        Some(push_tx),
        is_cloud_primary,
    );

    let emit_file_written = |tx: &mpsc::Sender<OutboundMessage>, rel: &str| {
        if is_cloud_primary {
            return;
        }
        let abs = memory_dir.join(rel).to_string_lossy().to_string();
        let msg = OutboundMessage::FileWritten {
            session_id: session_id.clone(),
            relative_path: rel.to_string(),
            abs_path: abs,
        };
        crate::observability::metrics().ipc_push_queue_depth.inc();
        if let Err(e) = tx.try_send(msg) {
            crate::observability::metrics().ipc_push_queue_depth.dec();
            tracing::warn!("FileWritten push dropped: {e}");
        }
    };

    let mut done_cleanly = false;

    loop {
        tokio::select! {
            event_opt = event_source.next_event() => {
                match event_opt {
                    None => break,
                    Some(ep) => {
                        let event = ep.event;

                        if event.action_type == "position" {
                            continue
                        }

                        if let (Some(partition), Some(offset)) = (ep.kafka_partition, ep.kafka_offset) {
                            state.update_kafka_position(partition, offset);
                        }

                        let converted = crate::convert::to_human_readable(&event);

                        let step = match writer.write_event(&event, &converted) {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::error!(session_id = %session_id, "Failed to write event: {e}");
                                continue;
                            }
                        };
                        crate::observability::metrics().steps_total.inc();
                        emit_file_written(&sync_tx, "commands/raw_input.md");
                        emit_file_written(&sync_tx, "commands/converted_input.md");
                        emit_file_written(&sync_tx, "commands/actuation_commands.json");
                        emit_file_written(&sync_tx, "commands/cc_commands.json");

                        let step_entry = StepEntry {
                            step_id: step,
                            timestamp: event.timestamp.clone(),
                            action_type: event.action_type.clone(),
                            action_subtype: event.action_subtype.clone(),
                            image_path: None,
                            image_fetched: false,
                            marked: false,
                            before_image_path: None,
                            after_image_path: None,
                            raw_command: event.raw_command.clone(),
                            converted_command: converted.clone(),
                        };
                        state.append_step(step_entry);

                        if event.action_type == "mouse" && event.success {
                            last_click = Some((event.mouse_x, event.mouse_y));
                        }

                        if let Some(ref vp) = vision {
                            match vp.process(&event, step, last_click).await {
                                Some(result) => {
                                    let img_path = result.image_path.clone();
                                    let before   = result.before_image_path.clone();
                                    let after    = result.after_image_path.clone();

                                    if is_automated && !reasoning_maps.is_degraded(&session_id).await {
                                        let ctx = state.recent_steps_context(record.context_window_steps);
                                        let push_msg = OutboundMessage::StepReadyForReasoning {
                                            session_id: session_id.clone(),
                                            step_id: step,
                                            action_type: event.action_type.clone(),
                                            action_subtype: event.action_subtype.clone(),
                                            converted_command: converted.clone(),
                                            at_frame_bytes: result.at_frame_bytes.clone(),
                                            before_frame_bytes: result.before_frame_bytes.clone(),
                                            after_frame_bytes: result.after_frame_bytes.clone(),
                                            context_steps: ctx,
                                        };
                                        crate::observability::metrics().ipc_push_queue_depth.inc();
                                        if let Err(e) = sync_tx.try_send(push_msg) {
                                            crate::observability::metrics().ipc_push_queue_depth.dec();
                                            tracing::warn!(
                                                session_id = %session_id,
                                                step_id = step,
                                                "StepReadyForReasoning push dropped — pipeline falling behind: {e}"
                                            );
                                        }
                                    }

                                    state.update_step_image(
                                        step,
                                        result.image_path,
                                        result.marked,
                                        result.before_image_path,
                                        result.after_image_path,
                                    );
                                    emit_file_written(&sync_tx, &img_path);
                                    if let Some(p) = before { emit_file_written(&sync_tx, &p); }
                                    if let Some(p) = after  { emit_file_written(&sync_tx, &p); }
                                }
                                None => {
                                    let reason = if !event.success {
                                        "command failed"
                                    } else if matches!(
                                        crate::vision::decide(&event, config.press_fetch_delay_ms, config.type_fetch_delay_ms),
                                        crate::vision::FetchDecision::Skip { .. }
                                    ) {
                                        "action type does not trigger fetch"
                                    } else {
                                        "fetch failed"
                                    };
                                    state.append_skipped_fetch(step, reason);
                                }
                            }
                        }

                        if let Err(e) = state.flush_if_due().await {
                            tracing::error!(session_id = %session_id, "Metadata flush failed: {e}");
                        } else {
                            emit_file_written(&sync_tx, "metadata.json");
                            if is_cloud_primary {
                                if let Err(e) = writer.flush_to_cloud().await {
                                    tracing::error!(session_id = %session_id, "Command file flush failed: {e}");
                                }
                            }
                        }
                    }
                }
            }

            _ = &mut done_rx => {
                done_cleanly = true;
                break;
            }

            _ = &mut eyes_down_rx => {
                tracing::warn!(session_id = %session_id, "The-Eyes went down — ending session as incomplete");
                break;
            }
        }
    }

    kafka_session_map.lock().await.remove(&session_id);
    let was_degraded = reasoning_maps.is_degraded(&session_id).await;
    reasoning_maps.remove_session(&session_id).await;

    if done_cleanly {
        if let Err(e) = writer.finalise().await {
            tracing::error!(session_id = %session_id, "finalise() failed: {e}");
        } else {
            emit_file_written(&sync_tx, "commands/actuation_commands.json");
            emit_file_written(&sync_tx, "commands/cc_commands.json");
        }

        state.mark_complete();
        state.clear_in_progress();
        if let Err(e) = state.flush_now().await {
            tracing::error!(session_id = %session_id, "Final metadata flush failed: {e}");
        } else {
            emit_file_written(&sync_tx, "metadata.json");
        }

        if is_automated {
            write_degraded_placeholders(
                &session_id,
                &memory_dir,
                &record.memory_name,
                &state,
                is_cloud_primary,
                &storage,
            ).await;
        }

        let next_status = if was_degraded {
            SessionStatus::PendingHumanAnnotation
        } else {
            SessionStatus::PendingAnnotation
        };

        if let Err(e) = registry
            .update_status(&session_id, next_status)
            .await
        {
            tracing::error!(session_id = %session_id, "Failed to update Redis status: {e}");
        }

        tracing::info!(
            session_id = %session_id,
            total_steps = writer.step_count(),
            was_degraded,
            "Session complete — watch loop done"
        );
    } else {
        if is_cloud_primary {
            state.mark_incomplete();
            state.set_in_progress("interrupted");
            if let Err(e) = writer.finalise().await {
                tracing::warn!(session_id = %session_id, "Failed to finalise command files on disconnect: {e}");
            }
            if let Err(e) = state.flush_now().await {
                tracing::warn!(session_id = %session_id, "Failed to flush on disconnect: {e}");
            }
        }
        let reason = event_source
            .disconnect_reason()
            .unwrap_or(DisconnectReason::TransportError(
                "The-Eyes liveness check failed".to_string(),
            ));
        disconnect_handler.handle(&reason).await;
    }

    crate::observability::metrics().active_sessions.dec();
    done_handles.lock().await.remove(&session_id);
    let _ = result_tx.send(writer.step_count());
    tracing::info!(session_id = %session_id, "Watch loop ended");

    /// Write model_degraded placeholder entries to reasoning.jsonl for any steps
/// that completed without VLM reasoning. Called once on clean done in automated mode.
/// Steps that already have an entry (VLM responded in time) are left untouched — 
/// upsert_entry will not overwrite them because we only write for missing step_ids.
async fn write_degraded_placeholders(
    session_id: &str,
    memory_dir: &std::path::Path,
    memory_name: &str,
    state: &CaptureState,
    is_cloud_primary: bool,
    storage: &Arc<dyn StorageBackend>,
) {
    let steps = state.all_steps();
    if steps.is_empty() {
        return;
    }

    let jsonl_path = memory_dir.join("reasoning").join("reasoning.jsonl");
    if let Err(e) = tokio::fs::create_dir_all(memory_dir.join("reasoning")).await {
        tracing::warn!(session_id, "write_degraded_placeholders: failed to create dir: {e}");
        return;
    }

    let existing_ids: std::collections::HashSet<u32> = if jsonl_path.exists() {
        match tokio::fs::read_to_string(&jsonl_path).await {
            Ok(raw) => raw.lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .filter_map(|v| v.get("step_id").and_then(|s| s.as_u64()).map(|n| n as u32))
                .collect(),
            Err(_) => std::collections::HashSet::new(),
        }
    } else {
        std::collections::HashSet::new()
    };

    let mut wrote_any = false;
    for step in steps {
        if existing_ids.contains(&step.step_id) {
            continue;
        }
        let entry = crate::session::reasoning::ReasoningEntry {
            step_id: step.step_id,
            timestamp_action: step.timestamp.clone(),
            timestamp_annotated: chrono::Utc::now().to_rfc3339(),
            action_type: step.action_type.clone(),
            action_subtype: step.action_subtype.clone(),
            raw_command: step.raw_command.clone(),
            converted_command: step.converted_command.clone(),
            image_path: step.image_path.clone(),
            reasoning: String::new(),
            skipped: false,
            source: "model_degraded".to_string(),
            provider: None,
            model_id: None,
            api_version: None,
            input_tokens: None,
            output_tokens: None,
            latency_ms: None,
            action_intent: None,
            confidence: None,
            keyboard_visual_annotation: None,
        };
        if let Err(e) = crate::session::reasoning::upsert_entry(memory_dir, &entry) {
            tracing::warn!(
                session_id,
                step_id = step.step_id,
                "write_degraded_placeholders: upsert failed: {e}"
            );
        } else {
            wrote_any = true;
        }
    }

    if wrote_any && is_cloud_primary {
        match tokio::fs::read(&jsonl_path).await {
            Ok(bytes) => {
                let cloud_path = format!("{}/reasoning/reasoning.jsonl", memory_name);
                if let Err(e) = storage.put(session_id, &cloud_path, bytes, "application/json").await {
                    tracing::warn!(session_id, "write_degraded_placeholders: cloud upload failed: {e}");
                }
            }
            Err(e) => {
                tracing::warn!(session_id, "write_degraded_placeholders: failed to read for upload: {e}");
            }
        }
    }
}
}