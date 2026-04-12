// /Memory-Archive/ma-core/src/annotator_mgmt.rs
//
// T10.22 — Annotator management REST API.
//
// Exposes four endpoints on a separate port (annotator_mgmt_port, default 9002)
// authenticated by a bearer token read from MA_ANNOTATOR_MGMT_TOKEN:
//
//   POST   /v1/annotators              — RegisterAnnotator
//   DELETE /v1/annotators/{id}         — DeactivateAnnotator
//   POST   /v1/annotators/{id}/rotate  — RotateAnnotatorKey
//   GET    /v1/annotators              — ListAnnotators
//
// The server shares the same Redis registry as the IPC path — no duplicate logic.
// HTTP/1.1 only, plaintext TCP (no TLS). This endpoint must be bound to a private
// network interface or protected by a network policy. It is not designed for
// public exposure.

use anyhow::Context;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::registry::SessionRegistry;

/// Launch the annotator management REST API server.
///
/// Reads the bearer token from MA_ANNOTATOR_MGMT_TOKEN at call time.
/// If the env var is empty the server refuses to start.
pub async fn serve(port: u16, registry: SessionRegistry) -> anyhow::Result<()> {
    let mgmt_token = std::env::var("MA_ANNOTATOR_MGMT_TOKEN").unwrap_or_default();
    if mgmt_token.is_empty() {
        anyhow::bail!(
            "CRITICAL: MA_ANNOTATOR_MGMT_TOKEN must be set when annotator_mgmt_port is configured. \
             Set the environment variable before starting ma-core: \
             export MA_ANNOTATOR_MGMT_TOKEN=<token>"
        );
    }

    let addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&addr).await
        .with_context(|| format!("Failed to bind annotator management API on {addr}"))?;

    tracing::info!(port, "Annotator management REST API listening");

    loop {
        let (mut stream, peer) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Annotator mgmt: accept error: {e}");
                continue;
            }
        };

        let token = mgmt_token.clone();
        let mut reg = registry.clone();

        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            let n = match stream.read(&mut buf).await {
                Ok(n) => n,
                Err(e) => {
                    tracing::debug!("Annotator mgmt: read error from {peer}: {e}");
                    return;
                }
            };

            let request = String::from_utf8_lossy(&buf[..n]);

            // Parse the first line: "METHOD /path HTTP/1.1"
            let first_line = match request.lines().next() {
                Some(l) => l,
                None => {
                    let _ = write_response(&mut stream, 400, "Bad Request", b"").await;
                    return;
                }
            };

            let parts: Vec<&str> = first_line.splitn(3, ' ').collect();
            if parts.len() < 2 {
                let _ = write_response(&mut stream, 400, "Bad Request", b"").await;
                return;
            }
            let method = parts[0];
            let path   = parts[1];

            // Bearer token auth.
            let auth_header = request
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("authorization: bearer "))
                .and_then(|l| l.splitn(3, ' ').nth(2))
                .unwrap_or("")
                .trim();

            if !constant_time_eq(auth_header, &token) {
                let body = br#"{"error":"UNAUTHORIZED","message":"Invalid or missing bearer token"}"#;
                let _ = write_json_response(&mut stream, 401, body).await;
                tracing::warn!(peer = %peer, "Annotator mgmt: unauthorized request");
                return;
            }

            // Extract the JSON body (everything after the blank line).
            let body_start = request.find("\r\n\r\n")
                .map(|i| i + 4)
                .or_else(|| request.find("\n\n").map(|i| i + 2))
                .unwrap_or(n);
            let body_bytes = &buf[body_start..n];

            let response = dispatch(method, path, body_bytes, &mut reg).await;
            let _ = stream.write_all(response.as_bytes()).await;
        });
    }
}

