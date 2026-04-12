// /Memory/Memory-Archive/ma-core/src/storage/mod.rs

#![allow(dead_code)]

pub mod router;
pub use router::StorageRouter;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::config::Config;

#[async_trait]
pub trait StorageBackend: Send + Sync {
    async fn put(
        &self,
        session_id: &str,
        relative_path: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<()>;

    async fn get(&self, session_id: &str, relative_path: &str) -> Result<Vec<u8>>;

    async fn list(&self, session_id: &str, prefix: &str) -> Result<Vec<String>>;

    async fn delete(&self, session_id: &str, relative_path: &str) -> Result<()>;
}

pub struct LocalBackend {
    storage_path: PathBuf,
}

impl LocalBackend {
    pub fn new(storage_path: &str) -> Self {
        Self {
            storage_path: PathBuf::from(storage_path),
        }
    }

    fn resolve(&self, session_id: &str, relative_path: &str) -> PathBuf {
        self.storage_path.join(session_id).join(relative_path)
    }
}

#[async_trait]
impl StorageBackend for LocalBackend {
    async fn put(
        &self,
        session_id: &str,
        relative_path: &str,
        bytes: Vec<u8>,
        _content_type: &str,
    ) -> Result<()> {
        let path = self.resolve(session_id, relative_path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }

    async fn get(&self, session_id: &str, relative_path: &str) -> Result<Vec<u8>> {
        let path = self.resolve(session_id, relative_path);
        Ok(tokio::fs::read(&path).await?)
    }

    async fn list(&self, session_id: &str, prefix: &str) -> Result<Vec<String>> {
        let base = self.resolve(session_id, prefix);
        let mut results = Vec::new();

        if !base.exists() {
            return Ok(results);
        }

        let mut stack = vec![base.clone()];
        while let Some(dir) = stack.pop() {
            let mut entries = tokio::fs::read_dir(&dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(rel) = path.strip_prefix(&self.storage_path.join(session_id)) {
                    results.push(rel.to_string_lossy().to_string());
                }
            }
        }

        Ok(results)
    }

    async fn delete(&self, session_id: &str, relative_path: &str) -> Result<()> {
        let path = self.resolve(session_id, relative_path);
        if path.exists() {
            tokio::fs::remove_file(&path).await?;
        }
        Ok(())
    }
}

pub struct S3Backend {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl S3Backend {
    pub async fn new(bucket: String, region: String) -> Result<Self> {
        let region_code = normalize_aws_region(&region);
        let region = aws_sdk_s3::config::Region::new(region_code);

        let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(region)
            .load()
            .await;

        let client = aws_sdk_s3::Client::new(&sdk_config);

        client
            .head_bucket()
            .bucket(&bucket)
            .send()
            .await
            .with_context(|| {
                format!(
                    "S3 bucket '{bucket}' is not accessible — \
                     check bucket name, region, and IAM permissions"
                )
            })?;

        tracing::info!("S3 bucket verified accessible");
        tracing::debug!(bucket = %bucket, "S3 bucket name");

        Ok(Self { client, bucket })
    }

    fn object_key(&self, session_id: &str, relative_path: &str) -> String {
        format!("sessions/{session_id}/{relative_path}")
    }
}

#[async_trait]
impl StorageBackend for S3Backend {
    async fn put(
        &self,
        session_id: &str,
        relative_path: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<()> {
        let key = self.object_key(session_id, relative_path);
        let body = aws_sdk_s3::primitives::ByteStream::from(bytes);

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .content_type(content_type)
            .body(body)
            .send()
            .await
            .with_context(|| format!("S3 put failed for {key}"))?;

        tracing::debug!(key = %key, "S3 put complete");
        Ok(())
    }

    async fn get(&self, session_id: &str, relative_path: &str) -> Result<Vec<u8>> {
        let key = self.object_key(session_id, relative_path);

        let resp = self.client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .with_context(|| format!("S3 get failed for {key}"))?;

        let bytes = resp
            .body
            .collect()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to collect S3 body for {key}: {e}"))?
            .into_bytes()
            .to_vec();

        Ok(bytes)
    }

    async fn list(&self, session_id: &str, prefix: &str) -> Result<Vec<String>> {
        let full_prefix = self.object_key(session_id, prefix);
        let mut results = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut req = self.client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&full_prefix);

            if let Some(token) = continuation_token {
                req = req.continuation_token(token);
            }

            let resp = req
                .send()
                .await
                .with_context(|| format!("S3 list failed for prefix {full_prefix}"))?;

            for obj in resp.contents() {
                if let Some(key) = obj.key() {
                    let stripped = key
                        .strip_prefix(&full_prefix)
                        .unwrap_or(key)
                        .to_string();
                    if !stripped.is_empty() {
                        results.push(stripped);
                    }
                }
            }

            if resp.is_truncated().unwrap_or(false) {
                continuation_token = resp.next_continuation_token().map(str::to_string);
            } else {
                break;
            }
        }

