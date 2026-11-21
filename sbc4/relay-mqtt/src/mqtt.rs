//! Eclipse Paho wiring: client + connect options (mTLS + password + LWT +
//! automatic reconnect), the callbacks that feed the worker, and the `Mqtt`
//! impl over `paho_mqtt::AsyncClient`.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use paho_mqtt as paho;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{timeout, Instant};

use crate::bridge::Mqtt;
use crate::config::{Config, Credentials};
use crate::protocol::{STATUS_OFFLINE, STATUS_TOPIC};

/// QoS for every subscribe and publish (commands are QoS 1 per the contract).
pub const QOS: i32 = paho::QOS_1;
/// Bounds of Paho's built-in reconnect backoff (whole seconds; Paho doubles
/// the interval after each failed attempt, with jitter).
pub const RECONNECT_MIN: Duration = Duration::from_secs(1);
pub const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// How long to wait for a SUBACK / PUBACK before treating the operation as
/// failed. With a clean session Paho drops in-flight tokens on reconnect
/// without completing them, so an unbounded wait could hang the worker.
pub const ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// What the Paho callbacks forward to the worker.
#[derive(Debug, PartialEq, Eq)]
pub enum BrokerEvent {
    Connected,
    Message {
        topic: String,
        payload: Vec<u8>,
        retain: bool,
        /// When the callback received it (stale commands are dropped).
        received: Instant,
    },
}

/// `ssl://host:port` for the configured broker.
pub fn server_uri(cfg: &Config) -> String {
    ssl_uri(&cfg.host, cfg.port)
}

/// `ssl://host:port`. An IPv6 literal needs brackets.
pub fn ssl_uri(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("ssl://[{host}]:{port}")
    } else {
        format!("ssl://{host}:{port}")
    }
}

pub fn create_options(cfg: &Config) -> paho::CreateOptions {
    create_options_for(cfg, server_uri(cfg))
}

/// `create_options` with an explicit server URI (the adapter test uses a
/// plain `tcp://` broker on loopback).
pub fn create_options_for(cfg: &Config, uri: String) -> paho::CreateOptions {
    paho::CreateOptionsBuilder::new_v3()
        .server_uri(uri)
        // Client id = username, per the contract.
        .client_id(cfg.username.clone())
        // In-memory only: nothing is written to disk (the unit runs with
        // ProtectSystem=strict), and a clean session keeps nothing anyway.
        .persistence(paho::PersistenceType::None)
        // No offline buffering: a publish while disconnected fails at once,
        // the worker marks the state for resync, and every reconnect
        // republishes all relays from the controller's current state.
        .send_while_disconnected(false)
        .finalize()
}

pub fn connect_options(cfg: &Config, creds: Credentials) -> Result<paho::ConnectOptions> {
    // Verification stays on: only our CA is trusted (OpenSSL's default trust
    // store is disabled) and the chain is verified. `verify(true)` checks the
    // broker address against the cert: Paho C tries X509_check_host (which,
    // for a cert with no DNS SAN, falls back to the subject CN) and then
    // X509_check_ip_asc (the IP SAN). Paho has no switch for the CN fallback;
    // it is harmless here because only our CA is trusted and rackpki issues
    // the broker cert with an IP SAN and never a CN of an IP to anyone else.
    // TLS >= 1.2: with OpenSSL >= 1.1 Paho uses TLS_client_method() and
    // ignores its `ssl_version` option; the vendored OpenSSL 3's default
    // security level (>= 1) refuses TLS 1.0/1.1, and TLS 1.3 stays allowed.
    let ssl = paho::SslOptionsBuilder::new()
        .trust_store(&cfg.ca_file)
        .context("ca_file path")?
        .key_store(&cfg.cert_file)
        .context("cert_file path")?
        .private_key(&cfg.key_file)
        .context("key_file path")?
        .disable_default_trust_store(true)
        .enable_server_cert_auth(true)
        .verify(true)
        .finalize();
    Ok(base_connect_options(cfg, creds.password)
        .ssl_options(ssl)
        .finalize())
}

/// Every connect option except TLS: MQTT 3.1.1, user name, password, clean
/// session, keep-alive, the retained `offline` LWT and automatic reconnect.
pub fn base_connect_options(cfg: &Config, password: String) -> paho::ConnectOptionsBuilder {
    // LWT: retained `offline` (QoS 1).
    let will = paho::Message::new_retained(STATUS_TOPIC, STATUS_OFFLINE, QOS);
    let mut b = paho::ConnectOptionsBuilder::with_mqtt_version(paho::MQTT_VERSION_3_1_1);
    b.user_name(cfg.username.clone())
        .password(password)
        .clean_session(true)
        .keep_alive_interval(Duration::from_secs(cfg.keep_alive_secs))
        .connect_timeout(Duration::from_secs(10))
        .will_message(will)
        // Paho reconnects forever on its own, including after a failed
        // first connect; each success fires the connected callback.
        .automatic_reconnect(RECONNECT_MIN, RECONNECT_MAX);
    b
}

