// /Memory-Archive/ma-core/src/ipc/messages.rs

use serde::{Deserialize, Serialize};

mod base64_bytes {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(deserializer)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
}

/// One prior step's context, sent alongside StepReadyForReasoning so the
/// VLM has conversation history for reasoning about the current step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextStep {
    pub step_id: u32,
    pub converted_command: String,
    pub reasoning: String,
}

/// Per-session VLM configuration supplied at registration time by the
/// orchestration layer. The api_key_ref is a reference string only —
/// the actual key is resolved by ma-app from the secrets store at
/// call time and never crosses the IPC wire or enters Redis as a key value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    pub model_provider: String,
    pub model_endpoint: String,
    pub model_api_key_ref: String,
    #[serde(default = "default_context_window_steps")]
    pub context_window_steps: u32,
    /// Optional fallback VLM. All three fields must be set together or all absent.
    /// If fallback_model_provider is non-empty, fallback_model_endpoint and
    /// fallback_api_key_ref must also be non-empty. Enforced in ipc/mod.rs.
    #[serde(default)]
    pub fallback_model_provider: String,
    #[serde(default)]
    pub fallback_model_endpoint: String,
    #[serde(default)]
    pub fallback_api_key_ref: String,
}

fn default_context_window_steps() -> u32 { 5 }

