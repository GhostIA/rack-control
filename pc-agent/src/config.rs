//! `/etc/rack-agent/config.toml`: the only per-host input. Every host runs the
//! same binary; identity (MQTT user, client id, topics) comes from `name`.
//!
//! Loaded with figment: the serde defaults below, then the TOML file, then
//! `RACK_AGENT_<KEY>` environment overrides (only for the keys in [`KEYS`],
//! so unrelated variables can never trip `deny_unknown_fields`).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use figment::providers::{Env, Format, Serialized, Toml};
use figment::value::Value;
use figment::Figment;
use serde::Deserialize;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/rack-agent/config.toml";

/// Prefix of the environment overrides, e.g. `RACK_AGENT_DRY_RUN=true`.
pub const ENV_PREFIX: &str = "RACK_AGENT_";

/// Every config key; also the only keys taken from the environment.
pub const KEYS: &[&str] = &[
    "name",
    "broker_host",
    "broker_port",
    "ca",
    "cert",
    "key",
    "password_file",
    "ceph_maintenance",
    "ceph_id",
    "ceph_conf",
    "dry_run",
    "log_level",
];

/// Keys that are not strings: their env values are parsed (`8883`, `true`).
/// Every other key takes the value verbatim, so `RACK_AGENT_NAME=1234` is the
/// string "1234", as it would be in the file.
const TYPED_KEYS: &[&str] = &["broker_port", "ceph_maintenance", "dry_run"];

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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    name: String,
    #[serde(default = "default_broker_host")]
    broker_host: String,
    #[serde(default = "default_broker_port")]
    broker_port: u16,
    ca: Option<PathBuf>,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
    password_file: Option<PathBuf>,
    #[serde(default)]
    ceph_maintenance: bool,
    #[serde(default = "default_ceph_id")]
    ceph_id: String,
    ceph_conf: Option<PathBuf>,
    #[serde(default)]
    dry_run: bool,
    log_level: Option<String>,
}

fn default_broker_host() -> String {
    "192.168.8.1".to_string()
}
fn default_broker_port() -> u16 {
    8883
}
fn default_ceph_id() -> String {
    "rack-maint".to_string()
}

/// Validated configuration with every path resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub name: String,
    pub broker_host: String,
    pub broker_port: u16,
    pub ca: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
    pub password_file: PathBuf,
    pub ceph_maintenance: bool,
    pub ceph_id: String,
    /// Passed as `--conf <path>` when set. On Proxmox /etc/ceph/ceph.conf is a
    /// symlink into /etc/pve, which the rack-agent user cannot read.
    pub ceph_conf: Option<PathBuf>,
    pub dry_run: bool,
    /// `RUST_LOG`-style log filter; `None` falls back to `RUST_LOG`, then `info`.
    pub log_level: Option<String>,
}

/// `[a-z0-9-]+`, at most 64 bytes, not starting with `-`. Used for `name` (it
/// becomes part of MQTT topics, so `/`, `+` and `#` must never get in) and for
/// `ceph_id` (it becomes a ceph argv element; a leading `-` could be read as a
/// flag).
pub fn is_valid_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl Config {
    /// Parses config TOML text (no environment overrides).
    pub fn parse(text: &str) -> Result<Self> {
        Self::extract(&Figment::from(Toml::string(text)))
    }

    /// Loads `path`, then applies the `RACK_AGENT_*` environment overrides.
    pub fn load(path: &Path) -> Result<Self> {
        Self::load_with(path, &env_overrides(ENV_PREFIX))
    }

    /// `overrides` are (lower-case key, raw value) pairs, as read from the
    /// environment by [`Config::load`]; tests pass them directly.
    fn load_with(path: &Path, overrides: &[(String, String)]) -> Result<Self> {
        // Read the file ourselves: figment's `Toml::file` treats a missing
        // file as empty, and a missing config must be an error.
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
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
        Self::extract(&figment).with_context(|| format!("in config {}{from_env}", path.display()))
    }

    fn extract(figment: &Figment) -> Result<Self> {
        let raw: RawConfig = figment.extract().context("parsing config")?;
        if !is_valid_ident(&raw.name) {
            bail!(
                "invalid name {:?}: must match [a-z0-9-]+ (max 64)",
                raw.name
            );
        }
        if !is_valid_ident(&raw.ceph_id) {
            bail!(
                "invalid ceph_id {:?}: must match [a-z0-9-]+ (max 64)",
                raw.ceph_id
            );
        }
        if let Some(p) = &raw.ceph_conf {
            let ok = p.is_absolute()
                && p.to_str().is_some_and(|s| {
                    !s.is_empty() && s.len() <= 256 && !s.chars().any(|c| c.is_control())
                });
            if !ok {
                bail!(
                    "invalid ceph_conf {:?}: must be an absolute UTF-8 path",
                    p.display()
                );
            }
        }
        if raw.broker_host.is_empty() {
            bail!("broker_host must not be empty");
        }
        if raw.broker_port == 0 {
            bail!("broker_port must not be 0");
        }
        if let Some(l) = &raw.log_level {
            crate::logging::check_directives(l)
                .with_context(|| format!("invalid log_level {l:?}"))?;
        }
        let name = raw.name;
        Ok(Config {
            ca: raw
                .ca
                .unwrap_or_else(|| PathBuf::from("/etc/rack/tls/ca.crt")),
            cert: raw
                .cert
                .unwrap_or_else(|| PathBuf::from(format!("/etc/rack/tls/{name}.crt"))),
            key: raw
                .key
                .unwrap_or_else(|| PathBuf::from(format!("/etc/rack/tls/{name}.key"))),
            password_file: raw
                .password_file
                .unwrap_or_else(|| PathBuf::from(format!("/etc/rack/{name}.password"))),
            name,
            broker_host: raw.broker_host,
            broker_port: raw.broker_port,
            ceph_maintenance: raw.ceph_maintenance,
            ceph_id: raw.ceph_id,
            ceph_conf: raw.ceph_conf,
            dry_run: raw.dry_run,
            log_level: raw.log_level,
        })
    }

    pub fn cmd_topic(&self) -> String {
        format!("rack/pc/{}/cmd", self.name)
    }

    pub fn status_topic(&self) -> String {
        format!("rack/pc/{}/status", self.name)
    }
}

