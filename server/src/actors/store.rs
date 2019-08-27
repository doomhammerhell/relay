//! This module contains the actor that forwards events and attachments to the Sentry store.
//! The actor uses kafka topics to forward data to Sentry

use chrono::Utc;
use std::sync::Arc;

use actix::prelude::*;
use bytes::Bytes;
use failure::Fail;
use futures::Future;
use rdkafka::error::KafkaError;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::ClientConfig;
use rmp_serde::to_vec_named;
use serde::Serialize;

use rmp_serde::encode::Error as SerdeError;

use semaphore_common::{tryf, Config, KafkaTopic};
use semaphore_general::protocol::EventId;

use crate::actors::controller::{Controller, Shutdown, Subscribe, TimeoutError};
use crate::utils::SyncHandle;

#[derive(Fail, Debug)]
pub enum StoreError {
    #[fail(display = "kafka message send canceled")]
    Canceled,
    #[fail(display = "failed to send kafka message")]
    Kafka(KafkaError),
    #[fail(display = "failed to serialize message")]
    SerializationError(SerdeError),
}

/// Actor for publishing events to Sentry through kafka topics
pub struct StoreForwarder {
    config: Arc<Config>,
    shutdown: SyncHandle,
    producer: FutureProducer,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum KafkaMessageType {
    Event,
    AttachmentChunk,
    Transaction,
}

#[derive(Debug, Serialize)]
struct EventKafkaMessage {
    ty: KafkaMessageType,
    payload: Bytes,
    start_time: i64,
}

impl StoreForwarder {
    pub fn new(config: Arc<Config>) -> Self {
        let mut client_config = ClientConfig::new();
        for config_p in config.kafka_config() {
            client_config.set(config_p.name.as_str(), config_p.value.as_str());
        }

        StoreForwarder {
            config,
            shutdown: SyncHandle::new(),
            producer: client_config
                .create()
                .expect("could not create kafka producer"), // TODO cleanup
        }
    }
}

/// StoreMessageForwarder is an async actor since the only thing it does is put the messages
/// in the kafka topics
impl Actor for StoreForwarder {
    type Context = Context<Self>;

    fn started(&mut self, context: &mut Self::Context) {
        log::info!("store forwarder started");
        Controller::from_registry().do_send(Subscribe(context.address().recipient()));
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        log::info!("store forwarder stopped");
    }
}

/// Message sent to the StoreForwarder containing an event
pub struct StoreEvent {
    pub event_id: EventId,
    pub payload: Bytes,
}

impl Message for StoreEvent {
    type Result = Result<(), StoreError>;
}

fn serialize_kafka_event(payload: Bytes) -> Result<Vec<u8>, StoreError> {
    let message = EventKafkaMessage {
        payload,
        start_time: Utc::now().timestamp(),
        ty: KafkaMessageType::Event,
    };

    match to_vec_named(&message) {
        Err(e) => Err(StoreError::SerializationError(e)),
        Ok(val) => Ok(val),
    }
}

impl Handler<StoreEvent> for StoreForwarder {
    type Result = ResponseFuture<(), StoreError>;

    fn handle(&mut self, msg: StoreEvent, _ctx: &mut Self::Context) -> Self::Result {
        let serialized_message = tryf!(serialize_kafka_event(msg.payload));

        let record = FutureRecord::to(self.config.kafka_topic_name(KafkaTopic::Events))
            .payload(&serialized_message)
            .key(msg.event_id.0.as_bytes().as_ref());

        let future = self.producer.send(record, 0).then(|result| match result {
            Ok(Ok(_)) => Ok(()),
            Ok(Err((kafka_error, _message))) => Err(StoreError::Kafka(kafka_error)),
            Err(_) => Err(StoreError::Canceled),
        });

        Box::new(future)
    }
}

impl Handler<Shutdown> for StoreForwarder {
    type Result = ResponseFuture<(), TimeoutError>;

    fn handle(&mut self, message: Shutdown, _context: &mut Self::Context) -> Self::Result {
        self.producer.flush(message.timeout);

        match message.timeout {
            Some(timeout) => self.shutdown.timeout(timeout),
            None => self.shutdown.now(),
        }
    }
}
