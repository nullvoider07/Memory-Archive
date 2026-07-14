// /Memory-Archive/ma-core/src/main.rs

mod annotator_management;
mod capture;
mod config;
mod convert;
mod ipc;
mod kafka;
mod observability;
mod registry;
mod session;
mod storage;
mod tls;
mod vision;

/// Decide the recovery status for a stale in-flight session found by the reconcile
/// sweep on startup. `redis_status` is the live status from Redis (one of
/// Annotating / PendingCompilation / ReasoningDegraded); `metadata_status` is the
/// `status` string from metadata.json. Returns `Some(target)` to update Redis, or
/// `None` to leave the session as-is.
///
/// metadata.json.status is frozen at "complete" by `mark_complete()` once capture
/// ends, so in local mode it does not track the annotation/compilation
/// sub-lifecycle. It is authoritative only when it carries a genuine resumable
/// status (cloud-primary crash recovery via Kafka replay writes one through
/// `mark_recovered`). When it reads "complete", the live Redis status is trusted and
/// the intended resumable state is restored: an interrupted annotation returns to
/// `pending_annotation`; `pending_compilation` and `reasoning_degraded` are already
/// resumable and are left untouched.
fn reconcile_target(
    redis_status: &registry::schema::SessionStatus,
    metadata_status: &str,
) -> Option<registry::schema::SessionStatus> {
    use registry::schema::SessionStatus;
    if metadata_status == "complete" {
        match redis_status {
            SessionStatus::Annotating => Some(SessionStatus::PendingAnnotation),
            _ => None,
        }
    } else if redis_status.to_string() == metadata_status {
        None
    } else {
        match metadata_status {
            "pending_annotation" => Some(SessionStatus::PendingAnnotation),
            "pending_human_annotation" => Some(SessionStatus::PendingHumanAnnotation),
            "pending_compilation" => Some(SessionStatus::PendingCompilation),
            "reasoning_degraded" => Some(SessionStatus::ReasoningDegraded),
            "incomplete" => Some(SessionStatus::Incomplete),
            _ => None,
        }
    }
}

/// Decide whether a session found in `sessions:active` by the startup sweep is
/// actually a finished recording whose Redis state went stale — e.g. an unclean
/// host shutdown rolled Redis back to a snapshot taken before `done` ran.
/// metadata.json.status is written as "complete" only by the clean `done` path
/// after the final flush, so it is authoritative here: the recording on disk is
/// finished and intact, and the session must be restored to the post-capture
/// status rather than demoted to incomplete (which would also rename its
/// directory). Returns the status to restore, or `None` to fall through to the
/// normal interrupted-session handling. A session that had already progressed
/// past pending_annotation before the rollback (annotated or finalized) is also
/// restored to PendingAnnotation — the sub-lifecycle lives only in Redis and
/// cannot be reconstructed, and pending_annotation is resumable from every later
/// stage.
fn active_sweep_restore_target(metadata_status: &str) -> Option<registry::schema::SessionStatus> {
    if metadata_status == "complete" {
        Some(registry::schema::SessionStatus::PendingAnnotation)
    } else {
        None
    }
}