        Ok(results)
    }

    async fn delete(&self, session_id: &str, relative_path: &str) -> Result<()> {
        let key = self.object_key(session_id, relative_path);

        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .with_context(|| format!("S3 delete failed for {key}"))?;

        tracing::debug!(key = %key, "S3 delete complete");
        Ok(())
    }
}

fn normalize_aws_region(region: &str) -> String {
    let region = region.trim();
    if region.is_empty() {
        return region.to_string();
    }
    let re = regex::Regex::new(r"[a-z]{2,}-[a-z]+-\d+").unwrap();
    if let Some(m) = re.find(region) {
        return m.as_str().to_string();
    }
    region.to_string()
}

const AZURE_API_VERSION: &str = "2026-02-06";
const TOKEN_REFRESH_BUFFER: Duration = Duration::from_secs(300);

struct CachedToken {
    token: String,
    expires_at: Instant,
}

pub struct AzureBackend {
    http: reqwest::Client,
    account: String,
    container: String,
    token_cache: Mutex<Option<CachedToken>>,
}

impl AzureBackend {
    pub async fn new(account: String, container: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("Failed to build HTTP client for AzureBackend")?;

        let backend = Self {
            http,
            account: account.clone(),
            container: container.clone(),
            token_cache: Mutex::new(None),
        };

        let token = backend.acquire_token().await.with_context(|| {
            format!(
                "Azure credential resolution failed for account '{account}'. \
                 Set AZURE_TENANT_ID + AZURE_CLIENT_ID + AZURE_CLIENT_SECRET, \
                 assign a Managed Identity, or run 'az login'."
            )
        })?;

        let url = format!(
            "https://{account}.blob.core.windows.net/{container}?restype=container"
        );

        let resp = backend
            .http
            .get(&url)
            .bearer_auth(&token)
            .header("x-ms-version", AZURE_API_VERSION)
            .send()
            .await
            .with_context(|| {
                format!("Azure container probe failed for '{container}' in account '{account}'")
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            anyhow::bail!(
                "Azure container is not accessible (HTTP {status}). \
                 Check account name, container name, and RBAC permissions \
                 (Storage Blob Data Contributor or Storage Blob Data Owner)."
            );
        }

        tracing::info!("Azure container verified accessible");
        tracing::debug!(account = %account, container = %container, "Azure storage identifiers");

        Ok(backend)
    }

    fn blob_url(&self, blob_name: &str) -> String {
        format!(
            "https://{}.blob.core.windows.net/{}/{}",
            self.account, self.container, blob_name
        )
    }

    fn container_url(&self) -> String {
        format!(
            "https://{}.blob.core.windows.net/{}",
            self.account, self.container
        )
    }

    fn blob_path(&self, session_id: &str, relative_path: &str) -> String {
        format!("sessions/{session_id}/{relative_path}")
    }

    async fn acquire_token(&self) -> Result<String> {
        {
            let cache = self.token_cache.lock().await;
            if let Some(ref cached) = *cache {
                if cached.expires_at > Instant::now() + TOKEN_REFRESH_BUFFER {
                    return Ok(cached.token.clone());
                }
            }
        }

        let result = self.try_service_principal().await?
            .or(self.try_managed_identity().await?)
            .or(self.try_azure_cli().await?);

        let (token, expires_in_secs) = result.ok_or_else(|| {
            anyhow::anyhow!(
                "No Azure credentials found. Set AZURE_TENANT_ID + AZURE_CLIENT_ID + \
                 AZURE_CLIENT_SECRET, use a Managed Identity, or run 'az login'."
            )
        })?;

        let expires_at = Instant::now() + Duration::from_secs(expires_in_secs);
        *self.token_cache.lock().await = Some(CachedToken {
            token: token.clone(),
            expires_at,
        });

        Ok(token)
    }

    async fn try_service_principal(&self) -> Result<Option<(String, u64)>> {
        let (tenant_id, client_id, client_secret) = match (
            std::env::var("AZURE_TENANT_ID").ok(),
            std::env::var("AZURE_CLIENT_ID").ok(),
            std::env::var("AZURE_CLIENT_SECRET").ok(),
        ) {
            (Some(t), Some(c), Some(s)) => (t, c, s),
            _ => return Ok(None),
        };

        let url = format!(
            "https://login.microsoftonline.com/{tenant_id}/oauth2/v2.0/token"
        );

        let resp = self
            .http
            .post(&url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", &client_id),
                ("client_secret", &client_secret),
                ("scope", "https://storage.azure.com/.default"),
            ])
            .send()
            .await
            .context("Service principal token request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let body_preview = body.chars().take(200).collect::<String>();
            anyhow::bail!("Service principal token request returned HTTP {status}: {body_preview}");
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse service principal token response")?;

        let token = json["access_token"]
            .as_str()
            .context("Missing access_token in service principal response")?
            .to_string();

        let expires_in = json["expires_in"].as_u64().unwrap_or(3600);

        tracing::debug!("Azure token acquired via service principal");
        Ok(Some((token, expires_in)))
    }

    async fn try_managed_identity(&self) -> Result<Option<(String, u64)>> {
        let imds_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .context("Failed to build IMDS HTTP client")?;

        let resp = match imds_client
            .get("http://169.254.169.254/metadata/identity/oauth2/token")
            .query(&[
                ("api-version", "2018-02-01"),
                ("resource", "https://storage.azure.com/"),
            ])
            .header("Metadata", "true")
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };

        if !resp.status().is_success() {
            return Ok(None);
        }

        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(_) => return Ok(None),
        };

