//! TOML configuration and credential loading.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

pub const DEFAULT_CONFIG: &str = "/etc/rack/relay-mqtt.toml";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Broker address. An IP literal is verified against the cert's IP SAN.
    pub host: String,
    pub port: u16,
    pub ca_file: PathBuf,
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
    /// MQTT username; also the client id (and the cert CN).
    pub username: String,
    pub password_file: PathBuf,
    /// rack_controller control socket.
    pub socket: PathBuf,
    pub keep_alive_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            host: "192.168.8.1".into(),
            port: 8883,
            ca_file: "/etc/rack/tls/ca.crt".into(),
            cert_file: "/etc/rack/tls/sbc4.crt".into(),
            key_file: "/etc/rack/tls/sbc4.key".into(),
            username: "sbc4".into(),
            password_file: "/etc/rack/sbc4.password".into(),
            socket: "/run/rack_controller/ctl.sock".into(),
            keep_alive_secs: 30,
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let c: Config = toml::from_str(text).context("parse config")?;
        if c.host.is_empty() {
            bail!("host must not be empty");
        }
        if c.username.is_empty() {
            bail!("username must not be empty");
        }
        if c.port == 0 {
            bail!("port must not be 0");
        }
        if !(5..=600).contains(&c.keep_alive_secs) {
            bail!("keep_alive_secs must be 5..=600");
        }
        Ok(c)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }
}

/// The broker password, read from disk. The TLS files are passed to Paho by
/// path (OpenSSL reads them on every connect); `load` only checks they can
/// be read, so a missing file is reported clearly at startup.
pub struct Credentials {
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials { .. }")
    }
}

/// The password file holds the password, optionally followed by exactly one
/// newline (`\n` or `\r\n`), which is removed. Nothing else is trimmed.
pub fn parse_password(raw: &str) -> Result<String> {
    let p = raw
        .strip_suffix("\r\n")
        .or_else(|| raw.strip_suffix('\n'))
        .unwrap_or(raw);
    if p.is_empty() {
        bail!("password file is empty");
    }
    if p.contains('\n') {
        bail!("password file has more than one line");
    }
    Ok(p.to_string())
}

impl Credentials {
    pub fn load(c: &Config) -> Result<Self> {
        for p in [&c.ca_file, &c.cert_file, &c.key_file] {
            // A full read (not just open) so a directory or unreadable file
            // is caught here, with a clear error, before Paho sees it.
            std::fs::read(p).with_context(|| format!("read {}", p.display()))?;
        }
        let pw_raw = std::fs::read_to_string(&c.password_file)
            .with_context(|| format!("read {}", c.password_file.display()))?;
        Ok(Credentials {
            password: parse_password(&pw_raw)
                .with_context(|| format!("in {}", c.password_file.display()))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_contract() {
        let c = Config::parse("").unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.host, "192.168.8.1");
        assert_eq!(c.port, 8883);
        assert_eq!(c.username, "sbc4");
        assert_eq!(c.socket, PathBuf::from("/run/rack_controller/ctl.sock"));
    }

    #[test]
    fn example_config_parses() {
        let c = Config::parse(include_str!("../config.example.toml")).unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn rejects_unknown_and_bad() {
        assert!(Config::parse("hots = \"x\"").is_err());
        assert!(Config::parse("host = \"\"").is_err());
        assert!(Config::parse("port = 0").is_err());
        assert!(Config::parse("port = 70000").is_err());
        assert!(Config::parse("keep_alive_secs = 0").is_err());
    }

    #[test]
    fn password_trims_one_newline_only() {
        assert_eq!(parse_password("s3cret\n").unwrap(), "s3cret");
        assert_eq!(parse_password("s3cret\r\n").unwrap(), "s3cret");
        assert_eq!(parse_password("s3cret").unwrap(), "s3cret");
        assert_eq!(parse_password(" s3 cret ").unwrap(), " s3 cret ");
        assert!(parse_password("").is_err());
        assert!(parse_password("\n").is_err());
        assert!(parse_password("a\n\n").is_err());
        assert!(parse_password("a\nb").is_err());
    }
}