/// Best-effort check that `pid` belongs to a running ma-core process. Guards the
/// stale-PID takeover against PID reuse: after a reboot the recorded PID can
/// belong to an arbitrary unrelated process, which must not be signalled.
#[cfg(unix)]
fn process_is_ma_core(pid: i32) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .map(|o| {
            let comm = String::from_utf8_lossy(&o.stdout);
            comm.trim()
                .rsplit('/')
                .next()
                .map(|name| name == "ma-core")
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

#[cfg(windows)]
fn process_is_ma_core(pid: i32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("ma-core"))
        .unwrap_or(false)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls ring crypto provider");

    let cfg = config::load()?;

    observability::init_tracing(&cfg.observability);

    tracing::info!("ma-core starting — v{}", env!("CARGO_PKG_VERSION"));
    tracing::info!("Config loaded from: {}", config::config_path().display());

    if let Err(e) = observability::init_metrics(&cfg.observability) {
        tracing::error!("Failed to initialize metrics endpoint: {e}");
    }

    // PID file lives next to config.json (~/.memory-archive/ma-core.pid) — the
    // same location the CLI's `server stop` reads. Previously it was written to
    // the storage_path parent, which the CLI never checks and which pollutes the
    // capture tree.
    let pid_path = config::config_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(std::env::temp_dir)
        .join("ma-core.pid");
    if pid_path.exists() {
        if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                if process_is_ma_core(pid) {
                    tracing::warn!(pid, "Existing ma-core process found — terminating stale process");
                    #[cfg(unix)]
                    {
                        unsafe { libc::kill(pid, libc::SIGTERM); }
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                    #[cfg(windows)]
                    {
                        let _ = std::process::Command::new("taskkill")
                            .args(["/PID", &pid.to_string(), "/F"])
                            .status();
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                } else {
                    tracing::warn!(
                        pid,
                        "Stale PID file does not point at a ma-core process — ignoring"
                    );
                }
            }
        }
    }
    if let Err(e) = std::fs::write(&pid_path, std::process::id().to_string()) {
        tracing::warn!("Failed to write PID file: {e}");
    }

    let mut registry = registry::SessionRegistry::connect(&cfg.redis_url).await?;
    tracing::info!("Redis session registry ready");

    let storage_router = storage::build_router(&cfg).await;
    tracing::info!(
        storage_mode = %cfg.storage_mode,
        "Storage router initialized"
    );

    let is_cloud_primary_with_kafka =
        cfg.storage_mode == "cloud_primary" && !cfg.kafka_broker.is_empty();

    let reasoning_maps = std::sync::Arc::new(crate::capture::ReasoningMaps::default());

    // Stale index cleanup — remove set members whose Redis Hash has expired.
    // This prevents accumulation of ghost entries in index sets after TTL expiry.
    for set_key in &[
        "sessions:active",
        "sessions:pending",
        "sessions:pending_human_annotation",
        "sessions:annotating",
        "sessions:pending_compilation",
        "sessions:reasoning_degraded",
    ] {
        match registry.cleanup_stale_index_entries(set_key).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(count = n, set = set_key, "Stale index entries removed"),
            Err(e) => tracing::warn!(set = set_key, "Stale index cleanup failed: {e}"),
        }
    }

    match registry.list_active().await {
        Err(e) => {
            tracing::error!("Startup sweep: failed to query active sessions: {e}");
        }
        Ok(sessions) if sessions.is_empty() => {
            tracing::info!("Startup sweep: no active sessions found");
        }
        Ok(sessions) => {
            for session_id in sessions {
                let record = match registry.get(&session_id).await {
                    Err(e) => {
                        tracing::error!(session_id = %session_id, "Startup sweep: failed to get record: {e}");
                        if let Err(re) = registry.remove_from_active_set(&session_id).await {
                            tracing::warn!(session_id = %session_id, "Startup sweep: failed to remove stale active set entry: {re}");
                        }
                        continue;
                    }
                    Ok(r) => r,
                };

                let memory_dir = std::path::PathBuf::from(&record.memory_path);

                // Resolve the session-specific backend using the pinned storage_backend field.
                let session_storage = storage_router.resolve_for_session(&record);

                let meta = if is_cloud_primary_with_kafka {
                    match session_storage.get(&session_id, &format!("{}/metadata.json", record.memory_name)).await {
                        Ok(bytes) => session::metadata::from_bytes(&bytes).ok(),
                        Err(_) => session::metadata::read(&memory_dir).ok(),
                    }
                } else {
                    session::metadata::read(&memory_dir).ok()
                };

                // If metadata cannot be read from either cloud or local disk,
                // the session is unrecoverable — mark it incomplete.
                let meta = match meta {
                    Some(m) => m,
                    None => {
                        tracing::warn!(
                            session_id = %session_id,
                            "Startup sweep: cannot read metadata — marking incomplete"
                        );
                        if !is_cloud_primary_with_kafka {
                            match session::mark_incomplete(&memory_dir) {
                                Ok(new_dir) => {
                                    if let Err(e) = registry
                                        .update_memory_path(&session_id, &new_dir.to_string_lossy())
                                        .await
                                    {
                                        tracing::error!(session_id = %session_id, "Startup sweep: memory_path update failed: {e}");
                                    }
                                }
                                Err(e) => {
                                    tracing::error!(session_id = %session_id, "Startup sweep: mark_incomplete failed: {e}");
                                }
                            }
                        }
                        if let Err(e) = registry
                            .update_status(&session_id, registry::schema::SessionStatus::Incomplete)
                            .await
                        {
                            tracing::error!(session_id = %session_id, "Startup sweep: Redis update failed: {e}");
                        }
                        continue;
                    }
                };

                // A finished recording (metadata.json frozen at "complete" by the
                // clean `done` path) listed as active means Redis is stale — e.g.
                // an unclean host shutdown rolled Redis back to a snapshot taken
                // before `done` updated it. Restore the post-capture status; do
                // NOT mark incomplete (that would also rename the memory dir of a
                // fully intact recording).
                if let Some(restore) = active_sweep_restore_target(&meta.status) {
                    tracing::warn!(
                        session_id = %session_id,
                        "Startup sweep: session is active in Redis but metadata.json is complete — restoring resumable status"
                    );
                    if let Err(e) = registry.update_status(&session_id, restore).await {
                        tracing::error!(session_id = %session_id, "Startup sweep: Redis restore failed: {e}");
                    }
                    continue;
                }

                let in_progress = meta.in_progress.as_deref();

                match in_progress {
                    None => {
                        // Session is active in Redis but has no in_progress flag —
                        // either registered but never started, or the watch loop
                        // was killed before it could flush the in_progress field.
                        // Either way it cannot resume: mark incomplete.
                        tracing::warn!(
                            session_id = %session_id,
                            "Startup sweep: active session with no in_progress flag — marking incomplete"
                        );
                        if !is_cloud_primary_with_kafka {
                            match session::mark_incomplete(&memory_dir) {
                                Ok(new_dir) => {
                                    if let Err(e) = registry
                                        .update_memory_path(&session_id, &new_dir.to_string_lossy())
                                        .await
                                    {
                                        tracing::error!(session_id = %session_id, "Startup sweep: memory_path update failed: {e}");
                                    }
                                }
                                Err(e) => {
                                    tracing::error!(session_id = %session_id, "Startup sweep: mark_incomplete failed: {e}");
                                }
                            }
                        }
                        if let Err(e) = registry
                            .update_status(&session_id, registry::schema::SessionStatus::Incomplete)
                            .await
                        {
                            tracing::error!(session_id = %session_id, "Startup sweep: Redis update failed: {e}");
                        }
                        continue;
                    }
                    Some("capturing") | Some("interrupted") => {}
                    Some(other) => {
                        tracing::debug!(
                            session_id = %session_id,
                            in_progress = %other,
                            "Startup sweep: unrecognised in_progress value — skipping"
                        );
                        continue;
                    }
                }

                // cloud_primary + Kafka: attempt Kafka replay recovery.
                if is_cloud_primary_with_kafka && in_progress == Some("interrupted") {
                    if let (Some(partition), Some(offset)) =
                        (meta.kafka_partition, meta.kafka_offset)
                    {
                        tracing::info!(
                            session_id = %session_id,
                            partition,
                            offset,
                            "Startup sweep: attempting Kafka replay recovery"
                        );

                        let recovery_result = async {
                            let mut rx = crate::kafka::consumer::replay_session_events(
                                &cfg.kafka_broker,
                                &session_id,
                                partition,
                                offset + 1,
                                cfg.kafka_channel_capacity,
                            )
                            .await?;

                            let mut state = crate::capture::CaptureState::new(
                                meta.clone(),
                                session_id.clone(),
                                memory_dir.clone(),
                                session_storage.clone(),
                                cfg.metadata_flush_interval,
                                true,
                                reasoning_maps.clone(),
                            );

                            while let Some(ke) = rx.recv().await {
                                if ke.event.action_type == "position" {
                                    continue;
                                }
                                let step_entry = crate::session::metadata::StepEntry {
                                    step_id: state.total_steps() + 1,
                                    timestamp: ke.event.timestamp.clone(),
                                    action_type: ke.event.action_type.clone(),
                                    action_subtype: ke.event.action_subtype.clone(),
                                    raw_command: ke.event.raw_command.clone(),
                                    converted_command: crate::convert::to_human_readable(&ke.event),
                                    image_path: None,
                                    image_fetched: false,
                                    marked: false,
                                    before_image_path: None,
                                    after_image_path: None,
                                };
                                state.append_step(step_entry);
                                state.update_kafka_position(ke.partition, ke.offset);
                            }

                            state.mark_recovered();
                            state.flush_now().await?;

                            anyhow::Ok(())
                        }
                        .await;

                        match recovery_result {
                            Ok(()) => {
                                if let Err(e) = registry
                                    .update_status(
                                        &session_id,
                                        registry::schema::SessionStatus::PendingAnnotation,
                                    )
                                    .await
                                {
                                    tracing::error!(
                                        session_id = %session_id,
                                        "Startup sweep: Redis update after recovery failed: {e}"
                                    );
                                } else {
                                    tracing::info!(
                                        session_id = %session_id,
                                        "Startup sweep: Kafka replay recovery complete"
                                    );
                                }
                                continue;
                            }
                            Err(e) => {
                                tracing::error!(
                                    session_id = %session_id,
                                    "Startup sweep: Kafka replay recovery failed: {e} — marking incomplete"
                                );
                            }
                        }
                    }
                }

                // local mode, no Kafka, or recovery failed — mark incomplete.
                tracing::warn!(
                    session_id = %session_id,
                    "Startup sweep: marking session incomplete"
                );

                if !is_cloud_primary_with_kafka {
                    match session::mark_incomplete(&memory_dir) {
                        Ok(new_dir) => {
                            if let Err(e) = registry
                                .update_memory_path(&session_id, &new_dir.to_string_lossy())
                                .await
                            {
                                tracing::error!(session_id = %session_id, "Startup sweep: memory_path update failed: {e}");
                            }
                        }
                        Err(e) => {
                            tracing::error!(session_id = %session_id, "Startup sweep: mark_incomplete failed: {e}");
                        }
                    }
                }

                if let Err(e) = registry
                    .update_status(&session_id, registry::schema::SessionStatus::Incomplete)
                    .await
                {
                    tracing::error!(session_id = %session_id, "Startup sweep: Redis update failed: {e}");
                }
            }
        }
    }

    // Reconcile sweep — recover annotating / pending_compilation /
    // reasoning_degraded sessions left stale in Redis by a prior unclean exit.
    //
    // metadata.json.status is frozen at "complete" by mark_complete() when capture
    // finishes; in local mode it never tracks the annotation/compilation
    // sub-lifecycle, which lives only in Redis. Mirroring it back into Redis would
    // demote every interrupted in-flight session to "complete" and make it
    // un-resumable. So metadata is treated as authoritative only when it carries a
    // genuine resumable status (cloud-primary crash recovery via Kafka replay writes
    // one through mark_recovered); when it reads "complete", the Redis status is
    // trusted and the intended resumable state is restored instead.
    let stale_candidates = {
        let mut ids = Vec::new();
        if let Ok(v) = registry.list_annotating().await { ids.extend(v); }
        if let Ok(v) = registry.list_pending_compilation().await { ids.extend(v); }
        if let Ok(v) = registry.list_reasoning_degraded().await { ids.extend(v); }
        ids
    };
    for session_id in stale_candidates {
        let record = match registry.get(&session_id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(session_id = %session_id, "Reconcile sweep: failed to get record: {e}");
                continue;
            }
        };
        let memory_dir = std::path::PathBuf::from(&record.memory_path);
        let session_storage = storage_router.resolve_for_session(&record);
        let meta = if is_cloud_primary_with_kafka {
            let cloud_path = format!("{}/metadata.json", record.memory_name);
            match session_storage.get(&session_id, &cloud_path).await
                .and_then(|b| session::metadata::from_bytes(&b).map_err(Into::into))
            {
                Ok(m) => m,
                Err(_) => continue,
            }
        } else {
            match session::metadata::read(&memory_dir) {
                Ok(m) => m,
                Err(_) => continue,
            }
        };
        let target = match reconcile_target(&record.status, &meta.status) {
            Some(t) => t,
            None => continue,
        };
        tracing::warn!(
            session_id = %session_id,
            redis_status = %record.status,
            metadata_status = %meta.status,
            recovered_status = %target,
            "Reconcile sweep: recovering stale in-flight session to a resumable status"
        );
        if let Err(e) = registry.update_status(&session_id, target).await {
            tracing::error!(session_id = %session_id, "Reconcile sweep: update failed: {e}");
        }
    }

    let socket_path = std::path::PathBuf::from(&cfg.ipc_socket_path);

    let done_handles = crate::capture::DoneHandleMap::default();
    let push_handles = crate::capture::PushHandleMap::default();
    let kafka_session_map = crate::kafka::KafkaSessionMap::default();

    {
        let dh = done_handles.clone();
        let mut reg = registry.clone();
        let sr = storage_router.clone();
        let cfg_signal = cfg.clone();
        let pid_path_signal = pid_path.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                let mut sigterm = signal(SignalKind::terminate())
                    .expect("Failed to register SIGTERM handler");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
            tracing::warn!("Shutdown signal received — flagging active sessions as interrupted");

            let handles = dh.lock().await;
            for session_id in handles.keys() {
                match reg.get(session_id).await {
                    Ok(record) => {
                        let memory_dir = std::path::PathBuf::from(&record.memory_path);
                        let session_storage = sr.resolve_for_session(&record);
                        if cfg_signal.storage_mode == "cloud_primary" {
                            let cloud_path = format!("{}/metadata.json", record.memory_name);
                            let updated = session_storage.get(session_id, &cloud_path).await
                                .and_then(|b| session::metadata::from_bytes(&b).map_err(Into::into))
                                .and_then(|mut meta| {
                                    meta.in_progress = Some("interrupted".to_string());
                                    serde_json::to_vec_pretty(&meta).map_err(Into::into)
                                });
                            match updated {
                                Ok(bytes) => {
                                    if let Err(e) = session_storage.put(session_id, &cloud_path, bytes, "application/json").await {
                                        tracing::error!(session_id = %session_id, "Signal handler: cloud in_progress flush failed: {e}");
                                    } else {
                                        tracing::warn!(session_id = %session_id, "Signal handler: session flagged as interrupted in cloud");
                                    }
                                }
                                Err(e) => tracing::error!(session_id = %session_id, "Signal handler: cloud metadata fetch/serialize failed: {e}"),
                            }
                        } else {
                            if let Err(e) = session::metadata::set_in_progress(&memory_dir, "interrupted") {
                                tracing::error!(
                                    session_id = %session_id,
                                    "Signal handler: failed to set in_progress: {e}"
                                );
                            } else {
                                tracing::warn!(
                                    session_id = %session_id,
                                    "Signal handler: session flagged as interrupted"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            session_id = %session_id,
                            "Signal handler: failed to get record: {e}"
                        );
                    }
                }
            }

            tracing::warn!("All active sessions flagged — exiting");
            let _ = std::fs::remove_file(&pid_path_signal);
            std::process::exit(0);
        });
    }

    if !cfg.kafka_broker.is_empty() && cfg.storage_mode == "cloud_primary" {
        let broker = cfg.kafka_broker.clone();
        let ksm = kafka_session_map.clone();
        let channel_capacity = cfg.kafka_channel_capacity;
        let lag_warn = cfg.observability.kafka_lag_warn;
        let webhook = cfg.observability.alert_webhook_url.clone();
        tokio::spawn(crate::kafka::consumer::run_kafka_consumer(broker, ksm, channel_capacity, lag_warn, webhook));
        tracing::info!("Kafka consumer spawned — broker: {}", cfg.kafka_broker);
    }

    let ipc_handle = tokio::spawn(ipc::serve(
        socket_path,
        registry.clone(),
        cfg.clone(),
        done_handles.clone(),
        push_handles.clone(),
        kafka_session_map.clone(),
        storage_router.clone(),
        reasoning_maps.clone(),
    ));

    if let Some(port) = cfg.ipc_port {
        let admin_token = std::env::var("MA_IPC_TOKEN").unwrap_or_default();
        if admin_token.is_empty() {
            tracing::error!(
                "CRITICAL: MA_IPC_TOKEN must be set when TCP IPC is enabled. \
                 Set the environment variable before starting ma-core: \
                 export MA_IPC_TOKEN=<token>"
            );
            std::process::exit(1);
        }

        let tls_acceptor = match tls::ensure_tls_assets(&cfg) {
            Ok(a) => a,
            Err(e) => {
                tracing::error!("Failed to initialize TLS assets: {e}");
                std::process::exit(1);
            }
        };

        let bind_addr = cfg.ipc_bind_addr.clone();
        let token     = admin_token;
        let reg       = registry.clone();
        let config    = cfg.clone();
        let dh        = done_handles.clone();
        let ph        = push_handles.clone();
        let ksm       = kafka_session_map.clone();
        let sr        = storage_router.clone();
        let rm_tcp    = reasoning_maps.clone();
        tokio::spawn(async move {
            if let Err(e) = ipc::serve_tcp(bind_addr, port, token, reg, config, dh, ph, ksm, sr, tls_acceptor, rm_tcp).await {
                tracing::error!("IPC TCP server error: {e}");
            }
        });
    }

    if let Some(mgmt_port) = cfg.annotator_mgmt_port {
        let reg_mgmt = registry.clone();
        tokio::spawn(async move {
            if let Err(e) = annotator_management::serve(mgmt_port, reg_mgmt).await {
                tracing::error!("Annotator management REST API error: {e}");
            }
        });
    }

    ipc_handle.await??;

    Ok(())
}