/// Install the callbacks that forward broker events to the worker. They run
/// on Paho's thread and only push into an unbounded channel, so no message is
/// lost and Paho is never blocked by the controller.
pub fn install_callbacks(cli: &paho::AsyncClient, tx: UnboundedSender<BrokerEvent>) {
    let txc = tx.clone();
    // Fires after the first connect and after every automatic reconnect.
    cli.set_connected_callback(move |_| {
        log::info!("connected to broker");
        let _ = txc.send(BrokerEvent::Connected);
    });
    cli.set_connection_lost_callback(|_| {
        log::warn!(
            "broker connection lost; Paho reconnects automatically ({}s..{}s)",
            RECONNECT_MIN.as_secs(),
            RECONNECT_MAX.as_secs()
        );
    });
    cli.set_message_callback(move |_, msg| {
        if let Some(m) = msg {
            let _ = tx.send(BrokerEvent::Message {
                topic: m.topic().to_string(),
                payload: m.payload().to_vec(),
                retain: m.retained(),
                received: Instant::now(),
            });
        }
    });
}

/// While the broker is unreachable, warn every `every` (Paho's automatic
/// reconnect logs each failed attempt only at debug level, under
/// `paho_mqtt::c_trace`).
pub fn spawn_watchdog(cli: paho::AsyncClient, every: Duration) {
    tokio::spawn(async move {
        let mut down_since: Option<Instant> = None;
        let mut tick = tokio::time::interval(every);
        tick.tick().await;
        loop {
            tick.tick().await;
            if cli.is_connected() {
                down_since = None;
                continue;
            }
            let since = *down_since.get_or_insert_with(Instant::now);
            log::warn!(
                "broker {} not connected (checked for {}s); Paho keeps reconnecting. \
                 RUST_LOG=info,paho_mqtt::c_trace=debug shows each attempt's error",
                cli.server_uri(),
                since.elapsed().as_secs()
            );
        }
    });
}

/// Start connecting. Returns immediately: if the first attempt fails, Paho's
/// automatic reconnect keeps trying, so the outcome is only logged.
pub fn start(cli: &paho::AsyncClient, opts: paho::ConnectOptions) {
    let tok = cli.connect(opts);
    tokio::spawn(async move {
        if let Err(e) = tok.await {
            log::warn!("first broker connect failed: {e}; Paho keeps retrying");
        }
    });
}

/// The `Mqtt` impl the worker uses in production.
pub struct ClientMqtt {
    cli: paho::AsyncClient,
    ack_timeout: Duration,
}

impl ClientMqtt {
    pub fn new(cli: paho::AsyncClient) -> Self {
        ClientMqtt {
            cli,
            ack_timeout: ACK_TIMEOUT,
        }
    }
}

/// Wait for a Paho token, bounded.
async fn wait<F, T>(what: &str, ack_timeout: Duration, tok: F) -> Result<()>
where
    F: std::future::Future<Output = paho::Result<T>>,
{
    match timeout(ack_timeout, tok).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(anyhow!("{what}: {e}")),
        Err(_) => Err(anyhow!(
            "{what}: no acknowledgement within {}s",
            ack_timeout.as_secs()
        )),
    }
}

impl Mqtt for ClientMqtt {
    /// Publish QoS 1 retained and wait for the PUBACK (the DeliveryToken).
    async fn publish_retained(&mut self, topic: &str, payload: &'static str) -> Result<()> {
        let msg = paho::Message::new_retained(topic, payload, QOS);
        let tok = self
            .cli
            .try_publish(msg)
            .map_err(|e| anyhow!("publish {topic}: {e}"))?;
        wait(&format!("publish {topic}"), self.ack_timeout, tok).await
    }

    /// Subscribe QoS 1 and wait for the SUBACK.
    async fn subscribe(&mut self, filter: &str) -> Result<()> {
        let tok = self.cli.subscribe(filter, QOS);
        wait(&format!("subscribe {filter}"), self.ack_timeout, tok).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_and_options_build() {
        let cfg = Config::default();
        assert_eq!(server_uri(&cfg), "ssl://192.168.8.1:8883");
        let v6 = Config {
            host: "fd00::1".into(),
            ..Config::default()
        };
        assert_eq!(server_uri(&v6), "ssl://[fd00::1]:8883");

        let creds = Credentials {
            password: "pw".into(),
        };
        let o = connect_options(&cfg, creds).unwrap();
        assert!(o.clean_session());
        assert!(o.has_ssl_options());
        assert_eq!(o.mqtt_version(), paho::MqttVersion::V3_1_1);
        // Creating the client validates the URI / client id with Paho C.
        let cli = paho::AsyncClient::new(create_options(&cfg)).unwrap();
        assert_eq!(cli.client_id(), "sbc4");
        assert_eq!(cli.server_uri(), "ssl://192.168.8.1:8883");
    }

    #[tokio::test]
    async fn publish_while_disconnected_fails_fast() {
        // Never connected: Paho refuses at once (no offline buffering), so
        // the bridge marks the state for resync instead of hanging.
        let cli = paho::AsyncClient::new(create_options(&Config::default())).unwrap();
        let mut m = ClientMqtt {
            cli,
            ack_timeout: Duration::from_secs(5),
        };
        let start = Instant::now();
        assert!(m
            .publish_retained("rack/relay/0/state", "open")
            .await
            .is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
