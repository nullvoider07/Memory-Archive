// /Memory-Archive/ma-core/src/registry/mod.rs
pub mod schema;

use std::collections::HashMap;

use anyhow::Context;
use chrono::Utc;
use redis::AsyncCommands;
use sha2::{Digest, Sha256};

use schema::{
    mode_index_key, os_index_key, session_key, SessionMode, SessionRecord, SessionStatus,
};

// SessionRegistry
#[derive(Clone)]
pub struct SessionRegistry {
    conn: redis::aio::ConnectionManager,
}

impl SessionRegistry {
    /// Connect to Redis and return a SessionRegistry.
    ///
    /// `redis_url` — e.g. "redis://127.0.0.1:6379"
    /// Uses ConnectionManager for automatic reconnection on transient failures.
    pub async fn connect(redis_url: &str) -> anyhow::Result<Self> {
        let client =
            redis::Client::open(redis_url).context("Invalid Redis URL")?;

        let conn = redis::aio::ConnectionManager::new(client)
            .await
            .context("Failed to connect to Redis")?;

        let display_url = redis_url.splitn(3, '@').last().unwrap_or(redis_url);
        tracing::info!("Redis connected: {display_url}");
        Ok(Self { conn })
    }

    // register
    /// Write a new session to Redis.
    ///
    /// - Creates the Hash at `session:{session_id}`
    /// - Adds the session_id to the appropriate index Sets
    /// - Does NOT set a TTL on active sessions (per policy)
    ///
    /// Returns an error if a session with the same ID already exists.
    pub async fn register(&mut self, record: &SessionRecord) -> anyhow::Result<()> {
        let key = session_key(&record.session_id);

        // Guard: reject duplicate session IDs.
        let exists: bool = self.conn.exists(&key).await
            .context("Redis EXISTS check failed")?;
        if exists {
            anyhow::bail!("Session '{}' already exists in registry", record.session_id);
        }

        // Write all Hash fields in a single HSET command.
        let pairs = record.to_redis_pairs();
        let flat: Vec<&str> = pairs
            .iter()
            .flat_map(|(k, v)| [k.as_str(), v.as_str()])
            .collect();

        redis::cmd("HSET")
            .arg(&key)
            .arg(&flat)
            .query_async::<()>(&mut self.conn)
            .await
            .context("HSET failed")?;

        // Update index sets.
        self.add_to_indexes(&record.session_id, &record.status, &record.os_type, &record.mode)
            .await?;

        tracing::info!(
            session_id = %record.session_id,
            mode = %record.mode,
            memory_name = %record.memory_name,
            "Session registered"
        );

        Ok(())
    }

    // get
    /// Read a session record from Redis.
    ///
    /// Returns an error if the session does not exist.
    pub async fn get(&mut self, session_id: &str) -> anyhow::Result<SessionRecord> {
        let key = session_key(session_id);

        let map: HashMap<String, String> = self.conn
            .hgetall(&key)
            .await
            .context("HGETALL failed")?;

        if map.is_empty() {
            anyhow::bail!("Session '{}' not found in registry", session_id);
        }

        SessionRecord::from_redis_map(map)
            .context("Failed to deserialise session record from Redis")
    }

    // update_status
    /// Update the status of a session and apply the appropriate TTL.
    ///
    /// - Removes the session_id from old status index set
    /// - Updates `status` and `updated_at` fields in the Hash
    /// - Adds the session_id to the new status index set
    /// - Applies TTL per SessionStatus::ttl_seconds()
    pub async fn update_status(
        &mut self,
        session_id: &str,
        new_status: SessionStatus,
    ) -> anyhow::Result<()> {
        let key = session_key(session_id);

        // Read current status to remove from old index set.
        let current_status_str: Option<String> = self.conn
            .hget(&key, "status")
            .await
            .context("HGET status failed")?;

        if let Some(ref s) = current_status_str {
            if let Ok(old_status) = s.parse::<SessionStatus>() {
                if let Some(old_set) = old_status.index_set() {
                    let _: () = self.conn
                        .srem(old_set, session_id)
                        .await
                        .context("SREM old status index failed")?;
                }
            }
        }

        // Update Hash fields.
        let now = Utc::now().to_rfc3339();
        redis::cmd("HSET")
            .arg(&key)
            .arg(&[
                "status", &new_status.to_string(),
                "updated_at", &now,
            ])
            .query_async::<()>(&mut self.conn)
            .await
            .context("HSET status update failed")?;

        // Add to new index set.
        if let Some(new_set) = new_status.index_set() {
            let _: () = self.conn
                .sadd(new_set, session_id)
                .await
                .context("SADD new status index failed")?;
        }

        // Apply TTL if this status requires one.
        if let Some(ttl) = new_status.ttl_seconds() {
            self.set_ttl(session_id, ttl).await?;
        }

        tracing::info!(
            session_id = %session_id,
            status = %new_status,
            "Session status updated"
        );

        Ok(())
    }

