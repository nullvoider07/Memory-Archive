// /Memory-Archive/ma-core/src/observability/mod.rs

use std::sync::{atomic::AtomicU64, OnceLock};

use prometheus_client::{
    encoding::{text::encode, EncodeLabelSet},
    metrics::{
        counter::Counter,
        family::Family,
        gauge::Gauge,
        histogram::Histogram,
    },
    registry::Registry,
};
use tokio::net::TcpListener;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::ObservabilityConfig;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ProviderLabels {
    pub provider: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ProviderErrorLabels {
    pub provider: String,
    pub error_type: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct BackendLabels {
    pub backend: String,
}

/// Label set for `ma_session_server_address_source`.
/// `source` is either "per_session" or "global_config".
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ServerAddressSourceLabels {
    pub source: String,
}

/// Label set for `ma_pricing_registry_fetch_status`.
/// `status` is one of "success", "signature_failure", "network_failure", "cache_hit".
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct PricingStatusLabels {
    pub status: String,
}

/// Label set for annotator metrics.
/// `annotator_id` is the annotator's unique identifier.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct AnnotatorIdLabels {
    pub annotator_id: String,
}

pub struct Metrics {
    pub active_sessions: Gauge<f64, AtomicU64>,
    pub steps_total: Counter<u64, AtomicU64>,
    pub kafka_consumer_lag: Gauge<f64, AtomicU64>,
    pub cloud_upload_errors_total: Counter<u64, AtomicU64>,
    pub cloud_upload_queue_depth: Gauge<f64, AtomicU64>,
    pub ipc_push_queue_depth: Gauge<f64, AtomicU64>,
    pub ipc_tcp_connections_active: Gauge<f64, AtomicU64>,
    pub vlm_requests_total: Counter<u64, AtomicU64>,
    pub vlm_requests_by_provider: Family<ProviderLabels, Counter<u64, AtomicU64>>,
    pub vlm_errors_by_provider: Family<ProviderErrorLabels, Counter<u64, AtomicU64>>,
    pub vlm_tokens_consumed_total: Counter<u64, AtomicU64>,
    pub vlm_request_latency_ms: Histogram,
    pub vlm_circuit_breaker_open: Gauge<f64, AtomicU64>,
    pub sessions_reasoning_degraded: Gauge<f64, AtomicU64>,
    pub storage_routing_decisions: Family<BackendLabels, Counter<u64, AtomicU64>>,
    pub storage_backend_errors: Family<BackendLabels, Counter<u64, AtomicU64>>,
    pub session_server_address_source: Family<ServerAddressSourceLabels, Counter<u64, AtomicU64>>,
    /// Pricing registry fetch results, labelled by status.
    pub pricing_registry_fetch_status: Family<PricingStatusLabels, Counter<u64, AtomicU64>>,
    /// Seconds since the loaded manifest was generated. 0 = no manifest loaded.
    pub pricing_registry_age_seconds: Gauge<f64, AtomicU64>,
    /// Live concurrent claim count per annotator.
    pub annotator_active_claims: Family<AnnotatorIdLabels, Gauge<f64, AtomicU64>>,
    /// Cumulative auth failures per annotator.
    pub annotator_auth_failures_total: Family<AnnotatorIdLabels, Counter<u64, AtomicU64>>,
}

static METRICS: OnceLock<Metrics> = OnceLock::new();
static REGISTRY: OnceLock<Registry> = OnceLock::new();

pub fn metrics() -> &'static Metrics {
    METRICS.get().expect("metrics not initialized — call init_metrics first")
}

