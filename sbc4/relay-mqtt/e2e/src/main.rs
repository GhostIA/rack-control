//! Docker e2e runner for relay-mqtt. Runs on the dev box:
//!
//! 1. vendors the relay-mqtt crate's dependencies (so the image build runs
//!    with no network) and generates throwaway test PKI + passwords into
//!    `e2e/work/` (gitignored, recreated every run);
//! 2. drives `docker compose` with fixed argv (no shell): broker up, seed a
//!    retained command, start fake-controller + relay-mqtt, run the check;
//! 3. always tears the stack down.
//!
//! The compose network is `internal: true`: containers have no route off the
//! docker host, so they can never reach 192.168.8.0/24.
#![forbid(unsafe_code)]

use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};

/// Static broker IP on the compose network (mirrors production's IP-only SAN).
const BROKER_IP: Ipv4Addr = Ipv4Addr::new(172, 31, 250, 10);
const PROJECT: &str = "relay-mqtt-e2e";

fn e2e_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn random_bytes(n: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut b = vec![0u8; n];
    fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b)
}

/// Mosquitto 2.x `$7$` hash: PBKDF2-HMAC-SHA512, 101 iterations, 12-byte salt.
fn mosquitto_hash(password: &str) -> Result<String> {
    const ITER: u32 = 101;
    let salt = random_bytes(12)?;
    let mut out = [0u8; 64];
    pbkdf2::pbkdf2_hmac::<sha2::Sha512>(password.as_bytes(), &salt, ITER, &mut out);
    Ok(format!(
        "$7${ITER}${}${}",
        B64.encode(salt),
        B64.encode(out)
    ))
}

fn write(path: &Path, data: &[u8]) -> Result<()> {
    fs::write(path, data).with_context(|| format!("write {}", path.display()))
}

struct Ca {
    cert: rcgen::Certificate,
    key: KeyPair,
}

fn leaf(
    ca: &Ca,
    cn: &str,
    sans: Vec<SanType>,
    eku: ExtendedKeyUsagePurpose,
) -> Result<(String, String)> {
    let key = KeyPair::generate()?;
    let mut p = CertificateParams::default();
    p.distinguished_name.push(DnType::CommonName, cn);
    p.subject_alt_names = sans;
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    p.extended_key_usages = vec![eku];
    let cert = p.signed_by(&key, &ca.cert, &ca.key)?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// Generate CA, broker cert (IP SAN), client certs and passwords.
fn gen_pki(work: &Path) -> Result<()> {
    if work.exists() {
        fs::remove_dir_all(work)?;
    }
    for d in ["broker", "sbc4", "tester"] {
        fs::create_dir_all(work.join(d))?;
    }

    let ca_key = KeyPair::generate()?;
    let mut p = CertificateParams::default();
    p.distinguished_name
        .push(DnType::CommonName, "relay-mqtt e2e TEST CA");
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca = Ca {
        cert: p.self_signed(&ca_key)?,
        key: ca_key,
    };
    let ca_pem = ca.cert.pem();

    let (srv_crt, srv_key) = leaf(
        &ca,
        "e2e-broker",
        vec![SanType::IpAddress(IpAddr::V4(BROKER_IP))],
        ExtendedKeyUsagePurpose::ServerAuth,
    )?;
    write(&work.join("broker/ca.crt"), ca_pem.as_bytes())?;
    write(&work.join("broker/server.crt"), srv_crt.as_bytes())?;
    write(&work.join("broker/server.key"), srv_key.as_bytes())?;

    let mut passwd = String::new();
    for user in ["sbc4", "tester"] {
        let (crt, key) = leaf(&ca, user, vec![], ExtendedKeyUsagePurpose::ClientAuth)?;
        let pw: String = random_bytes(18)?
            .iter()
            .map(|b| char::from(b"abcdefghijkmnpqrstuvwxyz23456789"[usize::from(b % 32)]))
            .collect();
        let dir = work.join(user);
        write(&dir.join("ca.crt"), ca_pem.as_bytes())?;
        write(&dir.join(format!("{user}.crt")), crt.as_bytes())?;
        write(&dir.join(format!("{user}.key")), key.as_bytes())?;
        // Trailing newline on purpose: the sidecar must trim exactly one.
        write(
            &dir.join(format!("{user}.password")),
            format!("{pw}\n").as_bytes(),
        )?;
        passwd.push_str(&format!("{user}:{}\n", mosquitto_hash(&pw)?));
    }
    write(&work.join("broker/passwd"), passwd.as_bytes())?;
    Ok(())
}

fn run(cmd: &mut Command) -> Result<()> {
    eprintln!("+ {cmd:?}");
    let st = cmd.status().with_context(|| format!("spawn {cmd:?}"))?;
    if !st.success() {
        bail!("{cmd:?} failed: {st}");
    }
    Ok(())
}

fn compose(args: &[&str]) -> Command {
    let mut c = Command::new("docker");
    c.current_dir(e2e_dir())
        .args(["compose", "-p", PROJECT, "-f", "compose.yaml"])
        .args(args);
    c
}

fn scenario() -> Result<()> {
    run(compose(&["build"]).arg("--pull"))?;
    run(&mut compose(&["up", "-d", "--wait", "broker"]))?;
    let broker = BROKER_IP.to_string();
    let tester = |mode: &'static str| {
        compose(&[
            "run",
            "--rm",
            "tester",
            mode,
            broker.as_str(),
            "/e2e/tester",
        ])
    };
    run(&mut tester("seed"))?;
    run(&mut compose(&["up", "-d", "controller", "relay-mqtt"]))?;
    run(&mut tester("check"))
}