        let token = match json["access_token"].as_str() {
            Some(t) => t.to_string(),
            None => return Ok(None),
        };

        let expires_in = json["expires_in"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(3600);

        tracing::debug!("Azure token acquired via Managed Identity");
        Ok(Some((token, expires_in)))
    }

    async fn try_azure_cli(&self) -> Result<Option<(String, u64)>> {
        let output = match tokio::process::Command::new("az")
            .args([
                "account",
                "get-access-token",
                "--resource",
                "https://storage.azure.com/",
                "--output",
                "json",
            ])
            .output()
            .await
        {
            Ok(o) => o,
            Err(_) => return Ok(None),
        };

        if !output.status.success() {
            return Ok(None);
        }

        let json: serde_json::Value = match serde_json::from_slice(&output.stdout) {
            Ok(j) => j,
            Err(_) => return Ok(None),
        };

        let token = match json["accessToken"].as_str() {
            Some(t) => t.to_string(),
            None => return Ok(None),
        };

        tracing::debug!("Azure token acquired via Azure CLI");
        Ok(Some((token, 3600)))
    }
}

#[async_trait]
impl StorageBackend for AzureBackend {
    async fn put(
        &self,
        session_id: &str,
        relative_path: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<()> {
        let blob_name = self.blob_path(session_id, relative_path);
        let url = self.blob_url(&blob_name);
        let token = self.acquire_token().await?;
        let content_length = bytes.len();

        let resp = self
            .http
            .put(&url)
            .bearer_auth(&token)
            .header("x-ms-version", AZURE_API_VERSION)
            .header("x-ms-blob-type", "BlockBlob")
            .header("Content-Type", content_type)
            .header("Content-Length", content_length)
            .body(bytes)
            .send()
            .await
            .with_context(|| format!("Azure put request failed for {blob_name}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let body_preview = body.chars().take(200).collect::<String>();
            anyhow::bail!("Azure put failed for {blob_name} (HTTP {status}): {body_preview}");
        }

        tracing::debug!(blob = %blob_name, "Azure put complete");
        Ok(())
    }

    async fn get(&self, session_id: &str, relative_path: &str) -> Result<Vec<u8>> {
        let blob_name = self.blob_path(session_id, relative_path);
        let url = self.blob_url(&blob_name);
        let token = self.acquire_token().await?;

        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .header("x-ms-version", AZURE_API_VERSION)
            .send()
            .await
            .with_context(|| format!("Azure get request failed for {blob_name}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let body_preview = body.chars().take(200).collect::<String>();
            anyhow::bail!("Azure get failed for {blob_name} (HTTP {status}): {body_preview}");
        }

        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("Failed to read Azure response body for {blob_name}"))?
            .to_vec();

        Ok(bytes)
    }

    async fn list(&self, session_id: &str, prefix: &str) -> Result<Vec<String>> {
        let full_prefix = self.blob_path(session_id, prefix);
        let base_url = self.container_url();
        let token = self.acquire_token().await?;
        let mut results = Vec::new();
        let mut marker: Option<String> = None;

        loop {
            let mut req = self
                .http
                .get(&base_url)
                .bearer_auth(&token)
                .header("x-ms-version", AZURE_API_VERSION)
                .query(&[
                    ("restype", "container"),
                    ("comp", "list"),
                    ("prefix", &full_prefix),
                ]);

            if let Some(ref m) = marker {
                req = req.query(&[("marker", m.as_str())]);
            }

            let resp = req
                .send()
                .await
                .with_context(|| format!("Azure list request failed for prefix {full_prefix}"))?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                let body_preview = body.chars().take(200).collect::<String>();
                anyhow::bail!(
                    "Azure list failed for prefix {full_prefix} (HTTP {status}): {body_preview}"
                );
            }

            let xml = resp
                .text()
                .await
                .with_context(|| "Failed to read Azure list response body")?;

            for name in extract_xml_tag_values(&xml, "Name") {
                let stripped = if name.starts_with(&full_prefix) {
                    name[full_prefix.len()..].to_string()
                } else {
                    name
                };
                if !stripped.is_empty() {
                    results.push(stripped);
                }
            }

            let next_marker = extract_xml_tag_values(&xml, "NextMarker")
                .into_iter()
                .next()
                .unwrap_or_default();

            if next_marker.is_empty() {
                break;
            }

            marker = Some(next_marker);
        }

        Ok(results)
    }

    async fn delete(&self, session_id: &str, relative_path: &str) -> Result<()> {
        let blob_name = self.blob_path(session_id, relative_path);
        let url = self.blob_url(&blob_name);
        let token = self.acquire_token().await?;

        let resp = self
            .http
            .delete(&url)
            .bearer_auth(&token)
            .header("x-ms-version", AZURE_API_VERSION)
            .send()
            .await
            .with_context(|| format!("Azure delete request failed for {blob_name}"))?;

        if !resp.status().is_success() && resp.status().as_u16() != 404 {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let body_preview = body.chars().take(200).collect::<String>();
            anyhow::bail!("Azure delete failed for {blob_name} (HTTP {status}): {body_preview}");
        }

        tracing::debug!(blob = %blob_name, "Azure delete complete");
        Ok(())
    }
}

fn extract_xml_tag_values(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut results = Vec::new();
    let mut rest = xml;

    while let Some(start) = rest.find(&open) {
        rest = &rest[start + open.len()..];
        if let Some(end) = rest.find(&close) {
            results.push(rest[..end].to_string());
            rest = &rest[end + close.len()..];
        } else {
            break;
        }
    }

    results
}

pub struct GcpBackend {
    storage: google_cloud_storage::client::Storage,
    control: google_cloud_storage::client::StorageControl,
    bucket: String,
}

impl GcpBackend {
    pub async fn new(bucket: String) -> Result<Self> {
        let storage = google_cloud_storage::client::Storage::builder()
            .build()
            .await
            .context("Failed to build GCP Storage client — check Application Default Credentials")?;

        let control = google_cloud_storage::client::StorageControl::builder()
            .build()
            .await
            .context("Failed to build GCP StorageControl client")?;

        control
            .get_bucket()
            .set_name(&format!("projects/_/buckets/{bucket}"))
            .send()
            .await
            .with_context(|| {
                format!(
                    "GCS bucket '{bucket}' is not accessible — \
                     check bucket name and IAM permissions \
                     (roles/storage.objectAdmin or roles/storage.objectUser)"
                )
            })?;

        tracing::info!("GCS bucket verified accessible");
        tracing::debug!(bucket = %bucket, "GCS bucket name");

        Ok(Self { storage, control, bucket })
    }

