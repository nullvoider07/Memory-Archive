// /Memory-Archive/ma-core/src/ipc/mod.rs

pub mod messages;

use std::path::PathBuf;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, BufReader};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;

use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::registry::SessionRegistry;
use crate::storage::StorageRouter;
use messages::{FileEntry, InboundMessage, OutboundMessage, QueueItem};

#[cfg(unix)]
pub async fn serve(
    socket_path: PathBuf,
    registry: SessionRegistry,
    config: Config,
    done_handles: crate::capture::DoneHandleMap,
    push_handles: crate::capture::PushHandleMap,
    kafka_session_map: crate::kafka::KafkaSessionMap,
    storage_router: std::sync::Arc<StorageRouter>,
    reasoning_maps: crate::capture::ReasoningMapsRef,
) -> anyhow::Result<()> {
    if socket_path.exists() {
        std::fs::remove_file(&socket_path).with_context(|| {
            format!("Failed to remove stale socket: {}", socket_path.display())
        })?;
    }

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("Failed to create socket directory: {}", parent.display())
        })?;
        // Restrict the socket's directory to the owner. The Unix IPC transport is
        // unauthenticated — every admin operation (register, delete, done) is
        // reachable by anyone who can connect to the socket — so the filesystem
        // permission is the entire access boundary. Do not rely on umask.
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .with_context(|| {
                format!("Failed to restrict socket directory permissions: {}", parent.display())
            })?;
    }

    let listener = UnixListener::bind(&socket_path).with_context(|| {
        format!("Failed to bind socket: {}", socket_path.display())
    })?;

    // Lock the socket itself to owner-only (0600). With the default umask the
    // bind above yields a group/other-accessible socket (0775 under umask 002);
    // since the transport carries no token, a same-group local user could
    // otherwise issue unauthenticated admin commands. Set perms before the accept
    // loop starts so there is no window where the socket is connectable by others.
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| {
            format!("Failed to restrict socket permissions: {}", socket_path.display())
        })?;

    tracing::info!("IPC Unix socket server ready");
    tracing::debug!("IPC socket path: {}", socket_path.display());

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tracing::debug!("IPC: new connection");
                let reg = registry.clone();
                let cfg = config.clone();
                let dh = done_handles.clone();
                let ph = push_handles.clone();
                let ksm = kafka_session_map.clone();
                let sr = storage_router.clone();
                let rm = reasoning_maps.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, reg, cfg, dh, ph, ksm, sr, rm).await {
                        tracing::warn!("IPC connection error: {e}");
                    }
                });
            }
            Err(e) => tracing::error!("IPC accept error: {e}"),
        }
    }
}

#[cfg(not(unix))]
pub async fn serve(
    _socket_path: PathBuf,
    _registry: SessionRegistry,
    _config: Config,
    _done_handles: crate::capture::DoneHandleMap,
    _push_handles: crate::capture::PushHandleMap,
    _kafka_session_map: crate::kafka::KafkaSessionMap,
    _storage_router: std::sync::Arc<StorageRouter>,
    _reasoning_maps: crate::capture::ReasoningMapsRef,
) -> anyhow::Result<()> {
    tracing::info!("Unix socket IPC not available on this platform — set ipc_port in config to use TCP mode");
    std::future::pending::<()>().await;
    Ok(())
}

#[cfg(unix)]
async fn handle_connection(
    stream: UnixStream,
    registry: SessionRegistry,
    config: Config,
    done_handles: crate::capture::DoneHandleMap,
    push_handles: crate::capture::PushHandleMap,
    kafka_session_map: crate::kafka::KafkaSessionMap,
    storage_router: std::sync::Arc<StorageRouter>,
    reasoning_maps: crate::capture::ReasoningMapsRef,
) -> anyhow::Result<()> {
    let (reader, writer) = stream.into_split();
    let lines = BufReader::with_capacity(64 * 1024, reader).lines();
    handle_connection_inner(lines, writer, registry, config, done_handles, push_handles, kafka_session_map, storage_router, reasoning_maps).await
}

async fn handle_connection_inner<R, W>(
    mut lines: tokio::io::Lines<BufReader<R>>,
    mut writer: W,
    registry: SessionRegistry,
    config: Config,
    done_handles: crate::capture::DoneHandleMap,
    push_handles: crate::capture::PushHandleMap,
    kafka_session_map: crate::kafka::KafkaSessionMap,
    storage_router: std::sync::Arc<StorageRouter>,
    reasoning_maps: crate::capture::ReasoningMapsRef,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    let (push_tx, mut push_rx) = mpsc::channel::<OutboundMessage>(16);

    loop {
        tokio::select! {
            result = lines.next_line() => {
                match result? {
                    None => break,
                    Some(line) => {
                        if line.len() > 4 * 1024 * 1024 {
                            tracing::warn!("IPC: oversized message ({} bytes) — dropping connection", line.len());
                            break;
                        }
                        let line = line.trim().to_string();
                        if line.is_empty() { continue; }
                        tracing::debug!("IPC ← [message received]");

                        let response = match serde_json::from_str::<InboundMessage>(&line) {
                            Ok(msg) => {
                                handle_message(
                                    msg,
                                    registry.clone(),
                                    config.clone(),
                                    push_tx.clone(),
                                    done_handles.clone(),
                                    push_handles.clone(),
                                    kafka_session_map.clone(),
                                    storage_router.clone(),
                                    reasoning_maps.clone(),
                                )
                                .await
                            }
                            Err(e) => OutboundMessage::Error {
                                code: "PARSE_ERROR".to_string(),
                                message: format!("Could not parse message: {e}"),
                            },
                        };

                        let mut json = serde_json::to_string(&response)?;
                        json.push('\n');
                        tracing::debug!("IPC → [response sent]");
                        writer.write_all(json.as_bytes()).await?;
                    }
                }
            }

            Some(msg) = push_rx.recv() => {
                if matches!(&msg, OutboundMessage::FileWritten { .. }) {
                    crate::observability::metrics().ipc_push_queue_depth.dec();
                }
                let mut json = serde_json::to_string(&msg)?;
                json.push('\n');
                tracing::debug!("IPC push → [message sent]");
                writer.write_all(json.as_bytes()).await?;
            }
        }
    }

    tracing::debug!("IPC: connection closed");
    Ok(())
}

const MAX_UPLOAD_BYTES: usize = 50 * 1024 * 1024; // 50 MB — reasoning.jsonl and metadata.json are always small

fn list_dir_recursive(dir: &std::path::Path, base: &std::path::Path) -> Vec<FileEntry> {
    list_dir_recursive_inner(dir, base, 0)
}

fn list_dir_recursive_inner(dir: &std::path::Path, base: &std::path::Path, depth: usize) -> Vec<FileEntry> {
    if depth > 10 {
        tracing::warn!(path = %dir.display(), "list_dir_recursive: max depth exceeded, truncating");
        return Vec::new();
    }
    let mut results = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                results.extend(list_dir_recursive_inner(&path, base, depth + 1));
            } else {
                let size = path.metadata().map(|m| m.len()).unwrap_or(0);
                if let Ok(rel) = path.strip_prefix(base) {
                    results.push(FileEntry {
                        path: rel.to_string_lossy().to_string(),
                        size,
                    });
                }
            }
        }
    }
    results
}

fn validate_relative_path(memory_path: &str, relative_path: &str) -> anyhow::Result<std::path::PathBuf> {
    if relative_path.is_empty() {
        anyhow::bail!("relative_path must not be empty");
    }
    let base = std::path::PathBuf::from(memory_path);
    let joined = base.join(relative_path);
    // Normalise without hitting the filesystem (no symlink resolution needed here
    // since we only need to reject .. traversal, not resolve real paths).
    let mut normalised = std::path::PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::ParentDir => {
                if !normalised.pop() {
                    anyhow::bail!("relative_path traverses outside session directory");
                }
            }
            std::path::Component::CurDir => {}
            c => normalised.push(c),
        }
    }
    if !normalised.starts_with(&base) {
        anyhow::bail!("relative_path traverses outside session directory");
    }
    Ok(normalised)
}

/// Parse every JSON line in a reasoning.jsonl upload and force `source = "human"`.
///
/// Returns the rewritten bytes — every line with a parseable JSON object has its
/// `source` field overwritten with `"human"` regardless of what the client sent.
/// Lines that cannot be parsed as JSON objects cause the entire upload to be rejected,
/// so a partially-valid file never reaches storage.
fn enforce_reasoning_source_human(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("reasoning.jsonl is not valid UTF-8"))?;

    let mut out_lines: Vec<String> = Vec::new();

    for (line_no, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut obj: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(line).map_err(|e| {
                anyhow::anyhow!("Line {}: not valid JSON: {e}", line_no + 1)
            })?;

        // Force source to "human" regardless of what the annotator sent.
        obj.insert("source".to_string(), serde_json::Value::String("human".to_string()));

        out_lines.push(serde_json::to_string(&obj)?);
    }

    if out_lines.is_empty() {
        anyhow::bail!("reasoning.jsonl upload contains no valid entries");
    }

    let mut result = out_lines.join("\n");
    result.push('\n');
    Ok(result.into_bytes())
}

/// Parse a metadata.json upload and allow only `annotated_steps` and `skipped_steps`
/// to be written by an annotator. All other fields are taken from the current on-disk
/// record; `status` in particular cannot be changed by an annotator.
///
/// This prevents annotators from escalating their access by writing arbitrary metadata
/// (e.g. changing status to "complete" to skip annotation).
fn enforce_metadata_annotator_fields(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("metadata.json is not valid UTF-8"))?;

    let incoming: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(text).map_err(|e| {
            anyhow::anyhow!("metadata.json is not valid JSON: {e}")
        })?;

    // Reject any attempt to write the status field from an annotator connection.
    if incoming.contains_key("status") {
        anyhow::bail!(
            "Annotators may not update the 'status' field in metadata.json. \
             Status transitions are controlled by ma-core."
        );
    }

    // Accept only the two counters. Build a minimal update object.
    let mut allowed: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    for key in &["annotated_steps", "skipped_steps"] {
        if let Some(val) = incoming.get(*key) {
            if !val.is_u64() && !val.is_i64() {
                anyhow::bail!("Field '{key}' must be an integer");
            }
            allowed.insert(key.to_string(), val.clone());
        }
    }

    if allowed.is_empty() {
        anyhow::bail!(
            "metadata.json upload from annotator must contain at least one of: \
             annotated_steps, skipped_steps"
        );
    }

    Ok(serde_json::to_vec_pretty(&allowed)?)
}

