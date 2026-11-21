//! Eclipse Paho glue: client/connect/TLS options, the callback -> channel
//! bridge, the real `Publisher` (a QoS 1 publish whose `DeliveryToken` is
//! awaited with a timeout) and the main receive loop.
//!
//! Paho owns the connection: it reconnects by itself (capped exponential
//! backoff, also when the very first connect fails) and calls the connected
//! callback after every successful CONNACK. Because the session is clean, the
//! loop re-subscribes and re-publishes the status on every such event.

use std::future::Future;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use log::{error, info, warn};
use paho_mqtt as paho;
use tokio::sync::mpsc;

use crate::agent::{Agent, CephRunner, Message, Outcome, PowerOff, Publisher};
use crate::config::Config;

pub const KEEP_ALIVE: Duration = Duration::from_secs(30);
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Paho's automatic reconnect: starts at the minimum, doubles per failure,
/// capped at the maximum (whole seconds; Paho adds jitter).
pub const RECONNECT_MIN: Duration = Duration::from_secs(1);
pub const RECONNECT_MAX: Duration = Duration::from_secs(30);
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(10);
const NOT_CONNECTED_WARN: Duration = Duration::from_secs(60);
/// Publishes made while disconnected are buffered by Paho and sent after the
/// reconnect (the waiting caller still gives up after its own timeout).
const MAX_BUFFERED: i32 = 16;
/// MQTT 3.1.1 SUBACK failure return code.
const SUBACK_FAILURE: i32 = 0x80;

/// What Paho's callback thread hands to the main task, in arrival order.
#[derive(Debug)]
pub enum LoopEvent {
    Connected,
    Message(Message),
}

/// `ssl://host:port`, with IPv6 literals bracketed.
pub fn broker_uri(cfg: &Config) -> String {
    let host = &cfg.broker_host;
    if host.contains(':') && !host.starts_with('[') {
        format!("ssl://[{host}]:{}", cfg.broker_port)
    } else {
        format!("ssl://{host}:{}", cfg.broker_port)
    }
}

/// An async client with in-memory persistence only (the unit runs with
/// `ProtectSystem=strict`, and a clean session has nothing worth persisting).
pub fn create_client(uri: &str, client_id: &str) -> Result<paho::AsyncClient> {
    paho::CreateOptionsBuilder::new()
        .server_uri(uri)
        .client_id(client_id)
        .mqtt_version(paho::MQTT_VERSION_3_1_1)
        .persistence(None)
        .send_while_disconnected(true)
        .max_buffered_messages(MAX_BUFFERED)
        .delete_oldest_messages(true)
        .create_client()
        .with_context(|| format!("creating MQTT client for {uri}"))
}

/// TLS: our CA only (no system trust store), server certificate verification
/// on, host/IP verification on (Paho checks an IP `broker_host` against the
/// certificate's IP SANs via `X509_check_ip_asc`), TLS 1.2 or 1.3, client cert + key.
/// No `ssl_version`: with OpenSSL >= 1.1 Paho uses TLS_client_method() and ignores
/// it; the vendored OpenSSL 3's default security level refuses TLS 1.0/1.1 and
/// TLS 1.3 stays allowed (the broker also requires >= 1.2).
pub fn tls_options(cfg: &Config) -> Result<paho::SslOptions> {
    // Paho only opens these at connect time, with vague errors; fail early.
    for p in [&cfg.ca, &cfg.cert, &cfg.key] {
        let data = std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
        if data.is_empty() {
            bail!("{} is empty", p.display());
        }
    }
    Ok(paho::SslOptionsBuilder::new()
        .trust_store(&cfg.ca)
        .context("ca path")?
        .key_store(&cfg.cert)
        .context("cert path")?
        .private_key(&cfg.key)
        .context("key path")?
        .disable_default_trust_store(true)
        .enable_server_cert_auth(true)
        .verify(true)
        .finalize())
}