pub fn init_metrics(config: &ObservabilityConfig) -> anyhow::Result<()> {
    let active_sessions: Gauge<f64, AtomicU64> = Gauge::default();
    let steps_total: Counter<u64, AtomicU64> = Counter::default();
    let kafka_consumer_lag: Gauge<f64, AtomicU64> = Gauge::default();
    let cloud_upload_errors_total: Counter<u64, AtomicU64> = Counter::default();
    let cloud_upload_queue_depth: Gauge<f64, AtomicU64> = Gauge::default();
    let ipc_push_queue_depth: Gauge<f64, AtomicU64> = Gauge::default();
    let ipc_tcp_connections_active: Gauge<f64, AtomicU64> = Gauge::default();
    let vlm_requests_total: Counter<u64, AtomicU64> = Counter::default();
    let vlm_requests_by_provider: Family<ProviderLabels, Counter<u64, AtomicU64>> = Family::default();
    let vlm_errors_by_provider: Family<ProviderErrorLabels, Counter<u64, AtomicU64>> = Family::default();
    let vlm_tokens_consumed_total: Counter<u64, AtomicU64> = Counter::default();
    let vlm_request_latency_ms = Histogram::new(
        [50.0, 100.0, 250.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0, 30000.0].into_iter(),
    );
    let vlm_circuit_breaker_open: Gauge<f64, AtomicU64> = Gauge::default();
    let sessions_reasoning_degraded: Gauge<f64, AtomicU64> = Gauge::default();
    let storage_routing_decisions: Family<BackendLabels, Counter<u64, AtomicU64>> = Family::default();
    let storage_backend_errors: Family<BackendLabels, Counter<u64, AtomicU64>> = Family::default();
    let session_server_address_source: Family<ServerAddressSourceLabels, Counter<u64, AtomicU64>> = Family::default();
    let pricing_registry_fetch_status: Family<PricingStatusLabels, Counter<u64, AtomicU64>> = Family::default();
    let pricing_registry_age_seconds: Gauge<f64, AtomicU64> = Gauge::default();
    let annotator_active_claims: Family<AnnotatorIdLabels, Gauge<f64, AtomicU64>> = Family::default();
    let annotator_auth_failures_total: Family<AnnotatorIdLabels, Counter<u64, AtomicU64>> = Family::default();

    let mut registry = Registry::default();

    registry.register(
        "ma_active_sessions",
        "Number of currently active capture sessions",
        active_sessions.clone(),
    );
    registry.register(
        "ma_steps_total",
        "Total number of actuation steps captured across all sessions",
        steps_total.clone(),
    );
    registry.register(
        "ma_kafka_consumer_lag",
        "Total consumer lag across all assigned Kafka partitions",
        kafka_consumer_lag.clone(),
    );
    registry.register(
        "ma_cloud_upload_errors_total",
        "Total number of cloud storage upload errors",
        cloud_upload_errors_total.clone(),
    );
    registry.register(
        "ma_cloud_upload_queue_depth",
        "Current number of pending cloud storage uploads",
        cloud_upload_queue_depth.clone(),
    );
    registry.register(
        "ma_ipc_push_queue_depth",
        "Current number of pending outbound IPC push messages from active watch loops",
        ipc_push_queue_depth.clone(),
    );
    registry.register(
        "ma_ipc_tcp_connections_active",
        "Number of currently active remote TCP IPC connections",
        ipc_tcp_connections_active.clone(),
    );
    registry.register(
        "ma_vlm_requests_total",
        "Total VLM API requests that returned reasoning results (all providers combined)",
        vlm_requests_total.clone(),
    );
    registry.register(
        "ma_vlm_requests_by_provider",
        "VLM API requests that returned reasoning results, labelled by provider name",
        vlm_requests_by_provider.clone(),
    );
    registry.register(
        "ma_vlm_errors_by_provider",
        "VLM API errors that triggered circuit breaker transitions, labelled by provider and error_type",
        vlm_errors_by_provider.clone(),
    );
    registry.register(
        "ma_vlm_tokens_consumed_total",
        "Total VLM tokens consumed (input + output) across all sessions",
        vlm_tokens_consumed_total.clone(),
    );
    registry.register(
        "ma_vlm_request_latency_ms",
        "VLM API request latency in milliseconds",
        vlm_request_latency_ms.clone(),
    );
    registry.register(
        "ma_vlm_circuit_breaker_open",
        "Number of sessions currently with the VLM circuit breaker open",
        vlm_circuit_breaker_open.clone(),
    );
    registry.register(
        "ma_sessions_reasoning_degraded",
        "Number of sessions currently in reasoning_degraded state",
        sessions_reasoning_degraded.clone(),
    );
    registry.register(
        "ma_storage_routing_decisions_total",
        "Number of storage backend routing decisions made at session registration, labelled by backend name",
        storage_routing_decisions.clone(),
    );
    registry.register(
        "ma_storage_backend_errors_total",
        "Number of storage backend errors, labelled by backend name",
        storage_backend_errors.clone(),
    );
    registry.register(
        "ma_session_server_address_source",
        "Sessions started, labelled by whether CC/Eyes addresses came from per-session registration or global config",
        session_server_address_source.clone(),
    );
    registry.register(
        "ma_pricing_registry_fetch_status",
        "Pricing manifest fetch attempts by outcome: success, signature_failure, network_failure, cache_hit",
        pricing_registry_fetch_status.clone(),
    );
    registry.register(
        "ma_pricing_registry_age_seconds",
        "Seconds since the loaded pricing manifest was generated; 0 when no manifest is loaded",
        pricing_registry_age_seconds.clone(),
    );
    registry.register(
        "ma_annotator_active_claims",
        "Current number of active session claims held per annotator",
        annotator_active_claims.clone(),
    );
    registry.register(
        "ma_annotator_auth_failures_total",
        "Cumulative annotator authentication failures, labelled by annotator_id",
        annotator_auth_failures_total.clone(),
    );

    METRICS
        .set(Metrics {
            active_sessions,
            steps_total,
            kafka_consumer_lag,
            cloud_upload_errors_total,
            cloud_upload_queue_depth,
            ipc_push_queue_depth,
            ipc_tcp_connections_active,
            vlm_requests_total,
            vlm_requests_by_provider,
            vlm_errors_by_provider,
            vlm_tokens_consumed_total,
            vlm_request_latency_ms,
            vlm_circuit_breaker_open,
            sessions_reasoning_degraded,
            storage_routing_decisions,
            storage_backend_errors,
            session_server_address_source,
            pricing_registry_fetch_status,
            pricing_registry_age_seconds,
            annotator_active_claims,
            annotator_auth_failures_total,
        })
        .map_err(|_| anyhow::anyhow!("init_metrics called more than once"))?;

    REGISTRY
        .set(registry)
        .map_err(|_| anyhow::anyhow!("Registry already initialized"))?;

    let port = config.metrics_port;
    let bind_addr = config.metrics_bind_addr.clone();
    let token = config.metrics_token.clone();
    tokio::spawn(async move {
        serve_metrics(bind_addr, port, token).await;
    });

    tracing::info!(
        port = config.metrics_port,
        "Prometheus metrics endpoint listening"
    );

    let upload_queue_warn = config.upload_queue_warn;
    let alert_webhook = config.alert_webhook_url.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let depth = metrics().cloud_upload_queue_depth.get() as u64;
            if depth > upload_queue_warn {
                tracing::warn!(
                    depth,
                    upload_queue_warn,
                    "Cloud upload queue depth above warning threshold"
                );
                send_alert(
                    &alert_webhook,
                    &format!("Cloud upload queue depth {depth} exceeds threshold {upload_queue_warn}"),
                ).await;
            }
        }
    });

    let ipc_push_queue_warn = config.ipc_push_queue_warn;
    let alert_webhook_ipc = config.alert_webhook_url.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let depth = metrics().ipc_push_queue_depth.get() as u64;
            if depth > ipc_push_queue_warn {
                tracing::warn!(
                    depth,
                    ipc_push_queue_warn,
                    "IPC push queue depth above warning threshold — orchestration layer may be falling behind"
                );
                send_alert(
                    &alert_webhook_ipc,
                    &format!("IPC push queue depth {depth} exceeds threshold {ipc_push_queue_warn}"),
                ).await;
            }
        }
    });

    // Pricing registry age alert.
    // If ma_pricing_registry_age_seconds exceeds 7 days (604800s) and is non-zero
    // (a manifest was loaded), fire an alert so operators know the registry is stale.
    let alert_webhook_pricing = config.alert_webhook_url.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let age = metrics().pricing_registry_age_seconds.get();
            if age > 604_800.0 {
                tracing::warn!(
                    age_seconds = age as u64,
                    "Pricing registry manifest is more than 7 days old — fetch may be failing"
                );
                send_alert(
                    &alert_webhook_pricing,
                    &format!(
                        "Pricing registry manifest is {:.0} days old (threshold: 7 days). \
                         Check pricing.memory-archive.dev reachability.",
                        age / 86_400.0
                    ),
                ).await;
            }
        }
    });

    Ok(())
}

