//! Test-only MQTT client for the docker e2e (runs inside the compose network
//! as user `tester`). Never deploy this.
//!
//! usage: e2e-check seed|check <host> <dir>
//!   <dir> holds ca.crt, tester.crt, tester.key, tester.password
//!
//! seed:  publish a RETAINED `rack/relay/5/set` = `open` before the sidecar
//!        starts, so `check` can prove retained commands are ignored.
//! check: wait for `rack/sbc4/status`=online and all 8 retained states, send
//!        bad commands (ch 9, `OPEN`) and `rack/relay/3/set`=`open`, then
//!        verify on a fresh connection that only relay 3 changed.
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use relay_mqtt::config::parse_password;
use rumqttc::{
    AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS, TlsConfiguration, Transport,
};
use tokio::time::{timeout, Instant};

type Seen = BTreeMap<String, (String, bool)>;

fn client(host: &str, dir: &Path, id: &str) -> Result<(AsyncClient, EventLoop)> {
    let rd = |f: &str| std::fs::read(dir.join(f)).with_context(|| format!("read {f}"));
    let pw = parse_password(&String::from_utf8(rd("tester.password")?)?)?;
    let mut o = MqttOptions::new(id, host, 8883);
    o.set_credentials("tester", pw);
    o.set_clean_session(true);
    o.set_keep_alive(Duration::from_secs(10));
    o.set_transport(Transport::tls_with_config(TlsConfiguration::Simple {
        ca: rd("ca.crt")?,
        alpn: None,
        client_auth: Some((rd("tester.crt")?, rd("tester.key")?)),
    }));
    Ok(AsyncClient::new(o, 32))
}

/// Poll until connected, retrying while the broker comes up.
async fn connect(el: &mut EventLoop) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match el.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => return Ok(()),
            Ok(_) => {}
            Err(e) if Instant::now() < deadline => {
                eprintln!("connect: {e}; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => bail!("could not connect: {e}"),
        }
    }
}

/// Collect publishes for `dur`, or until `done(seen)` holds.
async fn collect(el: &mut EventLoop, seen: &mut Seen, dur: Duration, done: impl Fn(&Seen) -> bool) {
    let deadline = Instant::now() + dur;
    while !done(seen) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        match timeout(left, el.poll()).await {
            Err(_) => return,
            Ok(Ok(Event::Incoming(Packet::Publish(p)))) => {
                let v = String::from_utf8_lossy(&p.payload).into_owned();
                println!("  recv {} = {v:?} retain={}", p.topic, p.retain);
                seen.insert(p.topic, (v, p.retain));
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                eprintln!("poll: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

fn val<'a>(seen: &'a Seen, topic: &str) -> Option<&'a str> {
    seen.get(topic).map(|(v, _)| v.as_str())
}

fn all_states(seen: &Seen) -> bool {
    (0..8).all(|n| seen.contains_key(&format!("rack/relay/{n}/state")))
}

async fn seed(host: &str, dir: &Path) -> Result<()> {
    let (c, mut el) = client(host, dir, "tester-seed")?;
    connect(&mut el).await?;
    c.publish("rack/relay/5/set", QoS::AtLeastOnce, true, "open")
        .await?;
    // Drive the loop until the PUBACK.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match timeout(
            deadline.saturating_duration_since(Instant::now()),
            el.poll(),
        )
        .await
        {
            Ok(Ok(Event::Incoming(Packet::PubAck(_)))) => break,
            Ok(Ok(_)) => {}
            Ok(Err(e)) => bail!("seed publish: {e}"),
            Err(_) => bail!("seed publish not acknowledged"),
        }
    }
    println!("seeded retained rack/relay/5/set=open");
    Ok(())
}

async fn check(host: &str, dir: &Path) -> Result<()> {
    let (c, mut el) = client(host, dir, "tester-check")?;
    connect(&mut el).await?;
    c.subscribe("rack/relay/+/state", QoS::AtLeastOnce).await?;
    c.subscribe("rack/sbc4/status", QoS::AtLeastOnce).await?;

    println!("waiting for sidecar online + initial 8 states");
    let mut seen = Seen::new();
    collect(&mut el, &mut seen, Duration::from_secs(90), |s| {
        val(s, "rack/sbc4/status") == Some("online") && all_states(s)
    })
    .await;
    if val(&seen, "rack/sbc4/status") != Some("online") {
        bail!("rack/sbc4/status never became online: {seen:?}");
    }
    if !all_states(&seen) {
        bail!("initial relay states incomplete: {seen:?}");
    }
    for n in 0..8 {
        if val(&seen, &format!("rack/relay/{n}/state")) != Some("closed") {
            bail!("relay {n} should start closed (retained set must be ignored): {seen:?}");
        }
    }

    println!("sending invalid commands");
    seen.clear();
    c.publish("rack/relay/9/set", QoS::AtLeastOnce, false, "open")
        .await?;
    c.publish("rack/relay/4/set", QoS::AtLeastOnce, false, "OPEN")
        .await?;
    c.publish("rack/relay/4/set", QoS::AtLeastOnce, false, "open\n")
        .await?;
    collect(&mut el, &mut seen, Duration::from_secs(3), |_| false).await;
    if !seen.is_empty() {
        bail!("invalid commands produced publishes: {seen:?}");
    }

    println!("sending rack/relay/3/set=open");
    c.publish("rack/relay/3/set", QoS::AtLeastOnce, false, "open")
        .await?;
    collect(&mut el, &mut seen, Duration::from_secs(15), |s| {
        val(s, "rack/relay/3/state") == Some("open")
    })
    .await;
    if val(&seen, "rack/relay/3/state") != Some("open") {
        bail!("no rack/relay/3/state=open: {seen:?}");
    }
    let _ = c.disconnect().await;

    println!("verifying retained state on a fresh connection");
    let (c2, mut el2) = client(host, dir, "tester-verify")?;
    connect(&mut el2).await?;
    c2.subscribe("rack/relay/#", QoS::AtLeastOnce).await?;
    let mut ret = Seen::new();
    collect(&mut el2, &mut ret, Duration::from_secs(3), |_| false).await;
    match ret.get("rack/relay/3/state") {
        Some((v, true)) if v == "open" => {}
        other => bail!("rack/relay/3/state not retained open: {other:?}"),
    }
    if ret.contains_key("rack/relay/9/state") {
        bail!("rack/relay/9/state must not exist");
    }
    for n in [0, 1, 2, 4, 5, 6, 7] {
        if val(&ret, &format!("rack/relay/{n}/state")) != Some("closed") {
            bail!("relay {n} changed unexpectedly: {ret:?}");
        }
    }
    println!("E2E PASS");
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.as_slice() {
        [m, host, dir] if m == "seed" => seed(host, Path::new(dir)).await,
        [m, host, dir] if m == "check" => check(host, Path::new(dir)).await,
        _ => {
            eprintln!("usage: e2e-check seed|check <host> <dir>");
            return ExitCode::from(2);
        }
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("E2E FAIL: {e:#}");
            ExitCode::FAILURE
        }
    }
}