/// MQTT 3.1.1, username = `name` (also the client id), clean session, LWT
/// retained QoS 1 `offline` on `status_topic`, Paho automatic reconnect.
pub fn connect_options(
    name: &str,
    password: &str,
    status_topic: &str,
    ssl: Option<paho::SslOptions>,
) -> paho::ConnectOptions {
    let mut b = paho::ConnectOptionsBuilder::with_mqtt_version(paho::MQTT_VERSION_3_1_1);
    b.clean_session(true)
        .user_name(name)
        .password(password)
        .keep_alive_interval(KEEP_ALIVE)
        .connect_timeout(CONNECT_TIMEOUT)
        .will_message(paho::Message::new_retained(status_topic, "offline", 1))
        .automatic_reconnect(RECONNECT_MIN, RECONNECT_MAX);
    if let Some(ssl) = ssl {
        b.ssl_options(ssl);
    }
    b.finalize()
}

pub fn from_paho(m: &paho::Message) -> Message {
    Message {
        topic: m.topic().to_string(),
        payload: m.payload().to_vec(),
        retain: m.retained(),
    }
}

/// Routes Paho's callbacks (run on Paho's own thread) into `tx`. Must be set
/// before `connect`. Sending on an unbounded channel never blocks Paho.
pub fn wire_callbacks(client: &paho::AsyncClient, tx: mpsc::UnboundedSender<LoopEvent>) {
    let ctx = tx.clone();
    client.set_connected_callback(move |_| {
        let _ = ctx.send(LoopEvent::Connected);
    });
    client.set_connection_lost_callback(|_| {
        warn!(
            "mqtt connection lost; paho reconnects automatically ({RECONNECT_MIN:?}..{RECONNECT_MAX:?} backoff)"
        );
    });
    client.set_message_callback(move |_, m| {
        if let Some(m) = m {
            let _ = tx.send(LoopEvent::Message(from_paho(&m)));
        }
    });
}

pub fn event_channel(client: &paho::AsyncClient) -> mpsc::UnboundedReceiver<LoopEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    wire_callbacks(client, tx);
    rx
}

/// Starts the first connect. If it fails, Paho keeps retrying on its own
/// (automatic reconnect). Paho's retries are silent, so this also warns
/// every `NOT_CONNECTED_WARN` while the client is not connected.
pub fn start_connect(client: &paho::AsyncClient, opts: paho::ConnectOptions) {
    let tok = client.connect(opts);
    let client = client.clone();
    tokio::spawn(async move {
        if let Err(e) = tok.await {
            warn!("initial mqtt connect failed: {e}; paho keeps retrying");
        }
        loop {
            tokio::time::sleep(NOT_CONNECTED_WARN).await;
            if !client.is_connected() {
                warn!(
                    "still not connected to {} (paho retrying); check the broker, TLS files and password",
                    client.server_uri()
                );
            }
        }
    });
}

/// Sends DISCONNECT (so the broker does not fire the will) and stops Paho's
/// reconnects. Bounded so a dead link cannot hang shutdown.
pub async fn disconnect(client: &paho::AsyncClient) {
    let opts = paho::DisconnectOptionsBuilder::new()
        .timeout(Duration::from_secs(2))
        .finalize();
    let _ = tokio::time::timeout(Duration::from_secs(3), client.disconnect(opts)).await;
}

/// Real publisher: QoS 1, retained; `Ok(true)` once Paho's delivery token
/// completes (PUBACK received), `Ok(false)` on timeout, `Err` if Paho fails
/// the token. Paho matches the PUBACK to the publish itself.
#[derive(Clone)]
pub struct MqttPublisher {
    client: paho::AsyncClient,
}

impl MqttPublisher {
    pub fn new(client: paho::AsyncClient) -> Self {
        Self { client }
    }
}