/// Decide the effective metrics bind address. Fail closed: the metrics endpoint
/// exposes session IDs, annotator IDs and token/cost counters, so a non-loopback
/// bind without a token is downgraded to loopback. Returns
/// `(effective_addr, fell_back)` — `fell_back` is true when the request was
/// downgraded. Pure and side-effect free so it is unit-testable.
fn resolve_metrics_bind(bind_addr: &str, token_empty: bool) -> (String, bool) {
    let is_loopback = matches!(bind_addr, "127.0.0.1" | "::1" | "localhost");
    if !is_loopback && token_empty {
        ("127.0.0.1".to_string(), true)
    } else {
        (bind_addr.to_string(), false)
    }
}

async fn serve_metrics(bind_addr: String, port: u16, token: String) {
    let (effective_addr, fell_back) = resolve_metrics_bind(&bind_addr, token.is_empty());
    if fell_back {
        tracing::error!(
            requested = %bind_addr,
            "CRITICAL: metrics_bind_addr is non-loopback but metrics_token is unset — \
             refusing to expose unauthenticated metrics. Falling back to 127.0.0.1. \
             Set observability.metrics_token to bind a public address."
        );
    }

    let addr = format!("{effective_addr}:{port}");
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(addr = %addr, "Failed to bind metrics listener: {e}");
            return;
        }
    };

    loop {
        let (mut stream, _peer) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Metrics listener accept error: {e}");
                continue;
            }
        };

        let token = token.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap_or(0);

            if !token.is_empty() {
                let request = String::from_utf8_lossy(&buf[..n]);
                let provided = request
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("authorization: bearer "))
                    .and_then(|l| l.splitn(3, ' ').nth(2))
                    .unwrap_or("")
                    .trim();

                if !constant_time_eq(provided, &token) {
                    let response = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nWWW-Authenticate: Bearer\r\n\r\n";
                    let _ = stream.write_all(response.as_bytes()).await;
                    return;
                }
            }

            let registry = match REGISTRY.get() {
                Some(r) => r,
                None => return,
            };

            let mut body = String::new();
            if let Err(e) = encode(&mut body, registry) {
                tracing::warn!("Failed to encode metrics: {e}");
                return;
            }

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );

            let _ = stream.write_all(response.as_bytes()).await;
        });
    }
}

