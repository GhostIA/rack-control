//! E2E driver for rack-agent. Runs on the dev box, drives `docker compose`
//! with fixed argv, and talks to the broker on 127.0.0.1:18883 as `tester`.
//!
//! Asserts:
//!  1. ACL: `intel-test` cannot receive another PC's cmd topic.
//!  2. A retained `shutdown` published before the agent connects is ignored.
//!  3. The agent publishes retained `online` on connect.
//!  4. `shutdown` on another PC's topic does nothing.
//!  5. `Shutdown`, `shutdown\n`, `shutdown now`, `` are ignored.
//!  6. `shutdown` -> retained `down`, then the container exits 0 (dry_run).
//!
//! Everything it generates (CA, certs, keys, passwd) is test-only and lives in
//! the gitignored `e2e/work/`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use rand::{distributions::Alphanumeric, Rng, RngCore};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use rumqttc::{
    AsyncClient, Event, Incoming, MqttOptions, QoS, SubscribeReasonCode, TlsConfiguration,
    Transport,
};
use tokio::sync::mpsc;
use tokio::time::{timeout, Instant};

const PROJECT: &str = "rack-agent-e2e";
const AGENT_CONTAINER: &str = "rack-agent-e2e-agent";
const PC: &str = "intel-test";
const OTHER: &str = "intel-other";
const BROKER_PORT: u16 = 18883;

struct Pki {
    ca_pem: String,
    tester: (String, String),
    pc: (String, String),
}

fn e2e_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn random_password() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(24)
        .map(char::from)
        .collect()
}

/// Mosquitto 2.x `$7$` line: PBKDF2-HMAC-SHA512, 101 iterations, 12-byte salt.
fn mosquitto_passwd_line(user: &str, password: &str) -> String {
    let mut salt = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut salt);
    let mut hash = [0u8; 64];
    pbkdf2::pbkdf2_hmac::<sha2::Sha512>(password.as_bytes(), &salt, 101, &mut hash);
    format!("{user}:$7$101${}${}", B64.encode(salt), B64.encode(hash))
}

fn leaf(
    cn: &str,
    sans: Vec<String>,
    eku: ExtendedKeyUsagePurpose,
    ca: &rcgen::Certificate,
    ca_key: &KeyPair,
) -> Result<(String, String)> {
    let key = KeyPair::generate()?;
    let mut p = CertificateParams::new(sans)?;
    p.distinguished_name.push(DnType::CommonName, cn);
    p.extended_key_usages = vec![eku];
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let cert = p.signed_by(&key, ca, ca_key)?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// Writes with an explicit 0644 (test-only material) regardless of umask, so
/// the container users (65534, mosquitto) can read the bind mounts.
fn write_file(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = path.as_ref();
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
    Ok(())
}

fn generate(work: &Path) -> Result<(Pki, String, String)> {
    let ca_key = KeyPair::generate()?;
    let mut p = CertificateParams::new(Vec::<String>::new())?;
    p.distinguished_name
        .push(DnType::CommonName, "rack-agent-e2e test CA");
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca = p.self_signed(&ca_key)?;
    let server = leaf(
        "mosquitto",
        vec!["mosquitto".into(), "127.0.0.1".into()],
        ExtendedKeyUsagePurpose::ServerAuth,
        &ca,
        &ca_key,
    )?;
    let tester = leaf(
        "tester",
        vec![],
        ExtendedKeyUsagePurpose::ClientAuth,
        &ca,
        &ca_key,
    )?;
    let pc = leaf(
        PC,
        vec![],
        ExtendedKeyUsagePurpose::ClientAuth,
        &ca,
        &ca_key,
    )?;

    let tester_pw = random_password();
    let pc_pw = random_password();

    let m = work.join("mosquitto");
    let a = work.join("agent");
    for d in [&m, &a] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(d)?;
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755))?;
    }
    // Test-only material; 0644 so the container users can read the bind mount.
    write_file(m.join("ca.crt"), ca.pem())?;
    write_file(m.join("server.crt"), &server.0)?;
    write_file(m.join("server.key"), &server.1)?;
    write_file(
        m.join("passwd"),
        format!(
            "{}\n{}\n",
            mosquitto_passwd_line("tester", &tester_pw),
            mosquitto_passwd_line(PC, &pc_pw)
        ),
    )?;
    write_file(
        m.join("acl"),
        format!(
            "user {PC}\n\
             topic read rack/pc/{PC}/cmd\n\
             topic write rack/pc/{PC}/status\n\
             \n\
             user tester\n\
             topic readwrite rack/#\n"
        ),
    )?;
    write_file(
        m.join("mosquitto.conf"),
        "per_listener_settings false\n\
         allow_anonymous false\n\
         password_file /mosquitto/config/passwd\n\
         acl_file /mosquitto/config/acl\n\
         persistence false\n\
         log_dest stdout\n\
         log_type all\n\
         listener 8883\n\
         cafile /mosquitto/config/ca.crt\n\
         certfile /mosquitto/config/server.crt\n\
         keyfile /mosquitto/config/server.key\n\
         require_certificate true\n\
         use_identity_as_username false\n\
         tls_version tlsv1.2\n",
    )?;

    write_file(a.join("ca.crt"), ca.pem())?;
    write_file(a.join(format!("{PC}.crt")), &pc.0)?;
    write_file(a.join(format!("{PC}.key")), &pc.1)?;
    write_file(a.join(format!("{PC}.password")), format!("{pc_pw}\n"))?;
    let d = "/etc/rack-agent-e2e";
    write_file(
        a.join("config.toml"),
        format!(
            "name = \"{PC}\"\n\
             broker_host = \"mosquitto\"\n\
             broker_port = 8883\n\
             ca = \"{d}/ca.crt\"\n\
             cert = \"{d}/{PC}.crt\"\n\
             key = \"{d}/{PC}.key\"\n\
             password_file = \"{d}/{PC}.password\"\n\
             dry_run = true\n"
        ),
    )?;
    Ok((
        Pki {
            ca_pem: ca.pem(),
            tester,
            pc,
        },
        tester_pw,
        pc_pw,
    ))
}