    // update_annotation_counters
    pub async fn update_annotation_counters(
        &mut self,
        session_id: &str,
        annotated: u32,
        skipped: u32,
    ) -> anyhow::Result<()> {
        let key = session_key(session_id);
        let now = Utc::now().to_rfc3339();

        redis::cmd("HSET")
            .arg(&key)
            .arg(&[
                "annotated_steps", &annotated.to_string(),
                "skipped_steps",   &skipped.to_string(),
                "updated_at",      &now,
            ])
            .query_async::<()>(&mut self.conn)
            .await
            .context("HSET annotation counters update failed")?;

        tracing::debug!(
            session_id = %session_id,
            annotated,
            skipped,
            "Annotation counters updated in Redis"
        );

        Ok(())
    }

    // set_ttl
    pub async fn set_ttl(&mut self, session_id: &str, seconds: u64) -> anyhow::Result<()> {
        let key = session_key(session_id);

        let _: () = self.conn
            .expire(&key, seconds as i64)
            .await
            .context("EXPIRE failed")?;

        tracing::debug!(
            session_id = %session_id,
            ttl_seconds = seconds,
            "TTL set on session key"
        );

        Ok(())
    }

    pub async fn list_active(&mut self) -> anyhow::Result<Vec<String>> {
        let ids: Vec<String> = self.conn
            .smembers("sessions:active")
            .await
            .context("SMEMBERS sessions:active failed")?;
        Ok(ids)
    }

    /// Return all session IDs currently in the `annotating` index set.
    ///
    /// Used by the startup sweep to reset stale annotating sessions back to
    /// pending_annotation — these are sessions where the TUI was killed without
    /// sending CloseAnnotation.
    #[allow(dead_code)]
    pub async fn list_annotating(&mut self) -> anyhow::Result<Vec<String>> {
        let ids: Vec<String> = self.conn
            .smembers("sessions:annotating")
            .await
            .context("SMEMBERS sessions:annotating failed")?;
        Ok(ids)
    }

    pub async fn list_pending_human_annotation(&mut self) -> anyhow::Result<Vec<String>> {
        let ids: Vec<String> = self.conn
            .smembers("sessions:pending_human_annotation")
            .await
            .context("SMEMBERS sessions:pending_human_annotation failed")?;
        Ok(ids)
    }

    /// Atomically claim a session for an annotator.
    ///
    /// Uses SETNX so only one annotator wins if multiple race simultaneously.
    /// Returns the claim_id on success, None if already claimed by someone else.
    #[allow(dead_code)]
    pub async fn claim_session(
        &mut self,
        session_id: &str,
        annotator_id: &str,
        claim_id: &str,
        ttl_secs: u64,
    ) -> anyhow::Result<Option<String>> {
        let key = format!("claim:{session_id}");
        let value = format!("{annotator_id}:{claim_id}");

        let set: bool = redis::cmd("SET")
            .arg(&key)
            .arg(&value)
            .arg("NX")
            .arg("EX")
            .arg(ttl_secs)
            .query_async(&mut self.conn)
            .await
            .context("SETNX claim failed")?;

        if set {
            tracing::info!(
                session_id = %session_id,
                annotator_id = %annotator_id,
                "Session claimed"
            );
            Ok(Some(claim_id.to_string()))
        } else {
            Ok(None)
        }
    }

    /// Release a claim. No-op if the key is already gone.
    pub async fn release_claim(&mut self, session_id: &str) -> anyhow::Result<()> {
        let key = format!("claim:{session_id}");
        let _: () = self.conn.del(&key).await.context("DEL claim failed")?;
        tracing::info!(session_id = %session_id, "Session claim released");
        Ok(())
    }

    /// Verify that a claim_id matches the stored claim value for a session.
    /// Returns true only when the key exists and the value contains the claim_id.
    pub async fn verify_claim_id(
        &mut self,
        session_id: &str,
        claim_id: &str,
    ) -> anyhow::Result<bool> {
        let key = format!("claim:{session_id}");
        let val: Option<String> = self.conn.get(&key).await.context("GET claim verify failed")?;
        Ok(val.as_deref().map(|v| claim_value_matches(v, claim_id)).unwrap_or(false))
    }

