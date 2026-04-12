// /Memory/Memory-Archive/ma-core/src/kafka/consumer.rs

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ma_proto::control_center::CommandEvent;
use anyhow::Context;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::message::Message;
use rdkafka::topic_partition_list::TopicPartitionList;
use tokio::sync::{mpsc, Mutex};

use crate::kafka::KafkaEnvelope;

#[derive(Debug)]
pub struct KafkaEvent {
    pub event: CommandEvent,
    pub partition: i32,
    pub offset: i64,
}

pub type KafkaSessionMap = Arc<Mutex<HashMap<String, mpsc::Sender<KafkaEvent>>>>;

const MAX_CONSECUTIVE_ERRORS: u32 = 10;
const LAG_CHECK_INTERVAL_SECS: u64 = 30;

pub async fn run_kafka_consumer(
    broker: String,
    session_map: KafkaSessionMap,
    channel_capacity: usize,
    lag_warn_threshold: i64,
    alert_webhook_url: String,
) {
    let consumer: StreamConsumer = match ClientConfig::new()
        .set("bootstrap.servers", &broker)
        .set("group.id", "memory-archive-workers")
        .set("enable.auto.commit", "true")
        .set("enable.auto.offset.store", "false")
        .set("auto.offset.reset", "earliest")
        .set("session.timeout.ms", "10000")
        .set("heartbeat.interval.ms", "3000")
        .create()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to create Kafka consumer: {e}");
            return;
        }
    };

    if let Err(e) = consumer.subscribe(&["control-center-events"]) {
        tracing::error!("Failed to subscribe to Kafka topic: {e}");
        return;
    }

    tracing::info!("Kafka consumer started — group: memory-archive-workers");
    tracing::debug!(broker = %broker, "Kafka broker address");

    {
        let assignment = consumer.assignment();
        match assignment {
            Ok(tpl) if tpl.count() > 0 => {
                let partitions: Vec<i32> = tpl
                    .elements()
                    .iter()
                    .map(|e| e.partition())
                    .collect();
                tracing::info!(
                    partitions = ?partitions,
                    count = partitions.len(),
                    "Kafka partition assignment received"
                );
            }
            _ => {
                tracing::info!("Kafka partition assignment pending — waiting for first rebalance");
            }
        }
    }

    // Spawn background lag monitor
    {
        let broker_clone = broker.clone();
        let lag_threshold = lag_warn_threshold;
        let webhook = alert_webhook_url.clone();
        tokio::spawn(async move {
            run_lag_monitor(broker_clone, lag_threshold, webhook).await;
        });
    }

    let mut consecutive_errors: u32 = 0;
    let mut was_degraded: bool = false;

    loop {
        match consumer.recv().await {
            Err(e) => {
                consecutive_errors += 1;
                tracing::error!(
                    consecutive_errors,
                    "Kafka consumer error: {e}"
                );

                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    tracing::error!(
                        consecutive_errors,
                        "Kafka broker unreachable after {MAX_CONSECUTIVE_ERRORS} consecutive errors — \
                         active sessions will stall until broker reconnects. \
                         Events will replay from last committed offset on recovery."
                    );
                    was_degraded = true;
                    consecutive_errors = 0;
                }

                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Ok(msg) => {
                if consecutive_errors > 0 || was_degraded {
                    tracing::info!(
                        previous_errors = consecutive_errors,
                        "Kafka broker connection restored — resuming event consumption"
                    );
                    was_degraded = false;
                }
                consecutive_errors = 0;

                let payload = match msg.payload() {
                    None => {
                        tracing::warn!("Kafka message with empty payload — skipping");
                        continue;
                    }
                    Some(p) => p,
                };

                let envelope: KafkaEnvelope = match serde_json::from_slice(payload) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::error!("Failed to deserialize Kafka envelope: {e}");
                        continue;
                    }
                };

                if envelope.event_type != "command_event" {
                    continue;
                }

                let event: CommandEvent = match serde_json::from_value(envelope.payload) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::error!(
                            session_id = %envelope.session_id,
                            "Failed to deserialize CommandEvent from envelope: {e}"
                        );
                        continue;
                    }
                };

                let session_id = envelope.session_id.clone();
                let partition = msg.partition();
                let offset = msg.offset();

                let sender = {
                    let map = session_map.lock().await;
                    map.get(&session_id).cloned()
                };

                match sender {
                    None => {
                        tracing::debug!(
                            session_id = %session_id,
                            partition,
                            offset,
                            "No active session registered for this event — skipping"
                        );
                    }
                    Some(tx) => {
                        let kafka_event = KafkaEvent {
                            event,
                            partition,
                            offset,
                        };

                        match tx.try_send(kafka_event) {
                            Ok(()) => {
                                if let Err(e) = consumer.store_offset_from_message(&msg) {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        "Failed to store Kafka offset after delivery: {e}"
                                    );
                                }
                                tracing::debug!(
                                    session_id = %session_id,
                                    partition,
                                    offset,
                                    "Event routed to session handler"
                                );
                            }
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                tracing::error!(
                                    session_id = %session_id,
                                    capacity = channel_capacity,
                                    "Session event channel full — session is not consuming fast enough. \
                                     Marking session as incomplete and removing from routing map."
                                );
                                session_map.lock().await.remove(&session_id);
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                tracing::warn!(
                                    session_id = %session_id,
                                    "Session channel closed — removing from routing map"
                                );
                                session_map.lock().await.remove(&session_id);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Replay Kafka events for a single session starting from a given offset.
///
/// Creates a dedicated consumer with a unique group ID so it never interferes
/// with the main "memory-archive-workers" consumer group. Manually assigns
/// the partition and seeks to `start_offset`, then reads until the partition
/// high watermark. Only events matching `session_id` are forwarded.
///
/// Called during startup crash recovery for cloud_primary interrupted sessions.
pub async fn replay_session_events(
    broker: &str,
    session_id: &str,
    partition: i32,
    start_offset: i64,
    capacity: usize,
) -> anyhow::Result<tokio::sync::mpsc::Receiver<KafkaEvent>> {
    use rdkafka::consumer::{BaseConsumer, Consumer};
    use rdkafka::topic_partition_list::{Offset, TopicPartitionList};

    let group_id = format!("memory-archive-recovery-{session_id}");

    let consumer: BaseConsumer = rdkafka::config::ClientConfig::new()
        .set("bootstrap.servers", broker)
        .set("group.id", &group_id)
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .context("Failed to create Kafka recovery consumer")?;

    // Manually assign partition and seek — don't use subscribe() which triggers
    // group rebalance and would interfere with the main consumer group.
    let mut tpl = TopicPartitionList::new();
    tpl.add_partition_offset(
        "control-center-events",
        partition,
        Offset::Offset(start_offset),
    )
    .context("Failed to add partition to recovery assignment")?;

    consumer
        .assign(&tpl)
        .context("Failed to assign recovery partition")?;

    // Fetch high watermark so we know when to stop.
    let (_, high_watermark) = consumer
        .fetch_watermarks(
            "control-center-events",
            partition,
            std::time::Duration::from_secs(10),
        )
        .context("Failed to fetch watermarks for recovery partition")?;

    tracing::info!(
        session_id = %session_id,
        partition,
        start_offset,
        high_watermark,
        "Kafka recovery: replaying events"
    );

    let (tx, rx) = tokio::sync::mpsc::channel::<KafkaEvent>(capacity);
    let session_id_owned = session_id.to_string();

    // Replay runs in a blocking thread — BaseConsumer is sync-only.
    tokio::task::spawn_blocking(move || {
        if start_offset >= high_watermark {
            tracing::info!(
                session_id = %session_id_owned,
                "Kafka recovery: no new events since last metadata flush — nothing to replay"
            );
            return;
        }

        let mut last_seen_offset = start_offset - 1;

        loop {
            match consumer.poll(std::time::Duration::from_millis(500)) {
                None => {
                    // Timeout — if we've already seen up to the high watermark we're done
                    if last_seen_offset >= high_watermark - 1 {
                        break;
                    }
                    continue;
                }
                Some(Err(e)) => {
                    tracing::error!(
                        session_id = %session_id_owned,
                        "Kafka recovery consumer error: {e}"
                    );
                    break;
                }
                Some(Ok(msg)) => {
                    let offset = msg.offset();
                    last_seen_offset = offset;

                    let payload = match msg.payload() {
                        None => {
                            if offset >= high_watermark - 1 { break; }
                            continue;
                        }
                        Some(p) => p,
                    };

                    let envelope: crate::kafka::KafkaEnvelope =
                        match serde_json::from_slice(payload) {
                            Ok(e) => e,
                            Err(e) => {
                                tracing::error!(
                                    session_id = %session_id_owned,
                                    "Recovery: failed to deserialize envelope: {e}"
                                );
                                if offset >= high_watermark - 1 { break; }
                                continue;
                            }
                        };

                    if envelope.event_type != "command_event"
                        || envelope.session_id != session_id_owned
                    {
                        if offset >= high_watermark - 1 { break; }
                        continue;
                    }

                    let event: ma_proto::control_center::CommandEvent =
                        match serde_json::from_value(envelope.payload) {
                            Ok(e) => e,
                            Err(e) => {
                                tracing::error!(
                                    session_id = %session_id_owned,
                                    "Recovery: failed to deserialize CommandEvent: {e}"
                                );
                                if offset >= high_watermark - 1 { break; }
                                continue;
                            }
                        };

                    let kafka_event = KafkaEvent {
                        event,
                        partition: msg.partition(),
                        offset,
                    };

                    if tx.blocking_send(kafka_event).is_err() {
                        break; // receiver dropped
                    }

                    if offset >= high_watermark - 1 {
                        break;
                    }
                }
            }
        }

        tracing::info!(
            session_id = %session_id_owned,
            "Kafka recovery: replay complete"
        );
    });

    Ok(rx)
}

async fn run_lag_monitor(broker: String, lag_warn_threshold: i64, alert_webhook_url: String) {
    let consumer: StreamConsumer = match ClientConfig::new()
        .set("bootstrap.servers", &broker)
        .set("group.id", "memory-archive-lag-monitor")
        .set("enable.auto.commit", "false")
        .create()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Lag monitor: failed to create consumer: {e}");
            return;
        }
    };

    loop {
        tokio::time::sleep(Duration::from_secs(LAG_CHECK_INTERVAL_SECS)).await;

        let assignment = match consumer.assignment() {
            Ok(tpl) => tpl,
            Err(e) => {
                tracing::warn!("Lag monitor: failed to get assignment: {e}");
                continue;
            }
        };

        if assignment.count() == 0 {
            continue;
        }

        let mut total_lag: i64 = 0;

        for elem in assignment.elements() {
            let topic = elem.topic();
            let partition = elem.partition();

            let watermarks = match consumer.fetch_watermarks(topic, partition, Duration::from_secs(5)) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!(
                        topic,
                        partition,
                        "Lag monitor: failed to fetch watermarks: {e}"
                    );
                    continue;
                }
            };

            let committed = match consumer.committed_offsets(
                TopicPartitionList::new(),
                Duration::from_secs(5),
            ) {
                Ok(_) => watermarks.0,
                Err(_) => watermarks.0,
            };

            let lag = watermarks.1 - committed;
            if lag > 0 {
                total_lag += lag;
                tracing::debug!(topic, partition, lag, "Partition lag");
            }
        }

        crate::observability::metrics().kafka_consumer_lag.set(total_lag as f64);

        if total_lag > lag_warn_threshold {
            tracing::warn!(
                total_lag,
                lag_warn_threshold,
                "Kafka consumer lag above warning threshold"
            );
            crate::observability::send_alert(
                &alert_webhook_url,
                &format!("Kafka consumer lag {total_lag} exceeds threshold {lag_warn_threshold}"),
            ).await;
        } else if total_lag > 0 {
            tracing::debug!(total_lag, "Kafka consumer lag within normal range");
        }
    }
}