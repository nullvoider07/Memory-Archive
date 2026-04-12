// /Memory/Memory-Archive/ma-core/src/kafka/mod.rs

pub mod consumer;
pub mod envelope;
pub use consumer::{KafkaEvent, KafkaSessionMap};
pub use envelope::KafkaEnvelope;