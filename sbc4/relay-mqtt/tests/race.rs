//! Race-condition tests: concurrent controller clients, command bursts during
//! a state sync, a controller restart mid-command, and a broker reconnect
//! while a state publish is in flight. The broker is a fake behind the `Mqtt`
//! trait; the controller is the real Unix-socket fake (or a variant of it).

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
use common::{start_fake, TempDir};
use futures_util::{SinkExt, StreamExt};
use relay_mqtt::bridge::{Bridge, Mqtt};
use relay_mqtt::controller::{Controller, ReplyKind, StrictLines, UnixController};
use relay_mqtt::fake_controller::{bind, handle, FakeState, Shared};
use relay_mqtt::mqtt::BrokerEvent;
use relay_mqtt::worker;
use tokio::net::UnixListener;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::codec::Framed;

fn timing() -> worker::Timing {
    worker::Timing {
        min_retry: Duration::from_millis(20),
        max_retry: Duration::from_millis(100),
        audit_every: Duration::from_secs(60),
        max_command_age: Duration::from_secs(10),
    }
}

fn ctl(dir: &TempDir) -> UnixController {
    UnixController::new(dir.sock(), Duration::from_secs(2))
}

fn set(tx: &UnboundedSender<BrokerEvent>, ch: u8, payload: &str) {
    tx.send(BrokerEvent::Message {
        topic: format!("rack/relay/{ch}/set"),
        payload: payload.as_bytes().to_vec(),
        retain: false,
        received: Instant::now(),
    })
    .unwrap();
}

/// Relay `ch`'s commanded state at the controller ("open"/"closed").
fn fake_relay(fake: &Shared, ch: usize) -> &'static str {
    if fake.lock().unwrap().relay_open[ch] {
        "open"
    } else {
        "closed"
    }
}

/// Fake broker. At the instant of every relay-state publish it checks the
/// value against the controller's commanded state: a publish that runs ahead
/// of the controller's `ok` (or that names a value the controller never
/// acknowledged) is recorded as a violation. `fail_next` makes the next N
/// publishes fail after a delay, like a connection dropping mid-flight.
#[derive(Clone)]
struct CheckedMqtt {
    fake: Arc<Mutex<Option<Shared>>>,
    published: Arc<Mutex<Vec<(String, String)>>>,
    violations: Arc<Mutex<Vec<String>>>,
    fail_next: Arc<Mutex<usize>>,
    failed: Arc<Mutex<Vec<(String, String)>>>,
}

impl CheckedMqtt {
    fn new(fake: Option<Shared>) -> Self {
        CheckedMqtt {
            fake: Arc::new(Mutex::new(fake)),
            published: Default::default(),
            violations: Default::default(),
            fail_next: Default::default(),
            failed: Default::default(),
        }
    }
    fn watch(&self, fake: Shared) {
        *self.fake.lock().unwrap() = Some(fake);
    }
    fn published(&self) -> Vec<(String, String)> {
        self.published.lock().unwrap().clone()
    }
    fn relay_pubs(&self, ch: u8) -> Vec<String> {
        let t = format!("rack/relay/{ch}/state");
        self.published()
            .into_iter()
            .filter(|(topic, _)| *topic == t)
            .map(|(_, v)| v)
            .collect()
    }
    fn assert_clean(&self) {
        let v = self.violations.lock().unwrap();
        assert!(v.is_empty(), "state published without controller ok: {v:?}");
    }
}

impl Mqtt for CheckedMqtt {
    async fn publish_retained(&mut self, topic: &str, payload: &'static str) -> Result<()> {
        let fail = {
            let mut n = self.fail_next.lock().unwrap();
            let f = *n > 0;
            if f {
                *n -= 1;
            }
            f
        };
        if fail {
            // In flight for a while, then the connection is gone.
            tokio::time::sleep(Duration::from_millis(30)).await;
            self.failed
                .lock()
                .unwrap()
                .push((topic.to_string(), payload.to_string()));
            bail!("connection lost before PUBACK");
        }
        if let Some(ch) = topic
            .strip_prefix("rack/relay/")
            .and_then(|r| r.strip_suffix("/state"))
            .and_then(|c| c.parse::<usize>().ok())
        {
            if let Some(fake) = self.fake.lock().unwrap().as_ref() {
                let at_ctl = fake_relay(fake, ch);
                if at_ctl != payload {
                    self.violations
                        .lock()
                        .unwrap()
                        .push(format!("{topic}={payload} but controller has {at_ctl}"));
                }
            }
        }
        self.published
            .lock()
            .unwrap()
            .push((topic.to_string(), payload.to_string()));
        Ok(())
    }
    async fn subscribe(&mut self, _filter: &str) -> Result<()> {
        Ok(())
    }
}

/// Poll `cond` for up to 3 s.
async fn eventually(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..150 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    cond()
}

