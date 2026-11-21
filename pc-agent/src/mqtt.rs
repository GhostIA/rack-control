//! rumqttc glue: connection options, the event-loop task, and the real
//! `Publisher` that waits for the PUBACK of its own publish.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result};
use log::{debug, info, warn};
use rumqttc::{
    AsyncClient, ConnectReturnCode, Event, EventLoop, Incoming, LastWill, MqttOptions, Outgoing,
    QoS, TlsConfiguration, Transport,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{timeout_at, Instant};

use crate::agent::{Message, Publisher};
use crate::config::{read_password, Config};

/// What the event-loop task tells the main task about.
#[derive(Debug)]
pub enum LoopEvent {
    Connected,
    Message(Message),
}

/// Packet-id events the publisher matches against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckEvent {
    Outgoing(u16),
    PubAck(u16),
}

pub fn options(cfg: &Config) -> Result<MqttOptions> {
    let ca = std::fs::read(&cfg.ca).with_context(|| format!("reading {}", cfg.ca.display()))?;
    let cert =
        std::fs::read(&cfg.cert).with_context(|| format!("reading {}", cfg.cert.display()))?;
    let key = std::fs::read(&cfg.key).with_context(|| format!("reading {}", cfg.key.display()))?;
    let password = read_password(&cfg.password_file)?;

    let mut o = MqttOptions::new(cfg.name.clone(), cfg.broker_host.clone(), cfg.broker_port);
    o.set_credentials(cfg.name.clone(), password);
    o.set_clean_session(true);
    o.set_keep_alive(Duration::from_secs(30));
    o.set_last_will(LastWill::new(
        cfg.status_topic(),
        "offline",
        QoS::AtLeastOnce,
        true,
    ));
    // Server verification stays on: rustls checks the chain against `ca` and the
    // name/IP in `broker_host` against the cert SAN.
    o.set_transport(Transport::tls_with_config(TlsConfiguration::Simple {
        ca,
        alpn: None,
        client_auth: Some((cert, key)),
    }));
    Ok(o)
}