async fn dispatch(
    method: &str,
    path: &str,
    body: &[u8],
    registry: &mut SessionRegistry,
) -> String {
    // POST /v1/annotators — RegisterAnnotator
    if method == "POST" && path == "/v1/annotators" {
        let input: serde_json::Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return json_error(400, "INVALID_JSON", &e.to_string()),
        };
        let annotator_id = input["annotator_id"].as_str().unwrap_or("").trim().to_string();
        if annotator_id.is_empty() {
            return json_error(400, "MISSING_FIELD", "annotator_id is required");
        }
        let allowed: Vec<String> = input["allowed_tenant_ids"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let max_claims: u32 = input["max_concurrent_claims"].as_u64().unwrap_or(0) as u32;

        match registry.register_annotator(&annotator_id, &allowed, max_claims).await {
            Err(e) if e.to_string() == "ANNOTATOR_EXISTS" => {
                json_error(409, "ANNOTATOR_EXISTS", &format!("Annotator '{annotator_id}' already exists"))
            }
            Err(e) => json_error(500, "REGISTER_FAILED", &e.to_string()),
            Ok(plaintext_key) => {
                tracing::info!(annotator_id = %annotator_id, "Annotator registered via REST API");
                let body = serde_json::json!({
                    "annotator_id": annotator_id,
                    "plaintext_key": plaintext_key,
                    "message": "Store this key securely — it will not be shown again."
                });
                json_ok(201, &body.to_string())
            }
        }
    }

    // DELETE /v1/annotators/{id} — DeactivateAnnotator
    else if method == "DELETE" && path.starts_with("/v1/annotators/") {
        let annotator_id = path.trim_start_matches("/v1/annotators/");
        if annotator_id.is_empty() || annotator_id.contains('/') {
            return json_error(400, "INVALID_PATH", "annotator_id must not be empty or contain '/'");
        }
        match registry.deactivate_annotator(annotator_id).await {
            Err(e) if e.to_string() == "ANNOTATOR_NOT_FOUND" => {
                json_error(404, "ANNOTATOR_NOT_FOUND", &format!("Annotator '{annotator_id}' not found"))
            }
            Err(e) => json_error(500, "DEACTIVATE_FAILED", &e.to_string()),
            Ok(()) => {
                let body = serde_json::json!({"annotator_id": annotator_id, "status": "deactivated"});
                json_ok(200, &body.to_string())
            }
        }
    }

    // POST /v1/annotators/{id}/rotate — RotateAnnotatorKey
    else if method == "POST" && path.starts_with("/v1/annotators/") && path.ends_with("/rotate") {
        let inner = path
            .trim_start_matches("/v1/annotators/")
            .trim_end_matches("/rotate");
        if inner.is_empty() || inner.contains('/') {
            return json_error(400, "INVALID_PATH", "annotator_id must not be empty");
        }
        let annotator_id = inner;
        match registry.rotate_annotator_key(annotator_id).await {
            Err(e) if e.to_string() == "ANNOTATOR_NOT_FOUND" => {
                json_error(404, "ANNOTATOR_NOT_FOUND", &format!("Annotator '{annotator_id}' not found"))
            }
            Err(e) => json_error(500, "ROTATE_FAILED", &e.to_string()),
            Ok(new_key) => {
                tracing::info!(annotator_id = %annotator_id, "Annotator key rotated via REST API");
                let body = serde_json::json!({
                    "annotator_id": annotator_id,
                    "new_plaintext_key": new_key,
                    "message": "Store this key securely — it will not be shown again."
                });
                json_ok(200, &body.to_string())
            }
        }
    }

    // GET /v1/annotators — ListAnnotators
    else if method == "GET" && path == "/v1/annotators" {
        match registry.list_annotators().await {
            Err(e) => json_error(500, "REDIS_ERROR", &e.to_string()),
            Ok(annotators) => {
                let items: Vec<serde_json::Value> = annotators.iter().map(|a| {
                    serde_json::json!({
                        "annotator_id":         a.annotator_id,
                        "status":               a.status,
                        "current_claims":       a.current_claims,
                        "last_auth_at":         a.last_auth_at,
                        "allowed_tenant_ids":   a.allowed_tenant_ids,
                        "max_concurrent_claims": a.max_concurrent_claims,
                    })
                }).collect();
                let body = serde_json::json!({"annotators": items}).to_string();
                json_ok(200, &body)
            }
        }
    }

    else {
        json_error(404, "NOT_FOUND", &format!("No route for {method} {path}"))
    }
}

fn json_ok(status: u16, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn json_error(status: u16, code: &str, message: &str) -> String {
    let body = format!(r#"{{"error":"{code}","message":{}}}"#, serde_json::json!(message));
    let reason = match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        _   => "Internal Server Error",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    reason: &str,
    body: &[u8],
) -> anyhow::Result<()> {
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    Ok(())
}

async fn write_json_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    body: &[u8],
) -> anyhow::Result<()> {
    let reason = if status == 401 { "Unauthorized" } else { "OK" };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    Ok(())
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}