    /// Refresh a claim's TTL. Returns false if the claim no longer exists
    /// (expired or stolen) — the annotator should be warned.
    pub async fn refresh_claim(
        &mut self,
        session_id: &str,
        claim_id: &str,
        ttl_secs: u64,
    ) -> anyhow::Result<bool> {
        let key = format!("claim:{session_id}");
        let current: Option<String> = self.conn.get(&key).await.context("GET claim failed")?;

        let still_valid = current
            .as_deref()
            .map(|v| claim_value_matches(v, claim_id))
            .unwrap_or(false);

        if still_valid {
            let _: () = self.conn.expire(&key, ttl_secs as i64).await
                .context("EXPIRE claim failed")?;
        }

        Ok(still_valid)
    }

    /// Return the annotator_id currently holding the claim, if any.
    pub async fn get_claim_owner(&mut self, session_id: &str) -> anyhow::Result<Option<String>> {
        let key = format!("claim:{session_id}");
        let val: Option<String> = self.conn.get(&key).await.context("GET claim owner failed")?;
        Ok(val.and_then(|v| v.split(':').next().map(str::to_string)))
    }

    /// Fetch lightweight session info for the annotation queue display.
    /// Returns (session_id, memory_name, total_steps, created_at) tuples.
    pub async fn get_queue_items(
        &mut self,
        session_ids: &[String],
    ) -> anyhow::Result<Vec<(String, String, u32, String)>> {
        let mut items = Vec::new();
        for session_id in session_ids {
            let key = session_key(session_id);
            let fields: Vec<Option<String>> = redis::cmd("HMGET")
                .arg(&key)
                .arg(&["memory_name", "total_steps", "created_at"])
                .query_async(&mut self.conn)
                .await
                .context("HMGET queue item failed")?;

            let memory_name = fields[0].clone().unwrap_or_default();
            let total_steps = fields[1].as_deref()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            let created_at = fields[2].clone().unwrap_or_default();

            items.push((session_id.clone(), memory_name, total_steps, created_at));
        }
        Ok(items)
    }

    pub async fn list_pending_compilation(&mut self) -> anyhow::Result<Vec<String>> {
        let ids: Vec<String> = self.conn
            .smembers("sessions:pending_compilation")
            .await
            .context("SMEMBERS sessions:pending_compilation failed")?;
        Ok(ids)
    }

    pub async fn list_reasoning_degraded(&mut self) -> anyhow::Result<Vec<String>> {
        let ids: Vec<String> = self.conn
            .smembers("sessions:reasoning_degraded")
            .await
            .context("SMEMBERS sessions:reasoning_degraded failed")?;
        Ok(ids)
    }

    pub async fn remove_from_active_set(&mut self, session_id: &str) -> anyhow::Result<()> {
        let _: () = self.conn
            .srem("sessions:active", session_id)
            .await
            .context("SREM sessions:active failed")?;
        Ok(())
    }

    pub async fn cleanup_stale_index_entries(&mut self, set_key: &str) -> anyhow::Result<u64> {
        let members: Vec<String> = self.conn
            .smembers(set_key)
            .await
            .context("SMEMBERS failed")?;

        let mut removed = 0u64;
        for session_id in members {
            let key = session_key(&session_id);
            let exists: bool = self.conn
                .exists(&key)
                .await
                .context("EXISTS check failed")?;
            if !exists {
                let _: () = self.conn
                    .srem(set_key, &session_id)
                    .await
                    .context("SREM failed")?;
                removed += 1;
                tracing::debug!(session_id = %session_id, set = %set_key, "Stale index entry removed");
            }
        }
        Ok(removed)
    }

    /// Permanently remove a session from Redis: the Hash, every status/OS/mode
    /// index set it could belong to, and its claim key.
    ///
    /// SREM is issued against all index sets defensively (not just the current
    /// status set) so orphaned memberships — e.g. a session id left in
    /// `sessions:by_mode:manual` after its Hash expired — are cleaned up in the
    /// same pass. When `record` is provided the exact OS/mode sets are targeted;
    /// when it is `None` (Hash already gone) all OS and mode sets are swept.
    ///
    /// Returns `true` if a session Hash existed and was deleted.
    pub async fn delete_session(
        &mut self,
        session_id: &str,
        record: Option<&SessionRecord>,
    ) -> anyhow::Result<bool> {
        if session_id.is_empty() {
            anyhow::bail!("delete_session called with an empty session id");
        }
        let key = session_key(session_id);

        // Delete the Hash. DEL returns the number of keys removed.
        let hash_removed: u64 = self.conn.del(&key).await.context("DEL session hash failed")?;

        // Remove from every status index set (defensive — cleans orphans too).
        for set in [
            "sessions:active",
            "sessions:pending",
            "sessions:pending_human_annotation",
            "sessions:annotating",
            "sessions:pending_compilation",
            "sessions:reasoning_degraded",
        ] {
            let _: () = self.conn.srem(set, session_id).await
                .with_context(|| format!("SREM {set} failed"))?;
        }

        // Remove from OS and mode index sets.
        match record {
            Some(r) => {
                let os_key = os_index_key(&r.os_type);
                let _: () = self.conn.srem(&os_key, session_id).await
                    .with_context(|| format!("SREM {os_key} failed"))?;
                let mode_key = mode_index_key(&r.mode);
                let _: () = self.conn.srem(&mode_key, session_id).await
                    .with_context(|| format!("SREM {mode_key} failed"))?;
            }
            None => {
                for os in ["LINUX", "WINDOWS", "MACOS"] {
                    let os_key = os_index_key(os);
                    let _: () = self.conn.srem(&os_key, session_id).await
                        .with_context(|| format!("SREM {os_key} failed"))?;
                }
                for mode in [SessionMode::Manual, SessionMode::Automated] {
                    let mode_key = mode_index_key(&mode);
                    let _: () = self.conn.srem(&mode_key, session_id).await
                        .with_context(|| format!("SREM {mode_key} failed"))?;
                }
            }
        }

        // Remove any outstanding claim.
        let claim_key = format!("claim:{session_id}");
        let _: () = self.conn.del(&claim_key).await.context("DEL claim failed")?;

        tracing::info!(
            session_id = %session_id,
            hash_removed = hash_removed > 0,
            "Session purged from registry"
        );

        Ok(hash_removed > 0)
    }

