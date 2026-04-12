// /Memory/Memory-Archive/ma-kafka-producer/src/main.rs

use std::time::Duration;

use anyhow::{Context, Result};
use ma_proto::control_center::{
    control_service_client::ControlServiceClient,
    WatchRequest,
};
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};
use serde::Serialize;

const TOPIC: &str = "control-center-events";

#[derive(Debug, Serialize)]
struct KafkaEnvelope<'a> {
    schema_version: &'static str,
    session_id: &'a str,
    event_type: &'static str,
    timestamp: &'a str,
    payload: serde_json::Value,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ma_kafka_producer=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();

    let cc_addr = get_arg(&args, "--cc-addr")
        .context("--cc-addr <gRPC address> is required e.g. http://127.0.0.1:50051")?;
    let kafka_broker = get_arg(&args, "--kafka-broker")
        .context("--kafka-broker <host:port> is required e.g. localhost:9092")?;
    let session_id = get_arg(&args, "--session-id")
        .context("--session-id <uuid> is required (use the id from memory-archive session register)")?;

    tracing::info!(session_id = %session_id, "ma-kafka-producer starting");
    tracing::debug!(cc_addr = %cc_addr, kafka_broker = %kafka_broker, "ma-kafka-producer addresses");

    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &kafka_broker)
        .set("message.timeout.ms", "5000")
        .set("queue.buffering.max.messages", "100000")
        .create()
        .context("Failed to create Kafka producer")?;

    let mut client = ControlServiceClient::connect(cc_addr.clone())
        .await
        .with_context(|| format!("Failed to connect to Control-Center at {cc_addr}"))?;

    let mut stream = client
        .watch_commands(WatchRequest {})
        .await
        .context("Failed to initiate WatchCommands stream")?
        .into_inner();

    tracing::info!("Connected to Control-Center — streaming to Kafka topic '{TOPIC}'");

    let mut published: u64 = 0;

    loop {
        match stream.message().await {
            Ok(None) => {
                tracing::info!(published, "Control-Center stream ended cleanly");
                break;
            }
            Err(e) => {
                tracing::error!(published, "Stream error: {e}");
                break;
            }
            Ok(Some(event)) => {
                if event.is_heartbeat {
                    continue;
                }

                let timestamp = event.timestamp.clone();

                let payload = match serde_json::to_value(&event) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(session_id = %session_id, "Failed to serialize CommandEvent: {e}");
                        continue;
                    }
                };

                let envelope = KafkaEnvelope {
                    schema_version: "1.0",
                    session_id: &session_id,
                    event_type: "command_event",
                    timestamp: &timestamp,
                    payload,
                };

                let json = match serde_json::to_string(&envelope) {
                    Ok(j) => j,
                    Err(e) => {
                        tracing::error!(session_id = %session_id, "Failed to serialize envelope: {e}");
                        continue;
                    }
                };

                let record = FutureRecord::to(TOPIC)
                    .key(session_id.as_str())
                    .payload(json.as_str());

                match producer.send(record, Duration::from_secs(5)).await {
                    Ok((partition, offset)) => {
                        published += 1;
                        tracing::debug!(
                            session_id = %session_id,
                            partition,
                            offset,
                            action_type = %event.action_type,
                            action_subtype = %event.action_subtype,
                            "Event published"
                        );
                    }
                    Err((e, _)) => {
                        tracing::error!(
                            session_id = %session_id,
                            "Kafka publish failed: {e}"
                        );
                    }
                }
            }
        }
    }

    tracing::info!(published, "ma-kafka-producer done");
    Ok(())
}

fn get_arg(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].clone())
}