//! Test-only fault-injection peer for the real Paho client on 127.0.0.1.
//! Normal broker behaviour is tested against `rumqttd`; this exists only for
//! what a real broker won't do on cue: drop the link while our QoS 1 publish
//! is unacknowledged, drop a link to force a reconnect, replay a retained
//! message after a specific reconnect, withhold PUBACKs, and expose the raw
//! CONNECT fields (will, clean session, credentials). Just enough MQTT 3.1.1
//! framing for that; no routing, no TLS.

use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ev {
    Connect {
        conn: usize,
        client_id: String,
        username: Option<String>,
        password: Option<Vec<u8>>,
        clean: bool,
        /// (topic, payload, qos, retain)
        will: Option<(String, Vec<u8>, u8, bool)>,
        keep_alive: u16,
    },
    Subscribe {
        conn: usize,
        topics: Vec<(String, u8)>,
    },
    Publish {
        conn: usize,
        topic: String,
        payload: Vec<u8>,
        qos: u8,
        retain: bool,
        acked: bool,
    },
    Disconnect {
        conn: usize,
    },
}

enum Cmd {
    Publish {
        topic: String,
        payload: Vec<u8>,
        qos: u8,
        retain: bool,
    },
    Close,
}

#[derive(Default)]
struct Behaviour {
    /// Drop the connection (no PUBACK) when a PUBLISH with this payload arrives.
    drop_on: Option<Vec<u8>>,
    /// After each SUBACK, send this retained message (topic, payload).
    retained_on_subscribe: Option<(String, Vec<u8>)>,
    /// Record PUBLISHes but never PUBACK them.
    withhold_acks: bool,
    /// Answer every SUBSCRIBE with SUBACK failure codes (0x80), like an ACL.
    reject_subscribe: bool,
}

pub struct Stub {
    pub port: u16,
    pub events: Arc<Mutex<Vec<Ev>>>,
    conns: Arc<Mutex<Vec<mpsc::UnboundedSender<Cmd>>>>,
    behaviour: Arc<Mutex<Behaviour>>,
}

impl Stub {
    pub async fn start() -> Stub {
        Self::serve(TcpListener::bind("127.0.0.1:0").await.unwrap())
    }

    /// A port nothing listens on (yet), for brokers that must bind by
    /// address (rumqttd) or start late. Ports come from a per-process
    /// sequence below the OS ephemeral range, so parallel tests never get the
    /// same one, and `start()`'s port-0 binds (ephemeral) can't take it.
    pub fn free_port() -> u16 {
        static NEXT: AtomicU16 = AtomicU16::new(0);
        let base = 20000 + (std::process::id() % 500) as u16 * 32;
        loop {
            let p = base + NEXT.fetch_add(1, Ordering::SeqCst) % 8000;
            if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
                return p;
            }
        }
    }

    fn serve(listener: TcpListener) -> Stub {
        let port = listener.local_addr().unwrap().port();
        let events: Arc<Mutex<Vec<Ev>>> = Arc::default();
        let conns: Arc<Mutex<Vec<mpsc::UnboundedSender<Cmd>>>> = Arc::default();
        let behaviour: Arc<Mutex<Behaviour>> = Arc::default();
        let (e, c, b) = (events.clone(), conns.clone(), behaviour.clone());
        tokio::spawn(async move {
            let next = AtomicUsize::new(0);
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    return;
                };
                let id = next.fetch_add(1, Ordering::SeqCst);
                let (tx, rx) = mpsc::unbounded_channel();
                c.lock().unwrap().push(tx);
                tokio::spawn(conn_task(id, sock, rx, e.clone(), b.clone()));
            }
        });
        Stub {
            port,
            events,
            conns,
            behaviour,
        }
    }

    pub fn publish(&self, conn: usize, topic: &str, payload: &[u8], qos: u8, retain: bool) {
        let _ = self.conns.lock().unwrap()[conn].send(Cmd::Publish {
            topic: topic.into(),
            payload: payload.to_vec(),
            qos,
            retain,
        });
    }

    pub fn drop_conn(&self, conn: usize) {
        let _ = self.conns.lock().unwrap()[conn].send(Cmd::Close);
    }

    pub fn set_drop_on(&self, payload: Option<Vec<u8>>) {
        self.behaviour.lock().unwrap().drop_on = payload;
    }

    pub fn set_reject_subscribe(&self, on: bool) {
        self.behaviour.lock().unwrap().reject_subscribe = on;
    }

    pub fn set_withhold_acks(&self, on: bool) {
        self.behaviour.lock().unwrap().withhold_acks = on;
    }

    pub fn set_retained_on_subscribe(&self, m: Option<(String, Vec<u8>)>) {
        self.behaviour.lock().unwrap().retained_on_subscribe = m;
    }

    /// Waits (max 20 s) until `pred` holds for the recorded events.
    pub async fn wait_for(&self, pred: impl Fn(&[Ev]) -> bool) {
        for _ in 0..2000 {
            if pred(&self.events.lock().unwrap()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "stub: condition not reached; events: {:?}",
            self.events.lock().unwrap()
        );
    }
}

fn packet(first: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![first];
    let mut len = body.len();
    loop {
        let mut b = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            b |= 0x80;
        }
        v.push(b);
        if len == 0 {
            break;
        }
    }
    v.extend_from_slice(body);
    v
}

fn put_str(v: &mut Vec<u8>, s: &[u8]) {
    v.extend_from_slice(&(s.len() as u16).to_be_bytes());
    v.extend_from_slice(s);
}