#[cfg(test)]
mod reconcile_tests {
    use super::reconcile_target;
    use crate::registry::schema::SessionStatus;

    // The bug this fixes: metadata.json.status is frozen at "complete" after
    // capture, so a session interrupted mid-annotation must NOT be demoted to
    // Complete — it must be recovered to PendingAnnotation so the TUI can resume.
    #[test]
    fn annotating_with_complete_metadata_recovers_to_pending_annotation() {
        assert_eq!(
            reconcile_target(&SessionStatus::Annotating, "complete"),
            Some(SessionStatus::PendingAnnotation),
        );
    }

    // pending_compilation / reasoning_degraded are already resumable; a frozen
    // "complete" metadata must leave them untouched (None = no Redis write).
    #[test]
    fn in_flight_non_annotating_with_complete_metadata_is_left_untouched() {
        assert_eq!(reconcile_target(&SessionStatus::PendingCompilation, "complete"), None);
        assert_eq!(reconcile_target(&SessionStatus::ReasoningDegraded, "complete"), None);
    }

    // Matching statuses are a no-op.
    #[test]
    fn matching_status_is_noop() {
        assert_eq!(reconcile_target(&SessionStatus::PendingCompilation, "pending_compilation"), None);
        assert_eq!(reconcile_target(&SessionStatus::ReasoningDegraded, "reasoning_degraded"), None);
    }

