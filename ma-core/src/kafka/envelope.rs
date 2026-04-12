use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KafkaEnvelope {
    pub schema_version: String,
    pub session_id: String,
    pub event_type: String,
    pub timestamp: String,
    pub payload: serde_json::Value,
}