fn publish_packet(topic: &str, payload: &[u8], qos: u8, retain: bool, pid: u16) -> Vec<u8> {
    let mut body = Vec::new();
    put_str(&mut body, topic.as_bytes());
    if qos > 0 {
        body.extend_from_slice(&pid.to_be_bytes());
    }
    body.extend_from_slice(payload);
    packet(0x30 | (qos << 1) | u8::from(retain), &body)
}

struct Cur<'a>(&'a [u8]);
impl Cur<'_> {
    fn u8(&mut self) -> u8 {
        let b = self.0[0];
        self.0 = &self.0[1..];
        b
    }
    fn u16(&mut self) -> u16 {
        let v = u16::from_be_bytes([self.0[0], self.0[1]]);
        self.0 = &self.0[2..];
        v
    }
    fn bytes(&mut self) -> Vec<u8> {
        let n = self.u16() as usize;
        let v = self.0[..n].to_vec();
        self.0 = &self.0[n..];
        v
    }
    fn string(&mut self) -> String {
        String::from_utf8(self.bytes()).unwrap()
    }
}

async fn read_packet(rd: &mut tokio::net::tcp::OwnedReadHalf) -> Option<(u8, Vec<u8>)> {
    let first = rd.read_u8().await.ok()?;
    let mut len = 0usize;
    let mut mult = 1usize;
    loop {
        let b = rd.read_u8().await.ok()?;
        len += (b & 0x7f) as usize * mult;
        mult *= 128;
        if b & 0x80 == 0 {
            break;
        }
    }
    let mut body = vec![0; len];
    rd.read_exact(&mut body).await.ok()?;
    Some((first, body))
}

async fn conn_task(
    id: usize,
    sock: TcpStream,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    events: Arc<Mutex<Vec<Ev>>>,
    behaviour: Arc<Mutex<Behaviour>>,
) {
    let (mut rd, mut wr) = sock.into_split();
    let (pkt_tx, mut pkts) = mpsc::unbounded_channel();
    let reader = tokio::spawn(async move {
        while let Some(p) = read_packet(&mut rd).await {
            if pkt_tx.send(p).is_err() {
                return;
            }
        }
    });
    let record = |e: Ev| events.lock().unwrap().push(e);
    let mut pid: u16 = 0;
    loop {
        let out: Vec<u8> = tokio::select! {
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Publish { topic, payload, qos, retain }) => {
                    pid = pid.wrapping_add(1).max(1);
                    publish_packet(&topic, &payload, qos, retain, pid)
                }
                Some(Cmd::Close) | None => break,
            },
            p = pkts.recv() => {
                let Some((first, body)) = p else { break };
                let mut c = Cur(&body);
                match first >> 4 {
                    1 => {
                        let _proto = c.bytes();
                        let _level = c.u8();
                        let flags = c.u8();
                        let keep_alive = c.u16();
                        let client_id = c.string();
                        let will = (flags & 0x04 != 0).then(|| {
                            let t = c.string();
                            let m = c.bytes();
                            (t, m, (flags >> 3) & 3, flags & 0x20 != 0)
                        });
                        let username = (flags & 0x80 != 0).then(|| c.string());
                        let password = (flags & 0x40 != 0).then(|| c.bytes());
                        record(Ev::Connect {
                            conn: id,
                            client_id,
                            username,
                            password,
                            clean: flags & 0x02 != 0,
                            will,
                            keep_alive,
                        });
                        packet(0x20, &[0, 0])
                    }
                    8 => {
                        let sub_pid = c.u16();
                        let mut topics = Vec::new();
                        while !c.0.is_empty() {
                            let t = c.string();
                            topics.push((t, c.u8()));
                        }
                        let mut body = sub_pid.to_be_bytes().to_vec();
                        let reject = behaviour.lock().unwrap().reject_subscribe;
                        body.extend(topics.iter().map(|(_, q)| if reject { 0x80 } else { *q }));
                        record(Ev::Subscribe { conn: id, topics });
                        let mut out = packet(0x90, &body);
                        let replay = behaviour.lock().unwrap().retained_on_subscribe.clone();
                        if let Some((t, m)) = replay {
                            pid = pid.wrapping_add(1).max(1);
                            out.extend(publish_packet(&t, &m, 1, true, pid));
                        }
                        out
                    }
                    3 => {
                        let qos = (first >> 1) & 3;
                        let retain = first & 1 != 0;
                        let topic = c.string();
                        let p = if qos > 0 { c.u16() } else { 0 };
                        let payload = c.0.to_vec();
                        let (drop_it, withhold) = {
                            let b = behaviour.lock().unwrap();
                            (b.drop_on.as_deref() == Some(&payload[..]), b.withhold_acks)
                        };
                        record(Ev::Publish {
                            conn: id,
                            topic,
                            payload,
                            qos,
                            retain,
                            acked: qos > 0 && !drop_it && !withhold,
                        });
                        if drop_it {
                            break;
                        }
                        if qos == 1 && !withhold {
                            packet(0x40, &p.to_be_bytes())
                        } else {
                            continue;
                        }
                    }
                    12 => packet(0xD0, &[]),
                    14 => {
                        record(Ev::Disconnect { conn: id });
                        break;
                    }
                    _ => continue, // PUBACK from the client etc.
                }
            }
        };
        if wr.write_all(&out).await.is_err() {
            break;
        }
    }
    reader.abort();
}