    fn object_name(&self, session_id: &str, relative_path: &str) -> String {
        format!("sessions/{session_id}/{relative_path}")
    }

    fn bucket_resource(&self) -> String {
        format!("projects/_/buckets/{}", self.bucket)
    }
}

#[async_trait]
impl StorageBackend for GcpBackend {
    async fn put(
        &self,
        session_id: &str,
        relative_path: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<()> {
        let name = self.object_name(session_id, relative_path);

        self.storage
            .write_object(&self.bucket, &name, bytes::Bytes::from(bytes))
            .set_content_type(content_type)
            .send_unbuffered()
            .await
            .with_context(|| format!("GCS put failed for {name}"))?;

        tracing::debug!(object = %name, "GCS put complete");
        Ok(())
    }

    async fn get(&self, session_id: &str, relative_path: &str) -> Result<Vec<u8>> {
        let name = self.object_name(session_id, relative_path);

        let mut reader = self.storage
            .read_object(&self.bucket, &name)
            .send()
            .await
            .with_context(|| format!("GCS get failed for {name}"))?;

        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = reader.next().await.transpose()
            .with_context(|| format!("GCS read error for {name}"))?
        {
            bytes.extend_from_slice(&chunk);
        }

        Ok(bytes)
    }

    async fn list(&self, session_id: &str, prefix: &str) -> Result<Vec<String>> {
        let full_prefix = self.object_name(session_id, prefix);
        let mut results = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut req = self.control
                .list_objects()
                .set_parent(&self.bucket_resource())
                .set_prefix(&full_prefix);

            if let Some(ref token) = page_token {
                req = req.set_page_token(token);
            }

            let resp = req
                .send()
                .await
                .with_context(|| format!("GCS list failed for prefix {full_prefix}"))?;

            for obj in resp.objects {
                let name = obj.name;
                let stripped = if name.starts_with(&full_prefix) {
                    name[full_prefix.len()..].to_string()
                } else {
                    name
                };
                if !stripped.is_empty() {
                    results.push(stripped);
                }
            }

            if !resp.next_page_token.is_empty() {
                page_token = Some(resp.next_page_token);
            } else {
                break;
            }
        }

        Ok(results)
    }

