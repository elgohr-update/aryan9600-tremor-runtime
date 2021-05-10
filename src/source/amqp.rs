// Copyright 2020-2021, The Tremor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
#![cfg(not(tarpaulin_include))]

use crate::source::prelude::*;
use serde::{Deserialize};
use crate::url::TremorUrl;
use url::Url;
use crate::errors::Error;
use lapin::{
    options::*, types::FieldTable, Connection, ConnectionProperties, Consumer,
};

/*enum QueueProperties {
    Durable 1
    Exclusive 2
    AutoDelete 4
    NoWait 8
}*/
#[derive(Deserialize, Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct Config {
    pub amqp_addr: String,
    queue_name: String,
    queue_options: QueueDeclareOptions,
    routing_key: String,
    exchange: String,  // "", by default
    #[serde(default = "Default::default")]
    pub close_on_done: bool,
    #[serde(default = "Default::default")]
    pub sleep_on_done: u64,
}

impl ConfigImpl for Config {}

pub struct Amqp {
    pub config: Config,
    onramp_id: TremorUrl,
}

pub struct Int {
    uid: u64,
    config: Config,
    amqp_url: Url,
    onramp_id: TremorUrl,
    origin_uri: EventOriginUri,
    consumer: Option<Consumer>,
    with_ack: bool,
}

impl std::fmt::Debug for Int {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Kafka")
    }
}

impl onramp::Impl for Amqp {
    fn from_config(id: &TremorUrl, config: &Option<YamlValue>) -> Result<Box<dyn Onramp>> {
        if let Some(config) = config {
            let config: Config = Config::new(config)?;
            Ok(Box::new(Self {
                config,
                onramp_id: id.clone(),
            }))
        } else {
            Err("Missing config for Amqp onramp".into())
        }
    }
}

impl Int {
    fn from_config(uid: u64, onramp_id: TremorUrl, config: &Config) -> Self {
        let amqp_url = Url::parse(&config.amqp_addr).unwrap();
        let origin_uri = EventOriginUri {
            uid,
            scheme: "amqp".to_string(),
            host: "not-connected".to_string(),
            port: None,
            path: vec![],
        };
        Self {
            uid,
            config: config.clone(),
            amqp_url,
            onramp_id,
            consumer: None,
            origin_uri,
            with_ack: false,
        }
    }
}

impl std::fmt::Debug for Amqp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Amqp")
    }
}

impl std::convert::From<lapin::Error> for Error {
    fn from(e: lapin::Error) -> Self {
        Self::from(format!("{:?}", e))
    }
}

#[async_trait::async_trait()]
impl Source for Int {
    fn is_transactional(&self) -> bool {
        self.with_ack
    }
    fn id(&self) -> &TremorUrl {
        &self.onramp_id
    }
    async fn pull_event(&mut self, _id: u64) -> Result<SourceReply> {
        if self.consumer.is_none() {
            return Ok(SourceReply::StateChange(SourceState::Disconnected))
        }
        if let Some(delivery) = self.consumer.as_mut().unwrap().next().await {
            let (_, delivery) = delivery.expect("error in consumer");
            // TODO: not sure what to do with _ack_result ... we got the ack acked
            let _ack_result = delivery.ack(BasicAckOptions::default()).await?;
            let data = delivery.data;
            let mut origin_uri = self.origin_uri.clone();
            origin_uri.path = vec![
                delivery.routing_key.to_string(),
            ];
            Ok(SourceReply::Data{
                        origin_uri,
                        data,
                        meta: None, // TODO: what can we put in meta here?
                        codec_override: None,
                        stream: 0,
                    })
        } else {
            Ok(SourceReply::StateChange(SourceState::Disconnected))
        }
    }
    async fn init(&mut self) -> Result<SourceState> {
        let conn = Connection::connect(
            &self.config.amqp_addr,
            ConnectionProperties::default(),
        )
        .await?;

        info!("[amqp] connected {}", self.config.amqp_addr);

        self.origin_uri = EventOriginUri {
            uid:    self.uid,
            scheme: self.amqp_url.scheme().to_string(),
            host:   match self.amqp_url.host_str() {
                Some(h) => h.to_string(),
                _ => "".to_string()
            },
            port:   self.amqp_url.port(),
            path:   match self.amqp_url.path_segments() {
                Some(pathvec) => pathvec.map(|x| String::from(x)).collect::<Vec<String>>(),
                _ => vec![]
            },
        };

        let channel = conn.create_channel().await?;

        channel
            .queue_declare(
                self.config.queue_name.as_str(),
                self.config.queue_options,
                FieldTable::default(),
            )
            .await?;
        
        channel
            .queue_bind(
                self.config.queue_name.as_str(),
                self.config.exchange.as_str(),
                self.config.routing_key.as_str(),
                QueueBindOptions::default(),
                FieldTable::default(),
            )
            .await?;
        
        self.consumer = match channel.basic_consume(
            &self.config.queue_name,
            "tremor",
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await {
            Ok(consumer) => Some(consumer),
            Err(_) => return Ok(SourceState::Disconnected)
        };
        
        Ok(SourceState::Connected)
    }
    fn trigger_breaker(&mut self) {}
    fn restore_breaker(&mut self) {}

    // If we fail a message we seek back to this failed
    // message to replay data from here.
    //
    // This might seek over multiple topics but since we internally only keep
    // track of a singular stream this is OK.
    //
    // If this is undesirable multiple onramps with an onramp per topic
    // should be used.
    fn fail(&mut self, id: u64) {
        if self.with_ack {
            error!("[amqp] fail not implemented, msgid: {}", id)
        }
    }
    fn ack(&mut self, id: u64) {
        if self.with_ack {
            error!("[amqp] ack not implemented, msgid: {}", id)
        }
    }
}

#[async_trait::async_trait]
impl Onramp for Amqp {
    async fn start(&mut self, config: OnrampConfig<'_>) -> Result<onramp::Addr> {
        let source = Int::from_config(config.onramp_uid, self.onramp_id.clone(), &self.config);
        SourceManager::start(source, config).await
    }
    fn default_codec(&self) -> &str {
        "json"
    }
}