/// Drives the rumqttc event loop forever (it reconnects on the next poll after
/// an error), forwarding what the main task needs. Ends after our DISCONNECT is
/// sent or when the main task is gone.
pub fn spawn_event_loop(
    mut eventloop: EventLoop,
) -> (
    mpsc::UnboundedReceiver<LoopEvent>,
    mpsc::UnboundedReceiver<AckEvent>,
    JoinHandle<()>,
) {
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let (ack_tx, ack_rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Incoming::ConnAck(ca))) => {
                    if ca.code == ConnectReturnCode::Success {
                        backoff = Duration::from_secs(1);
                        if ev_tx.send(LoopEvent::Connected).is_err() {
                            return;
                        }
                    } else {
                        warn!("broker refused connection: {:?}", ca.code);
                    }
                }
                Ok(Event::Incoming(Incoming::Publish(p))) => {
                    let m = Message {
                        topic: p.topic,
                        payload: p.payload.to_vec(),
                        retain: p.retain,
                    };
                    if ev_tx.send(LoopEvent::Message(m)).is_err() {
                        return;
                    }
                }
                Ok(Event::Incoming(Incoming::SubAck(s))) => {
                    debug!("suback {:?}", s.return_codes);
                    if s.return_codes
                        .iter()
                        .any(|c| matches!(c, rumqttc::SubscribeReasonCode::Failure))
                    {
                        warn!(
                            "broker rejected a subscription (ACL?): {:?}",
                            s.return_codes
                        );
                    }
                }
                Ok(Event::Incoming(Incoming::PubAck(a))) => {
                    let _ = ack_tx.send(AckEvent::PubAck(a.pkid));
                }
                Ok(Event::Outgoing(Outgoing::Publish(pkid))) => {
                    let _ = ack_tx.send(AckEvent::Outgoing(pkid));
                }
                Ok(Event::Outgoing(Outgoing::Disconnect)) => {
                    info!("disconnect sent");
                    return;
                }
                Ok(_) => {}
                Err(e) => {
                    if ev_tx.is_closed() {
                        return;
                    }
                    warn!("mqtt connection error: {e}; retrying in {backoff:?}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    });
    (ev_rx, ack_rx, handle)
}

/// Real publisher. All of the agent's publishes go through it one at a time
/// (the main task is sequential). The first `Outgoing::Publish` packet id seen
/// after our request that does not belong to an earlier, still-unacked publish
/// (rumqttc replays those after a reconnect) is ours; its PUBACK completes the
/// wait.
pub struct MqttPublisher {
    client: AsyncClient,
    acks: mpsc::UnboundedReceiver<AckEvent>,
    /// Packet ids of earlier publishes that went out but were never acked.
    unacked: HashSet<u16>,
}

impl MqttPublisher {
    pub fn new(client: AsyncClient, acks: mpsc::UnboundedReceiver<AckEvent>) -> Self {
        Self {
            client,
            acks,
            unacked: HashSet::new(),
        }
    }

    fn note_stale(&mut self, ev: AckEvent) {
        match ev {
            AckEvent::Outgoing(p) => {
                if self.unacked.len() >= 64 {
                    self.unacked.clear();
                }
                self.unacked.insert(p);
            }
            AckEvent::PubAck(p) => {
                self.unacked.remove(&p);
            }
        }
    }
}

impl Publisher for MqttPublisher {
    async fn publish_retained(
        &mut self,
        topic: &str,
        payload: &'static str,
        timeout: Duration,
    ) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        // Events queued so far belong to earlier (possibly timed-out) publishes.
        while let Ok(ev) = self.acks.try_recv() {
            self.note_stale(ev);
        }
        match timeout_at(
            deadline,
            self.client
                .publish(topic, QoS::AtLeastOnce, true, payload.as_bytes()),
        )
        .await
        {
            Err(_) => return Ok(false),
            Ok(r) => r.context("queueing publish")?,
        }
        let mut mine: Option<u16> = None;
        loop {
            let ev = match timeout_at(deadline, self.acks.recv()).await {
                Err(_) => {
                    if let Some(p) = mine {
                        self.note_stale(AckEvent::Outgoing(p));
                    }
                    return Ok(false);
                }
                Ok(None) => anyhow::bail!("mqtt event loop stopped"),
                Ok(Some(ev)) => ev,
            };
            match ev {
                AckEvent::Outgoing(p) if mine.is_none() && !self.unacked.contains(&p) => {
                    mine = Some(p)
                }
                AckEvent::PubAck(p) if Some(p) == mine => return Ok(true),
                AckEvent::Outgoing(p) if Some(p) == mine => {} // our own replay
                other => self.note_stale(other),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> (AsyncClient, EventLoop) {
        // Never polled: requests just sit in the channel.
        AsyncClient::new(MqttOptions::new("t", "localhost", 1), 10)
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn times_out_without_ack() {
        let (c, _el) = client();
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut p = MqttPublisher::new(c, rx);
        let t0 = Instant::now();
        let acked = p
            .publish_retained("a/b", "down", Duration::from_secs(5))
            .await
            .unwrap();
        assert!(!acked);
        assert_eq!(Instant::now() - t0, Duration::from_secs(5));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn matches_own_puback_only() {
        let (c, _el) = client();
        let (tx, rx) = mpsc::unbounded_channel();
        let mut p = MqttPublisher::new(c, rx);
        // Stale events from before this publish are discarded.
        tx.send(AckEvent::Outgoing(1)).unwrap();
        tx.send(AckEvent::PubAck(1)).unwrap();
        let tx2 = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            tx2.send(AckEvent::Outgoing(7)).unwrap();
            tx2.send(AckEvent::PubAck(3)).unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
            tx2.send(AckEvent::PubAck(7)).unwrap();
        });
        let t0 = Instant::now();
        assert!(p
            .publish_retained("a/b", "down", Duration::from_secs(5))
            .await
            .unwrap());
        assert_eq!(Instant::now() - t0, Duration::from_millis(20));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn replayed_stale_publish_not_credited() {
        let (c, _el) = client();
        let (tx, rx) = mpsc::unbounded_channel();
        let mut p = MqttPublisher::new(c, rx);
        // First publish goes out as pkid 5 and is never acked.
        let tx1 = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            tx1.send(AckEvent::Outgoing(5)).unwrap();
        });
        assert!(!p
            .publish_retained("a/b", "online", Duration::from_secs(1))
            .await
            .unwrap());
        // Second publish: a reconnect replays pkid 5 and it is acked before
        // our own pkid 6 even goes out. That must not count as our ack.
        let tx2 = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            tx2.send(AckEvent::Outgoing(5)).unwrap();
            tx2.send(AckEvent::PubAck(5)).unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
            tx2.send(AckEvent::Outgoing(6)).unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
            tx2.send(AckEvent::PubAck(6)).unwrap();
        });
        let t0 = Instant::now();
        assert!(p
            .publish_retained("a/b", "down", Duration::from_secs(5))
            .await
            .unwrap());
        assert_eq!(Instant::now() - t0, Duration::from_millis(30));
        assert!(p.unacked.is_empty());
    }
}