/// Reads the broker password: the whole file minus one trailing newline.
pub fn read_password(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading password file {}", path.display()))?;
    let pw = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(&text);
    if pw.is_empty() {
        bail!("password file {} is empty", path.display());
    }
    Ok(pw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config_gets_defaults() {
        let c = Config::parse("name = \"intel-d1babf\"\n").unwrap();
        assert_eq!(c.broker_host, "192.168.8.1");
        assert_eq!(c.broker_port, 8883);
        assert_eq!(c.ca, PathBuf::from("/etc/rack/tls/ca.crt"));
        assert_eq!(c.cert, PathBuf::from("/etc/rack/tls/intel-d1babf.crt"));
        assert_eq!(c.key, PathBuf::from("/etc/rack/tls/intel-d1babf.key"));
        assert_eq!(
            c.password_file,
            PathBuf::from("/etc/rack/intel-d1babf.password")
        );
        assert!(!c.ceph_maintenance);
        assert_eq!(c.ceph_id, "rack-maint");
        assert_eq!(c.ceph_conf, None);
        assert!(!c.dry_run);
        assert_eq!(c.log_level, None);
        assert_eq!(c.cmd_topic(), "rack/pc/intel-d1babf/cmd");
        assert_eq!(c.status_topic(), "rack/pc/intel-d1babf/status");
    }

    #[test]
    fn example_config_is_rejected_until_edited() {
        // The shipped example uses a placeholder name on purpose.
        assert!(Config::parse(include_str!("../config.example.toml")).is_err());
        let edited = include_str!("../config.example.toml").replace("CHANGE-ME", "intel-1ecbc8");
        let c = Config::parse(&edited).unwrap();
        assert_eq!(c.name, "intel-1ecbc8");
    }

    #[test]
    fn bad_names_rejected() {
        for n in [
            "",
            "-pc",
            "Intel",
            "a/b",
            "a+b",
            "a#",
            "a b",
            "pc\n",
            &"a".repeat(65),
        ] {
            let text = format!("name = {:?}\n", n);
            assert!(Config::parse(&text).is_err(), "name {n:?} accepted");
        }
    }

    #[test]
    fn bad_ceph_id_rejected() {
        for id in [
            "",
            "rack maint",
            "rack;rm",
            "--admin",
            "Rack",
            "client.admin",
            "x/y",
            "rack-maint\n",
        ] {
            let text = format!("name = \"pc\"\nceph_id = {:?}\n", id);
            assert!(Config::parse(&text).is_err(), "ceph_id {id:?} accepted");
        }
        assert!(Config::parse("name = \"pc\"\nceph_id = \"admin-2\"\n").is_ok());
    }

    #[test]
    fn ceph_conf_validated() {
        let c =
            Config::parse("name = \"pc\"\nceph_conf = \"/etc/rack-agent/ceph.conf\"\n").unwrap();
        assert_eq!(
            c.ceph_conf,
            Some(PathBuf::from("/etc/rack-agent/ceph.conf"))
        );
        for bad in ["", "ceph.conf", "--conf", "/etc/x\n"] {
            let text = format!("name = \"pc\"\nceph_conf = {:?}\n", bad);
            assert!(Config::parse(&text).is_err(), "ceph_conf {bad:?} accepted");
        }
    }

    #[test]
    fn log_level_validated() {
        let c = Config::parse("name = \"pc\"\nlog_level = \"info,paho_mqtt::c_trace=debug\"\n")
            .unwrap();
        assert_eq!(
            c.log_level.as_deref(),
            Some("info,paho_mqtt::c_trace=debug")
        );
        let e = format!(
            "{:#}",
            Config::parse("name = \"pc\"\nlog_level = \"info,x=loud\"\n").unwrap_err()
        );
        assert!(e.contains("invalid log_level"), "{e}");
    }

    #[test]
    fn unknown_keys_rejected() {
        let e = format!(
            "{:#}",
            Config::parse("name = \"pc\"\ndryrun = true\n").unwrap_err()
        );
        assert!(e.contains("dryrun"), "{e}");
        assert!(Config::parse("name = \"pc\"\n[dry_run]\nx = 1\n").is_err());
    }

    /// A fresh per-test directory under the temp dir.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rack-agent-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_file_applies_defaults() {
        let dir = scratch("load-defaults");
        let p = dir.join("config.toml");
        std::fs::write(&p, "name = \"intel-d1babf\"\n").unwrap();
        // No overrides: only the file and the defaults apply.
        let c = Config::load_with(&p, &[]).unwrap();
        assert_eq!(c, Config::parse("name = \"intel-d1babf\"\n").unwrap());
        assert_eq!(c.broker_host, "192.168.8.1");
        assert_eq!(c.broker_port, 8883);
        assert_eq!(c.ceph_id, "rack-maint");
        assert_eq!(c.cert, PathBuf::from("/etc/rack/tls/intel-d1babf.crt"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn example_config_file_loads_with_name_set() {
        let dir = scratch("load-example");
        let p = dir.join("config.toml");
        let edited = include_str!("../config.example.toml").replace("CHANGE-ME", "intel-1ecbc8");
        std::fs::write(&p, edited).unwrap();
        let c = Config::load_with(&p, &[]).unwrap();
        assert_eq!(c.name, "intel-1ecbc8");
        assert_eq!(c.broker_host, "192.168.8.1");
        assert_eq!(c.broker_port, 8883);
        assert!(!c.ceph_maintenance && !c.dry_run);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_file_is_an_error() {
        let dir = scratch("load-missing");
        let p = dir.join("nope.toml");
        let e = format!("{:#}", Config::load(&p).unwrap_err());
        assert!(
            e.contains(&format!("reading config {}", p.display())),
            "{e}"
        );
        // A directory is not a config file either.
        let e = format!("{:#}", Config::load(&dir).unwrap_err());
        assert!(
            e.starts_with(&format!("reading config {}: ", dir.display())),
            "{e}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn typo_key_in_file_names_file_and_key() {
        let dir = scratch("load-typo");
        let p = dir.join("config.toml");
        std::fs::write(&p, "name = \"pc\"\nbroker_hots = \"10.0.0.1\"\n").unwrap();
        let e = format!("{:#}", Config::load_with(&p, &[]).unwrap_err());
        assert!(
            e.starts_with(&format!(
                "in config {}: parsing config: unknown field: found `broker_hots`",
                p.display()
            )),
            "{e}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn validation_error_chain_names_file_and_key() {
        let dir = scratch("load-chain");
        let p = dir.join("config.toml");
        std::fs::write(&p, "name = \"pc\"\nceph_id = \"--admin\"\n").unwrap();
        let err = Config::load_with(&p, &[]).unwrap_err();
        // `{:#}` (what main prints) shows the whole chain on one line.
        let e = format!("{err:#}");
        assert!(
            e.starts_with(&format!("in config {}: invalid ceph_id", p.display())),
            "{e}"
        );
        assert_eq!(err.chain().count(), 2);
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
        let p = dir.join("config.toml");
        std::fs::write(&p, "name = \"pc\"\nbroker_port = 8883\n").unwrap();
        let c = Config::load_with(
            &p,
            &ov(&[
                ("broker_port", "18883"),
                ("dry_run", "true"),
                ("log_level", "info,rack_agent::mqtt=debug"),
                ("ceph_conf", "/etc/rack-agent/ceph.conf"),
            ]),
        )
        .unwrap();
        assert_eq!(c.name, "pc");
        assert_eq!(c.broker_port, 18883);
        assert!(c.dry_run);
        assert_eq!(c.log_level.as_deref(), Some("info,rack_agent::mqtt=debug"));
        assert_eq!(
            c.ceph_conf,
            Some(PathBuf::from("/etc/rack-agent/ceph.conf"))
        );
        // String keys take the value verbatim, even when it looks like a
        // number or a bool.
        let c = Config::load_with(&p, &ov(&[("name", "1234"), ("ceph_id", "true")])).unwrap();
        assert_eq!(c.name, "1234");
        assert_eq!(c.ceph_id, "true");
        // Overrides are validated like the file, and the error says so.
        let e = format!(
            "{:#}",
            Config::load_with(&p, &ov(&[("ceph_id", "--admin")])).unwrap_err()
        );
        assert!(e.contains("environment overrides"), "{e}");
        assert!(e.contains("invalid ceph_id"), "{e}");
        let e = format!(
            "{:#}",
            Config::load_with(&p, &ov(&[("broker_port", "x")])).unwrap_err()
        );
        assert!(e.contains("broker_port"), "{e}");
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
    fn password_trailing_newline_stripped() {
        let dir = std::env::temp_dir().join(format!("rack-agent-pw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("pw");
        std::fs::write(&p, "s3cret\n").unwrap();
        assert_eq!(read_password(&p).unwrap(), "s3cret");
        std::fs::write(&p, "s3cret\r\n").unwrap();
        assert_eq!(read_password(&p).unwrap(), "s3cret");
        std::fs::write(&p, "\n").unwrap();
        assert!(read_password(&p).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
