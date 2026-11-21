//! Real Unix-socket tests: the production `UnixController` against the fake
//! controller server listening in a temp dir.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use relay_mqtt::bridge::{Bridge, Mqtt, Outcome};
use relay_mqtt::controller::{Controller, ReplyKind, UnixController};
use relay_mqtt::fake_controller::{bind, run, FakeState, Shared};
use relay_mqtt::mqtt::BrokerEvent;
use relay_mqtt::protocol::Command;
use relay_mqtt::worker;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::task::JoinHandle;
use tokio::time::Instant;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        // Keep it short: sun_path is ~104 bytes on macOS.
        let p = std::env::temp_dir().join(format!(
            "rmq-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    fn sock(&self) -> PathBuf {
        self.0.join("ctl.sock")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
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

fn start_fake(sock: &std::path::Path, state: FakeState) -> (Shared, JoinHandle<()>) {
    let shared: Shared = Arc::new(Mutex::new(state));
    let l = bind(sock).unwrap();
    let s2 = shared.clone();
    let h = tokio::spawn(async move {
        let _ = run(l, s2).await;
    });
    (shared, h)
}

fn fast_timing() -> worker::Timing {
    worker::Timing {
        min_retry: Duration::from_millis(20),
        max_retry: Duration::from_millis(100),
        audit_every: Duration::from_millis(100),
        max_command_age: Duration::from_secs(10),
    }
}

fn ctl(sock: PathBuf) -> UnixController {
    UnixController::new(sock, Duration::from_secs(2))
}

#[tokio::test]
async fn command_round_trip_and_state() {
    let dir = TempDir::new();
    let (shared, _h) = start_fake(&dir.sock(), FakeState::default());
    let mqtt = RecMqtt::default();
    let mut b = Bridge::new(ctl(dir.sock()), mqtt.clone());

    b.on_connected().await;
    assert!(!b.needs_sync());
    let p = mqtt.take();
    assert_eq!(p.len(), 9);
    assert_eq!(p[0], ("rack/sbc4/status".into(), "online".into()));
    assert!(p[1..].iter().all(|(_, v)| v == "closed"));

    let o = b.on_message("rack/relay/3/set", b"open", false).await;
    assert!(matches!(o, Outcome::Applied(_, Command::Open)));
    assert_eq!(
        mqtt.take(),
        vec![("rack/relay/3/state".into(), "open".into())]
    );
    assert!(shared.lock().unwrap().relay_open[3]);

    // Invalid channel never reaches the socket.
    b.on_message("rack/relay/9/set", b"open", false).await;
    assert!(mqtt.take().is_empty());
    assert_eq!(
        shared.lock().unwrap().log,
        vec!["state".to_string(), "open 3".to_string()]
    );

    // State reflects the commanded relay.
    assert!(b.sync().await);
    let p = mqtt.take();
    assert_eq!(p[3], ("rack/relay/3/state".into(), "open".into()));
}

#[tokio::test]
async fn err_reply_publishes_nothing() {
    let dir = TempDir::new();
    let st = FakeState {
        fail_relays: vec![2],
        ..Default::default()
    };
    let (shared, _h) = start_fake(&dir.sock(), st);
    let mqtt = RecMqtt::default();
    let mut b = Bridge::new(ctl(dir.sock()), mqtt.clone());
    let o = b.on_message("rack/relay/2/set", b"open", false).await;
    assert!(matches!(o, Outcome::Rejected(..)), "{o:?}");
    assert!(mqtt.take().is_empty());
    assert!(!shared.lock().unwrap().relay_open[2]);
}

#[tokio::test]
async fn missing_socket_is_not_fatal_and_recovers() {
    let dir = TempDir::new();
    let mqtt = RecMqtt::default();
    let mut b = Bridge::new(ctl(dir.sock()), mqtt.clone());
    let o = b.on_message("rack/relay/1/set", b"close", false).await;
    assert!(matches!(o, Outcome::ControllerDown(..)));
    assert!(!b.sync().await);
    assert!(mqtt.take().is_empty());

    let (_shared, _h) = start_fake(&dir.sock(), FakeState::default());
    assert!(b.sync().await);
    assert_eq!(mqtt.take().len(), 8);
}

#[tokio::test]
async fn controller_restart_reconnects_and_resyncs() {
    let dir = TempDir::new();
    let (_s1, h1) = start_fake(&dir.sock(), FakeState::default());
    let mqtt = RecMqtt::default();
    let mut b = Bridge::new(ctl(dir.sock()), mqtt.clone());
    b.on_connected().await;
    b.on_message("rack/relay/4/set", b"open", false).await;
    mqtt.take();
    assert!(!b.needs_sync());

    // "Restart" the controller: kill the server (dropping its connections)
    // and start a fresh one with reset state.
    h1.abort();
    let _ = h1.await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (s2, _h2) = start_fake(&dir.sock(), FakeState::default());

    let o = b.on_message("rack/relay/5/set", b"open", false).await;
    assert!(matches!(o, Outcome::Applied(..)), "{o:?}");
    assert!(s2.lock().unwrap().relay_open[5]);
    assert!(b.needs_sync(), "new connection must trigger a resync");
}

#[tokio::test]
async fn garbage_and_overlong_replies_are_errors() {
    let dir = TempDir::new();
    let l = UnixListener::bind(dir.sock()).unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let mut r = BufReader::new(s);
                let mut line = String::new();
                while r.read_line(&mut line).await.unwrap_or(0) > 0 {
                    let reply = if line.starts_with("state") {
                        vec![b'x'; 4096]
                    } else {
                        b"ok\x00open 1\n".to_vec()
                    };
                    let _ = r.get_mut().write_all(&reply).await;
                    line.clear();
                }
            });
        }
    });
    let mut c = ctl(dir.sock());
    assert!(c.exchange("state", ReplyKind::UntilEnd).await.is_err());
    let mqtt = RecMqtt::default();
    let mut b = Bridge::new(c, mqtt.clone());
    let o = b.on_message("rack/relay/1/set", b"open", false).await;
    assert!(matches!(o, Outcome::Rejected(..)), "{o:?}");
    assert!(mqtt.take().is_empty());
}