impl Publisher for MqttPublisher {
    async fn publish_retained(
        &mut self,
        topic: &str,
        payload: &'static str,
        timeout: Duration,
    ) -> Result<bool> {
        let tok = self
            .client
            .publish(paho::Message::new_retained(topic, payload.as_bytes(), 1));
        match tokio::time::timeout(timeout, tok).await {
            Err(_) => Ok(false),
            Ok(Ok(())) => Ok(true),
            Ok(Err(e)) => Err(e).context("publish"),
        }
    }
}

/// Subscribes and waits for the SUBACK. Never fatal: a rejected subscription
/// (ACL) is logged and the agent keeps running, because exiting would only
/// crash-loop and, mid-shutdown, would let the `offline` will replace the
/// retained `down`. (Paho reports a single-topic SUBACK 0x80 as a generic
/// failure, so it can't be told apart from other errors.) A dead link is
/// caught by the keepalive, and the reconnect subscribes again.
async fn subscribe(client: &paho::AsyncClient, topics: &[String]) {
    for t in topics {
        info!("subscribing {t}");
    }
    let tok = client.subscribe_many_same_qos(topics, 1);
    match tokio::time::timeout(SUBSCRIBE_TIMEOUT, tok).await {
        Ok(Ok(rsp)) => {
            let granted = rsp.subscribe_many_response().unwrap_or_default();
            if granted.iter().any(|&q| q == SUBACK_FAILURE || q < 0) {
                error!("broker rejected a subscription (ACL?): {topics:?} -> {granted:?}");
            }
        }
        Ok(Err(e)) => error!(
            "subscribing {topics:?} failed: {e} (ACL rejection, or the link dropped; a reconnect resubscribes)"
        ),
        Err(_) => error!(
            "no SUBACK for {topics:?} within {SUBSCRIBE_TIMEOUT:?}; a reconnect resubscribes"
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Served {
    /// The agent asked to exit (dry run after a handled shutdown).
    Exit,
    /// `stop` resolved (SIGTERM/SIGINT).
    Stopped,
    /// The event channel closed (all callbacks gone).
    Closed,
}

/// The receive loop: one event at a time, in Paho's delivery order. Every
/// CONNACK re-subscribes (clean session) and re-publishes the status.
pub async fn serve<P: Publisher, O: PowerOff, C: CephRunner>(
    client: &paho::AsyncClient,
    agent: &mut Agent<P, O, C>,
    events: &mut mpsc::UnboundedReceiver<LoopEvent>,
    stop: impl Future<Output = ()>,
) -> Result<Served> {
    tokio::pin!(stop);
    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                None => return Ok(Served::Closed),
                Some(LoopEvent::Connected) => {
                    subscribe(client, &agent.subscriptions()).await;
                    agent.on_connected().await;
                }
                Some(LoopEvent::Message(m)) => {
                    if agent.handle(&m).await == Outcome::Exit {
                        return Ok(Served::Exit);
                    }
                }
            },
            _ = &mut stop => return Ok(Served::Stopped),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Adapter tests drive the real Paho client over loopback TCP (no TLS):
    //! - against `rumqttd` (an embedded, real MQTT broker) for normal flows,
    //!   password auth, the message burst and the initial-connect retry;
    //! - against `stub_broker` only where a fault must be injected that a real
    //!   broker won't produce on cue: dropping the link while our `down` is
    //!   unacknowledged, forcing a reconnect, withholding PUBACKs, and
    //!   inspecting the raw CONNECT (will, clean session, credentials).
    use super::*;
    use crate::agent::ACK_TIMEOUT;
    use crate::stub_broker::{Ev, Stub};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    const NAME: &str = "intel-test";
    const PASSWORD: &str = "s3cret";
    const CMD: &str = "rack/pc/intel-test/cmd";
    const STATUS: &str = "rack/pc/intel-test/status";
    const CEPH_SET: &str = "rack/ceph/maintenance/set";
    const SERVE_LIMIT: Duration = Duration::from_secs(30);

    fn cfg(extra: &str) -> Config {
        Config::parse(&format!("name = \"{NAME}\"\n{extra}")).unwrap()
    }

    #[test]
    fn broker_uri_formats() {
        assert_eq!(broker_uri(&cfg("")), "ssl://192.168.8.1:8883");
        assert_eq!(
            broker_uri(&cfg("broker_host = \"fd00::1\"\nbroker_port = 1883\n")),
            "ssl://[fd00::1]:1883"
        );
    }

    #[test]
    fn tls_options_need_readable_files() {
        let c = cfg("ca = \"/nonexistent/ca.crt\"\n");
        let e = format!("{:#}", tls_options(&c).unwrap_err());
        assert!(e.contains("/nonexistent/ca.crt"), "{e}");
        let dir = std::env::temp_dir().display().to_string();
        let c = cfg(&format!("ca = {dir:?}\n"));
        assert!(tls_options(&c).is_err(), "a directory accepted as ca");
    }

    #[test]
    fn client_identity() {
        let o = connect_options(NAME, "pw", STATUS, None);
        assert!(o.clean_session());
        let c = create_client("ssl://192.168.8.1:8883", NAME).unwrap();
        assert_eq!(c.client_id(), NAME);
        assert_eq!(c.server_uri(), "ssl://192.168.8.1:8883");
    }

    /// Never connected: Paho refuses the publish, which surfaces as `Err`
    /// (the agent then powers off anyway).
    #[tokio::test(flavor = "current_thread")]
    async fn publish_error_when_never_connected() {
        let c = create_client("tcp://127.0.0.1:9", NAME).unwrap();
        let mut p = MqttPublisher::new(c);
        assert!(p
            .publish_retained(STATUS, "down", ACK_TIMEOUT)
            .await
            .is_err());
    }

    // ---------------------------------------------------------------- rig

    type Log = Arc<Mutex<Vec<String>>>;

    /// Records PowerOff; `probe` is evaluated at that moment (e.g. "how many
    /// `down` had the broker received?").
    struct FakePower {
        probe: Box<dyn Fn() -> usize + Send>,
        log: Log,
    }
    impl PowerOff for FakePower {
        async fn power_off(&mut self) -> anyhow::Result<()> {
            let n = (self.probe)();
            self.log.lock().unwrap().push(format!("poweroff downs={n}"));
            Ok(())
        }
    }
    struct FakeCeph(Log);
    impl CephRunner for FakeCeph {
        async fn run(&mut self, args: &[String]) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("ceph {}", args.join(" ")));
            Ok(())
        }
    }

    struct Rig {
        client: paho::AsyncClient,
        agent: Agent<MqttPublisher, FakePower, FakeCeph>,
        log: Log,
        /// Paho callback events (before the test's forwarder).
        raw: mpsc::UnboundedReceiver<LoopEvent>,
    }

    fn rig(port: u16, extra: &str, probe: Box<dyn Fn() -> usize + Send>) -> Rig {
        let c = cfg(extra);
        let client = create_client(&format!("tcp://127.0.0.1:{port}"), NAME).unwrap();
        let (tx, raw) = mpsc::unbounded_channel();
        wire_callbacks(&client, tx);
        let log: Log = Arc::default();
        let agent = Agent::new(
            &c,
            MqttPublisher::new(client.clone()),
            FakePower {
                probe,
                log: log.clone(),
            },
            FakeCeph(log.clone()),
        );
        start_connect(
            &client,
            connect_options(NAME, PASSWORD, &c.status_topic(), None),
        );
        Rig {
            client,
            agent,
            log,
            raw,
        }
    }

    /// Forwards Paho events to the loop and closes the loop's channel after
    /// `n` messages, so `serve` returns `Closed` once it handled all of them.
    fn forward_n_messages(
        mut raw: mpsc::UnboundedReceiver<LoopEvent>,
        n: usize,
    ) -> mpsc::UnboundedReceiver<LoopEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut seen = 0;
            while seen < n {
                let Some(ev) = raw.recv().await else { return };
                if matches!(ev, LoopEvent::Message(_)) {
                    seen += 1;
                }
                if tx.send(ev).is_err() {
                    return;
                }
            }
        });
        rx
    }

    async fn wait_until(what: &str, pred: impl Fn() -> bool) {
        for _ in 0..2000 {
            if pred() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    // ------------------------------------------------------------ rumqttd

    /// What an observer link on the embedded broker saw: (topic, payload).
    type Seen = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    struct Rumqttd {
        port: u16,
        tx: rumqttd::local::LinkTx,
        seen: Seen,
    }

    impl Rumqttd {
        /// Starts rumqttd on 127.0.0.1:`port` with password auth for NAME, and
        /// an observer link subscribed to `rack/#`.
        fn start(port: u16) -> Rumqttd {
            let toml_cfg = format!(
                r#"
id = 0
[router]
max_connections = 64
max_outgoing_packet_count = 1000
max_segment_size = 1048576
max_segment_count = 10
[v4.1]
name = "v4-1"
listen = "127.0.0.1:{port}"
next_connection_delay_ms = 1
[v4.1.connections]
connection_timeout_ms = 5000
max_payload_size = 20480
max_inflight_count = 100
dynamic_filters = true
auth = {{ {NAME} = "{PASSWORD}" }}
"#
            );
            let config: rumqttd::Config = toml::from_str(&toml_cfg).unwrap();
            let mut broker = rumqttd::Broker::new(config);
            let (mut tx, mut rx) = broker.link("observer").unwrap();
            std::thread::spawn(move || {
                let _ = broker.start();
            });
            tx.subscribe("rack/#").unwrap();
            let seen: Seen = Arc::default();
            let s = seen.clone();
            std::thread::spawn(move || loop {
                match rx.recv() {
                    Ok(Some(rumqttd::Notification::Forward(f))) => s.lock().unwrap().push((
                        String::from_utf8_lossy(&f.publish.topic).into_owned(),
                        f.publish.payload.to_vec(),
                    )),
                    // The router parks a link whose buffer it filled; a
                    // local link must ask to be rescheduled.
                    Ok(Some(rumqttd::Notification::Unschedule)) => {
                        if rx.ready().is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            });
            Rumqttd { port, tx, seen }
        }

        fn count(seen: &Seen, topic: &str, payload: &[u8]) -> usize {
            seen.lock()
                .unwrap()
                .iter()
                .filter(|(t, p)| t == topic && p == payload)
                .count()
        }

        fn publish(&mut self, topic: &str, payload: &[u8]) {
            self.tx
                .publish(topic.to_string(), payload.to_vec())
                .unwrap();
        }

        async fn publish_retained(&mut self, topic: &str, payload: &[u8]) {
            let p =
                rumqttd::protocol::Publish::new(topic.as_bytes().to_vec(), payload.to_vec(), true);
            self.tx
                .send(rumqttd::protocol::Packet::Publish(p, None))
                .await
                .unwrap();
        }
    }

    /// Connect with password auth, subscribe, publish `online`; a retained
    /// `shutdown` already stored on the broker is delivered on subscribe and
    /// ignored.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rumqttd_online_and_stored_retained_shutdown_ignored() {
        let mut b = Rumqttd::start(Stub::free_port());
        // A link doesn't see its own publishes, so this isn't observable
        // here; if it were not stored, the loop below would never get its one
        // message and the test would time out rather than pass.
        b.publish_retained(CMD, b"shutdown").await;
        let seen = b.seen.clone();
        let s2 = seen.clone();
        let mut r = rig(
            b.port,
            "ceph_maintenance = true\n",
            Box::new(move || Rumqttd::count(&s2, STATUS, b"down")),
        );
        let mut events = forward_n_messages(r.raw, 1);
        let served = tokio::time::timeout(
            SERVE_LIMIT,
            serve(&r.client, &mut r.agent, &mut events, std::future::pending()),
        )
        .await;
        assert_eq!(served.unwrap().unwrap(), Served::Closed);
        assert!(r.log.lock().unwrap().is_empty());
        wait_until("online", || Rumqttd::count(&seen, STATUS, b"online") == 1).await;
        assert_eq!(Rumqttd::count(&seen, STATUS, b"down"), 0);
        disconnect(&r.client).await;
    }

    /// Wrong password: the broker refuses, and the agent never sees a
    /// `Connected` event (so it never subscribes or acts).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rumqttd_rejects_wrong_password() {
        let b = Rumqttd::start(Stub::free_port());
        let client = create_client(&format!("tcp://127.0.0.1:{}", b.port), NAME).unwrap();
        let mut events = event_channel(&client);
        start_connect(&client, connect_options(NAME, "wrong", STATUS, None));
        let r = tokio::time::timeout(Duration::from_millis(2500), events.recv()).await;
        assert!(r.is_err(), "connected with a wrong password");
        assert!(!client.is_connected());
        disconnect(&client).await;
    }

    /// Race (a): a burst delivered by Paho's callback thread (shutdown
    /// duplicates, near-miss payloads, another PC's topic, ceph) while the
    /// loop handles one message at a time. Exactly one `down`, one PowerOff,
    /// nothing acted on after it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn race_message_burst_from_paho_thread_powers_off_once() {
        const N: usize = 300;
        let mut b = Rumqttd::start(Stub::free_port());
        let seen = b.seen.clone();
        let s2 = seen.clone();
        let mut r = rig(
            b.port,
            "ceph_maintenance = true\n",
            Box::new(move || Rumqttd::count(&s2, STATUS, b"down")),
        );
        // (The observer link lags the PUBACK, so the probe isn't asserted.)
        // Messages on topics we subscribe to (another PC's cmd is never
        // delivered to us).
        let plan: Vec<(&str, &[u8])> = (0..N)
            .map(|i| match i {
                0..=2 => (CMD, &b"x"[..]),
                _ => match i % 5 {
                    0 => (CMD, &b"shutdown"[..]),
                    1 => (CMD, &b"shutdown\n"[..]),
                    2 => ("rack/pc/intel-other/cmd", &b"shutdown"[..]),
                    3 => (CEPH_SET, &b"on"[..]),
                    _ => (CMD, &b"Shutdown"[..]),
                },
            })
            .collect();
        let delivered = plan
            .iter()
            .filter(|(t, _)| *t == CMD || *t == CEPH_SET)
            .count();
        let mut events = forward_n_messages(r.raw, delivered);
        let s3 = seen.clone();
        let driver = async move {
            wait_until("online", || Rumqttd::count(&s3, STATUS, b"online") == 1).await;
            for (t, p) in plan {
                b.publish(t, p);
            }
        };
        let (served, ()) = tokio::join!(
            tokio::time::timeout(
                SERVE_LIMIT,
                serve(&r.client, &mut r.agent, &mut events, std::future::pending())
            ),
            driver
        );
        assert_eq!(served.unwrap().unwrap(), Served::Closed);
        // rumqttd keeps per-topic order only (as MQTT requires), so the ceph
        // `on`s may land before or after the first shutdown: ceph runs at
        // most once (both flags), strictly before the single PowerOff.
        let log = r.log.lock().unwrap().clone();
        let ceph = vec![
            "ceph --id rack-maint osd set noout".to_string(),
            "ceph --id rack-maint osd set norebalance".to_string(),
        ];
        let (before, last) = log.split_at(log.len() - 1);
        assert!(last[0].starts_with("poweroff"), "{log:?}");
        assert!(before.is_empty() || before == ceph.as_slice(), "{log:?}");
        wait_until("down", || Rumqttd::count(&seen, STATUS, b"down") >= 1).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(Rumqttd::count(&seen, STATUS, b"down"), 1);
        let states = Rumqttd::count(&seen, "rack/ceph/maintenance/state", b"on");
        assert_eq!(states, usize::from(!before.is_empty()));
        disconnect(&r.client).await;
    }

    /// The very first connect fails (nothing listening yet); Paho's own
    /// automatic reconnect connects once the broker appears.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initial_connect_failure_is_retried_by_paho() {
        let port = Stub::free_port();
        let client = create_client(&format!("tcp://127.0.0.1:{port}"), NAME).unwrap();
        let mut events = event_channel(&client);
        start_connect(&client, connect_options(NAME, PASSWORD, STATUS, None));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!client.is_connected());
        let _b = Rumqttd::start(port);
        let ev = tokio::time::timeout(SERVE_LIMIT, events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev, LoopEvent::Connected));
        disconnect(&client).await;
    }

    // ------------------------------------------- stub (fault injection)

    fn status_count(evs: &[Ev], payload: &str) -> usize {
        evs.iter()
            .filter(|e| matches!(e, Ev::Publish { topic, payload: p, .. } if topic == STATUS && p == payload.as_bytes()))
            .count()
    }

    fn stub_probe(stub: &Stub) -> Box<dyn Fn() -> usize + Send> {
        let evs = stub.events.clone();
        Box::new(move || status_count(&evs.lock().unwrap(), "down"))
    }

    /// The raw CONNECT carries the contract: client id = username = name,
    /// the password, clean session, LWT retained QoS 1 `offline`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connect_packet_contract() {
        let stub = Stub::start().await;
        let r = rig(stub.port, "", stub_probe(&stub));
        stub.wait_for(|e| !e.is_empty()).await;
        match &stub.events.lock().unwrap()[0] {
            Ev::Connect {
                client_id,
                username,
                password,
                clean,
                will,
                keep_alive,
                ..
            } => {
                assert_eq!(client_id, NAME);
                assert_eq!(username.as_deref(), Some(NAME));
                assert_eq!(password.as_deref(), Some(PASSWORD.as_bytes()));
                assert!(clean);
                assert_eq!(*keep_alive, 30);
                assert_eq!(
                    will.as_ref().unwrap(),
                    &(STATUS.to_string(), b"offline".to_vec(), 1, true)
                );
            }
            other => panic!("first event {other:?}"),
        }
        disconnect(&r.client).await;
    }

    /// A broker ACL rejecting our only subscription (single-topic SUBACK
    /// 0x80, which Paho fails as a generic error) is logged, not fatal: the
    /// loop keeps running and still publishes `online`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejected_subscription_is_not_fatal() {
        let stub = Stub::start().await;
        stub.set_reject_subscribe(true);
        let mut r = rig(stub.port, "", stub_probe(&stub));
        let mut events = r.raw;
        let stop = async {
            stub.wait_for(|e| status_count(e, "online") == 1).await;
        };
        let served = tokio::time::timeout(
            SERVE_LIMIT,
            serve(&r.client, &mut r.agent, &mut events, stop),
        )
        .await;
        assert_eq!(served.unwrap().unwrap(), Served::Stopped);
        disconnect(&r.client).await;
    }

    /// The PUBACK never comes: `Ok(false)` after the caller's timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn publish_times_out_without_puback() {
        let stub = Stub::start().await;
        stub.set_withhold_acks(true);
        let r = rig(stub.port, "", stub_probe(&stub));
        let c = r.client.clone();
        wait_until("connected", || c.is_connected()).await;
        let mut p = MqttPublisher::new(r.client.clone());
        let t0 = Instant::now();
        let timeout = Duration::from_millis(300);
        assert!(!p.publish_retained(STATUS, "down", timeout).await.unwrap());
        assert!(t0.elapsed() >= timeout);
        assert_eq!(status_count(&stub.events.lock().unwrap(), "down"), 1);
        disconnect(&r.client).await;
    }

    /// Race (b): the link drops while `down` is in flight (never PUBACKed).
    /// PowerOff happens exactly once, within the ack timeout, and after
    /// Paho's reconnect `down` is re-asserted (never `online`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn race_connection_lost_while_down_in_flight() {
        let stub = Stub::start().await;
        stub.set_drop_on(Some(b"down".to_vec()));
        let mut r = rig(stub.port, "", stub_probe(&stub));
        let log = r.log.clone();
        let mut events = r.raw;
        let stop = async {
            stub.wait_for(|e| status_count(e, "online") == 1).await;
            let t0 = Instant::now();
            stub.publish(0, CMD, b"shutdown", 1, false);
            stub.wait_for(|e| status_count(e, "down") >= 1).await;
            // The first `down` died with the connection; ack later ones.
            stub.set_drop_on(None);
            wait_until("poweroff", || !log.lock().unwrap().is_empty()).await;
            assert!(
                t0.elapsed() < ACK_TIMEOUT + Duration::from_millis(1500),
                "PowerOff not within the ack timeout: {:?}",
                t0.elapsed()
            );
            // After the reconnect the loop re-subscribes and re-asserts down.
            stub.wait_for(|e| {
                e.iter().any(|e| matches!(e, Ev::Publish { conn, topic, payload, acked: true, .. } if *conn > 0 && topic == STATUS && payload == b"down"))
            })
            .await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        let served = tokio::time::timeout(
            SERVE_LIMIT,
            serve(&r.client, &mut r.agent, &mut events, stop),
        )
        .await;
        assert_eq!(served.unwrap().unwrap(), Served::Stopped);
        let log = r.log.lock().unwrap().clone();
        assert_eq!(log.len(), 1, "{log:?}");
        assert!(log[0].starts_with("poweroff"), "{log:?}");
        let evs = stub.events.lock().unwrap().clone();
        let subs = evs
            .iter()
            .filter(|e| matches!(e, Ev::Subscribe { .. }))
            .count();
        assert!(subs >= 2, "not re-subscribed: {evs:?}");
        assert_eq!(status_count(&evs, "online"), 1, "{evs:?}");
        disconnect(&r.client).await;
    }

    /// Race (c): after a reconnect the broker replays a retained `shutdown`.
    /// It is ignored; the agent re-subscribes and re-publishes `online`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn race_reconnect_replaying_retained_shutdown_is_ignored() {
        let stub = Stub::start().await;
        let mut r = rig(stub.port, "", stub_probe(&stub));
        let mut events = forward_n_messages(r.raw, 1);
        let driver = async {
            stub.wait_for(|e| status_count(e, "online") == 1).await;
            // Only the reconnect's SUBACK is followed by the retained replay.
            stub.set_retained_on_subscribe(Some((CMD.to_string(), b"shutdown".to_vec())));
            stub.drop_conn(0);
        };
        let (served, ()) = tokio::join!(
            tokio::time::timeout(
                SERVE_LIMIT,
                serve(&r.client, &mut r.agent, &mut events, std::future::pending())
            ),
            driver
        );
        assert_eq!(served.unwrap().unwrap(), Served::Closed);
        assert!(r.log.lock().unwrap().is_empty());
        let evs = stub.events.lock().unwrap().clone();
        let subs = evs
            .iter()
            .filter(|e| matches!(e, Ev::Subscribe { .. }))
            .count();
        assert_eq!(subs, 2, "{evs:?}");
        assert_eq!(status_count(&evs, "online"), 2, "{evs:?}");
        assert_eq!(status_count(&evs, "down"), 0, "{evs:?}");
        disconnect(&r.client).await;
    }
}