pub fn init_tracing(config: &ObservabilityConfig) {
    let filter = EnvFilter::new(&config.log_level)
        .add_directive("aws_config=warn".parse().unwrap())
        .add_directive("aws_smithy_runtime=warn".parse().unwrap());

    match config.log_output.as_str() {
        "file" => {
            if config.log_file_path.is_empty() {
                tracing::warn!("log_output is 'file' but log_file_path is not set — falling back to stdout");
                init_stdout_tracing(filter);
            } else {
                init_file_tracing(filter, &config.log_file_path);
            }
        }
        "forward" => {
            if config.log_forward_url.is_empty() {
                tracing::warn!("log_output is 'forward' but log_forward_url is not set — falling back to stdout");
            }
            init_stdout_tracing(filter);
        }
        _ => {
            init_stdout_tracing(filter);
        }
    }
}

fn init_stdout_tracing(filter: EnvFilter) {
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_thread_ids(false)
                .with_file(false)
                .with_line_number(false),
        )
        .init();
}

fn init_file_tracing(filter: EnvFilter, log_file_path: &str) {
    use std::path::Path;

    let path = Path::new(log_file_path);
    let dir = path.parent().unwrap_or(Path::new("."));
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("ma-core.log");

    let file_appender = tracing_appender::rolling::never(dir, filename);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    Box::leak(Box::new(guard));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(non_blocking),
        )
        .init();

    tracing::info!("File logging initialized");
    tracing::debug!(log_file = log_file_path, "Log file path");
}

pub async fn send_alert(webhook_url: &str, message: &str) {
    if webhook_url.is_empty() {
        return;
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[ALERT] Failed to build HTTP client for webhook: {e}");
            eprintln!("[ALERT] {message}");
            return;
        }
    };

    let payload = serde_json::json!({
        "source": "ma-core",
        "message": message,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });

    match client.post(webhook_url).json(&payload).send().await {
        Ok(resp) if resp.status().is_success() => {
            tracing::debug!(webhook_url, "Alert delivered via webhook");
        }
        Ok(resp) => {
            eprintln!(
                "[ALERT] Webhook delivery failed — HTTP {}: {message}",
                resp.status()
            );
        }
        Err(e) => {
            eprintln!("[ALERT] Webhook delivery error — {e}: {message}");
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

#[cfg(test)]
mod tests {
    use super::resolve_metrics_bind;

    #[test]
    fn loopback_without_token_is_allowed() {
        assert_eq!(resolve_metrics_bind("127.0.0.1", true), ("127.0.0.1".to_string(), false));
        assert_eq!(resolve_metrics_bind("::1", true), ("::1".to_string(), false));
        assert_eq!(resolve_metrics_bind("localhost", true), ("localhost".to_string(), false));
    }

    #[test]
    fn public_bind_without_token_falls_back_to_loopback() {
        let (addr, fell_back) = resolve_metrics_bind("0.0.0.0", true);
        assert_eq!(addr, "127.0.0.1");
        assert!(fell_back, "must downgrade an unauthenticated public bind");

        let (addr, fell_back) = resolve_metrics_bind("10.0.0.5", true);
        assert_eq!(addr, "127.0.0.1");
        assert!(fell_back);
    }

    #[test]
    fn public_bind_with_token_is_allowed() {
        let (addr, fell_back) = resolve_metrics_bind("0.0.0.0", false);
        assert_eq!(addr, "0.0.0.0");
        assert!(!fell_back, "a token authorises a public bind");
    }
}