async fn handle_message(
    msg: InboundMessage,
    mut registry: SessionRegistry,
    config: Config,
    push_tx: mpsc::Sender<OutboundMessage>,
    done_handles: crate::capture::DoneHandleMap,
    push_handles: crate::capture::PushHandleMap,
    kafka_session_map: crate::kafka::KafkaSessionMap,
    storage_router: std::sync::Arc<StorageRouter>,
    reasoning_maps: crate::capture::ReasoningMapsRef,
) -> OutboundMessage {
    match msg {
        InboundMessage::Ping => {
            tracing::debug!("IPC: Ping");
            OutboundMessage::Pong {
                version: env!("CARGO_PKG_VERSION").to_string(),
            }
        }

        InboundMessage::RegisterSession {
            mode,
            os_type,
            os_version,
            os_architecture,
            os_environment_id,
            capture_server_id,
            actuation_server_id,
            memory_name,
            reasoning_model_id,
            tenant_id,
            session_config,
            capture_server_addr: capture_server_addr_raw,
            the_eyes_addr: the_eyes_addr_raw,
        } => {
            use crate::registry::schema::{SessionMode, SessionRecord, SessionStatus};
            use chrono::Utc;
            use uuid::Uuid;
 
            let session_id = Uuid::new_v4().to_string();
            let now = Utc::now();
 
            if memory_name.is_empty()
                || memory_name.contains('/')
                || memory_name.contains('\\')
                || memory_name.contains("..")
                || memory_name.starts_with('.')
            {
                return OutboundMessage::Error {
                    code: "INVALID_MEMORY_NAME".to_string(),
                    message: "memory_name must not be empty, contain path separators, or start with '.'".to_string(),
                };
            }
 
            // Validate tenant_id — becomes part of cloud storage paths
            // so we enforce a strict character set now.
            let tenant_id_clean = match &tenant_id {
                None => String::new(),
                Some(t) => {
                    let t = t.trim();
                    if t.is_empty() {
                        return OutboundMessage::Error {
                            code: "INVALID_TENANT_ID".to_string(),
                            message: "tenant_id must not be empty if provided.".to_string(),
                        };
                    }
                    if t.len() > 128 {
                        return OutboundMessage::Error {
                            code: "INVALID_TENANT_ID".to_string(),
                            message: "tenant_id must not exceed 128 characters.".to_string(),
                        };
                    }
                    if !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                        return OutboundMessage::Error {
                            code: "INVALID_TENANT_ID".to_string(),
                            message: "tenant_id must contain only alphanumeric characters, hyphens, and underscores.".to_string(),
                        };
                    }
                    t.to_string()
                }
            };
 
            // Validate per-session server addresses.
            // HTTP is accepted (CC and Eyes are internal services on the same trusted network).
            // Only basic URL structure is checked — reachability is not tested at registration time.
            fn is_valid_http_url(s: &str) -> bool {
                s.starts_with("http://") || s.starts_with("https://")
            }

            let capture_server_addr = if capture_server_addr_raw.is_empty() {
                String::new()
            } else if !is_valid_http_url(&capture_server_addr_raw) {
                return OutboundMessage::Error {
                    code: "INVALID_SERVER_ADDR".to_string(),
                    message: format!(
                        "capture_server_addr must be a valid HTTP/HTTPS URL, got: {capture_server_addr_raw:?}"
                    ),
                };
            } else {
                capture_server_addr_raw
            };

            let the_eyes_addr = if the_eyes_addr_raw.is_empty() {
                String::new()
            } else if !is_valid_http_url(&the_eyes_addr_raw) {
                return OutboundMessage::Error {
                    code: "INVALID_SERVER_ADDR".to_string(),
                    message: format!(
                        "the_eyes_addr must be a valid HTTP/HTTPS URL, got: {the_eyes_addr_raw:?}"
                    ),
                };
            } else {
                the_eyes_addr_raw
            };

            // Validate session_config — model_endpoint must use HTTPS to prevent
            // plaintext VLM API calls. context_window_steps is hard-clamped to
            // 1–50 to bound the size of StepReadyForReasoning push payloads.
            // model_api_key_ref is validated for presence and length only —
            // it is an opaque secrets store reference and must never be logged.
            // Fallback fields must all be present together or all absent.
            // 2-provider limit: primary + at most one fallback.
            let (
                model_provider,
                model_endpoint,
                model_api_key_ref,
                context_window_steps,
                fallback_model_provider,
                fallback_model_endpoint,
                fallback_api_key_ref,
            ) = match &session_config {
                None => (
                    String::new(), String::new(), String::new(), 5u32,
                    String::new(), String::new(), String::new(),
                ),
                Some(cfg) => {
                    if cfg.model_provider.is_empty() || cfg.model_provider.len() > 64 {
                        return OutboundMessage::Error {
                            code: "INVALID_SESSION_CONFIG".to_string(),
                            message: "model_provider must be 1–64 characters.".to_string(),
                        };
                    }
                    if cfg.model_api_key_ref.is_empty() || cfg.model_api_key_ref.len() > 512 {
                        return OutboundMessage::Error {
                            code: "INVALID_SESSION_CONFIG".to_string(),
                            message: "model_api_key_ref must be 1–512 characters.".to_string(),
                        };
                    }
                    if cfg.model_endpoint.is_empty() || cfg.model_endpoint.len() > 2048 {
                        return OutboundMessage::Error {
                            code: "INVALID_SESSION_CONFIG".to_string(),
                            message: "model_endpoint must be 1–2048 characters.".to_string(),
                        };
                    }
                    // SSRF / credential-in-transit mitigation: reject HTTP.
                    // Localhost (127.x, ::1) is permitted for test environments
                    // but still requires HTTPS to keep the code path consistent.
                    if !cfg.model_endpoint.starts_with("https://") {
                        return OutboundMessage::Error {
                            code: "INVALID_SESSION_CONFIG".to_string(),
                            message: "model_endpoint must use HTTPS. Plaintext HTTP is not \
                                      permitted for VLM API calls.".to_string(),
                        };
                    }
                    let steps = cfg.context_window_steps.clamp(1, 50);
                    if cfg.context_window_steps != steps {
                        tracing::warn!(
                            requested = cfg.context_window_steps,
                            clamped = steps,
                            "context_window_steps out of range 1–50, clamped"
                        );
                    }

                    // Validate fallback fields — must be all present or all absent.
                    let has_fallback_provider = !cfg.fallback_model_provider.is_empty();
                    let has_fallback_endpoint = !cfg.fallback_model_endpoint.is_empty();
                    let has_fallback_key_ref  = !cfg.fallback_api_key_ref.is_empty();

                    let fallback_count = [has_fallback_provider, has_fallback_endpoint, has_fallback_key_ref]
                        .iter()
                        .filter(|&&b| b)
                        .count();
                    if fallback_count > 0 && fallback_count < 3 {
                        return OutboundMessage::Error {
                            code: "INVALID_SESSION_CONFIG".to_string(),
                            message: "fallback_model_provider, fallback_model_endpoint, and \
                                      fallback_api_key_ref must all be set together or all absent."
                                .to_string(),
                        };
                    }

                    if has_fallback_provider {
                        if cfg.fallback_model_provider.len() > 64 {
                            return OutboundMessage::Error {
                                code: "INVALID_SESSION_CONFIG".to_string(),
                                message: "fallback_model_provider must not exceed 64 characters."
                                    .to_string(),
                            };
                        }
                        if cfg.fallback_model_endpoint.len() > 2048 {
                            return OutboundMessage::Error {
                                code: "INVALID_SESSION_CONFIG".to_string(),
                                message: "fallback_model_endpoint must not exceed 2048 characters."
                                    .to_string(),
                            };
                        }
                        if !cfg.fallback_model_endpoint.starts_with("https://") {
                            return OutboundMessage::Error {
                                code: "INVALID_SESSION_CONFIG".to_string(),
                                message: "fallback_model_endpoint must use HTTPS.".to_string(),
                            };
                        }
                        if cfg.fallback_api_key_ref.len() > 512 {
                            return OutboundMessage::Error {
                                code: "INVALID_SESSION_CONFIG".to_string(),
                                message: "fallback_api_key_ref must not exceed 512 characters."
                                    .to_string(),
                            };
                        }
                    }

                    (
                        cfg.model_provider.clone(),
                        cfg.model_endpoint.clone(),
                        cfg.model_api_key_ref.clone(),
                        steps,
                        cfg.fallback_model_provider.clone(),
                        cfg.fallback_model_endpoint.clone(),
                        cfg.fallback_api_key_ref.clone(),
                    )
                }
            };
 
            let memory_path = format!("{}/{}", config.storage_path, memory_name);
            let ma_core_addr = config.ipc_port.map_or(String::new(), |p| {
                if config.ipc_bind_addr == "0.0.0.0" {
                    tracing::warn!(
                        "ipc_bind_addr is 0.0.0.0 — ma_core_addr stored as '0.0.0.0:{p}' which \
                         remote clients cannot use for server discovery. \
                         Set a specific address: memory-archive config --ipc-bind-addr <host-ip>"
                    );
                }
                format!("{}:{}", config.ipc_bind_addr, p)
            });
 
            let mode_parsed = match mode.parse::<SessionMode>() {
                Ok(m) => m,
                Err(e) => {
                    return OutboundMessage::Error {
                        code: "INVALID_MODE".to_string(),
                        message: e.to_string(),
                    }
                }
            };
 
            // Resolve storage backend for this session at registration time.
            // The selected name is pinned in Redis for the session's lifetime.
            let (backend_name, storage) = storage_router.resolve(&tenant_id_clean);

            let record = SessionRecord {
                session_id: session_id.clone(),
                mode: mode_parsed,
                status: SessionStatus::Active,
                os_type,
                os_version,
                os_architecture,
                os_environment_id,
                capture_server_id,
                actuation_server_id,
                reasoning_model_id,
                memory_name,
                memory_path,
                ma_core_addr,
                created_at: now,
                updated_at: now,
                total_steps: 0,
                annotated_steps: 0,
                skipped_steps: 0,
                tenant_id: tenant_id_clean,
                model_provider,
                model_endpoint,
                model_api_key_ref,
                context_window_steps,
                fallback_model_provider,
                fallback_model_endpoint,
                fallback_api_key_ref,
                storage_backend: backend_name,
                capture_server_addr,
                the_eyes_addr,
            };

            match registry.register(&record).await {
                Ok(()) => {
                    match crate::session::initialise(&record, &config.storage_path, config.storage_mode == "cloud_primary") {
                        Ok(memory_dir) => {
                            if config.storage_mode == "cloud_primary" {
                                let meta_path = memory_dir.join("metadata.json");
                                match std::fs::read(&meta_path) {
                                    Ok(bytes) => {
                                        if let Err(e) = storage.put(&session_id, &format!("{}/metadata.json", record.memory_name), bytes, "application/json").await {
                                            tracing::warn!(session_id = %session_id, "Failed to upload initial metadata to cloud: {e}");
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(session_id = %session_id, "Failed to read initial metadata for cloud upload: {e}");
                                    }
                                }
                                if let Err(e) = std::fs::remove_dir_all(&memory_dir) {
                                    tracing::warn!(session_id = %session_id, "Failed to remove local dir after cloud upload: {e}");
                                }
                            }
                            // model_api_key_ref intentionally excluded from this log line.
                            crate::observability::metrics()
                                .storage_routing_decisions
                                .get_or_create(&crate::observability::BackendLabels {
                                    backend: record.storage_backend.clone(),
                                })
                                .inc();
                            tracing::info!(
                                session_id = %session_id,
                                memory_dir = %memory_dir.display(),
                                tenant_id = %record.tenant_id,
                                model_provider = %record.model_provider,
                                storage_backend = %record.storage_backend,
                                "Session registered via IPC"
                            );
                            OutboundMessage::SessionRegistered { session_id }
                        }
                        Err(e) => {
                            if let Err(re) = registry
                                .update_status(
                                    &session_id,
                                    crate::registry::schema::SessionStatus::Incomplete,
                                )
                                .await
                            {
                                tracing::error!(
                                    "Failed to roll back session after dir init failure: {re}"
                                );
                            }
                            OutboundMessage::Error {
                                code: "DIR_INIT_FAILED".to_string(),
                                message: e.to_string(),
                            }
                        }
                    }
                }
                Err(e) => OutboundMessage::Error {
                    code: "REGISTER_FAILED".to_string(),
                    message: e.to_string(),
                },
            }
        }

        InboundMessage::GetSessionStatus { session_id } => {
            match registry.get(&session_id).await {
                Ok(record) => {
                    let pairs = record.to_redis_pairs();
                    let mut map: std::collections::HashMap<String, serde_json::Value> = pairs
                        .into_iter()
                        .map(|(k, v)| (k, serde_json::Value::String(v)))
                        .collect();
                    // Strip the secrets store reference from all GetSessionStatus responses.
                    // The caller who registered the session already knows the ref they sent;
                    // echoing it back in status responses creates unnecessary exposure.
                    map.remove("model_api_key_ref");
                    OutboundMessage::SessionStatus { session: map }
                }
                Err(e) => OutboundMessage::Error {
                    code: "SESSION_NOT_FOUND".to_string(),
                    message: e.to_string(),
                },
            }
        }

        InboundMessage::StartWatch { session_id } => {
            match registry.get(&session_id).await {
                Err(e) => OutboundMessage::Error {
                    code: "SESSION_NOT_FOUND".to_string(),
                    message: e.to_string(),
                },
                Ok(record) => {
                    let memory_path = record.memory_path.clone();

                    if config.storage_mode != "cloud_primary"
                        && !std::path::Path::new(&memory_path).exists()
                    {
                        return OutboundMessage::Error {
                            code: "MEMORY_DIR_MISSING".to_string(),
                            message: format!(
                                "Memory directory no longer exists: {memory_path}. \
                                The session may have been deleted. \
                                Register a new session to start again."
                            ),
                        };
                    }

                    // Resolve the pinned backend for this session before spawning the watch loop.
                    let storage = storage_router.resolve_for_session(&record);

                    push_handles
                        .lock()
                        .await
                        .insert(session_id.clone(), push_tx.clone());

                    tokio::spawn(crate::capture::run_watch_loop(
                        session_id.clone(),
                        registry,
                        config,
                        push_tx,
                        done_handles,
                        kafka_session_map,
                        storage,
                        reasoning_maps.clone(),
                    ));
                    tracing::info!(session_id = %session_id, "Watch loop spawned");
                    OutboundMessage::WatchStarted {
                        session_id,
                        memory_path,
                        model_provider: record.model_provider.clone(),
                        model_endpoint: record.model_endpoint.clone(),
                        model_api_key_ref: record.model_api_key_ref.clone(),
                        fallback_model_provider: record.fallback_model_provider.clone(),
                        fallback_model_endpoint: record.fallback_model_endpoint.clone(),
                        fallback_api_key_ref: record.fallback_api_key_ref.clone(),
                    }
                }
            }
        }

        InboundMessage::Done { session_id } => {
            let done_handle = done_handles.lock().await.remove(&session_id);

            let is_cloud_primary = config.storage_mode == "cloud_primary";
            let closing_storage = match registry.get(&session_id).await.ok() {
                Some(ref r) => storage_router.resolve_for_session(r),
                None => storage_router.resolve("").1,
            };
            let fetch_closing_image = |memory_dir: std::path::PathBuf, sid: String, addr: String, sync_tx: Option<mpsc::Sender<OutboundMessage>>, memory_name: String| async move {
                if addr.is_empty() {
                    return;
                }
                let client = match crate::vision::client::EyesClient::new(addr) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(session_id = %sid, "Closing image: failed to create client: {e}");
                        return;
                    }
                };
                let ts = chrono::Utc::now().to_rfc3339();
                match client.fetch_at(&ts).await {
                    Err(e) => {
                        tracing::warn!(session_id = %sid, "Closing image: fetch failed: {e}");
                    }
                    Ok((bytes, ext)) => {
                        let filename = format!("closing_state.{ext}");
                        let rel_path = format!("vision/frames/{filename}");
                        if is_cloud_primary {
                            let cloud_path = format!("{}/{}", memory_name, rel_path);
                            if let Err(e) = closing_storage.put(&sid, &cloud_path, bytes, "image/png").await {
                                tracing::warn!(session_id = %sid, "Closing image: cloud upload failed: {e}");
                            } else {
                                tracing::info!(session_id = %sid, "Closing state image uploaded to cloud");
                            }
                        } else {
                            let path = memory_dir.join("vision").join("frames").join(&filename);
                            match std::fs::write(&path, &bytes) {
                                Err(e) => {
                                    tracing::warn!(session_id = %sid, "Closing image: write failed: {e}");
                                }
                                Ok(()) => {
                                    if let Err(e) = crate::session::metadata::set_closing_image(
                                        &memory_dir,
                                        &rel_path,
                                    ) {
                                        tracing::error!(session_id = %sid, "Closing image: metadata update failed: {e}");
                                    } else {
                                        tracing::info!(session_id = %sid, "Closing state image saved");
                                        if let Some(tx) = sync_tx {
                                            let abs = memory_dir.join(&rel_path).to_string_lossy().to_string();
                                            let _ = tx.try_send(OutboundMessage::FileWritten {
                                                session_id: sid.clone(),
                                                relative_path: rel_path,
                                                abs_path: abs,
                                            });
                                            let meta_abs = memory_dir.join("metadata.json").to_string_lossy().to_string();
                                            let _ = tx.try_send(OutboundMessage::FileWritten {
                                                session_id: sid,
                                                relative_path: "metadata.json".to_string(),
                                                abs_path: meta_abs,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            };

            match done_handle {
                Some((tx, result_rx)) => {
                    let _ = tx.send(());

                    let memory_dir = match registry.get(&session_id).await {
                        Ok(r) => std::path::PathBuf::from(&r.memory_path),
                        Err(_) => std::path::PathBuf::new(),
                    };
                    if !memory_dir.as_os_str().is_empty() {
                        let sid = session_id.clone();
                        let addr = config.the_eyes_addr.clone();
                        let md = memory_dir.clone();
                        let closing_tx = push_handles.lock().await.get(&session_id).cloned();
                        let mem_name = memory_dir
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        tokio::spawn(fetch_closing_image(md, sid, addr, closing_tx, mem_name));
                    }

                    let total_steps = result_rx.await.unwrap_or(0);

                    if let Some(start_push_tx) = push_handles.lock().await.remove(&session_id) {
                        let _ = start_push_tx
                            .send(OutboundMessage::SessionComplete {
                                session_id: session_id.clone(),
                                total_steps,
                            })
                            .await;
                    }

                    tracing::info!(
                        session_id = %session_id,
                        total_steps,
                        "Done signal sent to watch loop"
                    );
                    OutboundMessage::SessionComplete { session_id, total_steps }
                }

                None => {
                    match registry.get(&session_id).await {
                        Err(_) => OutboundMessage::Error {
                            code: "NOT_FOUND".to_string(),
                            message: format!(
                                "Session '{session_id}' not found in registry. Cannot finalize."
                            ),
                        },
                        Ok(record) => {
                            let memory_dir = std::path::PathBuf::from(&record.memory_path);
                            let storage = storage_router.resolve_for_session(&record);

                            let (drained_input, drained_output) = reasoning_maps
                                .drain_tokens(&session_id)
                                .await;
                            let drained_provider_tokens = reasoning_maps
                                .drain_provider_tokens(&session_id)
                                .await;
                            if drained_input > 0 || drained_output > 0 || !drained_provider_tokens.is_empty() {
                                if let Ok(mut meta) = crate::session::metadata::read(&memory_dir) {
                                    meta.total_input_tokens += drained_input;
                                    meta.total_output_tokens += drained_output;
                                    for (provider, (pin, pout)) in drained_provider_tokens {
                                        let entry = meta.token_costs_by_provider
                                            .entry(provider)
                                            .or_insert_with(crate::session::metadata::ProviderTokenCounts::default);
                                        entry.input_tokens += pin;
                                        entry.output_tokens += pout;
                                    }
                                    if let Err(e) = crate::session::metadata::write(&memory_dir, &meta) {
                                        tracing::error!(session_id = %session_id, "Done: failed to flush token counts to metadata: {e}");
                                    }
                                }
                            }

                            let total_steps = if config.storage_mode == "cloud_primary" {
                                let cloud_path = format!("{}/metadata.json", record.memory_name);
                                storage.get(&session_id, &cloud_path).await.ok()
                                    .and_then(|b| crate::session::metadata::from_bytes(&b).ok())
                                    .map(|m| m.total_steps)
                                    .unwrap_or(record.total_steps)
                            } else {
                                crate::session::metadata::read(&memory_dir)
                                    .map(|m| m.total_steps)
                                    .unwrap_or(record.total_steps)
                            };

                            let mut writer = crate::capture::CommandWriter::new(
                                &memory_dir,
                                config.storage_mode == "cloud_primary",
                                session_id.clone(),
                                storage.clone(),
                            );
                            if let Err(e) = writer.finalise().await {
                                tracing::error!(session_id = %session_id, "Direct finalise() failed: {e}");
                            }
                            if config.storage_mode == "cloud_primary" {
                                let cloud_path = format!("{}/metadata.json", record.memory_name);
                                match storage.get(&session_id, &cloud_path).await
                                    .and_then(|b| crate::session::metadata::from_bytes(&b).map_err(Into::into))
                                {
                                    Ok(mut meta) => {
                                        meta.status = "complete".to_string();
                                        meta.completed_at = Some(chrono::Utc::now());
                                        meta.in_progress = None;
                                        match serde_json::to_vec_pretty(&meta) {
                                            Ok(bytes) => {
                                                if let Err(e) = storage.put(&session_id, &cloud_path, bytes, "application/json").await {
                                                    tracing::error!(session_id = %session_id, "Direct cloud metadata update failed: {e}");
                                                }
                                            }
                                            Err(e) => tracing::error!(session_id = %session_id, "Direct cloud metadata serialize failed: {e}"),
                                        }
                                    }
                                    Err(e) => tracing::error!(session_id = %session_id, "Direct cloud metadata fetch failed: {e}"),
                                }
                            } else {
                                if let Err(e) = crate::session::metadata::mark_complete(&memory_dir) {
                                    tracing::error!(session_id = %session_id, "Direct mark_complete() failed: {e}");
                                }
                                if let Err(e) = crate::session::metadata::clear_in_progress(&memory_dir) {
                                    tracing::error!(session_id = %session_id, "Direct clear_in_progress() failed: {e}");
                                }
                            }
                            // Match the watch-loop completion path: annotation
                            // routing depends on whether reasoning degraded, not
                            // on session mode. LoadSession rejects
                            // pending_human_annotation, so the previous
                            // mode-based branch left manual sessions finished
                            // through this path un-annotatable.
                            let was_degraded = reasoning_maps.is_degraded(&session_id).await;
                            reasoning_maps.remove_session(&session_id).await;
                            let done_status = if was_degraded {
                                crate::registry::schema::SessionStatus::PendingHumanAnnotation
                            } else {
                                crate::registry::schema::SessionStatus::PendingAnnotation
                            };
                            if let Err(e) = registry
                                .update_status(
                                    &session_id,
                                    done_status,
                                )
                                .await
                            {
                                tracing::error!(session_id = %session_id, "Direct Redis update failed: {e}");
                            }

                            {
                                let sid = session_id.clone();
                                let addr = config.the_eyes_addr.clone();
                                let md = memory_dir.clone();
                                let closing_tx = push_handles.lock().await.get(&session_id).cloned();
                                let mem_name = memory_dir
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                tokio::spawn(fetch_closing_image(md, sid, addr, closing_tx, mem_name));
                            }

                            if let Some(start_push_tx) = push_handles.lock().await.remove(&session_id) {
                                let _ = start_push_tx
                                    .send(OutboundMessage::SessionComplete {
                                        session_id: session_id.clone(),
                                        total_steps,
                                    })
                                    .await;
                            }

                            tracing::info!(
                                session_id = %session_id,
                                total_steps,
                                "Session finalized directly (no live watch loop)"
                            );
                            OutboundMessage::SessionComplete { session_id, total_steps }
                        }
                    }
                }
            }
        }

        InboundMessage::LoadSession { session_id } => {
            match registry.get(&session_id).await {
                Err(e) => OutboundMessage::Error {
                    code: "SESSION_NOT_FOUND".to_string(),
                    message: e.to_string(),
                },
                Ok(record) => {
                    use crate::registry::schema::SessionStatus;

                    match &record.status {
                        SessionStatus::PendingAnnotation | SessionStatus::Annotating => {}
                        other => {
                            return OutboundMessage::Error {
                                code: "INVALID_STATUS".to_string(),
                                message: format!(
                                    "Session '{session_id}' has status '{other}' — \
                                     only 'pending_annotation' or 'annotating' sessions \
                                     can be loaded for annotation."
                                ),
                            };
                        }
                    }

                    if let Err(e) = registry
                        .update_status(&session_id, SessionStatus::Annotating)
                        .await
                    {
                        return OutboundMessage::Error {
                            code: "REDIS_ERROR".to_string(),
                            message: e.to_string(),
                        };
                    }

                    let was_interrupted = matches!(
                        record.status,
                        crate::registry::schema::SessionStatus::Annotating
                    );

                    tracing::info!(
                        session_id = %session_id,
                        was_interrupted,
                        "Session loaded for annotation"
                    );
                    OutboundMessage::SessionLoaded {
                        session_id,
                        memory_path: record.memory_path,
                        was_interrupted,
                    }
                }
            }
        }

        InboundMessage::UpdateAnnotationProgress { session_id, annotated, skipped } => {
            if let Err(e) = registry
                .update_annotation_counters(&session_id, annotated, skipped)
                .await
            {
                tracing::warn!(
                    session_id = %session_id,
                    "Failed to update annotation counters in Redis: {e}"
                );
            }

            tracing::info!(
                session_id = %session_id,
                annotated,
                skipped,
                "Annotation progress updated"
            );

            OutboundMessage::AnnotationProgressUpdated {
                session_id,
                annotated,
                skipped,
            }
        }

        InboundMessage::CloseAnnotation { session_id } => {
            if let Err(e) = registry
                .update_status(
                    &session_id,
                    crate::registry::schema::SessionStatus::PendingAnnotation,
                )
                .await
            {
                tracing::warn!(
                    session_id = %session_id,
                    "CloseAnnotation: failed to update Redis status: {e}"
                );
                return OutboundMessage::Error {
                    code: "REDIS_ERROR".to_string(),
                    message: e.to_string(),
                };
            }

            tracing::info!(session_id = %session_id, "Annotation closed — status → pending_annotation");
            OutboundMessage::AnnotationClosed { session_id }
        }

        InboundMessage::CompleteAnnotation { session_id } => {
            if let Err(e) = registry
                .update_status(
                    &session_id,
                    crate::registry::schema::SessionStatus::PendingCompilation,
                )
                .await
            {
                tracing::warn!(
                    session_id = %session_id,
                    "CompleteAnnotation: failed to update Redis status: {e}"
                );
                return OutboundMessage::Error {
                    code: "REDIS_ERROR".to_string(),
                    message: e.to_string(),
                };
            }

            tracing::info!(session_id = %session_id, "Annotation complete — status → pending_compilation");
            OutboundMessage::AnnotationCompleted { session_id }
        }

        InboundMessage::FinalizeMemory { session_id } => {
            if let Err(e) = registry
                .update_status(
                    &session_id,
                    crate::registry::schema::SessionStatus::Complete,
                )
                .await
            {
                tracing::warn!(session_id = %session_id, "FinalizeMemory: status update failed: {e}");
                return OutboundMessage::Error {
                    code: "REDIS_ERROR".to_string(),
                    message: e.to_string(),
                };
            }

            const NINETY_DAYS_SECS: u64 = 90 * 24 * 3600;
            if let Err(e) = registry.set_ttl(&session_id, NINETY_DAYS_SECS).await {
                tracing::warn!(session_id = %session_id, "FinalizeMemory: TTL set failed: {e}");
            }

            tracing::info!(session_id = %session_id, "Memory finalized — status → complete, TTL 90d");
            OutboundMessage::MemoryFinalized { session_id }
        }

        InboundMessage::FetchFile { session_id, relative_path } => {
            match registry.get(&session_id).await {
                Err(e) => OutboundMessage::Error {
                    code: "SESSION_NOT_FOUND".to_string(),
                    message: e.to_string(),
                },
                Ok(record) => {
                    let storage = storage_router.resolve_for_session(&record);
                    let bytes_result: anyhow::Result<Vec<u8>> = if config.storage_mode == "cloud_primary" {
                        let cloud_path = format!("{}/{}", record.memory_name, relative_path);
                        storage.get(&session_id, &cloud_path).await
                    } else {
                        match validate_relative_path(&record.memory_path, &relative_path) {
                            Err(e) => return OutboundMessage::Error {
                                code: "INVALID_PATH".to_string(),
                                message: e.to_string(),
                            },
                            Ok(abs) => tokio::fs::read(&abs).await.map_err(Into::into),
                        }
                    };

                    match bytes_result {
                        Ok(data) => {
                            let size = data.len() as u64;
                            tracing::debug!(
                                session_id = %session_id,
                                path = %relative_path,
                                bytes = size,
                                "FetchFile served"
                            );
                            OutboundMessage::FileData {
                                session_id,
                                relative_path,
                                bytes: data,
                                size,
                            }
                        }
                        Err(e) => OutboundMessage::Error {
                            code: "FILE_NOT_FOUND".to_string(),
                            message: format!("Could not read '{relative_path}': {e}"),
                        },
                    }
                }
            }
        }

        InboundMessage::ListSessionFiles { session_id, prefix } => {
            match registry.get(&session_id).await {
                Err(e) => OutboundMessage::Error {
                    code: "SESSION_NOT_FOUND".to_string(),
                    message: e.to_string(),
                },
                Ok(record) => {
                    let storage = storage_router.resolve_for_session(&record);
                    let files: Vec<FileEntry> = if config.storage_mode == "cloud_primary" {
                        let cloud_prefix = if prefix.is_empty() {
                            format!("{}/", record.memory_name)
                        } else {
                            format!("{}/{}", record.memory_name, prefix)
                        };
                        match storage.list(&session_id, &cloud_prefix).await {
                            Ok(paths) => paths.into_iter().map(|p| FileEntry { path: p, size: 0 }).collect(),
                            Err(e) => {
                                return OutboundMessage::Error {
                                    code: "LIST_FAILED".to_string(),
                                    message: e.to_string(),
                                }
                            }
                        }
                    } else {
                        let base = std::path::PathBuf::from(&record.memory_path);
                        let scan_dir = if prefix.is_empty() {
                            base.clone()
                        } else {
                            match validate_relative_path(&record.memory_path, &prefix) {
                                Ok(p) => p,
                                Err(e) => return OutboundMessage::Error {
                                    code: "INVALID_PATH".to_string(),
                                    message: e.to_string(),
                                },
                            }
                        };
                        list_dir_recursive(&scan_dir, &base)
                    };

                    tracing::debug!(
                        session_id = %session_id,
                        count = files.len(),
                        "ListSessionFiles served"
                    );
                    OutboundMessage::SessionFileList { session_id, files }
                }
            }
        }

        InboundMessage::UploadFile { session_id, relative_path, bytes, content_type } => {
            // Annotator write authority enforcement.
            //
            // Only two paths are writable by annotator connections:
            //   "reasoning/reasoning.jsonl"  — source field forced to "human"
            //   "metadata.json"              — only annotated_steps/skipped_steps updatable
            //
            // All other paths return WRITE_FORBIDDEN regardless of annotator or session.
            // This is enforced here in the Rust handler — client-side restrictions are
            // not sufficient for training data integrity guarantees.
            const ALLOWED_REASONING: &str = "reasoning/reasoning.jsonl";
            const ALLOWED_METADATA:  &str = "metadata.json";

            if relative_path != ALLOWED_REASONING && relative_path != ALLOWED_METADATA {
                return OutboundMessage::Error {
                    code: "WRITE_FORBIDDEN".to_string(),
                    message: format!(
                        "Annotators may only write to '{ALLOWED_REASONING}' or '{ALLOWED_METADATA}'. \
                         Path '{relative_path}' is not permitted."
                    ),
                };
            }

            if bytes.len() > MAX_UPLOAD_BYTES {
                return OutboundMessage::Error {
                    code: "PAYLOAD_TOO_LARGE".to_string(),
                    message: format!("Upload exceeds maximum allowed size of {} bytes", MAX_UPLOAD_BYTES),
                };
            }

            // For reasoning.jsonl: parse each line, force source = "human", reject malformed input.
            // For metadata.json: allow only annotated_steps and skipped_steps field updates.
            let write_bytes: Vec<u8> = if relative_path == ALLOWED_REASONING {
                match enforce_reasoning_source_human(&bytes) {
                    Ok(b) => b,
                    Err(e) => return OutboundMessage::Error {
                        code: "INVALID_PAYLOAD".to_string(),
                        message: format!("reasoning.jsonl parse error: {e}"),
                    },
                }
            } else {
                // metadata.json — strip disallowed fields before writing.
                match enforce_metadata_annotator_fields(&bytes) {
                    Ok(b) => b,
                    Err(e) => return OutboundMessage::Error {
                        code: "INVALID_PAYLOAD".to_string(),
                        message: format!("metadata.json parse error: {e}"),
                    },
                }
            };

            match registry.get(&session_id).await {
                Err(e) => OutboundMessage::Error {
                    code: "SESSION_NOT_FOUND".to_string(),
                    message: e.to_string(),
                },
                Ok(record) => {
                    let storage = storage_router.resolve_for_session(&record);
                    let result: anyhow::Result<()> = if config.storage_mode == "cloud_primary" {
                        let cloud_path = format!("{}/{}", record.memory_name, relative_path);
                        storage.put(&session_id, &cloud_path, write_bytes, &content_type).await
                    } else {
                        match validate_relative_path(&record.memory_path, &relative_path) {
                            Err(e) => return OutboundMessage::Error {
                                code: "INVALID_PATH".to_string(),
                                message: e.to_string(),
                            },
                            Ok(abs) => async {
                                if let Some(parent) = abs.parent() {
                                    tokio::fs::create_dir_all(parent).await?;
                                }
                                tokio::fs::write(&abs, &write_bytes).await?;
                                Ok(())
                            }.await,
                        }
                    };

                    match result {
                        Ok(()) => OutboundMessage::FileUploaded { session_id, relative_path },
                        Err(e) => OutboundMessage::Error {
                            code: "UPLOAD_FAILED".to_string(),
                            message: format!("Could not write '{relative_path}': {e}"),
                        },
                    }
                }
            }
        }

        InboundMessage::ReasoningResult {
            session_id,
            step_id,
            reasoning,
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
        } => {
            let record = match registry.get(&session_id).await {
                Err(e) => {
                    return OutboundMessage::Error {
                        code: "SESSION_NOT_FOUND".to_string(),
                        message: e.to_string(),
                    }
                }
                Ok(r) => r,
            };

            let storage = storage_router.resolve_for_session(&record);

            // Resolve the step entry so we can embed raw/converted command text.
            let step_meta: Option<crate::session::metadata::StepEntry> = {
                let memory_dir = std::path::PathBuf::from(&record.memory_path);
                let meta_result = if config.storage_mode == "cloud_primary" {
                    let cloud_path = format!("{}/metadata.json", record.memory_name);
                    storage
                        .get(&session_id, &cloud_path)
                        .await
                        .ok()
                        .and_then(|b| crate::session::metadata::from_bytes(&b).ok())
                } else {
                    crate::session::metadata::read(&memory_dir).ok()
                };
                meta_result.and_then(|m| {
                    m.steps.into_iter().find(|s| s.step_id == step_id)
                })
            };

            let step = match step_meta {
                Some(s) => s,
                None => {
                    tracing::warn!(
                        session_id = %session_id,
                        step_id,
                        "ReasoningResult: step not found in metadata — using empty command fields"
                    );
                    crate::session::metadata::StepEntry {
                        step_id,
                        timestamp: String::new(),
                        action_type: String::new(),
                        action_subtype: String::new(),
                        image_path: None,
                        image_fetched: false,
                        marked: false,
                        before_image_path: None,
                        after_image_path: None,
                        raw_command: String::new(),
                        converted_command: String::new(),
                    }
                }
            };

            let provider_name = if provider.is_empty() { None } else { Some(provider.clone()) };
            let entry = crate::session::reasoning::build_automated_entry(
                &step,
                reasoning,
                source,
                provider_name,
                Some(model_id),
                Some(api_version),
                Some(input_tokens),
                Some(output_tokens),
                Some(latency_ms),
                action_intent,
                confidence,
                keyboard_visual_annotation,
            );

            // Write reasoning.jsonl to local scratch dir.
            // In cloud_primary mode we also upload the updated file.
            let memory_dir = std::path::PathBuf::from(&record.memory_path);

            if config.storage_mode == "cloud_primary" {
                // Ensure the scratch dir exists for cloud_primary mode.
                let reasoning_dir = memory_dir.join("reasoning");
                if let Err(e) = tokio::fs::create_dir_all(&reasoning_dir).await {
                    tracing::warn!(
                        session_id = %session_id,
                        "ReasoningResult: failed to create scratch reasoning dir: {e}"
                    );
                }
            }

            // Serialise the file write under a per-session mutex so concurrent
            // ReasoningResult messages for the same session don't corrupt the file.
            let write_lock = reasoning_maps.session_write_lock(&session_id).await;
            let _guard = write_lock.lock().await;

            if let Err(e) = crate::session::reasoning::upsert_entry(&memory_dir, &entry) {
                tracing::error!(
                    session_id = %session_id,
                    step_id,
                    "ReasoningResult: failed to write reasoning.jsonl: {e}"
                );
                return OutboundMessage::Error {
                    code: "WRITE_FAILED".to_string(),
                    message: format!("Failed to write reasoning.jsonl: {e}"),
                };
            }

            if config.storage_mode == "cloud_primary" {
                let jsonl_path = memory_dir.join("reasoning").join("reasoning.jsonl");
                match tokio::fs::read(&jsonl_path).await {
                    Ok(bytes) => {
                        let cloud_path = format!(
                            "{}/reasoning/reasoning.jsonl",
                            record.memory_name
                        );
                        if let Err(e) = storage
                            .put(&session_id, &cloud_path, bytes, "application/json")
                            .await
                        {
                            crate::observability::metrics().cloud_upload_errors_total.inc();
                            tracing::error!(
                                session_id = %session_id,
                                step_id,
                                "ReasoningResult: cloud upload of reasoning.jsonl failed: {e}"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            session_id = %session_id,
                            "ReasoningResult: failed to read reasoning.jsonl for cloud upload: {e}"
                        );
                    }
                }
            }

            crate::observability::metrics().vlm_requests_total.inc();
            crate::observability::metrics()
                .vlm_requests_by_provider
                .get_or_create(&crate::observability::ProviderLabels {
                    provider: provider.clone(),
                })
                .inc();
            crate::observability::metrics().vlm_tokens_consumed_total
                .inc_by((input_tokens + output_tokens) as u64);
            crate::observability::metrics().vlm_request_latency_ms
                .observe(latency_ms as f64);

            reasoning_maps
                .add_tokens(&session_id, input_tokens as u64, output_tokens as u64)
                .await;

            if !provider.is_empty() {
                reasoning_maps
                    .add_provider_tokens(
                        &session_id,
                        &provider,
                        input_tokens as u64,
                        output_tokens as u64,
                    )
                    .await;
            }

            tracing::info!(
                session_id = %session_id,
                step_id,
                source = %entry.source,
                input_tokens,
                output_tokens,
                "ReasoningResult written"
            );

            OutboundMessage::ReasoningResultAccepted { session_id, step_id }
        }

        InboundMessage::ReasoningDegraded { session_id, step_range_start } => {
            reasoning_maps.mark_degraded(&session_id).await;

            let provider_for_error = registry.get(&session_id).await
                .map(|r| r.model_provider)
                .unwrap_or_default();

            if let Err(e) = registry
                .update_status(&session_id, crate::registry::schema::SessionStatus::ReasoningDegraded)
                .await
            {
                tracing::error!(
                    session_id = %session_id,
                    "ReasoningDegraded: failed to update Redis status: {e}"
                );
            }

            crate::observability::metrics().vlm_circuit_breaker_open.inc();
            crate::observability::metrics().sessions_reasoning_degraded.inc();
            crate::observability::metrics()
                .vlm_errors_by_provider
                .get_or_create(&crate::observability::ProviderErrorLabels {
                    provider: provider_for_error.clone(),
                    error_type: "circuit_open".to_string(),
                })
                .inc();

            tracing::warn!(
                session_id = %session_id,
                step_range_start,
                provider = %provider_for_error,
                "Circuit breaker opened — StepReadyForReasoning pushes suppressed"
            );

            let webhook = config.observability.alert_webhook_url.clone();
            if !webhook.is_empty() {
                let sid = session_id.clone();
                let prov = provider_for_error.clone();
                tokio::spawn(async move {
                    crate::observability::send_alert(
                        &webhook,
                        &format!(
                            "VLM circuit breaker opened: session={sid} provider={prov} — \
                             session will fall back to human annotation"
                        ),
                    ).await;
                });
            }

            OutboundMessage::ReasoningDegradedEvent { session_id, step_range_start }
        }

        InboundMessage::CircuitReset { session_id } => {
            reasoning_maps.reset_degraded(&session_id).await;

            if let Err(e) = registry
                .update_status(&session_id, crate::registry::schema::SessionStatus::Active)
                .await
            {
                tracing::error!(
                    session_id = %session_id,
                    "CircuitReset: failed to update Redis status: {e}"
                );
            }

            crate::observability::metrics().vlm_circuit_breaker_open.dec();
            crate::observability::metrics().sessions_reasoning_degraded.dec();

            tracing::info!(
                session_id = %session_id,
                "Circuit breaker closed — StepReadyForReasoning pushes resuming"
            );

            OutboundMessage::CircuitResetAck { session_id }
        }

        InboundMessage::CompileMemory { session_id } => {
            match registry.get(&session_id).await {
                Err(e) => OutboundMessage::Error {
                    code: "SESSION_NOT_FOUND".to_string(),
                    message: e.to_string(),
                },
                Ok(record) => {
                    use crate::registry::schema::SessionStatus;
                    match &record.status {
                        SessionStatus::PendingCompilation => {}
                        other => return OutboundMessage::Error {
                            code: "INVALID_STATUS".to_string(),
                            message: format!(
                                "Session '{session_id}' has status '{other}' — \
                                 CompileMemory requires pending_compilation."
                            ),
                        },
                    }
                    tracing::info!(
                        session_id = %session_id,
                        memory_path = %record.memory_path,
                        "CompileMemory: signalling scaffold readiness"
                    );
                    OutboundMessage::MemoryCompileReady {
                        session_id,
                        memory_path: record.memory_path,
                    }
                }
            }
        }

        InboundMessage::PricingRegistryStatus { status, manifest_age_seconds } => {
            crate::observability::metrics()
                .pricing_registry_fetch_status
                .get_or_create(&crate::observability::PricingStatusLabels {
                    status: status.clone(),
                })
                .inc();

            if let Some(age) = manifest_age_seconds {
                if age >= 0 {
                    crate::observability::metrics()
                        .pricing_registry_age_seconds
                        .set(age as f64);
                }
            }

            if status == "signature_failure" {
                let webhook = config.observability.alert_webhook_url.clone();
                tokio::spawn(async move {
                    crate::observability::send_alert(
                        &webhook,
                        "CRITICAL: Pricing registry signature verification failed — \
                         manifest discarded, falling back to cache or baseline",
                    ).await;
                });
            }

            tracing::debug!(status = %status, "PricingRegistryStatus received from ma-app");
            OutboundMessage::PricingStatusAck
        }

        InboundMessage::RegisterAnnotator { annotator_id, allowed_tenant_ids, max_concurrent_claims } => {
            match registry.register_annotator(&annotator_id, &allowed_tenant_ids, max_concurrent_claims).await {
                Err(e) if e.to_string() == "ANNOTATOR_EXISTS" => OutboundMessage::Error {
                    code: "ANNOTATOR_EXISTS".to_string(),
                    message: format!("Annotator '{annotator_id}' already exists."),
                },
                Err(e) => OutboundMessage::Error {
                    code: "REGISTER_FAILED".to_string(),
                    message: e.to_string(),
                },
                Ok(plaintext_key) => {
                    tracing::info!(annotator_id = %annotator_id, "Annotator registered via IPC");
                    OutboundMessage::AnnotatorRegistered { annotator_id, plaintext_key }
                }
            }
        }

        InboundMessage::DeactivateAnnotator { annotator_id } => {
            match registry.deactivate_annotator(&annotator_id).await {
                Err(e) if e.to_string() == "ANNOTATOR_NOT_FOUND" => OutboundMessage::Error {
                    code: "ANNOTATOR_NOT_FOUND".to_string(),
                    message: format!("Annotator '{annotator_id}' not found."),
                },
                Err(e) => OutboundMessage::Error {
                    code: "DEACTIVATE_FAILED".to_string(),
                    message: e.to_string(),
                },
                Ok(()) => OutboundMessage::AnnotatorDeactivated { annotator_id },
            }
        }

        InboundMessage::RotateAnnotatorKey { annotator_id } => {
            match registry.rotate_annotator_key(&annotator_id).await {
                Err(e) if e.to_string() == "ANNOTATOR_NOT_FOUND" => OutboundMessage::Error {
                    code: "ANNOTATOR_NOT_FOUND".to_string(),
                    message: format!("Annotator '{annotator_id}' not found."),
                },
                Err(e) => OutboundMessage::Error {
                    code: "ROTATE_FAILED".to_string(),
                    message: e.to_string(),
                },
                Ok(new_plaintext_key) => OutboundMessage::AnnotatorKeyRotated {
                    annotator_id,
                    new_plaintext_key,
                },
            }
        }

        InboundMessage::ListAnnotators => {
            match registry.list_annotators().await {
                Err(e) => OutboundMessage::Error {
                    code: "REDIS_ERROR".to_string(),
                    message: e.to_string(),
                },
                Ok(annotators) => OutboundMessage::AnnotatorList { annotators },
            }
        }

        InboundMessage::DeleteSession { session_id, force } => {
            use crate::registry::schema::SessionStatus;

            // A session id is always a UUID generated at registration. Reject
            // anything else — most importantly an empty id, which would make the
            // storage purge resolve to the storage root and delete every session.
            if uuid::Uuid::parse_str(&session_id).is_err() {
                return OutboundMessage::Error {
                    code: "INVALID_SESSION_ID".to_string(),
                    message: format!("'{session_id}' is not a valid session id."),
                };
            }

            match registry.get(&session_id).await {
                Ok(record) => {
                    // Refuse to delete an in-flight session unless forced.
                    if matches!(record.status, SessionStatus::Active | SessionStatus::Annotating)
                        && !force
                    {
                        return OutboundMessage::Error {
                            code: "SESSION_IN_FLIGHT".to_string(),
                            message: format!(
                                "Session '{session_id}' is '{}' — finish it (done/release) or pass \
                                 force to delete anyway.",
                                record.status
                            ),
                        };
                    }

                    // For a forced delete of an active session, drop the watch-loop
                    // handles first so it cannot rewrite files after the purge.
                    if record.status == SessionStatus::Active {
                        let _ = done_handles.lock().await.remove(&session_id);
                        let _ = push_handles.lock().await.remove(&session_id);
                    }

                    let redis_removed = match registry.delete_session(&session_id, Some(&record)).await {
                        Ok(v) => v,
                        Err(e) => return OutboundMessage::Error {
                            code: "DELETE_FAILED".to_string(),
                            message: format!("Redis purge failed: {e}"),
                        },
                    };

                    let storage = storage_router.resolve_for_session(&record);
                    let storage_objects_removed = storage.delete_all(&session_id).await
                        .unwrap_or_else(|e| {
                            tracing::warn!(session_id = %session_id, "DeleteSession: storage purge failed: {e}");
                            0
                        });

                    let local_dir_removed = crate::session::purge_memory_dir(&record.memory_path)
                        .unwrap_or_else(|e| {
                            tracing::warn!(session_id = %session_id, "DeleteSession: local dir purge failed: {e}");
                            false
                        });

                    tracing::info!(
                        session_id = %session_id,
                        memory_name = %record.memory_name,
                        storage_objects_removed,
                        local_dir_removed,
                        "Session deleted"
                    );

                    OutboundMessage::SessionDeleted {
                        session_id,
                        memory_name: record.memory_name,
                        redis_removed,
                        storage_objects_removed,
                        local_dir_removed,
                    }
                }
                Err(_) => {
                    // Hash already gone — sweep orphaned index/claim entries and any
                    // storage still keyed under this session id. memory_name is
                    // unknown, so the local memory directory cannot be located.
                    let redis_removed = match registry.delete_session(&session_id, None).await {
                        Ok(v) => v,
                        Err(e) => return OutboundMessage::Error {
                            code: "DELETE_FAILED".to_string(),
                            message: format!("Redis orphan sweep failed: {e}"),
                        },
                    };

                    let storage = storage_router.resolve("").1;
                    let storage_objects_removed = storage.delete_all(&session_id).await
                        .unwrap_or_else(|e| {
                            tracing::warn!(session_id = %session_id, "DeleteSession: orphan storage purge failed: {e}");
                            0
                        });

                    tracing::info!(
                        session_id = %session_id,
                        storage_objects_removed,
                        "Session orphan entries swept (no Hash present)"
                    );

                    OutboundMessage::SessionDeleted {
                        session_id,
                        memory_name: String::new(),
                        redis_removed,
                        storage_objects_removed,
                        local_dir_removed: false,
                    }
                }
            }
        }

        InboundMessage::AnnotatorAuth { .. }
        | InboundMessage::ListAnnotationQueue
        | InboundMessage::ClaimSession { .. }
        | InboundMessage::ReleaseSession { .. }
        | InboundMessage::HeartbeatClaim { .. } => OutboundMessage::Error {
            code: "FORBIDDEN".to_string(),
            message: "This is an annotator-only operation. Connect with an annotator_key to use it.".to_string(),
        },
    }
}

pub async fn serve_tcp(
    bind_addr: String,
    port: u16,
    token: String,
    registry: SessionRegistry,
    config: Config,
    done_handles: crate::capture::DoneHandleMap,
    push_handles: crate::capture::PushHandleMap,
    kafka_session_map: crate::kafka::KafkaSessionMap,
    storage_router: std::sync::Arc<StorageRouter>,
    tls_acceptor: TlsAcceptor,
    reasoning_maps: crate::capture::ReasoningMapsRef,
) -> anyhow::Result<()> {
    use tokio::net::TcpListener;

    let addr = format!("{bind_addr}:{port}");
    let listener = TcpListener::bind(&addr).await
        .with_context(|| format!("Failed to bind TCP IPC on {addr}"))?;

    tracing::info!("IPC TCP server ready (TLS 1.3)");
    tracing::debug!("IPC TCP bind address: {addr}");

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tracing::debug!("IPC TCP: new connection from {peer}");
                crate::observability::metrics().ipc_tcp_connections_active.inc();
                let reg   = registry.clone();
                let cfg   = config.clone();
                let dh    = done_handles.clone();
                let ph    = push_handles.clone();
                let tok   = token.clone();
                let ksm   = kafka_session_map.clone();
                let sr    = storage_router.clone();
                let acc   = tls_acceptor.clone();
                let rm    = reasoning_maps.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_tcp_connection(stream, peer, tok, acc, reg, cfg, dh, ph, ksm, sr, rm).await {
                        tracing::warn!("IPC TCP connection error from {peer}: {e}");
                    }
                    crate::observability::metrics().ipc_tcp_connections_active.dec();
                });
            }
            Err(e) => tracing::error!("IPC TCP accept error: {e}"),
        }
    }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn handle_annotator_connection<R, W>(
    mut lines: tokio::io::Lines<BufReader<R>>,
    mut writer: W,
    mut registry: SessionRegistry,
    config: Config,
    storage_router: std::sync::Arc<StorageRouter>,
    peer: std::net::SocketAddr,
    annotator_id: String,
    key: String,
    _reasoning_maps: crate::capture::ReasoningMapsRef,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    let send_msg = |msg: OutboundMessage| async move {
        serde_json::to_string(&msg).map(|mut s| { s.push('\n'); s })
    };

    // Per-annotator credential registry auth flow.
    //
    // 1. Look up annotator:{annotator_id} in Redis.
    // 2. Reject immediately if the record is absent — same response as a wrong
    //    key so the caller cannot distinguish between "unknown ID" and "wrong
    //    key" (no oracle leak).
    // 3. Reject if status != "active".
    // 4. Compute SHA-256(key) and compare against stored key_hash using
    //    constant-time comparison.
    // 5. On success: update last_auth_at and reset the failure counter.
    // 6. On failure: increment the failure counter (60-second rolling TTL).
    match registry.get_annotator_fields(&annotator_id).await {
        Err(e) => {
            tracing::error!(peer = %peer, "Annotator auth: Redis error: {e}");
            let msg = send_msg(OutboundMessage::Error {
                code: "INTERNAL_ERROR".to_string(),
                message: "Internal error during authentication.".to_string(),
            }).await?;
            writer.write_all(msg.as_bytes()).await?;
            return Ok(());
        }
        Ok(None) => {
            // Unknown annotator_id — same response as wrong key.
            tracing::warn!(peer = %peer, annotator_id = %annotator_id, "Annotator auth failed — unknown annotator_id");
            let _ = registry.increment_annotator_auth_failures(&annotator_id).await;
            crate::observability::metrics()
                .annotator_auth_failures_total
                .get_or_create(&crate::observability::AnnotatorIdLabels {
                    annotator_id: annotator_id.clone(),
                })
                .inc();
            let msg = send_msg(OutboundMessage::Error {
                code: "AUTH_FAILED".to_string(),
                message: "Authentication failed.".to_string(),
            }).await?;
            writer.write_all(msg.as_bytes()).await?;
            return Ok(());
        }
        Ok(Some(fields)) => {
            let status = fields.get("status").map(String::as_str).unwrap_or("");
            if status != "active" {
                tracing::warn!(peer = %peer, annotator_id = %annotator_id, "Annotator auth failed — account not active (status={status})");
                let _ = registry.increment_annotator_auth_failures(&annotator_id).await;
                crate::observability::metrics()
                    .annotator_auth_failures_total
                    .get_or_create(&crate::observability::AnnotatorIdLabels {
                        annotator_id: annotator_id.clone(),
                    })
                    .inc();
                let msg = send_msg(OutboundMessage::Error {
                    code: "AUTH_FAILED".to_string(),
                    message: "Authentication failed.".to_string(),
                }).await?;
                writer.write_all(msg.as_bytes()).await?;
                return Ok(());
            }

            let stored_hash = fields.get("key_hash").map(String::as_str).unwrap_or("");
            let candidate_hash = format!("{:x}", Sha256::digest(key.as_bytes()));

            if !constant_time_eq(&candidate_hash, stored_hash) {
                tracing::warn!(peer = %peer, annotator_id = %annotator_id, "Annotator auth failed — invalid key");
                let _ = registry.increment_annotator_auth_failures(&annotator_id).await;
                crate::observability::metrics()
                    .annotator_auth_failures_total
                    .get_or_create(&crate::observability::AnnotatorIdLabels {
                        annotator_id: annotator_id.clone(),
                    })
                    .inc();
                let msg = send_msg(OutboundMessage::Error {
                    code: "AUTH_FAILED".to_string(),
                    message: "Authentication failed.".to_string(),
                }).await?;
                writer.write_all(msg.as_bytes()).await?;
                return Ok(());
            }

            // Auth succeeded — update last_auth_at and clear failure counter.
            let _ = registry.update_annotator_last_auth(&annotator_id).await;
            let _ = registry.reset_annotator_auth_failures(&annotator_id).await;
        }
    }

    tracing::info!(peer = %peer, annotator_id = %annotator_id, "Annotator authenticated");

    let auth_msg = send_msg(OutboundMessage::AnnotatorAuthenticated {
        annotator_id: annotator_id.clone(),
    }).await?;
    writer.write_all(auth_msg.as_bytes()).await?;

    loop {
        let line = match lines.next_line().await? {
            None => break,
            Some(l) => l,
        };
        let line = line.trim().to_string();
        if line.is_empty() { continue; }

        if line.len() > 4 * 1024 * 1024 {
            tracing::warn!(annotator_id = %annotator_id, "Annotator: oversized message — closing");
            break;
        }

        let response = match serde_json::from_str::<InboundMessage>(&line) {
            Err(e) => OutboundMessage::Error {
                code: "PARSE_ERROR".to_string(),
                message: format!("Could not parse message: {e}"),
            },
            Ok(msg) => handle_annotator_message(
                msg,
                &annotator_id,
                &mut registry,
                &config,
                &storage_router,
            ).await,
        };

        let mut resp_json = serde_json::to_string(&response)?;
        resp_json.push('\n');
        writer.write_all(resp_json.as_bytes()).await?;
    }

    tracing::debug!(annotator_id = %annotator_id, "Annotator connection closed");
    Ok(())
}

/// Handle a single message from an annotator connection.
///
/// Admin messages (RegisterSession, StartWatch, Done, FinalizeMemory, etc.)
/// return FORBIDDEN. Annotators can only browse the queue, claim sessions,
/// and perform annotation operations on their claimed session.
async fn handle_annotator_message(
    msg: InboundMessage,
    annotator_id: &str,
    registry: &mut SessionRegistry,
    config: &Config,
    storage_router: &std::sync::Arc<StorageRouter>,
) -> OutboundMessage {
    const CLAIM_TTL_SECS: u64 = 30 * 60;

    match msg {
        InboundMessage::Ping => OutboundMessage::Pong {
            version: env!("CARGO_PKG_VERSION").to_string(),
        },

        InboundMessage::ListAnnotationQueue => {
            // Fetch this annotator's allowed_tenant_ids for queue filtering.
            let allowed_tenants: Vec<String> = match registry.get_annotator_fields(annotator_id).await {
                Ok(Some(fields)) => fields
                    .get("allowed_tenant_ids")
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default(),
                _ => vec![],
            };
            // Empty allowed_tenants means all tenants are visible.
            let filter_tenants = !allowed_tenants.is_empty();

            let ids = match registry.list_pending_human_annotation().await {
                Err(e) => return OutboundMessage::Error {
                    code: "REDIS_ERROR".to_string(),
                    message: e.to_string(),
                },
                Ok(v) => v,
            };

            // Filter out already-claimed sessions and apply tenant scoping.
            let mut unclaimed = Vec::new();
            for id in &ids {
                match registry.get_claim_owner(id).await {
                    Ok(None) => {}
                    _ => continue,
                }

                if filter_tenants {
                    // Fetch tenant_id for this session and check against allowed prefixes.
                    match registry.get(&id).await {
                        Ok(record) => {
                            let visible = allowed_tenants.iter().any(|prefix| {
                                record.tenant_id.starts_with(prefix.as_str())
                            });
                            if !visible { continue; }
                        }
                        Err(_) => continue,
                    }
                }

                unclaimed.push(id.clone());
            }

            let items = match registry.get_queue_items(&unclaimed).await {
                Err(e) => return OutboundMessage::Error {
                    code: "REDIS_ERROR".to_string(),
                    message: e.to_string(),
                },
                Ok(v) => v,
            };

            let mut sessions: Vec<QueueItem> = items
                .into_iter()
                .map(|(sid, name, steps, created)| QueueItem {
                    session_id: sid,
                    memory_name: name,
                    total_steps: steps,
                    created_at: created,
                })
                .collect();

            // Sort oldest first so auto-claim is deterministic.
            sessions.sort_by(|a, b| a.created_at.cmp(&b.created_at));

            OutboundMessage::AnnotationQueue { sessions }
        }

        InboundMessage::ClaimSession { session_id } => {
            // Enforce max_concurrent_claims before attempting SET NX.
            //
            // We read max_concurrent_claims from the annotator registry and,
            // if non-zero, run an atomic Lua script that counts the annotator's
            // active claims and conditionally sets the new claim key — all in
            // one Redis round trip. This eliminates the race between "count
            // claims" and "set claim" that would exist with two separate commands.
            //
            // Lua script contract:
            //   KEYS[1] = "claim:{target_id}"   — key to SET NX
            //   ARGV[1] = "{annotator_id}:{claim_id}" — value to store
            //   ARGV[2] = ttl_seconds (string)
            //   ARGV[3] = annotator_id prefix to match: "{annotator_id}:"
            //   ARGV[4] = max_concurrent_claims (string; "0" = unlimited)
            //
            // Returns: "ok"     — claim set, annotator is under the limit
            //          "limit"  — claim rejected, current count returned in ERR
            //          "taken"  — claim key already existed (another annotator)
            //
            // The claim value format is "{annotator_id}:{claim_id}" which lets
            // get_claim_owner() extract the annotator_id with a split(':').next().

            // Fetch annotator limits.
            let max_concurrent_claims: u32 = match registry.get_annotator_fields(annotator_id).await {
                Ok(Some(fields)) => fields
                    .get("max_concurrent_claims")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                _ => 0,
            };

            // Resolve the target session (auto-claim oldest if empty).
            let target_id = if session_id.is_empty() {
                let ids = match registry.list_pending_human_annotation().await {
                    Err(e) => return OutboundMessage::Error {
                        code: "REDIS_ERROR".to_string(),
                        message: e.to_string(),
                    },
                    Ok(v) => v,
                };

                let mut oldest: Option<(String, String)> = None;
                for id in &ids {
                    if registry.get_claim_owner(id).await.ok().flatten().is_none() {
                        if let Ok(record) = registry.get(id).await {
                            let ts = record.created_at.to_rfc3339();
                            if oldest.as_ref().map(|(_, t)| &ts < t).unwrap_or(true) {
                                oldest = Some((id.clone(), ts));
                            }
                        }
                    }
                }

                match oldest {
                    None => return OutboundMessage::Error {
                        code: "QUEUE_EMPTY".to_string(),
                        message: "No sessions available for annotation.".to_string(),
                    },
                    Some((id, _)) => id,
                }
            } else {
                session_id
            };

            let claim_id = uuid::Uuid::new_v4().to_string();

            // Atomic check-and-claim via Lua.
            //
            // The script scans claim:* keys to count this annotator's active
            // claims in the same transaction as the SET NX, so the count and
            // the set are never split across concurrent requests.
            let lua_script = r#"
                local claim_key = KEYS[1]
                local claim_val = ARGV[1]
                local ttl       = tonumber(ARGV[2])
                local prefix    = ARGV[3]
                local max_c     = tonumber(ARGV[4])

                if max_c > 0 then
                    local cursor = "0"
                    local count  = 0
                    repeat
                        local res = redis.call("SCAN", cursor, "MATCH", "claim:*", "COUNT", 200)
                        cursor = res[1]
                        local keys = res[2]
                        for _, k in ipairs(keys) do
                            local v = redis.call("GET", k)
                            if v and string.sub(v, 1, #prefix) == prefix then
                                count = count + 1
                            end
                        end
                    until cursor == "0"
                    if count >= max_c then
                        return {"limit", tostring(count)}
                    end
                end

                local set = redis.call("SET", claim_key, claim_val, "NX", "EX", ttl)
                if set == false then
                    return {"taken"}
                end
                return {"ok"}
            "#;

            let claim_key = format!("claim:{target_id}");
            let claim_val = format!("{annotator_id}:{claim_id}");
            let prefix    = format!("{annotator_id}:");

            let result: Vec<String> = match redis::Script::new(lua_script)
                .key(&claim_key)
                .arg(&claim_val)
                .arg(CLAIM_TTL_SECS)
                .arg(&prefix)
                .arg(max_concurrent_claims)
                .invoke_async(registry.raw_conn())
                .await
            {
                Err(e) => return OutboundMessage::Error {
                    code: "REDIS_ERROR".to_string(),
                    message: e.to_string(),
                },
                Ok(v) => v,
            };

            match result.first().map(String::as_str) {
                Some("ok") => {
                    tracing::info!(
                        session_id = %target_id,
                        annotator_id = %annotator_id,
                        "Session claimed from queue"
                    );
                    crate::observability::metrics()
                        .annotator_active_claims
                        .get_or_create(&crate::observability::AnnotatorIdLabels {
                            annotator_id: annotator_id.to_string(),
                        })
                        .inc();
                    OutboundMessage::SessionClaimed {
                        session_id: target_id,
                        claim_id,
                    }
                }
                Some("taken") => OutboundMessage::ClaimConflict {
                    session_id: target_id,
                },
                Some("limit") => {
                    let current_count = result.get(1)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0u32);
                    OutboundMessage::ClaimLimitReached {
                        annotator_id: annotator_id.to_string(),
                        current_count,
                        limit: max_concurrent_claims,
                    }
                }
                _ => OutboundMessage::Error {
                    code: "REDIS_ERROR".to_string(),
                    message: "Unexpected Lua script result.".to_string(),
                },
            }
        }

        InboundMessage::HeartbeatClaim { session_id, claim_id } => {
            // Only the annotator holding the claim may refresh its TTL. Without
            // this gate, refresh_claim's id check alone could be bypassed and an
            // annotator could keep another annotator's claim alive indefinitely.
            let owner = registry.get_claim_owner(&session_id).await.ok().flatten();
            if owner.as_deref() != Some(annotator_id) {
                return OutboundMessage::Error {
                    code: "NOT_OWNER".to_string(),
                    message: "You do not hold the claim for this session.".to_string(),
                };
            }
            match registry.refresh_claim(&session_id, &claim_id, CLAIM_TTL_SECS).await {
                Err(e) => OutboundMessage::Error {
                    code: "REDIS_ERROR".to_string(),
                    message: e.to_string(),
                },
                Ok(false) => OutboundMessage::Error {
                    code: "CLAIM_LOST".to_string(),
                    message: format!(
                        "Claim on session '{session_id}' has expired or was released. \
                         Return to queue: memory-archive annotator queue"
                    ),
                },
                Ok(true) => OutboundMessage::ClaimRefreshed { session_id },
            }
        }

        InboundMessage::ReleaseSession { session_id, claim_id } => {
            // Verify ownership before releasing.
            let owner = registry.get_claim_owner(&session_id).await.ok().flatten();
            let owned = owner.as_deref() == Some(annotator_id);

            if !owned {
                return OutboundMessage::Error {
                    code: "NOT_OWNER".to_string(),
                    message: "You do not hold the claim for this session.".to_string(),
                };
            }

            // Verify claim_id matches.
            let claim_valid = registry.verify_claim_id(&session_id, &claim_id).await.unwrap_or(false);

            if !claim_valid {
                return OutboundMessage::Error {
                    code: "CLAIM_MISMATCH".to_string(),
                    message: "Claim ID does not match.".to_string(),
                };
            }

            if let Err(e) = registry.release_claim(&session_id).await {
                return OutboundMessage::Error {
                    code: "REDIS_ERROR".to_string(),
                    message: e.to_string(),
                };
            }

            // Transition back to pending_human_annotation if still in annotating.
            if let Ok(record) = registry.get(&session_id).await {
                if record.status == crate::registry::schema::SessionStatus::Annotating {
                    let _ = registry.update_status(
                        &session_id,
                        crate::registry::schema::SessionStatus::PendingHumanAnnotation,
                    ).await;
                }
            }

            tracing::info!(
                session_id = %session_id,
                annotator_id = %annotator_id,
                "Session released back to queue"
            );
            crate::observability::metrics()
                .annotator_active_claims
                .get_or_create(&crate::observability::AnnotatorIdLabels {
                    annotator_id: annotator_id.to_string(),
                })
                .dec();
            OutboundMessage::SessionReleased { session_id }
        }

        // Annotation operations — require an active claim.
        InboundMessage::LoadSession { .. }
        | InboundMessage::GetSessionStatus { .. }
        | InboundMessage::UpdateAnnotationProgress { .. }
        | InboundMessage::CloseAnnotation { .. }
        | InboundMessage::CompleteAnnotation { .. }
        | InboundMessage::FetchFile { .. }
        | InboundMessage::ListSessionFiles { .. }
        | InboundMessage::UploadFile { .. } => {
            // Extract session_id from whichever variant matched.
            let sid = match &msg {
                InboundMessage::LoadSession { session_id } => session_id.clone(),
                InboundMessage::GetSessionStatus { session_id } => session_id.clone(),
                InboundMessage::UpdateAnnotationProgress { session_id, .. } => session_id.clone(),
                InboundMessage::CloseAnnotation { session_id } => session_id.clone(),
                InboundMessage::CompleteAnnotation { session_id } => session_id.clone(),
                InboundMessage::FetchFile { session_id, .. } => session_id.clone(),
                InboundMessage::ListSessionFiles { session_id, .. } => session_id.clone(),
                InboundMessage::UploadFile { session_id, .. } => session_id.clone(),
                _ => unreachable!(),
            };

            // Verify the annotator holds the claim.
            let owner = registry.get_claim_owner(&sid).await.ok().flatten();
            if owner.as_deref() != Some(annotator_id) {
                return OutboundMessage::Error {
                    code: "NOT_OWNER".to_string(),
                    message: format!(
                        "You do not hold the claim for session '{sid}'. \
                         Claim it first: memory-archive annotator claim --session {sid}"
                    ),
                };
            }

            // Delegate to the same logic used by admin connections.
            handle_annotator_session_op(msg, registry, config, storage_router).await
        }

        _ => OutboundMessage::Error {
            code: "FORBIDDEN".to_string(),
            message: "This operation is not permitted for annotator connections.".to_string(),
        },
    }
}

/// Execute an annotation session operation that has already been claim-verified.
async fn handle_annotator_session_op(
    msg: InboundMessage,
    registry: &mut SessionRegistry,
    config: &Config,
    storage_router: &std::sync::Arc<StorageRouter>,
) -> OutboundMessage {
    match msg {
        InboundMessage::GetSessionStatus { session_id } => {
            match registry.get(&session_id).await {
                Ok(record) => {
                    let pairs = record.to_redis_pairs();
                    let mut map: std::collections::HashMap<String, serde_json::Value> = pairs
                        .into_iter()
                        .map(|(k, v)| (k, serde_json::Value::String(v)))
                        .collect();
                    // Strip the secrets store reference from all GetSessionStatus responses.
                    // The caller who registered the session already knows the ref they sent;
                    // echoing it back in status responses creates unnecessary exposure.
                    map.remove("model_api_key_ref");
                    OutboundMessage::SessionStatus { session: map }
                }
                Err(e) => OutboundMessage::Error {
                    code: "SESSION_NOT_FOUND".to_string(),
                    message: e.to_string(),
                },
            }
        }

        InboundMessage::LoadSession { session_id } => {
            use crate::registry::schema::SessionStatus;
            match registry.get(&session_id).await {
                Err(e) => OutboundMessage::Error {
                    code: "SESSION_NOT_FOUND".to_string(),
                    message: e.to_string(),
                },
                Ok(record) => {
                    match &record.status {
                        SessionStatus::PendingHumanAnnotation | SessionStatus::Annotating => {}
                        other => return OutboundMessage::Error {
                            code: "INVALID_STATUS".to_string(),
                            message: format!(
                                "Session '{session_id}' has status '{other}' — cannot load for annotation."
                            ),
                        },
                    }
                    let was_interrupted = matches!(record.status, SessionStatus::Annotating);
                    if let Err(e) = registry.update_status(&session_id, SessionStatus::Annotating).await {
                        return OutboundMessage::Error {
                            code: "REDIS_ERROR".to_string(),
                            message: e.to_string(),
                        };
                    }
                    OutboundMessage::SessionLoaded {
                        session_id,
                        memory_path: record.memory_path,
                        was_interrupted,
                    }
                }
            }
        }

        InboundMessage::UpdateAnnotationProgress { session_id, annotated, skipped } => {
            if let Err(e) = registry.update_annotation_counters(&session_id, annotated, skipped).await {
                tracing::warn!(session_id = %session_id, "Counter update failed: {e}");
            }
            OutboundMessage::AnnotationProgressUpdated { session_id, annotated, skipped }
        }

        InboundMessage::CloseAnnotation { session_id } => {
            use crate::registry::schema::SessionStatus;
            if let Err(e) = registry.update_status(&session_id, SessionStatus::PendingHumanAnnotation).await {
                return OutboundMessage::Error {
                    code: "REDIS_ERROR".to_string(),
                    message: e.to_string(),
                };
            }
            OutboundMessage::AnnotationClosed { session_id }
        }

        InboundMessage::CompleteAnnotation { session_id } => {
            use crate::registry::schema::SessionStatus;
            if let Err(e) = registry.update_status(&session_id, SessionStatus::PendingCompilation).await {
                return OutboundMessage::Error {
                    code: "REDIS_ERROR".to_string(),
                    message: e.to_string(),
                };
            }
            OutboundMessage::AnnotationCompleted { session_id }
        }

        InboundMessage::FetchFile { session_id, relative_path } => {
            match registry.get(&session_id).await {
                Err(e) => OutboundMessage::Error {
                    code: "SESSION_NOT_FOUND".to_string(),
                    message: e.to_string(),
                },
                Ok(record) => {
                    let storage = storage_router.resolve_for_session(&record);
                    let bytes_result: anyhow::Result<Vec<u8>> = if config.storage_mode == "cloud_primary" {
                        if relative_path.contains("..") || relative_path.starts_with('/') {
                            return OutboundMessage::Error {
                                code: "INVALID_PATH".to_string(),
                                message: "relative_path must not contain '..' or start with '/'".to_string(),
                            };
                        }
                        let cloud_path = format!("{}/{}", record.memory_name, relative_path);
                        storage.get(&session_id, &cloud_path).await
                    } else {
                        match validate_relative_path(&record.memory_path, &relative_path) {
                            Err(e) => return OutboundMessage::Error {
                                code: "INVALID_PATH".to_string(),
                                message: e.to_string(),
                            },
                            Ok(abs) => tokio::fs::read(&abs).await.map_err(Into::into),
                        }
                    };
                    match bytes_result {
                        Ok(data) => {
                            let size = data.len() as u64;
                            OutboundMessage::FileData { session_id, relative_path, bytes: data, size }
                        }
                        Err(e) => OutboundMessage::Error {
                            code: "FILE_NOT_FOUND".to_string(),
                            message: format!("Could not read '{relative_path}': {e}"),
                        },
                    }
                }
            }
        }

        InboundMessage::ListSessionFiles { session_id, prefix } => {
            match registry.get(&session_id).await {
                Err(e) => OutboundMessage::Error {
                    code: "SESSION_NOT_FOUND".to_string(),
                    message: e.to_string(),
                },
                Ok(record) => {
                    let storage = storage_router.resolve_for_session(&record);
                    let files: Vec<FileEntry> = if config.storage_mode == "cloud_primary" {
                        let cloud_prefix = if prefix.is_empty() {
                            format!("{}/", record.memory_name)
                        } else {
                            format!("{}/{}", record.memory_name, prefix)
                        };
                        match storage.list(&session_id, &cloud_prefix).await {
                            Ok(paths) => paths.into_iter().map(|p| FileEntry { path: p, size: 0 }).collect(),
                            Err(e) => return OutboundMessage::Error {
                                code: "LIST_FAILED".to_string(),
                                message: e.to_string(),
                            },
                        }
                    } else {
                        let base = std::path::PathBuf::from(&record.memory_path);
                        let scan_dir = if prefix.is_empty() {
                            base.clone()
                        } else {
                            match validate_relative_path(&record.memory_path, &prefix) {
                                Ok(p) => p,
                                Err(e) => return OutboundMessage::Error {
                                    code: "INVALID_PATH".to_string(),
                                    message: e.to_string(),
                                },
                            }
                        };
                        list_dir_recursive(&scan_dir, &base)
                    };
                    OutboundMessage::SessionFileList { session_id, files }
                }
            }
        }

        InboundMessage::UploadFile { session_id, relative_path, bytes, content_type } => {
            // Annotator write authority enforcement — identical to the inner
            // handler. Only reasoning.jsonl (source forced to "human") and
            // metadata.json (annotator-updatable fields only) are writable;
            // enforced server-side for training-data integrity.
            const ALLOWED_REASONING: &str = "reasoning/reasoning.jsonl";
            const ALLOWED_METADATA:  &str = "metadata.json";

            if relative_path != ALLOWED_REASONING && relative_path != ALLOWED_METADATA {
                return OutboundMessage::Error {
                    code: "WRITE_FORBIDDEN".to_string(),
                    message: format!(
                        "Annotators may only write to '{ALLOWED_REASONING}' or '{ALLOWED_METADATA}'. \
                         Path '{relative_path}' is not permitted."
                    ),
                };
            }

            if bytes.len() > MAX_UPLOAD_BYTES {
                return OutboundMessage::Error {
                    code: "PAYLOAD_TOO_LARGE".to_string(),
                    message: format!("Upload exceeds maximum allowed size of {} bytes", MAX_UPLOAD_BYTES),
                };
            }

            let write_bytes: Vec<u8> = if relative_path == ALLOWED_REASONING {
                match enforce_reasoning_source_human(&bytes) {
                    Ok(b) => b,
                    Err(e) => return OutboundMessage::Error {
                        code: "INVALID_PAYLOAD".to_string(),
                        message: format!("reasoning.jsonl parse error: {e}"),
                    },
                }
            } else {
                match enforce_metadata_annotator_fields(&bytes) {
                    Ok(b) => b,
                    Err(e) => return OutboundMessage::Error {
                        code: "INVALID_PAYLOAD".to_string(),
                        message: format!("metadata.json parse error: {e}"),
                    },
                }
            };

            match registry.get(&session_id).await {
                Err(e) => OutboundMessage::Error {
                    code: "SESSION_NOT_FOUND".to_string(),
                    message: e.to_string(),
                },
                Ok(record) => {
                    let storage = storage_router.resolve_for_session(&record);
                    let result: anyhow::Result<()> = if config.storage_mode == "cloud_primary" {
                        let cloud_path = format!("{}/{}", record.memory_name, relative_path);
                        storage.put(&session_id, &cloud_path, write_bytes, &content_type).await
                    } else {
                        match validate_relative_path(&record.memory_path, &relative_path) {
                            Err(e) => return OutboundMessage::Error {
                                code: "INVALID_PATH".to_string(),
                                message: e.to_string(),
                            },
                            Ok(abs) => async {
                                if let Some(parent) = abs.parent() {
                                    tokio::fs::create_dir_all(parent).await?;
                                }
                                tokio::fs::write(&abs, &write_bytes).await?;
                                Ok(())
                            }.await,
                        }
                    };
                    match result {
                        Ok(()) => OutboundMessage::FileUploaded { session_id, relative_path },
                        Err(e) => OutboundMessage::Error {
                            code: "UPLOAD_FAILED".to_string(),
                            message: format!("Could not write '{relative_path}': {e}"),
                        },
                    }
                }
            }
        }

        InboundMessage::RegisterAnnotator { .. }
        | InboundMessage::DeactivateAnnotator { .. }
        | InboundMessage::RotateAnnotatorKey { .. }
        | InboundMessage::ListAnnotators
        | InboundMessage::PricingRegistryStatus { .. } => OutboundMessage::Error {
            code: "FORBIDDEN".to_string(),
            message: "This operation requires an admin connection.".to_string(),
        },

        _ => OutboundMessage::Error {
            code: "FORBIDDEN".to_string(),
            message: "This operation is not permitted for annotator connections.".to_string(),
        },
    }
}

async fn handle_tcp_connection(
    stream: tokio::net::TcpStream,
    peer: std::net::SocketAddr,
    token: String,
    tls_acceptor: TlsAcceptor,
    registry: SessionRegistry,
    config: Config,
    done_handles: crate::capture::DoneHandleMap,
    push_handles: crate::capture::PushHandleMap,
    kafka_session_map: crate::kafka::KafkaSessionMap,
    storage_router: std::sync::Arc<StorageRouter>,
    reasoning_maps: crate::capture::ReasoningMapsRef,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;

    // TLS handshake — abort silently on failure so a port scan or bad client
    // doesn't fill logs with noisy errors at warn level
    let tls_stream = match tls_acceptor.accept(stream).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("IPC TCP: TLS handshake failed from {peer}: {e}");
            return Ok(());
        }
    };

    let (reader, mut writer) = tokio::io::split(tls_stream);
    let mut lines = BufReader::new(reader).lines();

    let first_line = match lines.next_line().await? {
        None => {
            tracing::warn!("IPC TCP: connection closed before auth from {peer}");
            return Ok(());
        }
        Some(l) => l.trim().to_string(),
    };

    if first_line.starts_with('{') {
        // Annotator path — first message is JSON AnnotatorAuth.
        if let Ok(InboundMessage::AnnotatorAuth { annotator_id, key }) =
            serde_json::from_str::<InboundMessage>(&first_line)
        {
            return handle_annotator_connection(
                lines, writer, registry, config, storage_router, peer, annotator_id, key, reasoning_maps,
            ).await;
        }
        let msg = serde_json::to_string(&OutboundMessage::Error {
            code: "PARSE_ERROR".to_string(),
            message: "First message must be a plaintext admin token or a JSON annotator_auth.".to_string(),
        })? + "\n";
        writer.write_all(msg.as_bytes()).await?;
        return Ok(());
    }

    // Admin path — plaintext token comparison.
    if !token.is_empty() && !constant_time_eq(&first_line, &token) {
        tracing::warn!("IPC TCP: rejected connection from {peer} — invalid token");
        let msg = serde_json::to_string(&OutboundMessage::Error {
            code: "AUTH_FAILED".to_string(),
            message: "Invalid token.".to_string(),
        })? + "\n";
        writer.write_all(msg.as_bytes()).await?;
        return Ok(());
    }

    tracing::debug!("IPC TCP: authenticated from {peer}");
    handle_connection_inner(lines, writer, registry, config, done_handles, push_handles, kafka_session_map, storage_router, reasoning_maps).await
}

#[cfg(test)]
mod path_tests {
    use super::validate_relative_path;

    const BASE: &str = "/tmp/ma-session-base";

    #[test]
    fn accepts_paths_inside_base() {
        assert!(validate_relative_path(BASE, "reasoning/reasoning.jsonl").is_ok());
        assert!(validate_relative_path(BASE, "metadata.json").is_ok());
        // Interior '..' that stays within base is allowed.
        assert!(validate_relative_path(BASE, "a/../b").is_ok());
    }

    #[test]
    fn rejects_parent_traversal() {
        // The M-2 case: a prefix that climbs out of the session directory.
        assert!(validate_relative_path(BASE, "../etc/passwd").is_err());
        assert!(validate_relative_path(BASE, "a/../../etc").is_err());
        assert!(validate_relative_path(BASE, "../../../../etc").is_err());
    }

    #[test]
    fn rejects_absolute_paths() {
        assert!(validate_relative_path(BASE, "/etc/passwd").is_err());
    }

    #[test]
    fn rejects_empty() {
        assert!(validate_relative_path(BASE, "").is_err());
    }
}