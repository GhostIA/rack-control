//! The real Paho adapter (callbacks, `ClientMqtt`, connect options minus TLS)
//! against an embedded `rumqttd` broker on 127.0.0.1, with the sidecar worker
//! and the Unix-socket fake controller. No network beyond loopback.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::{start_fake, TempDir};
use paho_mqtt as paho;
use relay_mqtt::bridge::Bridge;
use relay_mqtt::config::Config;
use relay_mqtt::controller::UnixController;
use relay_mqtt::fake_controller::FakeState;
use relay_mqtt::mqtt::{self, ClientMqtt};
use relay_mqtt::worker;
use tokio::time::{timeout, Instant};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Start rumqttd (MQTT 3.1.1, plain TCP, no auth) on 127.0.0.1:`port`.
fn start_broker(port: u16) {
    let cfg = format!(
        r#"
id = 0
[router]
max_connections = 64
max_outgoing_packet_count = 200
max_segment_size = 1048576
max_segment_count = 10

[v4.1]
name = "v4-1"
listen = "127.0.0.1:{port}"
next_connection_delay_ms = 1
[v4.1.connections]
connection_timeout_ms = 5000
max_payload_size = 4096
max_inflight_count = 100
dynamic_filters = true
"#
    );
    let cfg: rumqttd::Config = toml::from_str(&cfg).unwrap();
    std::thread::spawn(move || {
        let mut broker = rumqttd::Broker::new(cfg);
        broker.start().unwrap();
    });
}

/// A plain client standing in for the orchestrator.
async fn orchestrator(
    uri: &str,
    id: &str,
) -> (
    paho::AsyncClient,
    paho::AsyncReceiver<Option<paho::Message>>,
) {
    let mut cli = paho::CreateOptionsBuilder::new_v3()
        .server_uri(uri)
        .client_id(id)
        .persistence(paho::PersistenceType::None)
        .create_client()
        .unwrap();
    let rx = cli.get_stream(256);
    let opts = paho::ConnectOptionsBuilder::new_v3()
        .clean_session(true)
        .finalize();
    // The broker thread may still be binding.
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Err(e) = cli.connect(opts.clone()).await {
        assert!(Instant::now() < deadline, "broker never came up: {e}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (cli, rx)
}

type Seen = BTreeMap<String, String>;

async fn collect_until(
    rx: &paho::AsyncReceiver<Option<paho::Message>>,
    seen: &mut Seen,
    done: impl Fn(&Seen) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(seen) {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, rx.recv()).await {
            Ok(Ok(Some(m))) => {
                seen.insert(
                    m.topic().to_string(),
                    String::from_utf8_lossy(m.payload()).into_owned(),
                );
            }
            Ok(Ok(None)) => {}
            Ok(Err(_)) | Err(_) => return,
        }
    }
}

fn all_states(s: &Seen) -> bool {
    (0..8).all(|n| s.contains_key(&format!("rack/relay/{n}/state")))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paho_adapter_end_to_end_with_embedded_broker() {
    let port = free_port();
    start_broker(port);
    let uri = format!("tcp://127.0.0.1:{port}");

    // A retained command left on the broker must never be applied.
    let (orch, rx) = orchestrator(&uri, "orchestrator").await;
    orch.publish(paho::Message::new_retained("rack/relay/5/set", "open", 1))
        .await
        .unwrap();

    let dir = TempDir::new();
    let mut st = FakeState::default();
    st.relay_open[0] = true;
    let (fake, _h) = start_fake(&dir.sock(), st);

    // The sidecar, wired exactly as in main.rs except for TLS.
    let cfg = Config::default();
    let cli = paho::AsyncClient::new(mqtt::create_options_for(&cfg, uri.clone())).unwrap();
    let (tx, wrx) = tokio::sync::mpsc::unbounded_channel();
    mqtt::install_callbacks(&cli, tx);
    mqtt::start(
        &cli,
        mqtt::base_connect_options(&cfg, "pw".into()).finalize(),
    );
    let bridge = Bridge::new(
        UnixController::new(dir.sock(), Duration::from_secs(2)),
        ClientMqtt::new(cli.clone()),
    );
    let w = tokio::spawn(worker::run(bridge, wrx, worker::Timing::default()));

    orch.subscribe("rack/relay/+/state", 1).await.unwrap();
    orch.subscribe("rack/sbc4/status", 1).await.unwrap();
    let mut seen = Seen::new();
    collect_until(&rx, &mut seen, |s| {
        s.get("rack/sbc4/status").map(String::as_str) == Some("online") && all_states(s)
    })
    .await;
    assert_eq!(
        seen.get("rack/sbc4/status").map(String::as_str),
        Some("online")
    );
    assert!(all_states(&seen), "{seen:?}");
    for n in 0..8 {
        let want = if n == 0 { "open" } else { "closed" };
        assert_eq!(seen[&format!("rack/relay/{n}/state")], want, "relay {n}");
    }

    // Invalid commands do nothing; a valid one is applied and published.
    seen.clear();
    for (t, p) in [
        ("rack/relay/9/set", "open"),
        ("rack/relay/4/set", "OPEN"),
        ("rack/relay/3/set", "open"),
    ] {
        orch.publish(paho::Message::new(t, p, 1)).await.unwrap();
    }
    collect_until(&rx, &mut seen, |s| s.contains_key("rack/relay/3/state")).await;
    assert_eq!(
        seen.get("rack/relay/3/state").map(String::as_str),
        Some("open")
    );
    assert_eq!(seen.len(), 1, "{seen:?}");

    let log = fake.lock().unwrap().log.clone();
    assert_eq!(log.iter().filter(|l| *l == "open 3").count(), 1, "{log:?}");
    assert!(
        !log.iter().any(|l| l == "open 5" || l == "open 4"),
        "{log:?}"
    );
    assert!(fake.lock().unwrap().relay_open[3]);
    assert!(!fake.lock().unwrap().relay_open[5]);
    w.abort();
}

/// Loopback TCP proxy in front of the broker. `cut()` drops every proxied
/// connection (like a broker restart or a network blip); new connections
/// are accepted again right away.
struct Proxy {
    port: u16,
    cut: tokio::sync::broadcast::Sender<()>,
}

impl Proxy {
    async fn start(broker_port: u16) -> Self {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let (cut, _) = tokio::sync::broadcast::channel(4);
        let cut2 = cut.clone();
        tokio::spawn(async move {
            loop {
                let (mut down, _) = l.accept().await.unwrap();
                let mut cut_rx = cut2.subscribe();
                tokio::spawn(async move {
                    let Ok(mut up) =
                        tokio::net::TcpStream::connect(("127.0.0.1", broker_port)).await
                    else {
                        return;
                    };
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut down, &mut up) => {}
                        _ = cut_rx.recv() => {}
                    }
                });
            }
        });
        Proxy { port, cut }
    }

    fn cut(&self) {
        let _ = self.cut.send(());
    }
}