#[tokio::test]
async fn worker_retries_sync_until_controller_appears() {
    let dir = TempDir::new();
    let mqtt = RecMqtt::default();
    let b = Bridge::new(ctl(dir.sock()), mqtt.clone());
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let w = tokio::spawn(worker::run(b, rx, fast_timing()));
    tx.send(BrokerEvent::Connected).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    // Only the status so far: the socket is missing.
    assert_eq!(
        mqtt.take(),
        vec![("rack/sbc4/status".into(), "online".into())]
    );

    let (_s, _h) = start_fake(&dir.sock(), FakeState::default());
    let mut got = Vec::new();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        got.extend(mqtt.take());
        if got.len() >= 8 {
            break;
        }
    }
    assert_eq!(got.len(), 8, "{got:?}");

    // A command through the worker.
    tx.send(BrokerEvent::Message {
        topic: "rack/relay/3/set".into(),
        payload: b"open".to_vec(),
        retain: false,
        received: Instant::now(),
    })
    .unwrap();
    tx.send(BrokerEvent::Message {
        topic: "rack/relay/3/set".into(),
        payload: b"close".to_vec(),
        retain: true,
        received: Instant::now(),
    })
    .unwrap();
    drop(tx);
    w.await.unwrap();
    assert_eq!(
        mqtt.take(),
        vec![("rack/relay/3/state".into(), "open".into())]
    );
}

