//! `/etc/rack-agent/config.toml`: the only per-host input. Every host runs the
//! same binary; identity (MQTT user, client id, topics) comes from `name`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/rack-agent/config.toml";

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
    pub fn parse(text: &str) -> Result<Self> {
        let raw: RawConfig = toml::from_str(text).context("parsing config TOML")?;
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
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in config {}", path.display()))
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
    fn unknown_keys_rejected() {
        assert!(Config::parse("name = \"pc\"\ndryrun = true\n").is_err());
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