    // Annotator credential registry

    /// Return all fields of an annotator's Redis Hash, or None if not found.
    /// Key format: `annotator:{annotator_id}`.
    pub async fn get_annotator_fields(
        &mut self,
        annotator_id: &str,
    ) -> anyhow::Result<Option<HashMap<String, String>>> {
        let key = format!("annotator:{annotator_id}");
        let map: HashMap<String, String> = self.conn
            .hgetall(&key)
            .await
            .context("HGETALL annotator failed")?;
        if map.is_empty() {
            Ok(None)
        } else {
            Ok(Some(map))
        }
    }

    /// Update the `last_auth_at` field of an annotator record to now.
    pub async fn update_annotator_last_auth(&mut self, annotator_id: &str) -> anyhow::Result<()> {
        let key = format!("annotator:{annotator_id}");
        let now = Utc::now().to_rfc3339();
        redis::cmd("HSET")
            .arg(&key)
            .arg(&["last_auth_at", &now])
            .query_async::<()>(&mut self.conn)
            .await
            .context("HSET annotator last_auth_at failed")?;
        Ok(())
    }

    /// Increment the auth failure counter for an annotator and set a 60-second
    /// rolling TTL so the counter resets automatically after a quiet period.
    /// Returns the new failure count after incrementing.
    pub async fn increment_annotator_auth_failures(
        &mut self,
        annotator_id: &str,
    ) -> anyhow::Result<u64> {
        let key = format!("annotator:{annotator_id}:auth_failures");
        let count: u64 = self.conn
            .incr(&key, 1u64)
            .await
            .context("INCR annotator auth_failures failed")?;
        let _: () = self.conn
            .expire(&key, 60i64)
            .await
            .context("EXPIRE annotator auth_failures failed")?;
        Ok(count)
    }

    /// Reset the auth failure counter for an annotator after a successful auth.
    pub async fn reset_annotator_auth_failures(&mut self, annotator_id: &str) -> anyhow::Result<()> {
        let key = format!("annotator:{annotator_id}:auth_failures");
        let _: () = self.conn
            .del(&key)
            .await
            .context("DEL annotator auth_failures failed")?;
        Ok(())
    }

    /// Register a new annotator in the Redis credential registry.
    ///
    /// Generates a 32-byte cryptographically random key, stores its SHA-256 hex
    /// hash, and returns the plaintext. The plaintext is never stored — the
    /// caller must distribute it to the annotator and then discard it.
    ///
    /// Returns `Err` if `annotator_id` already exists (ANNOTATOR_EXISTS guard).
    pub async fn register_annotator(
        &mut self,
        annotator_id: &str,
        allowed_tenant_ids: &[String],
        max_concurrent_claims: u32,
    ) -> anyhow::Result<String> {
        let key = format!("annotator:{annotator_id}");
        let exists: bool = self.conn.exists(&key).await
            .context("EXISTS annotator check failed")?;
        if exists {
            anyhow::bail!("ANNOTATOR_EXISTS");
        }

        // Generate 32 random bytes (256-bit key).
        let mut raw = [0u8; 32];
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut raw);
        let plaintext = raw.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let key_hash = format!("{:x}", Sha256::digest(plaintext.as_bytes()));

        let now = Utc::now().to_rfc3339();
        let tenants_json = serde_json::to_string(allowed_tenant_ids)
            .unwrap_or_else(|_| "[]".to_string());

