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
use paho_mqtt as paho;
use relay_mqtt::config::parse_password;
use tokio::time::{timeout, Instant};

type Seen = BTreeMap<String, (String, bool)>;

struct Client {
    cli: paho::AsyncClient,
    rx: paho::AsyncReceiver<Option<paho::Message>>,
}

fn client(host: &str, dir: &Path, id: &str) -> Result<(Client, paho::ConnectOptions)> {
    let path = |f: &str| dir.join(f);
    let pw = parse_password(
        &std::fs::read_to_string(path("tester.password")).context("read tester.password")?,
    )?;
    let ssl = paho::SslOptionsBuilder::new()
        .trust_store(path("ca.crt"))?
        .key_store(path("tester.crt"))?
        .private_key(path("tester.key"))?
        .disable_default_trust_store(true)
        .enable_server_cert_auth(true)
        .verify(true)
        .finalize();
    let opts = paho::ConnectOptionsBuilder::with_mqtt_version(paho::MQTT_VERSION_3_1_1)
        .user_name("tester")
        .password(pw)
        .clean_session(true)
        .keep_alive_interval(Duration::from_secs(10))
        .ssl_options(ssl)
        .finalize();
    let mut cli = paho::CreateOptionsBuilder::new_v3()
        .server_uri(relay_mqtt::mqtt::ssl_uri(host, 8883))
        .client_id(id)
        .persistence(paho::PersistenceType::None)
        .create_client()?;
    let rx = cli.get_stream(256);
    Ok((Client { cli, rx }, opts))
}

/// Connect, retrying while the broker comes up.
async fn connect(c: &Client, opts: paho::ConnectOptions) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match c.cli.connect(opts.clone()).await {
            Ok(_) => return Ok(()),
            Err(e) if Instant::now() < deadline => {
                eprintln!("connect: {e}; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => bail!("could not connect: {e}"),
        }
    }
}

async fn publish(c: &Client, topic: &str, payload: &str, retained: bool) -> Result<()> {
    let m = paho::MessageBuilder::new()
        .topic(topic)
        .payload(payload)
        .qos(1)
        .retained(retained)
        .finalize();
    timeout(Duration::from_secs(10), c.cli.publish(m))
        .await
        .with_context(|| format!("publish {topic}: no PUBACK"))?
        .with_context(|| format!("publish {topic}"))?;
    Ok(())
}

/// Collect publishes for `dur`, or until `done(seen)` holds.
async fn collect(c: &Client, seen: &mut Seen, dur: Duration, done: impl Fn(&Seen) -> bool) {
    let deadline = Instant::now() + dur;
    while !done(seen) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        match timeout(left, c.rx.recv()).await {
            Err(_) => return,
            Ok(Ok(Some(m))) => {
                let v = String::from_utf8_lossy(m.payload()).into_owned();
                println!("  recv {} = {v:?} retain={}", m.topic(), m.retained());
                seen.insert(m.topic().to_string(), (v, m.retained()));
            }
            Ok(Ok(None)) => {
                eprintln!("connection lost");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Ok(Err(e)) => {
                eprintln!("stream: {e}");
                return;
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
    let (c, opts) = client(host, dir, "tester-seed")?;
    connect(&c, opts).await?;
    // Wait for the PUBACK.
    publish(&c, "rack/relay/5/set", "open", true).await?;
    println!("seeded retained rack/relay/5/set=open");
    Ok(())
}

async fn check(host: &str, dir: &Path) -> Result<()> {
    let (c, opts) = client(host, dir, "tester-check")?;
    connect(&c, opts).await?;
    c.cli.subscribe("rack/relay/+/state", 1).await?;
    c.cli.subscribe("rack/sbc4/status", 1).await?;

    println!("waiting for sidecar online + initial 8 states");
    let mut seen = Seen::new();
    collect(&c, &mut seen, Duration::from_secs(90), |s| {
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
    publish(&c, "rack/relay/9/set", "open", false).await?;
    publish(&c, "rack/relay/4/set", "OPEN", false).await?;
    publish(&c, "rack/relay/4/set", "open\n", false).await?;
    collect(&c, &mut seen, Duration::from_secs(3), |_| false).await;
    if !seen.is_empty() {
        bail!("invalid commands produced publishes: {seen:?}");
    }

    println!("sending rack/relay/3/set=open");
    publish(&c, "rack/relay/3/set", "open", false).await?;
    collect(&c, &mut seen, Duration::from_secs(15), |s| {
        val(s, "rack/relay/3/state") == Some("open")
    })
    .await;
    if val(&seen, "rack/relay/3/state") != Some("open") {
        bail!("no rack/relay/3/state=open: {seen:?}");
    }
    let _ = c.cli.disconnect(None).await;

    println!("verifying retained state on a fresh connection");
    let (c2, opts2) = client(host, dir, "tester-verify")?;
    connect(&c2, opts2).await?;
    c2.cli.subscribe("rack/relay/#", 1).await?;
    let mut ret = Seen::new();
    collect(&c2, &mut ret, Duration::from_secs(3), |_| false).await;
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