    async fn delete(&self, session_id: &str, relative_path: &str) -> Result<()> {
        let name = self.object_name(session_id, relative_path);

        self.control
            .delete_object()
            .set_bucket(&self.bucket_resource())
            .set_object(&name)
            .send()
            .await
            .with_context(|| format!("GCS delete failed for {name}"))?;

        tracing::debug!(object = %name, "GCS delete complete");
        Ok(())
    }
}

/// Construct a StorageRouter from config.
///
/// Flat config (cloud.provider + cloud.aws/azure/gcp) is treated as a
/// single-entry pool with a default rule — no migration needed.
///
/// Multi-backend config uses cloud.backends + cloud.routing_rules.
/// Individual backend startup failures abort ma-core with a clear error.
pub async fn build_router(config: &Config) -> Arc<StorageRouter> {
    use router::{RoutingRule, RuleMatcher};

    let mut pool: HashMap<String, Arc<dyn StorageBackend>> = HashMap::new();
    let mut rules: Vec<RoutingRule> = Vec::new();

    if !config.cloud.backends.is_empty() {
        if config.storage_mode != "cloud_primary" {
            // Local mode — register each named backend as LocalBackend for
            // routing name resolution. No cloud probing.
            for bc in &config.cloud.backends {
                tracing::info!(
                    backend = %bc.name,
                    provider = %bc.provider,
                    "Named backend registered as LocalBackend (storage_mode=local)"
                );
                pool.insert(
                    bc.name.clone(),
                    Arc::new(LocalBackend::new(&config.storage_path)) as Arc<dyn StorageBackend>,
                );
            }
        } else {
            // cloud_primary — initialize and probe each named cloud backend.
            let mut errors: Vec<String> = Vec::new();
            for bc in &config.cloud.backends {
                let backend: Arc<dyn StorageBackend> = match bc.provider.as_str() {
                    "aws" => {
                        match S3Backend::new(bc.bucket.clone(), bc.region.clone()).await {
                            Ok(b) => { tracing::info!(backend = %bc.name, "S3Backend initialized"); Arc::new(b) }
                            Err(e) => { errors.push(format!("backend '{}' (aws): {e}", bc.name)); continue; }
                        }
                    }
                    "azure" => {
                        match AzureBackend::new(bc.account.clone(), bc.container.clone()).await {
                            Ok(b) => { tracing::info!(backend = %bc.name, "AzureBackend initialized"); Arc::new(b) }
                            Err(e) => { errors.push(format!("backend '{}' (azure): {e}", bc.name)); continue; }
                        }
                    }
                    "gcp" => {
                        match GcpBackend::new(bc.bucket.clone()).await {
                            Ok(b) => { tracing::info!(backend = %bc.name, "GcpBackend initialized"); Arc::new(b) }
                            Err(e) => { errors.push(format!("backend '{}' (gcp): {e}", bc.name)); continue; }
                        }
                    }
                    other => { errors.push(format!("backend '{}': unknown provider '{other}'", bc.name)); continue; }
                };
                pool.insert(bc.name.clone(), backend);
            }
            if !errors.is_empty() {
                for e in &errors {
                    tracing::error!("Storage backend initialization failed: {e}");
                    let backend_name = e.split('\'').nth(1).unwrap_or("unknown");
                    crate::observability::metrics()
                        .storage_backend_errors
                        .get_or_create(&crate::observability::BackendLabels {
                            backend: backend_name.to_string(),
                        })
                        .inc();
                }
                let error_list = errors.join("; ");
                tracing::error!(count = errors.len(), "One or more storage backends failed to initialize — aborting startup");
                if !config.observability.alert_webhook_url.is_empty() {
                    crate::observability::send_alert(
                        &config.observability.alert_webhook_url,
                        &format!("Storage backend initialization failed — ma-core cannot start: {error_list}"),
                    ).await;
                }
                std::process::exit(1);
            }
        } // end cloud_primary else

        // Routing rules apply for both local and cloud_primary.
        let mut has_default = false;
        for rule_cfg in &config.cloud.routing_rules {
            if rule_cfg.match_default {
                rules.push(RoutingRule { matcher: RuleMatcher::Default, backend_name: rule_cfg.backend.clone() });
                has_default = true;
            } else if !rule_cfg.match_tenant_prefix.is_empty() {
                rules.push(RoutingRule { matcher: RuleMatcher::TenantPrefix(rule_cfg.match_tenant_prefix.clone()), backend_name: rule_cfg.backend.clone() });
            }
        }
        if !has_default {
            if let Some(first) = config.cloud.backends.first() {
                tracing::warn!(backend = %first.name, "No default routing rule — using first backend as implicit default");
                rules.push(RoutingRule { matcher: RuleMatcher::Default, backend_name: first.name.clone() });
            }
        }
    } else {
        let (name, backend) = build_single_backend_from_flat(config).await;
        pool.insert(name.clone(), backend);
        rules.push(RoutingRule { matcher: RuleMatcher::Default, backend_name: name });
    }
    Arc::new(StorageRouter::new(pool, rules))
}

async fn build_single_backend_from_flat(config: &Config) -> (String, Arc<dyn StorageBackend>) {
    match config.storage_mode.as_str() {
        "cloud_primary" => match config.cloud.provider.as_str() {
            "aws" => {
                let bucket = config.cloud.aws.bucket.clone();
                let region = config.cloud.aws.region.clone();
                match S3Backend::new(bucket, region).await {
                    Ok(b) => {
                        tracing::info!("S3Backend initialized");
                        ("aws".to_string(), Arc::new(b) as Arc<dyn StorageBackend>)
                    }
                    Err(e) => {
                        tracing::error!("Failed to initialize S3Backend: {e} — falling back to LocalBackend");
                        ("local".to_string(), Arc::new(LocalBackend::new(&config.storage_path)) as Arc<dyn StorageBackend>)
                    }
                }
            }
            "azure" => {
                let account = config.cloud.azure.account.clone();
                let container = config.cloud.azure.container.clone();
                match AzureBackend::new(account, container).await {
                    Ok(b) => {
                        tracing::info!("AzureBackend initialized");
                        ("azure".to_string(), Arc::new(b) as Arc<dyn StorageBackend>)
                    }
                    Err(e) => {
                        tracing::error!("Failed to initialize AzureBackend: {e} — falling back to LocalBackend");
                        ("local".to_string(), Arc::new(LocalBackend::new(&config.storage_path)) as Arc<dyn StorageBackend>)
                    }
                }
            }
            "gcp" => {
                let bucket = config.cloud.gcp.bucket.clone();
                match GcpBackend::new(bucket).await {
                    Ok(b) => {
                        tracing::info!("GcpBackend initialized");
                        ("gcp".to_string(), Arc::new(b) as Arc<dyn StorageBackend>)
                    }
                    Err(e) => {
                        tracing::error!("Failed to initialize GcpBackend: {e} — falling back to LocalBackend");
                        ("local".to_string(), Arc::new(LocalBackend::new(&config.storage_path)) as Arc<dyn StorageBackend>)
                    }
                }
            }
            other => {
                tracing::warn!(
                    provider = %other,
                    "Unknown cloud provider in cloud_primary mode — falling back to LocalBackend. \
                     Configure a valid provider: aws | azure | gcp"
                );
                ("local".to_string(), Arc::new(LocalBackend::new(&config.storage_path)) as Arc<dyn StorageBackend>)
            }
        },
        _ => (
            "local".to_string(),
            Arc::new(LocalBackend::new(&config.storage_path)) as Arc<dyn StorageBackend>,
        ),
    }
}