#[tokio::test]
async fn idle_controller_restart_is_caught_by_audit() {
    let dir = TempDir::new();
    let (s1, h1) = start_fake(&dir.sock(), FakeState::default());
    let mqtt = RecMqtt::default();
    let b = Bridge::new(ctl(dir.sock()), mqtt.clone());
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let w = tokio::spawn(worker::run(b, rx, fast_timing()));
    tx.send(BrokerEvent::Connected).unwrap();
    tx.send(BrokerEvent::Message {
        topic: "rack/relay/2/set".into(),
        payload: b"open".to_vec(),
        retain: false,
        received: Instant::now(),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(s1.lock().unwrap().relay_open[2]);
    let p = mqtt.take();
    assert_eq!(
        p.last().unwrap(),
        &("rack/relay/2/state".into(), "open".into())
    );

    // An unchanged audit publishes nothing.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(mqtt.take().is_empty());

    // Controller restarts (all relays closed) with no commands arriving.
    h1.abort();
    let _ = h1.await;
    let (_s2, _h2) = start_fake(&dir.sock(), FakeState::default());
    let mut got = Vec::new();
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        got.extend(mqtt.take());
        if got.contains(&("rack/relay/2/state".into(), "closed".into())) {
            break;
        }
    }
    assert!(
        got.contains(&("rack/relay/2/state".into(), "closed".into())),
        "{got:?}"
    );
    drop(tx);
    w.abort();
}

#[tokio::test]
async fn stale_commands_are_dropped() {
    let dir = TempDir::new();
    let (shared, _h) = start_fake(&dir.sock(), FakeState::default());
    let mqtt = RecMqtt::default();
    let b = Bridge::new(ctl(dir.sock()), mqtt.clone());
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let w = tokio::spawn(worker::run(b, rx, fast_timing()));
    tx.send(BrokerEvent::Message {
        topic: "rack/relay/1/set".into(),
        payload: b"open".to_vec(),
        retain: false,
        received: Instant::now() - Duration::from_secs(11),
    })
    .unwrap();
    drop(tx);
    w.await.unwrap();
    assert!(!shared.lock().unwrap().log.iter().any(|l| l == "open 1"));
}

#[tokio::test]
async fn timeout_is_not_retried() {
    let dir = TempDir::new();
    let l = UnixListener::bind(dir.sock()).unwrap();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let (s, _) = l.accept().await.unwrap();
            let seen = seen2.clone();
            tokio::spawn(async move {
                let mut r = BufReader::new(s);
                let mut line = String::new();
                while r.read_line(&mut line).await.unwrap_or(0) > 0 {
                    seen.lock().unwrap().push(line.trim_end().to_string());
                    if line.starts_with("open") {
                        // Hang: never reply to open.
                    } else {
                        let _ = r.get_mut().write_all(b"ok\n").await;
                    }
                    line.clear();
                }
            });
        }
    });
    let mut c = UnixController::new(dir.sock(), Duration::from_millis(200));
    // Establish the connection so the next request runs on a reused one.
    assert!(c.exchange("close 1", ReplyKind::Single).await.is_ok());
    assert!(c.exchange("open 1", ReplyKind::Single).await.is_err());
    assert_eq!(*seen.lock().unwrap(), vec!["close 1", "open 1"]);
}

#[tokio::test]
async fn extra_reply_lines_force_reconnect() {
    let dir = TempDir::new();
    let l = UnixListener::bind(dir.sock()).unwrap();
    let conns = Arc::new(AtomicU32::new(0));
    let conns2 = conns.clone();
    tokio::spawn(async move {
        loop {
            let (s, _) = l.accept().await.unwrap();
            conns2.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut r = BufReader::new(s);
                let mut line = String::new();
                while r.read_line(&mut line).await.unwrap_or(0) > 0 {
                    let v: Vec<&str> = line.trim_end().split(' ').collect();
                    // Reply with a spurious trailing line.
                    let reply = format!("ok {} {}\nend\n", v[0], v[1]);
                    let _ = r.get_mut().write_all(reply.as_bytes()).await;
                    line.clear();
                }
            });
        }
    });
    let mqtt = RecMqtt::default();
    let mut b = Bridge::new(ctl(dir.sock()), mqtt.clone());
    tokio::time::sleep(Duration::from_millis(20)).await;
    for (ch, payload, want) in [(1, &b"open"[..], "open"), (2, b"close", "closed")] {
        let o = b
            .on_message(&format!("rack/relay/{ch}/set"), payload, false)
            .await;
        assert!(matches!(o, Outcome::Applied(..)), "{o:?}");
        let p = mqtt.take();
        assert_eq!(
            p,
            vec![(format!("rack/relay/{ch}/state"), want.to_string())]
        );
    }
    assert_eq!(conns.load(Ordering::SeqCst), 2);
}