fn compose(args: &[&str]) -> Result<String> {
    let file = e2e_dir().join("compose.yaml");
    let mut argv = vec![
        "compose",
        "-p",
        PROJECT,
        "-f",
        file.to_str().context("path")?,
    ];
    argv.extend_from_slice(args);
    docker(&argv)
}

fn docker(args: &[&str]) -> Result<String> {
    let out = Command::new("docker")
        .args(args)
        .output()
        .with_context(|| format!("running docker {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "docker {} failed: {}\n{}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[derive(Debug, Clone)]
struct Msg {
    topic: String,
    payload: Vec<u8>,
    retain: bool,
}

struct Client {
    client: AsyncClient,
    rx: mpsc::UnboundedReceiver<Msg>,
    subacks: mpsc::UnboundedReceiver<Vec<SubscribeReasonCode>>,
}

async fn connect(
    pki: &Pki,
    user: &str,
    client_id: &str,
    password: &str,
    cert: &(String, String),
) -> Result<Client> {
    let mut o = MqttOptions::new(client_id, "127.0.0.1", BROKER_PORT);
    o.set_credentials(user, password);
    o.set_clean_session(true);
    o.set_keep_alive(Duration::from_secs(10));
    o.set_transport(Transport::tls_with_config(TlsConfiguration::Simple {
        ca: pki.ca_pem.clone().into_bytes(),
        alpn: None,
        client_auth: Some((cert.0.clone().into_bytes(), cert.1.clone().into_bytes())),
    }));
    let (client, mut el) = AsyncClient::new(o, 32);
    let (tx, rx) = mpsc::unbounded_channel();
    let (stx, subacks) = mpsc::unbounded_channel();
    let (ctx, mut crx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            match el.poll().await {
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    let _ = ctx.send(Ok(()));
                }
                Ok(Event::Incoming(Incoming::Publish(p))) => {
                    let _ = tx.send(Msg {
                        topic: p.topic,
                        payload: p.payload.to_vec(),
                        retain: p.retain,
                    });
                }
                Ok(Event::Incoming(Incoming::SubAck(s))) => {
                    let _ = stx.send(s.return_codes);
                }
                Ok(_) => {}
                Err(e) => {
                    if ctx.send(Err(e.to_string())).is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    });
    match timeout(Duration::from_secs(5), crx.recv()).await {
        Ok(Some(Ok(()))) => Ok(Client {
            client,
            rx,
            subacks,
        }),
        Ok(Some(Err(e))) => bail!("{user} connect failed: {e}"),
        _ => bail!("{user} connect timed out"),
    }
}

impl Client {
    async fn subscribe(&mut self, topic: &str) -> Result<Vec<SubscribeReasonCode>> {
        self.client.subscribe(topic, QoS::AtLeastOnce).await?;
        timeout(Duration::from_secs(5), self.subacks.recv())
            .await
            .context("suback timeout")?
            .context("client gone")
    }

    async fn publish(&self, topic: &str, payload: &[u8], retain: bool) -> Result<()> {
        self.client
            .publish(topic, QoS::AtLeastOnce, retain, payload.to_vec())
            .await?;
        Ok(())
    }

    /// Waits for `payload` on `topic`; returns the message.
    async fn expect(&mut self, topic: &str, payload: &[u8], within: Duration) -> Result<Msg> {
        let deadline = Instant::now() + within;
        loop {
            let m = tokio::time::timeout_at(deadline, self.rx.recv())
                .await
                .with_context(|| {
                    format!(
                        "timed out waiting for {topic}={}",
                        String::from_utf8_lossy(payload)
                    )
                })?
                .context("client gone")?;
            if m.topic == topic && m.payload == payload {
                return Ok(m);
            }
            println!(
                "    (saw {} = {:?})",
                m.topic,
                String::from_utf8_lossy(&m.payload)
            );
        }
    }

    /// Fails if anything arrives on `topic` within `dur`.
    async fn expect_silence(&mut self, topic: &str, dur: Duration) -> Result<()> {
        let deadline = Instant::now() + dur;
        while let Ok(m) = tokio::time::timeout_at(deadline, self.rx.recv()).await {
            let m = m.context("client gone")?;
            ensure!(
                m.topic != topic,
                "unexpected {} = {:?}",
                m.topic,
                String::from_utf8_lossy(&m.payload)
            );
        }
        Ok(())
    }
}

fn agent_state() -> Result<(String, i64)> {
    let out = docker(&[
        "inspect",
        "-f",
        "{{.State.Status}} {{.State.ExitCode}}",
        AGENT_CONTAINER,
    ])?;
    let mut it = out.split_whitespace();
    let status = it.next().unwrap_or_default().to_string();
    let code = it.next().and_then(|c| c.parse().ok()).unwrap_or(-1);
    Ok((status, code))
}

async fn wait_broker(pki: &Pki, pw: &str) -> Result<Client> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match connect(pki, "tester", "tester", pw, &pki.tester).await {
            Ok(c) => return Ok(c),
            Err(e) if Instant::now() < deadline => {
                println!("    broker not ready: {e:#}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn scenario(pki: &Pki, tester_pw: &str, pc_pw: &str) -> Result<()> {
    let status = format!("rack/pc/{PC}/status");
    let cmd = format!("rack/pc/{PC}/cmd");
    let other_cmd = format!("rack/pc/{OTHER}/cmd");

    println!("==> waiting for broker");
    let mut tester = wait_broker(pki, tester_pw).await?;
    tester.subscribe("rack/pc/+/status").await?;

    println!("==> [1] ACL: {PC} must not receive {other_cmd}");
    {
        let mut probe = connect(pki, PC, "intel-test-probe", pc_pw, &pki.pc).await?;
        let codes = probe.subscribe(&other_cmd).await?;
        println!("    suback for {other_cmd}: {codes:?}");
        tester.publish(&other_cmd, b"shutdown", false).await?;
        probe
            .expect_silence(&other_cmd, Duration::from_secs(2))
            .await?;
        probe.client.disconnect().await?;
    }

    println!("==> [2] publishing RETAINED shutdown to {cmd} before the agent starts");
    tester.publish(&cmd, b"shutdown", true).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    println!("==> starting agent container");
    compose(&["up", "-d", "--no-build", "agent"])?;

    println!("==> [3] expecting {status}=online");
    tester
        .expect(&status, b"online", Duration::from_secs(30))
        .await?;
    println!("    online seen; checking the retained shutdown is ignored");
    tester
        .expect_silence(&status, Duration::from_secs(3))
        .await?;
    ensure!(
        agent_state()?.0 == "running",
        "agent exited after retained shutdown"
    );
    // Clear the retained command so it can't confuse later checks.
    tester.publish(&cmd, b"", true).await?;

    println!("==> [4] shutdown to another PC ({other_cmd}) must do nothing");
    tester.publish(&other_cmd, b"shutdown", false).await?;
    tester
        .expect_silence(&status, Duration::from_secs(2))
        .await?;

    println!("==> [5] non-exact payloads must be ignored");
    for p in [&b"Shutdown"[..], b"shutdown\n", b"shutdown now", b""] {
        tester.publish(&cmd, p, false).await?;
    }
    tester
        .expect_silence(&status, Duration::from_secs(3))
        .await?;
    ensure!(
        agent_state()?.0 == "running",
        "agent exited on a non-exact payload"
    );

    println!("==> [6] shutdown -> down, then exit 0");
    tester.publish(&cmd, b"shutdown", false).await?;
    tester
        .expect(&status, b"down", Duration::from_secs(10))
        .await?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (s, code) = agent_state()?;
        if s == "exited" {
            ensure!(code == 0, "agent exited with {code}");
            break;
        }
        ensure!(Instant::now() < deadline, "agent still {s} after shutdown");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // Retained value seen by a fresh subscriber must be `down` (clean
    // DISCONNECT means the `offline` will did not fire).
    let mut fresh = connect(pki, "tester", "tester-2", tester_pw, &pki.tester).await?;
    fresh.subscribe(&status).await?;
    let m = fresh
        .expect(&status, b"down", Duration::from_secs(5))
        .await?;
    ensure!(m.retain, "down was not retained");
    println!("PASS: all e2e assertions held");
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    if docker(&["info", "--format", "{{.ServerVersion}}"]).is_err() {
        bail!("Docker is not running; e2e skipped");
    }
    let work = e2e_dir().join("work");
    let _ = std::fs::remove_dir_all(&work);
    let (pki, tester_pw, pc_pw) = generate(&work)?;

    let _ = compose(&["down", "-v", "--remove-orphans"]);
    println!("==> building agent image");
    compose(&["build", "agent"])?;
    compose(&["up", "-d", "mosquitto"])?;

    let result = scenario(&pki, &tester_pw, &pc_pw).await;

    if let Ok(logs) = compose(&["logs", "--no-color", "agent"]) {
        println!("---- agent logs ----\n{logs}");
    }
    if result.is_err() {
        if let Ok(logs) = compose(&["logs", "--no-color", "mosquitto"]) {
            println!("---- mosquitto logs ----\n{logs}");
        }
    }
    let _ = compose(&["down", "-v", "--remove-orphans"]);
    result
}