    // Cloud crash recovery: metadata carries a genuine resumable status written by
    // mark_recovered/Kafka replay — mirror it into Redis.
    #[test]
    fn authoritative_resumable_metadata_is_mirrored() {
        assert_eq!(
            reconcile_target(&SessionStatus::Annotating, "pending_annotation"),
            Some(SessionStatus::PendingAnnotation),
        );
        assert_eq!(
            reconcile_target(&SessionStatus::ReasoningDegraded, "pending_human_annotation"),
            Some(SessionStatus::PendingHumanAnnotation),
        );
        assert_eq!(
            reconcile_target(&SessionStatus::Annotating, "incomplete"),
            Some(SessionStatus::Incomplete),
        );
    }

    // Unknown metadata status strings are ignored rather than panicking.
    #[test]
    fn unknown_metadata_status_is_ignored() {
        assert_eq!(reconcile_target(&SessionStatus::Annotating, "banana"), None);
    }
}

#[cfg(test)]
mod active_sweep_tests {
    use super::active_sweep_restore_target;
    use crate::registry::schema::SessionStatus;

    // The bug this fixes: a session whose `done` completed on disk (metadata.json
    // frozen at "complete") but whose Redis state rolled back to active — e.g. an
    // unclean host shutdown restored a pre-`done` RDB snapshot — was demoted to
    // incomplete and its directory renamed, even though the recording was fully
    // intact. It must instead be restored to pending_annotation.
    #[test]
    fn completed_metadata_restores_pending_annotation() {
        assert_eq!(
            active_sweep_restore_target("complete"),
            Some(SessionStatus::PendingAnnotation),
        );
    }

    // Genuinely interrupted or never-started sessions fall through to the normal
    // incomplete handling.
    #[test]
    fn non_complete_metadata_falls_through() {
        assert_eq!(active_sweep_restore_target("active"), None);
        assert_eq!(active_sweep_restore_target("incomplete"), None);
        assert_eq!(active_sweep_restore_target(""), None);
    }
}