        redis::cmd("HSET")
            .arg(&key)
            .arg(&[
                "annotator_id",          annotator_id,
                "key_hash",              &key_hash,
                "status",                "active",
                "allowed_tenant_ids",    &tenants_json,
                "max_concurrent_claims", &max_concurrent_claims.to_string(),
                "created_at",            &now,
                "deactivated_at",        "",
                "last_auth_at",          "",
            ])
            .query_async::<()>(&mut self.conn)
            .await
            .context("HSET register_annotator failed")?;

        tracing::info!(annotator_id = %annotator_id, "Annotator registered");
        Ok(plaintext)
    }

    /// Set annotator status to "deactivated". Record is preserved for audit history.
    pub async fn deactivate_annotator(&mut self, annotator_id: &str) -> anyhow::Result<()> {
        let key = format!("annotator:{annotator_id}");
        let exists: bool = self.conn.exists(&key).await
            .context("EXISTS annotator check failed")?;
        if !exists {
            anyhow::bail!("ANNOTATOR_NOT_FOUND");
        }
        let now = Utc::now().to_rfc3339();
        redis::cmd("HSET")
            .arg(&key)
            .arg(&[
                "status",           "deactivated",
                "deactivated_at",   &now,
            ])
            .query_async::<()>(&mut self.conn)
            .await
            .context("HSET deactivate_annotator failed")?;
        tracing::info!(annotator_id = %annotator_id, "Annotator deactivated");
        Ok(())
    }

    /// Generate a new key for an annotator and update key_hash in Redis.
    /// Returns the new plaintext key (shown once, then gone).
    pub async fn rotate_annotator_key(&mut self, annotator_id: &str) -> anyhow::Result<String> {
        let key = format!("annotator:{annotator_id}");
        let exists: bool = self.conn.exists(&key).await
            .context("EXISTS annotator check failed")?;
        if !exists {
            anyhow::bail!("ANNOTATOR_NOT_FOUND");
        }

        let mut raw = [0u8; 32];
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut raw);
        let plaintext = raw.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let key_hash = format!("{:x}", Sha256::digest(plaintext.as_bytes()));

        redis::cmd("HSET")
            .arg(&key)
            .arg(&["key_hash", &key_hash])
            .query_async::<()>(&mut self.conn)
            .await
            .context("HSET rotate_annotator_key failed")?;

        tracing::info!(annotator_id = %annotator_id, "Annotator key rotated");
        Ok(plaintext)
    }

    /// List all annotators with their live claim counts.
    /// Live count is derived by scanning `claim:*` keys — bounded and acceptable
    /// since annotator registry size is small relative to session count.
    pub async fn list_annotators(
        &mut self,
    ) -> anyhow::Result<Vec<crate::ipc::messages::AnnotatorInfo>> {
        // Scan for all annotator keys.
        let pattern = "annotator:*";
        let mut cursor: u64 = 0;
        let mut annotator_keys: Vec<String> = Vec::new();
        loop {
            let (next_cursor, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(pattern)
                .arg("COUNT")
                .arg(100u64)
                .query_async(&mut self.conn)
                .await
                .context("SCAN annotator keys failed")?;
            for k in keys {
                // Exclude sub-keys like annotator:{id}:auth_failures
                if k.matches(':').count() == 1 {
                    annotator_keys.push(k);
                }
            }
            cursor = next_cursor;
            if cursor == 0 { break; }
        }

        let mut result = Vec::new();
        for annotator_key in annotator_keys {
            let fields: HashMap<String, String> = self.conn
                .hgetall(&annotator_key)
                .await
                .context("HGETALL annotator list failed")?;
            if fields.is_empty() { continue; }

            let annotator_id = fields.get("annotator_id").cloned().unwrap_or_default();
            if annotator_id.is_empty() { continue; }

            let current_claims = self.count_annotator_claims(&annotator_id).await.unwrap_or(0);

            let allowed: Vec<String> = fields
                .get("allowed_tenant_ids")
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();

            result.push(crate::ipc::messages::AnnotatorInfo {
                annotator_id,
                status: fields.get("status").cloned().unwrap_or_default(),
                current_claims,
                last_auth_at: fields.get("last_auth_at").cloned().unwrap_or_default(),
                allowed_tenant_ids: allowed,
                max_concurrent_claims: fields
                    .get("max_concurrent_claims")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
            });
        }

        result.sort_by(|a, b| a.annotator_id.cmp(&b.annotator_id));
        Ok(result)
    }

    /// Count the number of active claims held by an annotator.
    ///
    /// Scans `claim:*` keys and counts those whose value starts with
    /// `{annotator_id}:`. This is O(N) in the number of live claims across all
    /// annotators — acceptable since claims are bounded by session count and
    /// TTL (30 minutes). The Lua atomic variant wraps this same logic
    /// inside EVAL so the check-and-claim is one round trip.
    pub async fn count_annotator_claims(&mut self, annotator_id: &str) -> anyhow::Result<u32> {
        let prefix = format!("{annotator_id}:");
        let mut cursor: u64 = 0;
        let mut count: u32 = 0;
        loop {
            let (next_cursor, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg("claim:*")
                .arg("COUNT")
                .arg(200u64)
                .query_async(&mut self.conn)
                .await
                .context("SCAN claim keys failed")?;
            for key in &keys {
                let val: Option<String> = self.conn.get(key).await.unwrap_or(None);
                if val.as_deref().map(|v| v.starts_with(&prefix)).unwrap_or(false) {
                    count += 1;
                }
            }
            cursor = next_cursor;
            if cursor == 0 { break; }
        }
        Ok(count)
    }

    /// Expose the raw connection manager for use with Redis scripts.
    /// Only for cases where the higher-level methods are insufficient (Lua eval).
    pub fn raw_conn(&mut self) -> &mut redis::aio::ConnectionManager {
        &mut self.conn
    }

    // Internal helpers
    async fn add_to_indexes(
        &mut self,
        session_id: &str,
        status: &SessionStatus,
        os_type: &str,
        mode: &SessionMode,
    ) -> anyhow::Result<()> {
        // Status index
        if let Some(set) = status.index_set() {
            let _: () = self.conn
                .sadd(set, session_id)
                .await
                .context("SADD status index failed")?;
        }

        // OS type index
        let os_key = os_index_key(os_type);
        let _: () = self.conn
            .sadd(&os_key, session_id)
            .await
            .context("SADD OS index failed")?;

        // Mode index
        let mode_key = mode_index_key(mode);
        let _: () = self.conn
            .sadd(&mode_key, session_id)
            .await
            .context("SADD mode index failed")?;

        Ok(())
    }
}

// Unit tests
//
/// True iff the stored claim value (`"{annotator_id}:{claim_id}"`) ends with
/// exactly `":{claim_id}"` and `claim_id` is non-empty.
///
/// Replaces a bare `str::contains`, which is a substring test: `contains("")`
/// is always true, so an empty (or substring) `claim_id` would spuriously match
/// any claim. Requiring a non-empty id that is the final `:`-delimited segment
/// makes the check exact.
fn claim_value_matches(stored: &str, claim_id: &str) -> bool {
    !claim_id.is_empty()
        && stored
            .strip_suffix(claim_id)
            .map(|prefix| prefix.ends_with(':'))
            .unwrap_or(false)
}

#[cfg(test)]
mod claim_match_tests {
    use super::claim_value_matches;

    #[test]
    fn exact_claim_id_matches() {
        assert!(claim_value_matches("annot-1:abc-123", "abc-123"));
    }

    #[test]
    fn empty_claim_id_never_matches() {
        // The bug: "".contains was always true. Must now be false.
        assert!(!claim_value_matches("annot-1:abc-123", ""));
        assert!(!claim_value_matches("annot-1:", ""));
    }

    #[test]
    fn substring_claim_id_does_not_match() {
        assert!(!claim_value_matches("annot-1:abc-123", "123"));   // suffix but not a segment
        assert!(!claim_value_matches("annot-1:abc-123", "abc"));   // prefix of the id
        assert!(!claim_value_matches("annot-1:abc-123", "abc-12")); // partial id
    }

    #[test]
    fn wrong_claim_id_does_not_match() {
        assert!(!claim_value_matches("annot-1:abc-123", "def-456"));
    }

    #[test]
    fn annotator_id_containing_colon_still_matches_final_segment() {
        assert!(claim_value_matches("a:b:the-claim", "the-claim"));
    }
}

// These tests require a running Redis instance.
// Run with: cargo test -p ma-core -- --test-threads=1
//
// --test-threads=1 prevents parallel tests from interfering with each other
// via shared Redis state (each test uses a unique session_id via uuid).

#[cfg(test)]
mod tests {
    use super::*;
    use schema::SessionMode;

    const TEST_REDIS_URL: &str = "redis://127.0.0.1:6379";

    /// Build a minimal SessionRecord for testing.
    fn test_record(session_id: &str) -> SessionRecord {
        let now = Utc::now();
        SessionRecord {
            session_id: session_id.to_string(),
            mode: SessionMode::Manual,
            status: SessionStatus::Active,
            os_type: "LINUX".to_string(),
            os_version: "Ubuntu 24.04 LTS".to_string(),
            os_architecture: "x86_64".to_string(),
            os_environment_id: "env-test-001".to_string(),
            capture_server_id: "cap-test-001".to_string(),
            capture_server_addr: String::new(),
            actuation_server_id: "act-test-001".to_string(),
            the_eyes_addr: String::new(),
            reasoning_model_id: None,
            memory_name: "test_memory".to_string(),
            memory_path: std::env::temp_dir().join("test_memory").to_string_lossy().to_string(),
            ma_core_addr: String::new(),
            created_at: now,
            updated_at: now,
            total_steps: 0,
            annotated_steps: 0,
            skipped_steps: 0,
            tenant_id: String::new(),
            model_provider: String::new(),
            model_endpoint: String::new(),
            model_api_key_ref: String::new(),
            context_window_steps: 5,
            storage_backend: String::new(),
            fallback_model_provider: String::new(),
            fallback_model_endpoint: String::new(),
            fallback_api_key_ref: String::new(),
        }
    }

    /// Clean up a test session from Redis after each test.
    async fn cleanup(registry: &mut SessionRegistry, session_id: &str) {
        let key = session_key(session_id);
        let _ = registry.conn.del::<_, ()>(&key).await;
        let _ = registry.conn.srem::<_, _, ()>("sessions:active", session_id).await;
        let _ = registry.conn.srem::<_, _, ()>("sessions:annotating", session_id).await;
        let _ = registry.conn.srem::<_, _, ()>("sessions:by_os:LINUX", session_id).await;
        let _ = registry.conn.srem::<_, _, ()>("sessions:by_mode:manual", session_id).await;
    }

    #[tokio::test]
    async fn test_register_and_get() {
        let mut registry = SessionRegistry::connect(TEST_REDIS_URL)
            .await
            .expect("Redis must be running for this test");

        let id = format!("test-{}", uuid::Uuid::new_v4());
        let record = test_record(&id);

        registry.register(&record).await.expect("register failed");

        let fetched = registry.get(&id).await.expect("get failed");
        assert_eq!(fetched.session_id, id);
        assert_eq!(fetched.mode, SessionMode::Manual);
        assert_eq!(fetched.status, SessionStatus::Active);
        assert_eq!(fetched.os_type, "LINUX");
        assert_eq!(fetched.memory_name, "test_memory");
        assert!(fetched.reasoning_model_id.is_none());

        cleanup(&mut registry, &id).await;
    }

    #[tokio::test]
    async fn test_register_duplicate_rejected() {
        let mut registry = SessionRegistry::connect(TEST_REDIS_URL)
            .await
            .expect("Redis must be running");

        let id = format!("test-{}", uuid::Uuid::new_v4());
        let record = test_record(&id);

        registry.register(&record).await.expect("first register failed");
        let result = registry.register(&record).await;
        assert!(result.is_err(), "Duplicate register should fail");

        cleanup(&mut registry, &id).await;
    }

    #[tokio::test]
    async fn test_get_missing_session_errors() {
        let mut registry = SessionRegistry::connect(TEST_REDIS_URL)
            .await
            .expect("Redis must be running");

        let result = registry.get("session-that-does-not-exist-xyz").await;
        assert!(result.is_err(), "get on missing session should return error");
    }

    #[tokio::test]
    async fn test_update_status() {
        let mut registry = SessionRegistry::connect(TEST_REDIS_URL)
            .await
            .expect("Redis must be running");

        let id = format!("test-{}", uuid::Uuid::new_v4());
        let record = test_record(&id);

        registry.register(&record).await.expect("register failed");
        registry
            .update_status(&id, SessionStatus::PendingAnnotation)
            .await
            .expect("update_status failed");

        let fetched = registry.get(&id).await.expect("get failed");
        assert_eq!(fetched.status, SessionStatus::PendingAnnotation);

        // Verify removed from active set, added to pending set.
        let in_active: bool = registry.conn
            .sismember("sessions:active", &id)
            .await
            .unwrap();
        let in_pending: bool = registry.conn
            .sismember("sessions:pending", &id)
            .await
            .unwrap();
        assert!(!in_active, "Should not be in sessions:active after status update");
        assert!(in_pending, "Should be in sessions:pending after status update");

        cleanup(&mut registry, &id).await;
        let _ = registry.conn.srem::<_, _, ()>("sessions:pending", &id).await;
    }

    #[tokio::test]
    async fn test_annotating_status_uses_index_and_ttl() {
        let mut registry = SessionRegistry::connect(TEST_REDIS_URL)
            .await
            .expect("Redis must be running");

        let id = format!("test-{}", uuid::Uuid::new_v4());
        let record = test_record(&id);

        registry.register(&record).await.expect("register failed");
        registry
            .update_status(&id, SessionStatus::Annotating)
            .await
            .expect("update_status to Annotating failed");

        // Should no longer be in sessions:active.
        let in_active: bool = registry.conn
            .sismember("sessions:active", &id)
            .await
            .unwrap();
        assert!(!in_active, "Should not be in sessions:active after transitioning to Annotating");

        // Should be in sessions:annotating.
        let in_annotating: bool = registry.conn
            .sismember("sessions:annotating", &id)
            .await
            .unwrap();
        assert!(in_annotating, "Should be in sessions:annotating");

        // TTL should be set (~7 days).
        let ttl: i64 = redis::cmd("TTL")
            .arg(session_key(&id))
            .query_async(&mut registry.conn)
            .await
            .unwrap();
        assert!(ttl > 0, "Annotating session should have a TTL set, got {ttl}");

        cleanup(&mut registry, &id).await;
    }

    #[tokio::test]
    async fn test_list_annotating() {
        let mut registry = SessionRegistry::connect(TEST_REDIS_URL)
            .await
            .expect("Redis must be running");

        let id = format!("test-{}", uuid::Uuid::new_v4());
        let record = test_record(&id);

        registry.register(&record).await.expect("register failed");
        registry
            .update_status(&id, SessionStatus::Annotating)
            .await
            .expect("update_status failed");

        let annotating = registry.list_annotating().await.expect("list_annotating failed");
        assert!(annotating.contains(&id), "Session should appear in list_annotating");

        cleanup(&mut registry, &id).await;
    }

    #[tokio::test]
    async fn test_set_ttl() {
        let mut registry = SessionRegistry::connect(TEST_REDIS_URL)
            .await
            .expect("Redis must be running");

        let id = format!("test-{}", uuid::Uuid::new_v4());
        let record = test_record(&id);

        registry.register(&record).await.expect("register failed");
        registry.set_ttl(&id, 3600).await.expect("set_ttl failed");

        let ttl: i64 = redis::cmd("TTL")
            .arg(session_key(&id))
            .query_async(&mut registry.conn)
            .await
            .unwrap();
        assert!(ttl > 3500 && ttl <= 3600, "TTL should be ~3600, got {ttl}");

        cleanup(&mut registry, &id).await;
    }

    #[tokio::test]
    async fn test_index_sets_populated_on_register() {
        let mut registry = SessionRegistry::connect(TEST_REDIS_URL)
            .await
            .expect("Redis must be running");

        let id = format!("test-{}", uuid::Uuid::new_v4());
        let record = test_record(&id);

        registry.register(&record).await.expect("register failed");

        let in_active: bool = registry.conn.sismember("sessions:active", &id).await.unwrap();
        let in_os: bool = registry.conn.sismember("sessions:by_os:LINUX", &id).await.unwrap();
        let in_mode: bool = registry.conn.sismember("sessions:by_mode:manual", &id).await.unwrap();

        assert!(in_active, "Should be in sessions:active");
        assert!(in_os, "Should be in sessions:by_os:LINUX");
        assert!(in_mode, "Should be in sessions:by_mode:manual");

        cleanup(&mut registry, &id).await;
    }

    #[tokio::test]
    async fn test_delete_session_purges_hash_and_indexes() {
        let mut registry = SessionRegistry::connect(TEST_REDIS_URL)
            .await
            .expect("Redis must be running");

        let id = format!("test-{}", uuid::Uuid::new_v4());
        let record = test_record(&id);

        registry.register(&record).await.expect("register failed");
        // Add a claim so we can confirm it is removed too.
        let _: () = registry.conn.set(format!("claim:{id}"), "annot-x:claim-y").await.unwrap();

        let existed = registry
            .delete_session(&id, Some(&record))
            .await
            .expect("delete_session failed");
        assert!(existed, "delete_session should report the hash existed");

        let hash_exists: bool = registry.conn.exists(session_key(&id)).await.unwrap();
        let in_active: bool = registry.conn.sismember("sessions:active", &id).await.unwrap();
        let in_os: bool = registry.conn.sismember("sessions:by_os:LINUX", &id).await.unwrap();
        let in_mode: bool = registry.conn.sismember("sessions:by_mode:manual", &id).await.unwrap();
        let claim_exists: bool = registry.conn.exists(format!("claim:{id}")).await.unwrap();

        assert!(!hash_exists, "session Hash should be gone");
        assert!(!in_active, "should be removed from sessions:active");
        assert!(!in_os, "should be removed from sessions:by_os:LINUX");
        assert!(!in_mode, "should be removed from sessions:by_mode:manual");
        assert!(!claim_exists, "claim key should be gone");

        // Second delete is a no-op orphan sweep and reports the hash was absent.
        let existed_again = registry
            .delete_session(&id, None)
            .await
            .expect("second delete_session failed");
        assert!(!existed_again, "second delete should report no hash existed");
    }
}