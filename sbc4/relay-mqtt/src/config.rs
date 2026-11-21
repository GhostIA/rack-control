//! TOML configuration and credential loading.
//!
//! Loaded with figment: the defaults ([`Config::default`]), then the TOML
//! file, then `RELAY_MQTT_<KEY>` environment overrides (only for the keys in
//! [`KEYS`], so unrelated variables can never trip `deny_unknown_fields`).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use figment::providers::{Env, Format, Serialized, Toml};
use figment::value::Value;
use figment::Figment;
use serde::Deserialize;

pub const DEFAULT_CONFIG: &str = "/etc/rack/relay-mqtt.toml";

/// Prefix of the environment overrides, e.g. `RELAY_MQTT_KEEP_ALIVE_SECS=10`.
pub const ENV_PREFIX: &str = "RELAY_MQTT_";

/// Every config key; also the only keys taken from the environment.
pub const KEYS: &[&str] = &[
    "host",
    "port",
    "ca_file",
    "cert_file",
    "key_file",
    "username",
    "password_file",
    "socket",
    "keep_alive_secs",
    "log_level",
];

/// Keys that are not strings: their env values are parsed (`8883`). Every
/// other key takes the value verbatim, so `RELAY_MQTT_USERNAME=1234` is the
/// string "1234", as it would be in the file.
const TYPED_KEYS: &[&str] = &["port", "keep_alive_secs"];

/// `<prefix><KEY>` variables for the known keys only, as (lower-case key,
/// raw value). Anything else with the prefix is ignored.
fn env_overrides(prefix: &str) -> Vec<(String, String)> {
    Env::prefixed(prefix)
        .only(KEYS)
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v))
        .collect()
}

fn override_value(key: &str, raw: &str) -> Value {
    if TYPED_KEYS.contains(&key) {
        raw.parse().unwrap_or_else(|_| Value::from(raw))
    } else {
        Value::from(raw)
    }
}

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
    /// `RUST_LOG`-style log filter; `None` falls back to `RUST_LOG`, then `info`.
    pub log_level: Option<String>,
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
            log_level: None,
        }
    }
}

impl Config {
    /// Parses config TOML text (no environment overrides).
    pub fn parse(text: &str) -> Result<Self> {
        Self::extract(&Figment::from(Toml::string(text)))
    }

    /// Loads `path`, then applies the `RELAY_MQTT_*` environment overrides.
    pub fn load(path: &Path) -> Result<Self> {
        Self::load_with(path, &env_overrides(ENV_PREFIX))
    }

    /// `overrides` are (lower-case key, raw value) pairs, as read from the
    /// environment by [`Config::load`]; tests pass them directly.
    fn load_with(path: &Path, overrides: &[(String, String)]) -> Result<Self> {
        // Read the file ourselves: figment's `Toml::file` treats a missing
        // file as empty, and a missing config must be an error.
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let mut figment = Figment::from(Toml::string(&text));
        for (key, raw) in overrides {
            figment = figment.merge(Serialized::default(key, override_value(key, raw)));
        }
        // A bad value may come from the environment rather than the file.
        let from_env = if overrides.is_empty() {
            String::new()
        } else {
            format!(" (with {ENV_PREFIX}* environment overrides)")
        };
        Self::extract(&figment).with_context(|| format!("in {}{from_env}", path.display()))
    }

    fn extract(figment: &Figment) -> Result<Self> {
        let c: Config = figment.extract().context("parse config")?;
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
        if let Some(l) = &c.log_level {
            crate::logging::check_directives(l)
                .with_context(|| format!("invalid log_level {l:?}"))?;
        }
        Ok(c)
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

    /// A fresh per-test directory under the temp dir.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("relay-mqtt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_file_applies_defaults() {
        let dir = scratch("load-defaults");
        let p = dir.join("relay-mqtt.toml");
        std::fs::write(&p, "port = 18883\n").unwrap();
        // No overrides: only the file and the defaults apply.
        let c = Config::load_with(&p, &[]).unwrap();
        assert_eq!(
            c,
            Config {
                port: 18883,
                ..Config::default()
            }
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn example_config_file_loads() {
        let dir = scratch("load-example");
        let p = dir.join("relay-mqtt.toml");
        std::fs::write(&p, include_str!("../config.example.toml")).unwrap();
        assert_eq!(Config::load_with(&p, &[]).unwrap(), Config::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_file_is_an_error() {
        let dir = scratch("load-missing");
        let p = dir.join("nope.toml");
        let e = format!("{:#}", Config::load(&p).unwrap_err());
        assert!(e.starts_with(&format!("read {}: ", p.display())), "{e}");
        // A directory is not a config file either.
        let e = format!("{:#}", Config::load(&dir).unwrap_err());
        assert!(e.starts_with(&format!("read {}: ", dir.display())), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn typo_key_in_file_names_file_and_key() {
        let dir = scratch("load-typo");
        let p = dir.join("relay-mqtt.toml");
        std::fs::write(&p, "hots = \"10.0.0.1\"\n").unwrap();
        let err = Config::load_with(&p, &[]).unwrap_err();
        // `{:#}` (what main logs) shows the whole chain on one line.
        let e = format!("{err:#}");
        assert!(
            e.starts_with(&format!("in {}: parse config: unknown field", p.display())),
            "{e}"
        );
        assert!(e.contains("`hots`"), "{e}");
        assert!(err.chain().count() >= 3, "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn ov(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn env_overrides_file() {
        let dir = scratch("load-env");
        let p = dir.join("relay-mqtt.toml");
        std::fs::write(&p, "keep_alive_secs = 30\n").unwrap();
        let c = Config::load_with(
            &p,
            &ov(&[
                ("keep_alive_secs", "10"),
                ("socket", "/tmp/ctl.sock"),
                ("log_level", "info,paho_mqtt::c_trace=debug"),
            ]),
        )
        .unwrap();
        assert_eq!(c.keep_alive_secs, 10);
        assert_eq!(c.socket, PathBuf::from("/tmp/ctl.sock"));
        assert_eq!(
            c.log_level.as_deref(),
            Some("info,paho_mqtt::c_trace=debug")
        );
        assert_eq!(c.host, "192.168.8.1");
        // String keys take the value verbatim, even when it looks like a
        // number or a bool.
        let c = Config::load_with(&p, &ov(&[("username", "1234"), ("host", "true")])).unwrap();
        assert_eq!(c.username, "1234");
        assert_eq!(c.host, "true");
        // Overrides are validated like the file, and the error says so.
        let e = format!(
            "{:#}",
            Config::load_with(&p, &ov(&[("port", "0")])).unwrap_err()
        );
        assert!(e.contains("environment overrides"), "{e}");
        assert!(e.contains("port must not be 0"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn env_reads_only_known_keys() {
        // With an empty prefix every variable (PATH, HOME, ...) is a
        // candidate; only config keys may come through.
        let got = env_overrides("");
        assert!(
            got.iter().all(|(k, _)| KEYS.contains(&k.as_str())),
            "{got:?}"
        );
    }

    #[test]
    fn log_level_validated() {
        assert_eq!(
            Config::parse("log_level = \"debug\"")
                .unwrap()
                .log_level
                .as_deref(),
            Some("debug")
        );
        let e = format!("{:#}", Config::parse("log_level = \"x=loud\"").unwrap_err());
        assert!(e.contains("invalid log_level"), "{e}");
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