// (a) 8 tasks, each with its own UnixController connection to the same
// controller, hammer their own channel. Every reply must be the reply to that
// task's own request, and the final state must be each channel's last command.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_clients_get_their_own_replies() {
    let dir = TempDir::new();
    let (shared, _h) = start_fake(&dir.sock(), FakeState::default());
    const ROUNDS: usize = 60;
    let mut tasks = Vec::new();
    for ch in 0..8u8 {
        let mut c = ctl(&dir);
        tasks.push(tokio::spawn(async move {
            let mut last = "";
            for i in 0..ROUNDS + usize::from(ch) {
                // A different pattern per channel, so the final states differ.
                let verb = if (i + usize::from(ch)) % 3 == 0 {
                    "open"
                } else {
                    "close"
                };
                let req = format!("{verb} {ch}");
                let reply = c.exchange(&req, ReplyKind::Single).await.unwrap();
                assert_eq!(reply, vec![format!("ok {req}")], "cross-talk on ch {ch}");
                if i % 10 == 0 {
                    // Interleave multi-line replies too.
                    let st = c.exchange("state", ReplyKind::UntilEnd).await.unwrap();
                    assert_eq!(st.last().map(String::as_str), Some("end"));
                    assert_eq!(st.len(), 17);
                }
                last = verb;
            }
            (ch, last)
        }));
    }
    for t in tasks {
        let (ch, last) = t.await.unwrap();
        let want = last == "open";
        assert_eq!(
            shared.lock().unwrap().relay_open[usize::from(ch)],
            want,
            "ch {ch} final state must be its last command ({last})"
        );
    }
}

/// A controller whose `state` reply is held until `gate` is notified.
fn start_gated(dir: &TempDir, gate: Arc<Notify>, state_seen: Arc<Notify>) -> Shared {
    let shared: Shared = Default::default();
    let l: UnixListener = bind(&dir.sock()).unwrap();
    let s = shared.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = l.accept().await.unwrap();
            let (s, gate, seen) = (s.clone(), gate.clone(), state_seen.clone());
            tokio::spawn(async move {
                let mut conn = Framed::new(stream, StrictLines::new(128));
                while let Some(Ok(line)) = conn.next().await {
                    if line == "state" {
                        seen.notify_one();
                        gate.notified().await;
                    }
                    for r in handle(&s, &line) {
                        conn.feed(r).await.unwrap();
                    }
                    SinkExt::<String>::flush(&mut conn).await.unwrap();
                }
            });
        }
    });
    shared
}

