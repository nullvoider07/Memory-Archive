// /Memory-Archive/ma-core/src/vision/client.rs

use anyhow::{Context, Result};
use reqwest::Client;
use std::time::Duration;

pub struct EyesClient {
    base_url: String,
    client: Client,
}

impl EyesClient {
    pub fn new(base_url: String) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .context("Failed to build HTTP client for EyesClient")?;
        Ok(Self { base_url, client })
    }

    /// Returns true if The-Eyes /health endpoint responds with 2xx.
    pub async fn is_alive(&self) -> bool {
        self.client
            .get(format!("{}/health", self.base_url))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    /// Query GET /status and return `capture_interval_ms`.
    /// Used by VisionPipeline::new() to calibrate before-frame timing (T2.11).
    /// Returns Err if the endpoint is unreachable or the field is absent.
    #[allow(dead_code)]
    pub async fn get_capture_interval(&self) -> Result<u64> {
        let resp: serde_json::Value = self.client
            .get(format!("{}/status", self.base_url))
            .send()
            .await
            .context("Failed to reach The-Eyes /status")?
            .json()
            .await
            .context("Failed to parse /status JSON")?;

        resp["capture_interval_ms"]
            .as_u64()
            .context("'capture_interval_ms' missing or not a number in /status response")
    }

    /// Fetch the frame whose timestamp is nearest to the given ISO 8601 timestamp.
    /// Returns the raw image bytes and a file extension derived from Content-Type (e.g. "png", "webp").
    pub async fn fetch_at(&self, timestamp: &str) -> Result<(Vec<u8>, String)> {
        let target = parse_timestamp_unix(timestamp)?;
        self.fetch_closest(target).await
    }

    /// Fetch the frame closest to a given Unix timestamp via GET /frames/closest.
    /// Returns (bytes, extension) — extension is derived from the Content-Type header.
    async fn fetch_closest(&self, target_unix_ms: i64) -> Result<(Vec<u8>, String)> {
        let resp = self.client
            .get(format!("{}/frames/closest", self.base_url))
            .query(&[("timestamp", target_unix_ms.to_string())])
            .send()
            .await
            .context("Failed to fetch closest frame from The-Eyes")?;

        let ext = content_type_to_ext(
            resp.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
        );

        let bytes = resp
            .bytes()
            .await
            .context("Failed to read frame body bytes")?;

        Ok((bytes.to_vec(), ext))
    }
}

/// Parse an ISO 8601 / RFC 3339 timestamp string into a Unix timestamp in milliseconds.
/// Preserving sub-second precision ensures before/at/after frames are distinct when
/// Control-Center emits timestamps with millisecond components (Windows, Linux).
fn parse_timestamp_unix(ts: &str) -> Result<i64> {
    // Full RFC3339 with timezone (e.g. "2026-02-25T12:58:04.286Z")
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        return Ok(dt.timestamp_millis());
    }
    // Fallback: naive datetime, treat as UTC (e.g. "2026-02-25T12:58:04")
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(
        ts.get(..19).unwrap_or(ts),
        "%Y-%m-%dT%H:%M:%S",
    ) {
        return Ok(dt.and_utc().timestamp_millis());
    }
    anyhow::bail!("Cannot parse timestamp: {ts:?}")
}

/// Map a Content-Type value to a file extension. Defaults to "png" if unrecognised.
fn content_type_to_ext(content_type: &str) -> String {
    match content_type.split(';').next().unwrap_or("").trim() {
        "image/webp"  => "webp",
        "image/jpeg"  => "jpg",
        "image/bmp"   => "bmp",
        "image/tiff"  => "tiff",
        _             => "png",
    }
    .to_string()
}