mod base64_bytes_opt {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error> {
        if bytes.is_empty() {
            serializer.serialize_str("")
        } else {
            serializer.serialize_str(&STANDARD.encode(bytes))
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s.is_empty() {
            return Ok(Vec::new());
        }
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

/// All messages that ma-app (Python) can send to ma-core (Rust).
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboundMessage {
    /// Liveness check. Rust replies with Pong.
    Ping,

    /// Register a new session in Redis and create the memory directory.
    RegisterSession {
        mode: String,
        os_type: String,
        os_version: String,
        os_architecture: String,
        os_environment_id: String,
        capture_server_id: String,
        actuation_server_id: String,
        memory_name: String,
        reasoning_model_id: Option<String>,
        #[serde(default)]
        tenant_id: Option<String>,
        #[serde(default)]
        session_config: Option<SessionConfig>,
        /// HTTP address of the CC server for this session. Optional — empty = use global config.
        /// Must be a valid HTTP or HTTPS URL if non-empty. Validated in ipc/mod.rs.
        #[serde(default)]
        capture_server_addr: String,
        /// HTTP address of The-Eyes server for this session. Optional — empty = use global config.
        /// Must be a valid HTTP or HTTPS URL if non-empty. Validated in ipc/mod.rs.
        #[serde(default)]
        the_eyes_addr: String,
    },

    /// Fetch the current state of a session from Redis.
    GetSessionStatus {
        session_id: String,
    },

    /// Start watching a session's memory directory for changes.
    StartWatch {
        session_id: String,
    },

    /// Signal that actuation is complete — finalise capture files.
    Done {
        session_id: String,
    },

    /// Mark a session as actively being annotated in the TUI.
    /// Transitions status: pending_annotation → annotating.
    LoadSession {
        session_id: String,
    },

    /// Update annotated/skipped counters in Redis after each step is saved.
    UpdateAnnotationProgress {
        session_id: String,
        annotated: u32,
        skipped: u32,
    },

    /// Transition session status annotating → pending_annotation on clean TUI quit.
    CloseAnnotation {
        session_id: String,
    },

    /// Transition session status annotating → pending_compilation on annotation complete.
    CompleteAnnotation {
        session_id: String,
    },

    /// Mark memory as fully compiled — status → complete, apply 90-day TTL.
    FinalizeMemory {
        session_id: String,
    },

    /// Deliver VLM reasoning result for a step back to ma-core.
    /// Sent by ma-app after a successful VLM API response.
    /// ma-core attaches the reasoning to the in-memory step record and writes
    /// the full reasoning.jsonl entry.
    ReasoningResult {
        session_id: String,
        step_id: u32,
        reasoning: String,
        source: String,
        #[serde(default)]
        provider: String,
        model_id: String,
        api_version: String,
        input_tokens: u32,
        output_tokens: u32,
        latency_ms: u32,
        action_intent: Option<String>,
        confidence: Option<f32>,
        keyboard_visual_annotation: Option<serde_json::Value>,
    },

    /// Notify ma-core that the VLM circuit breaker has opened for a session.
    /// ma-core stops emitting StepReadyForReasoning for this session and
    /// transitions Redis status to reasoning_degraded.
    ReasoningDegraded {
        session_id: String,
        step_range_start: u32,
    },

    /// Notify ma-core that the VLM circuit breaker has closed after a successful
    /// trial request. ma-core resumes StepReadyForReasoning pushes and transitions
    /// Redis status back to active.
    CircuitReset {
        session_id: String,
    },

    /// Trigger memory.md compilation via IPC — used by the CUA in automated mode.
    /// ma-core scaffolds memory.md from reasoning.jsonl and signals readiness.
    CompileMemory {
        session_id: String,
    },

    /// Fetch a single session file via the ma-core proxy.
    /// In cloud_primary mode, ma-core fetches from cloud using its own credentials.
    /// In local mode, ma-core reads from disk at memory_path/relative_path.
    /// Remote annotator machines never need cloud credentials.
    FetchFile {
        session_id: String,
        relative_path: String,
    },

    /// List all session files, optionally filtered by a path prefix.
    /// Empty prefix lists all session files.
    ListSessionFiles {
        session_id: String,
        prefix: String,
    },

    /// Upload a file to session storage via the ma-core proxy.
    /// Used by remote annotators to persist reasoning.jsonl and metadata.json
    /// without requiring cloud credentials on the annotator machine.
    UploadFile {
        session_id: String,
        relative_path: String,
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
        content_type: String,
    },

    /// Annotator login — identifies the annotator and validates the shared key.
    /// Sent as the first JSON message by an annotator TCP connection.
    /// On success the connection is locked to annotator-scoped operations.
    AnnotatorAuth {
        annotator_id: String,
        key: String,
    },

    /// List all sessions in the pending_human_annotation queue.
    ListAnnotationQueue,

    /// Claim a session from the queue.
    /// session_id: specific session to claim, or empty string to auto-claim oldest.
    ClaimSession {
        session_id: String,
    },

    /// Release a previously claimed session back to the queue.
    ReleaseSession {
        session_id: String,
        claim_id: String,
    },

    /// Refresh the claim TTL — sent by the TUI heartbeat every 5 minutes.
    HeartbeatClaim {
        session_id: String,
        claim_id: String,
    },

    /// Report pricing registry fetch outcome to ma-core so it can
    /// update Prometheus metrics. Sent by ma-app after PricingRegistry.load().
    /// Best-effort — ma-core ignores if it cannot be processed.
    PricingRegistryStatus {
        /// "success" | "signature_failure" | "network_failure" | "cache_hit"
        status: String,
        /// Age of the manifest in seconds since its generated_at timestamp.
        /// None when no manifest was successfully loaded.
        #[serde(default)]
        manifest_age_seconds: Option<i64>,
    },

    /// Admin: register a new annotator in the Redis credential registry.
    /// Generates a random 256-bit key, stores its SHA-256 hash.
    /// Returns the plaintext key once in AnnotatorRegistered — it is never stored.
    RegisterAnnotator {
        annotator_id: String,
        /// JSON-encoded array of tenant ID prefixes this annotator can access.
        /// Empty array means all tenants are visible.
        #[serde(default)]
        allowed_tenant_ids: Vec<String>,
        /// Maximum concurrent session claims. 0 = unlimited.
        #[serde(default)]
        max_concurrent_claims: u32,
    },

    /// Admin: set an annotator's status to "deactivated".
    /// In-flight connections are not terminated; they fail on next claim attempt.
    DeactivateAnnotator {
        annotator_id: String,
    },

    /// Admin: generate a new random key for an annotator, update key_hash in Redis.
    /// Returns the new plaintext key once in AnnotatorKeyRotated.
    RotateAnnotatorKey {
        annotator_id: String,
    },

    /// Admin: list all registered annotators with live claim counts.
    ListAnnotators,

    /// Admin: permanently delete a session from everywhere — the Redis Hash, all
    /// index sets (status, by_os, by_mode), any claim, all storage objects, and
    /// the local memory directory. Irreversible.
    ///
    /// `force` is required to delete a session that is still `active` or
    /// `annotating`; without it such sessions are rejected as in-flight.
    /// If the session Hash is already gone, the delete still sweeps orphaned
    /// index/claim entries and any storage under the session id.
    DeleteSession {
        session_id: String,
        #[serde(default)]
        force: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    pub session_id: String,
    pub memory_name: String,
    pub total_steps: u32,
    pub created_at: String,
}

/// Per-annotator info returned by ListAnnotators.
/// `current_claims` is the live count from Redis at query time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotatorInfo {
    pub annotator_id: String,
    pub status: String,
    pub current_claims: u32,
    pub last_auth_at: String,
    pub allowed_tenant_ids: Vec<String>,
    pub max_concurrent_claims: u32,
}

/// All messages that ma-core (Rust) can send back to ma-app (Python).
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutboundMessage {
    /// Response to Ping.
    Pong {
        version: String,
    },

    /// Response to RegisterSession.
    SessionRegistered {
        session_id: String,
    },

    /// Response to GetSessionStatus.
    SessionStatus {
        session: std::collections::HashMap<String, serde_json::Value>,
    },

    /// Response to StartWatch — watch loop spawned successfully.
    /// Includes the session's per-session VLM config so ma-app can build the
    /// per-session ModelRouter without a separate Redis round-trip.
    /// model_api_key_ref and fallback_api_key_ref are reference strings only —
    /// not credential values. Empty strings indicate fields are not configured.
    WatchStarted {
        session_id: String,
        memory_path: String,
        model_provider: String,
        model_endpoint: String,
        model_api_key_ref: String,
        fallback_model_provider: String,
        fallback_model_endpoint: String,
        fallback_api_key_ref: String,
    },

    /// Pushed to Python when a session ends cleanly.
    SessionComplete {
        session_id: String,
        total_steps: u32,
    },

    /// Pushed to Python when a tool disconnects mid-session.
    SessionDisconnected {
        session_id: String,
        reason: String,
    },

    /// Response to LoadSession — session is now in annotating status.
    SessionLoaded {
        session_id: String,
        memory_path: String,
        was_interrupted: bool,
    },

    /// Response to CloseAnnotation.
    AnnotationClosed {
        session_id: String,
    },

    /// Response to CompleteAnnotation.
    AnnotationCompleted {
        session_id: String,
    },

    /// Response to UpdateAnnotationProgress.
    AnnotationProgressUpdated {
        session_id: String,
        annotated: u32,
        skipped: u32,
    },

    /// Response to FinalizeMemory.
    MemoryFinalized {
        session_id: String,
    },

    /// Response to ReasoningResult — reasoning.jsonl entry written successfully.
    ReasoningResultAccepted {
        session_id: String,
        step_id: u32,
    },

    /// Response to CircuitReset — ma-core will resume StepReadyForReasoning pushes.
    CircuitResetAck {
        session_id: String,
    },

    /// Pushed to the orchestration layer when capture begins for a session.
    /// Signals that the watch loop is active and events are being processed.
    SessionStarted {
        session_id: String,
    },

    /// Pushed to the orchestration layer when a session encounters an
    /// unrecoverable error — capture cannot continue.
    SessionFailed {
        session_id: String,
        reason: String,
    },

    /// Pushed to the orchestration layer and ma-app when the VLM circuit
    /// breaker opens. Capture continues in degraded mode.
    /// Also sent as a response to the ReasoningDegraded inbound message.
    ReasoningDegradedEvent {
        session_id: String,
        step_range_start: u32,
    },

    /// Pushed to ma-app when a step is ready for VLM reasoning.
    /// Contains the step data needed to call the VLM API.
    StepReadyForReasoning {
        session_id: String,
        step_id: u32,
        action_type: String,
        action_subtype: String,
        converted_command: String,
        #[serde(with = "base64_bytes")]
        at_frame_bytes: Vec<u8>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[serde(with = "base64_bytes_opt")]
        before_frame_bytes: Vec<u8>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[serde(with = "base64_bytes_opt")]
        after_frame_bytes: Vec<u8>,
        context_steps: Vec<ContextStep>,
    },

    /// Response to CompileMemory — scaffold is ready at memory_path.
    MemoryCompileReady {
        session_id: String,
        memory_path: String,
    },

    /// Pushed after every atomic file write — consumed by the Python sync worker.
    /// Not emitted in cloud_primary mode.
    FileWritten {
        session_id: String,
        relative_path: String,
        abs_path: String,
    },

    /// Response to FetchFile — contains the requested file's bytes as base64.
    FileData {
        session_id: String,
        relative_path: String,
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
        size: u64,
    },

    /// Response to ListSessionFiles.
    SessionFileList {
        session_id: String,
        files: Vec<FileEntry>,
    },

    /// Response to UploadFile — confirms the file was written to storage.
    FileUploaded {
        session_id: String,
        relative_path: String,
    },

    /// Response to AnnotatorAuth — connection is now annotator-scoped.
    AnnotatorAuthenticated {
        annotator_id: String,
    },

    /// Response to ListAnnotationQueue.
    AnnotationQueue {
        sessions: Vec<QueueItem>,
    },

    /// Response to ClaimSession — annotator now holds the exclusive claim.
    SessionClaimed {
        session_id: String,
        claim_id: String,
    },

    /// Response to ClaimSession — session already claimed by someone else.
    ClaimConflict {
        session_id: String,
    },

    /// Response to ReleaseSession — claim deleted, session back in queue.
    SessionReleased {
        session_id: String,
    },

    /// Response to HeartbeatClaim — claim TTL refreshed.
    ClaimRefreshed {
        session_id: String,
    },

    /// Response to RegisterAnnotator — plaintext key returned once only.
    AnnotatorRegistered {
        annotator_id: String,
        plaintext_key: String,
    },

    /// Response to DeactivateAnnotator.
    AnnotatorDeactivated {
        annotator_id: String,
    },

    /// Response to RotateAnnotatorKey — new plaintext key returned once only.
    AnnotatorKeyRotated {
        annotator_id: String,
        new_plaintext_key: String,
    },

    /// Response to ListAnnotators.
    AnnotatorList {
        annotators: Vec<AnnotatorInfo>,
    },

    /// Response to ClaimSession when the annotator has reached max_concurrent_claims.
    ClaimLimitReached {
        annotator_id: String,
        current_count: u32,
        limit: u32,
    },

    /// Response to PricingRegistryStatus.
    PricingStatusAck,

    /// Response to DeleteSession — the session was purged.
    /// `redis_removed` is true if a session Hash existed and was deleted;
    /// `storage_objects_removed` counts storage objects deleted (0 for a
    /// local-mode session, whose files live in the memory directory);
    /// `local_dir_removed` is true if a local memory directory was removed.
    SessionDeleted {
        session_id: String,
        memory_name: String,
        redis_removed: bool,
        storage_objects_removed: usize,
        local_dir_removed: bool,
    },

    /// Generic error response.
    Error {
        code: String,
        message: String,
    },
}