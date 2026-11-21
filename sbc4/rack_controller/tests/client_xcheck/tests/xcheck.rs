//! Cross-check: relay-mqtt's production `UnixController`, `Bridge` and
//! protocol parser against rack_controller's real C++ socket server (core.cpp
//! on a fake HAL, `bin/test_server`). Run via `make xcheck`.

use std::path::PathBuf;
use std::process::{Child, Command as Proc};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use relay_mqtt::bridge::{Bridge, Mqtt, Outcome};
use relay_mqtt::controller::{Controller, ReplyKind, UnixController};
use relay_mqtt::protocol::{parse_state, Channel, Command, RelayState};

struct Server {
    child: Child,
    dir: PathBuf,
}

impl Server {
    fn start(extra: &[&str]) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let bin = std::env::var("RC_TEST_SERVER")
            .expect("RC_TEST_SERVER must point at bin/test_server (use `make xcheck`)");
        let dir = PathBuf::from(format!(
            "/tmp/rcx-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let child = Proc::new(bin)
            .arg(dir.join("ctl.sock"))
            .args(extra)
            .spawn()
            .expect("spawn test_server");
        let s = Server { child, dir };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !s.sock().exists() {
            assert!(Instant::now() < deadline, "test_server never bound its socket");
            std::thread::sleep(Duration::from_millis(20));
        }
        s
    }
    fn sock(&self) -> PathBuf {
        self.dir.join("ctl.sock")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Proc::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[derive(Clone, Default)]
struct RecMqtt(Arc<Mutex<Vec<(String, String)>>>);

impl RecMqtt {
    fn take(&self) -> Vec<(String, String)> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

impl Mqtt for RecMqtt {
    async fn publish_retained(&mut self, topic: &str, payload: &'static str) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .push((topic.to_string(), payload.to_string()));
        Ok(())
    }
    async fn subscribe(&mut self, _filter: &str) -> Result<()> {
        Ok(())
    }
}

fn ctl(s: &Server) -> UnixController {
    UnixController::new(s.sock(), Duration::from_secs(3))
}

#[tokio::test]
async fn bridge_round_trip_against_rack_controller() {
    let srv = Server::start(&[]);
    let mqtt = RecMqtt::default();
    let mut b = Bridge::new(ctl(&srv), mqtt.clone());

    // Connect: status + all 8 relays published from our `state` reply.
    b.on_connected().await;
    assert!(!b.needs_sync(), "state reply must parse");
    let p = mqtt.take();
    assert_eq!(p.len(), 9);
    assert_eq!(p[0], ("rack/sbc4/status".into(), "online".into()));
    for (i, (t, v)) in p[1..].iter().enumerate() {
        assert_eq!(t, &format!("rack/relay/{i}/state"));
        assert_eq!(v, "closed");
    }

    // Every channel, both verbs: our `ok <verb> <ch>` is accepted.
    for n in 0..8u8 {
        let ch = Channel::new(n).unwrap();
        for (payload, cmd, st) in [
            (&b"open"[..], Command::Open, "open"),
            (&b"close"[..], Command::Close, "closed"),
        ] {
            let o = b
                .on_message(&format!("rack/relay/{n}/set"), payload, false)
                .await;
            assert_eq!(o, Outcome::Applied(ch, cmd), "relay {n} {payload:?}");
            assert_eq!(mqtt.take(), vec![(ch.state_topic(), st.to_string())]);
        }
    }

    // The audit (periodic `state`) agrees with what was published.
    b.on_message("rack/relay/5/set", b"open", false).await;
    mqtt.take();
    assert!(b.audit().await);
    assert!(mqtt.take().is_empty(), "audit must find nothing changed");
    assert!(b.sync().await);
    let p = mqtt.take();
    assert_eq!(p[5], ("rack/relay/5/state".into(), "open".into()));
    assert!(!b.needs_sync());
}

#[tokio::test]
async fn err_reply_is_rejected_and_nothing_published() {
    let srv = Server::start(&["--fail-relay", "2"]);
    let mqtt = RecMqtt::default();
    let mut b = Bridge::new(ctl(&srv), mqtt.clone());
    let o = b.on_message("rack/relay/2/set", b"open", false).await;
    match o {
        Outcome::Rejected(ch, Command::Open, reply) => {
            assert_eq!(ch.index(), 2);
            assert!(reply.starts_with("err "), "{reply:?}");
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
    assert!(mqtt.take().is_empty());
    // The connection stays usable after an err.
    let o = b.on_message("rack/relay/3/set", b"open", false).await;
    assert!(matches!(o, Outcome::Applied(_, Command::Open)), "{o:?}");
}

#[tokio::test]
async fn raw_exchanges_on_one_connection() {
    let srv = Server::start(&[]);
    let mut c = ctl(&srv);
    for _ in 0..3 {
        let lines = c.exchange("state", ReplyKind::UntilEnd).await.unwrap();
        assert_eq!(lines.last().map(String::as_str), Some("end"));
        let r = parse_state(&lines).unwrap();
        assert!(r.iter().all(|x| x.attached && x.state == RelayState::Closed));
        assert_eq!(lines.len(), 8 + 4 + 4 + 1);
    }
    let ch = Channel::new(7).unwrap();
    let r = c.exchange("open 7", ReplyKind::Single).await.unwrap();
    assert!(Command::Open.is_ok_reply(ch, &r[0]), "{r:?}");
    let r = c.exchange("fan 0 0.5", ReplyKind::Single).await.unwrap();
    assert_eq!(r, vec!["ok fan 0 0.50"]);
    let r = c.exchange("ssr 1 off", ReplyKind::Single).await.unwrap();
    assert_eq!(r, vec!["ok ssr 1 off"]);
    let r = c.exchange("open 9", ReplyKind::Single).await.unwrap();
    assert!(r[0].starts_with("err "));
    // An `err` reply to state-shaped garbage is one line; the client handles it.
    let r = c.exchange("state now", ReplyKind::UntilEnd).await.unwrap();
    assert_eq!(r.len(), 1);
    assert!(r[0].starts_with("err "));
    // Still in step afterwards; exactly one connection was used.
    let lines = c.exchange("state", ReplyKind::UntilEnd).await.unwrap();
    assert_eq!(parse_state(&lines).unwrap()[7].state, RelayState::Open);
    assert!(c.take_reconnected());
    assert!(!c.take_reconnected());
}