// (b) A burst of set messages arrives while the connect-time `state` sync is
// waiting on the controller. Nothing may be published before its `ok`, and no
// command may be lost or reordered.
#[tokio::test]
async fn burst_during_state_sync() {
    let dir = TempDir::new();
    let gate = Arc::new(Notify::new());
    let state_seen = Arc::new(Notify::new());
    let fake = start_gated(&dir, gate.clone(), state_seen.clone());
    let mqtt = CheckedMqtt::new(Some(fake.clone()));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let w = tokio::spawn(worker::run(
        Bridge::new(ctl(&dir), mqtt.clone()),
        rx,
        timing(),
    ));

    tx.send(BrokerEvent::Connected).unwrap();
    state_seen.notified().await; // the sync is now blocked at the controller
    let mut sent = Vec::new();
    for i in 0..24u8 {
        let ch = i % 8;
        let p = if !(8..16).contains(&i) {
            "open"
        } else {
            "close"
        };
        set(&tx, ch, p);
        sent.push(format!("{p} {ch}"));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Still blocked: nothing but the status may be out.
    assert_eq!(
        mqtt.published(),
        vec![("rack/sbc4/status".to_string(), "online".to_string())]
    );
    // (The gated `state` is logged only once released.) No command may be
    // interleaved into the pending `state` exchange.
    assert!(fake.lock().unwrap().log.is_empty());
    gate.notify_one();

    assert!(
        eventually(|| fake.lock().unwrap().log.len() == 1 + sent.len()).await,
        "{:?}",
        fake.lock().unwrap().log
    );
    drop(tx);
    w.await.unwrap();

    // Every command reached the controller once, in order.
    assert_eq!(fake.lock().unwrap().log[1..], sent[..]);
    mqtt.assert_clean();
    for ch in 0..8u8 {
        // Sync (closed), then open, close, open: the final retained value is
        // the last command, and each value was published in order.
        assert_eq!(
            mqtt.relay_pubs(ch),
            vec!["closed", "open", "closed", "open"],
            "ch {ch}"
        );
        assert_eq!(fake_relay(&fake, usize::from(ch)), "open");
    }
}

// (c) The controller dies while processing `open 3` (no reply) and comes back
// later with every relay reset. No `open` may be published for that command,
// and the sidecar re-syncs all 8 states once the socket is back.
#[tokio::test]
async fn controller_restart_mid_command() {
    let dir = TempDir::new();
    let first: Shared = Default::default();
    let l = bind(&dir.sock()).unwrap();
    let s1 = first.clone();
    let died = Arc::new(Notify::new());
    let died2 = died.clone();
    // Serves normally until it receives `open 3`, then exits: listener and
    // connection close without a reply, like a crash mid-command.
    tokio::spawn(async move {
        let (stream, _) = l.accept().await.unwrap();
        let mut conn = Framed::new(stream, StrictLines::new(128));
        while let Some(Ok(line)) = conn.next().await {
            if line == "open 3" {
                s1.lock().unwrap().log.push(line);
                break;
            }
            for r in handle(&s1, &line) {
                conn.feed(r).await.unwrap();
            }
            SinkExt::<String>::flush(&mut conn).await.unwrap();
        }
        drop(conn);
        drop(l);
        died2.notify_one();
    });

    let mqtt = CheckedMqtt::new(Some(first.clone()));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let w = tokio::spawn(worker::run(
        Bridge::new(ctl(&dir), mqtt.clone()),
        rx,
        timing(),
    ));
    tx.send(BrokerEvent::Connected).unwrap();
    assert!(eventually(|| mqtt.published().len() == 9).await);

    set(&tx, 3, "open");
    died.notified().await;
    // Down for a while: the retry-once hits a refused socket; resync retries.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(mqtt.published().len(), 9, "nothing published while down");

    let (second, _h) = start_fake(&dir.sock(), FakeState::default());
    mqtt.watch(second.clone());
    assert!(
        eventually(|| mqtt.published().len() >= 17).await,
        "{:?}",
        mqtt.published()
    );
    drop(tx);
    w.await.unwrap();

    mqtt.assert_clean();
    assert!(
        !mqtt.relay_pubs(3).contains(&"open".to_string()),
        "unacknowledged open published: {:?}",
        mqtt.published()
    );
    // The re-sync after the restart republished all 8 from the new controller.
    let resync = &mqtt.published()[9..17];
    for (n, (t, v)) in resync.iter().enumerate() {
        assert_eq!(t, &format!("rack/relay/{n}/state"));
        assert_eq!(v, "closed");
    }
    // The command never reached the new controller (dead connection, no reply,
    // retry refused): it is dropped, not replayed later.
    assert_eq!(second.lock().unwrap().log, vec!["state"]);
}

// (d) The broker connection drops while the state publish for an acknowledged
// command is in flight. After the reconnect the state is published (from the
// controller's own `state`), and nothing unacknowledged is ever published.
#[tokio::test]
async fn broker_reconnect_during_state_publish() {
    let dir = TempDir::new();
    let (fake, _h) = start_fake(&dir.sock(), FakeState::default());
    let mqtt = CheckedMqtt::new(Some(fake.clone()));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    // Slow resync retries, so the reconnect (not the retry timer) does it.
    let t = worker::Timing {
        min_retry: Duration::from_secs(5),
        max_retry: Duration::from_secs(5),
        ..timing()
    };
    let w = tokio::spawn(worker::run(Bridge::new(ctl(&dir), mqtt.clone()), rx, t));
    tx.send(BrokerEvent::Connected).unwrap();
    assert!(eventually(|| mqtt.published().len() == 9).await);

    // The next publish (relay 3's state after `ok open 3`) is lost in flight.
    *mqtt.fail_next.lock().unwrap() = 1;
    set(&tx, 3, "open");
    // A command for another relay is queued behind it and must still apply.
    set(&tx, 5, "open");
    assert!(eventually(|| mqtt.relay_pubs(5).contains(&"open".to_string())).await);
    assert_eq!(
        *mqtt.failed.lock().unwrap(),
        vec![("rack/relay/3/state".to_string(), "open".to_string())]
    );
    assert_eq!(mqtt.relay_pubs(3), vec!["closed"], "not yet republished");

    // Broker reconnect.
    tx.send(BrokerEvent::Connected).unwrap();
    assert!(
        eventually(|| mqtt.relay_pubs(3).last().map(String::as_str) == Some("open")).await,
        "{:?}",
        mqtt.published()
    );
    drop(tx);
    w.await.unwrap();
    mqtt.assert_clean();
    let p = mqtt.published();
    let online = p.iter().filter(|(t, _)| t == "rack/sbc4/status").count();
    assert_eq!(online, 2, "online on each connect");
    assert_eq!(mqtt.relay_pubs(5).last().map(String::as_str), Some("open"));
}

// (d') Same, but the publish of an `ok`-ed command fails and the broker comes
// back without a CONNACK reaching the worker yet: the retry timer's resync
// publishes it once the broker accepts publishes again.
#[tokio::test]
async fn failed_state_publish_is_resynced_by_timer() {
    let dir = TempDir::new();
    let (fake, _h) = start_fake(&dir.sock(), FakeState::default());
    let mqtt = CheckedMqtt::new(Some(fake.clone()));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let w = tokio::spawn(worker::run(
        Bridge::new(ctl(&dir), mqtt.clone()),
        rx,
        timing(),
    ));
    tx.send(BrokerEvent::Connected).unwrap();
    assert!(eventually(|| mqtt.published().len() == 9).await);
    // The state publish and the first resync attempt's publish both fail.
    *mqtt.fail_next.lock().unwrap() = 2;
    set(&tx, 6, "open");
    assert!(eventually(|| mqtt.relay_pubs(6).last().map(String::as_str) == Some("open")).await);
    drop(tx);
    w.await.unwrap();
    mqtt.assert_clean();
    assert_eq!(mqtt.failed.lock().unwrap().len(), 2);
}
