//! rumqttc wiring: options (mTLS + password + LWT), the event-loop task, and
//! the `Mqtt` impl over `AsyncClient`.

use std::time::Duration;

use anyhow::{Context, Result};
use rumqttc::{
    AsyncClient, ConnectionError, Event, EventLoop, LastWill, MqttOptions, Packet, QoS,
    TlsConfiguration, Transport,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;

use crate::backoff::Backoff;
use crate::bridge::Mqtt;
use crate::config::{Config, Credentials};
use crate::protocol::{STATUS_OFFLINE, STATUS_TOPIC};

/// What the event-loop task forwards to the worker.
#[derive(Debug, PartialEq, Eq)]
pub enum BrokerEvent {
    Connected,
    Message {
        topic: String,
        payload: Vec<u8>,
        retain: bool,
        /// When the event loop received it (stale commands are dropped).
        received: Instant,
    },
}

pub fn options(cfg: &Config, creds: Credentials) -> MqttOptions {
    // Client id = username, per the contract.
    let mut o = MqttOptions::new(cfg.username.clone(), cfg.host.clone(), cfg.port);
    o.set_credentials(cfg.username.clone(), creds.password);
    o.set_clean_session(true);
    o.set_keep_alive(Duration::from_secs(cfg.keep_alive_secs));
    o.set_last_will(LastWill::new(
        STATUS_TOPIC,
        STATUS_OFFLINE,
        QoS::AtLeastOnce,
        true,
    ));
    // Verification stays on: rumqttc builds a rustls config with only our CA
    // as root; an IP host becomes an IP ServerName checked against the SAN.
    o.set_transport(Transport::tls_with_config(TlsConfiguration::Simple {
        ca: creds.ca,
        alpn: None,
        client_auth: Some((creds.cert, creds.key)),
    }));
    o
}

/// Poll the event loop forever, reconnecting with backoff. Never returns
/// unless the worker side has gone away.
pub async fn run_eventloop(mut el: EventLoop, tx: UnboundedSender<BrokerEvent>) {
    let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
    loop {
        match el.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                log::info!("connected to broker");
                backoff.reset();
                // rumqttc replays requests left over from the previous
                // connection first; the worker's resubscribe and full state
                // publish follow them, so the newest values win.
                if tx.send(BrokerEvent::Connected).is_err() {
                    return;
                }
            }
            Ok(Event::Incoming(Packet::Publish(p))) => {
                let ev = BrokerEvent::Message {
                    topic: p.topic,
                    payload: p.payload.to_vec(),
                    retain: p.retain,
                    received: Instant::now(),
                };
                if tx.send(ev).is_err() {
                    return;
                }
            }
            Ok(_) => {}
            Err(e) => {
                let wait = backoff.next_delay();
                log_conn_error(&e, wait);
                tokio::time::sleep(wait).await;
            }
        }
    }
}

fn log_conn_error(e: &ConnectionError, wait: Duration) {
    log::warn!("broker connection: {e}; retrying in {}s", wait.as_secs());
}

pub struct ClientMqtt(pub AsyncClient);

impl Mqtt for ClientMqtt {
    async fn publish_retained(&mut self, topic: &str, payload: &'static str) -> Result<()> {
        self.0
            .publish(topic, QoS::AtLeastOnce, true, payload.as_bytes())
            .await
            .with_context(|| format!("queue publish {topic}"))
    }

    async fn subscribe(&mut self, filter: &str) -> Result<()> {
        self.0
            .subscribe(filter, QoS::AtLeastOnce)
            .await
            .with_context(|| format!("queue subscribe {filter}"))
    }
}