fn main() -> ExitCode {
    let crate_dir = e2e_dir().parent().expect("e2e has a parent").to_path_buf();
    let work = e2e_dir().join("work");
    let prep = (|| -> Result<()> {
        run(Command::new("cargo")
            .current_dir(&crate_dir)
            .args(["vendor", "--locked", "--versioned-dirs", "target/vendor"])
            .stdout(std::process::Stdio::null()))?;
        gen_pki(&work)
    })();
    if let Err(e) = prep {
        eprintln!("e2e setup failed: {e:#}");
        return ExitCode::FAILURE;
    }

    let result = scenario();
    if result.is_err() {
        let _ = run(&mut compose(&[
            "logs",
            "--no-color",
            "broker",
            "controller",
            "relay-mqtt",
        ]));
    }
    let _ = run(&mut compose(&[
        "down",
        "-v",
        "--remove-orphans",
        "--timeout",
        "5",
    ]));
    let _ = fs::remove_dir_all(&work);
    match result {
        Ok(()) => {
            eprintln!("relay-mqtt e2e: PASS");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("relay-mqtt e2e: FAIL: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pki_files_generated() {
        let dir = std::env::temp_dir().join(format!("rmq-e2e-pki-{}", std::process::id()));
        gen_pki(&dir).unwrap();
        for f in [
            "broker/ca.crt",
            "broker/server.crt",
            "broker/server.key",
            "broker/passwd",
            "sbc4/sbc4.crt",
            "sbc4/sbc4.key",
            "sbc4/sbc4.password",
            "tester/tester.key",
        ] {
            assert!(dir.join(f).metadata().unwrap().len() > 0, "{f}");
        }
        let pw = fs::read_to_string(dir.join("broker/passwd")).unwrap();
        assert!(pw.starts_with("sbc4:$7$101$"));
        assert!(pw.contains("\ntester:$7$101$"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hash_format() {
        let h = mosquitto_hash("pw").unwrap();
        let parts: Vec<_> = h.split('$').collect();
        assert_eq!(parts[..3], ["", "7", "101"]);
        assert_eq!(B64.decode(parts[3]).unwrap().len(), 12);
        assert_eq!(B64.decode(parts[4]).unwrap().len(), 64);
    }
}