/// Collect every message (in order) until `done` holds or 15 s pass.
async fn collect_events(
    rx: &paho::AsyncReceiver<Option<paho::Message>>,
    out: &mut Vec<(String, String)>,
    done: impl Fn(&[(String, String)]) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done(out) {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, rx.recv()).await {
            Ok(Ok(Some(m))) => out.push((
                m.topic().to_string(),
                String::from_utf8_lossy(m.payload()).into_owned(),
            )),
            Ok(Ok(None)) => {}
            Ok(Err(_)) | Err(_) => return,
        }
    }
}

fn has(ev: &[(String, String)], t: &str, p: &str) -> bool {
    ev.iter().any(|(a, b)| a == t && b == p)
}

// Paho's automatic reconnect after the broker connection drops: the LWT fires,
// and on reconnect the sidecar re-subscribes (clean session), publishes
// `online` and all 8 states again, and handles new commands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paho_reconnects_resubscribes_and_resyncs() {
    let port = free_port();
    start_broker(port);
    let proxy = Proxy::start(port).await;
    let (orch, rx) = orchestrator(&format!("tcp://127.0.0.1:{port}"), "orch2").await;
    orch.subscribe("rack/relay/+/state", 1).await.unwrap();
    orch.subscribe("rack/sbc4/status", 1).await.unwrap();

    let dir = TempDir::new();
    let (fake, _h) = start_fake(&dir.sock(), FakeState::default());
    let cfg = Config::default();
    let uri = format!("tcp://127.0.0.1:{}", proxy.port);
    let cli = paho::AsyncClient::new(mqtt::create_options_for(&cfg, uri)).unwrap();
    let (tx, wrx) = tokio::sync::mpsc::unbounded_channel();
    mqtt::install_callbacks(&cli, tx);
    mqtt::start(
        &cli,
        mqtt::base_connect_options(&cfg, "pw".into()).finalize(),
    );
    let bridge = Bridge::new(
        UnixController::new(dir.sock(), Duration::from_secs(2)),
        ClientMqtt::new(cli.clone()),
    );
    let w = tokio::spawn(worker::run(bridge, wrx, worker::Timing::default()));

    let mut ev = Vec::new();
    collect_events(&rx, &mut ev, |e| {
        has(e, "rack/sbc4/status", "online") && has(e, "rack/relay/7/state", "closed")
    })
    .await;
    assert!(has(&ev, "rack/relay/7/state", "closed"), "{ev:?}");

    // The controller changes behind the sidecar's back while it is offline;
    // the reconnect sync must publish the controller's actual state.
    ev.clear();
    proxy.cut();
    fake.lock().unwrap().relay_open[2] = true;
    collect_events(&rx, &mut ev, |e| {
        has(e, "rack/sbc4/status", "online") && has(e, "rack/relay/7/state", "closed")
    })
    .await;
    let status: Vec<&str> = ev
        .iter()
        .filter(|(t, _)| t == "rack/sbc4/status")
        .map(|(_, p)| p.as_str())
        .collect();
    assert_eq!(status, vec!["offline", "online"], "LWT then online: {ev:?}");
    assert!(has(&ev, "rack/relay/2/state", "open"), "{ev:?}");
    assert_eq!(
        ev.iter().filter(|(t, _)| t.ends_with("/state")).count(),
        8,
        "full sync after reconnect: {ev:?}"
    );

    // The subscription was renewed: a command after the reconnect works.
    ev.clear();
    orch.publish(paho::Message::new("rack/relay/6/set", "open", 1))
        .await
        .unwrap();
    collect_events(&rx, &mut ev, |e| has(e, "rack/relay/6/state", "open")).await;
    assert!(has(&ev, "rack/relay/6/state", "open"), "{ev:?}");
    assert!(fake.lock().unwrap().relay_open[6]);
    w.abort();
}
