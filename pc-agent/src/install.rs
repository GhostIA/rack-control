//! `rack-agent install [--dry-run] [--config <file>]`: replaces install.sh.
//! A plan of fixed steps is computed first (pure, unit-tested), then either
//! printed (`--dry-run`) or executed in order. All commands use fixed argv.

use std::fmt;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::config::{Config, DEFAULT_CONFIG_PATH};

pub const UNIT: &str = include_str!("../rack-agent.service");
pub const POLKIT_RULE: &str = include_str!("../polkit/10-rack-poweroff.rules");
pub const EXAMPLE_CONFIG: &str = include_str!("../config.example.toml");

pub const BIN_PATH: &str = "/usr/local/bin/rack-agent";
pub const UNIT_PATH: &str = "/etc/systemd/system/rack-agent.service";
pub const POLKIT_PATH: &str = "/etc/polkit-1/rules.d/10-rack-poweroff.rules";
pub const CONFIG_DIR: &str = "/etc/rack-agent";
pub const USER: &str = "rack-agent";
const POLKITD: &str = "/usr/lib/polkit-1/polkitd";
const USERADD: &str = "/usr/sbin/useradd";
const SYSTEMCTL: &str = "/usr/bin/systemctl";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    File(PathBuf),
    BuiltinExample,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    CopyBinary {
        from: PathBuf,
    },
    WriteFile {
        path: &'static str,
        contents: &'static str,
    },
    CreateUser,
    CreateConfigDir,
    InstallConfig {
        src: ConfigSource,
    },
    KeepConfig,
    Run(Vec<&'static str>),
    Warn(String),
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::CopyBinary { from } => {
                write!(f, "copy {} -> {BIN_PATH} (0755)", from.display())
            }
            Step::WriteFile { path, .. } => write!(f, "write {path} (0644)"),
            Step::CreateUser => write!(
                f,
                "run {USERADD} --system --user-group --no-create-home --shell /usr/sbin/nologin {USER}"
            ),
            Step::CreateConfigDir => write!(f, "create {CONFIG_DIR}/ (0755 root:root)"),
            Step::InstallConfig { src } => {
                let s = match src {
                    ConfigSource::File(p) => p.display().to_string(),
                    ConfigSource::BuiltinExample => "built-in config.example.toml".into(),
                };
                write!(f, "install {s} -> {DEFAULT_CONFIG_PATH} (0640 root:{USER})")
            }
            Step::KeepConfig => write!(f, "keep existing {DEFAULT_CONFIG_PATH}"),
            Step::Run(argv) => write!(f, "run {}", argv.join(" ")),
            Step::Warn(s) => write!(f, "WARNING: {s}"),
        }
    }
}

/// Facts about the target host the plan depends on.
#[derive(Debug, Clone)]
pub struct HostState {
    pub exe: PathBuf,
    pub user_exists: bool,
    pub config_exists: bool,
    /// Whether the config that will be in place after install validates.
    pub config_valid: bool,
    pub polkitd_present: bool,
}

pub fn plan(host: &HostState, config_src: Option<&Path>) -> Vec<Step> {
    let mut steps = Vec::new();
    if host.exe != Path::new(BIN_PATH) {
        steps.push(Step::CopyBinary {
            from: host.exe.clone(),
        });
    }
    steps.push(Step::WriteFile {
        path: UNIT_PATH,
        contents: UNIT,
    });
    steps.push(Step::WriteFile {
        path: POLKIT_PATH,
        contents: POLKIT_RULE,
    });
    if !host.polkitd_present {
        steps.push(Step::Warn(format!(
            "{POLKITD} not found: without polkit, logind refuses PowerOff for \
             {USER}. Install it with `apt install polkitd`."
        )));
    }
    if !host.user_exists {
        steps.push(Step::CreateUser);
    }
    steps.push(Step::CreateConfigDir);
    if host.config_exists {
        steps.push(Step::KeepConfig);
    } else {
        steps.push(Step::InstallConfig {
            src: match config_src {
                Some(p) => ConfigSource::File(p.to_path_buf()),
                None => ConfigSource::BuiltinExample,
            },
        });
    }
    steps.push(Step::Run(vec![SYSTEMCTL, "daemon-reload"]));
    // Upgrade path: a running agent picks up the new binary; no-op otherwise.
    steps.push(Step::Run(vec![SYSTEMCTL, "try-restart", "rack-agent"]));
    if host.config_valid {
        steps.push(Step::Run(vec![SYSTEMCTL, "enable", "--now", "rack-agent"]));
    } else {
        steps.push(Step::Run(vec![SYSTEMCTL, "enable", "rack-agent"]));
        steps.push(Step::Warn(format!(
            "{DEFAULT_CONFIG_PATH} is not valid yet (placeholder?): not starting. \
             Edit it, then `systemctl start rack-agent`."
        )));
    }
    steps
}

fn passwd_has(file: &str, name: &str) -> Result<Option<u32>> {
    let text = fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    for line in text.lines() {
        let mut f = line.split(':');
        if f.next() == Some(name) {
            let id = f
                .nth(1)
                .and_then(|s| s.parse().ok())
                .with_context(|| format!("bad {file} entry for {name}"))?;
            return Ok(Some(id));
        }
    }
    Ok(None)
}

fn gather(config_src: Option<&Path>) -> Result<HostState> {
    let exe = std::env::current_exe().context("locating own binary")?;
    let exe = fs::canonicalize(&exe).unwrap_or(exe);
    let config_exists = Path::new(DEFAULT_CONFIG_PATH).exists();
    if let Some(p) = config_src {
        // Refuse a bad --config up front instead of installing it.
        Config::load(p)?;
        if config_exists {
            bail!(
                "{DEFAULT_CONFIG_PATH} already exists and is never overwritten; \
                 edit it in place, or remove it and re-run install --config {}",
                p.display()
            );
        }
    }
    let config_valid = if config_exists {
        Config::load(Path::new(DEFAULT_CONFIG_PATH)).is_ok()
    } else {
        match config_src {
            Some(_) => true,
            None => Config::parse(EXAMPLE_CONFIG).is_ok(),
        }
    };
    Ok(HostState {
        exe,
        user_exists: passwd_has("/etc/passwd", USER)?.is_some(),
        config_exists,
        config_valid,
        polkitd_present: Path::new(POLKITD).exists(),
    })
}

fn is_root() -> Result<bool> {
    // /proc/self is owned by the effective uid; avoids libc/unsafe.
    let m = fs::metadata("/proc/self").context("install is Linux-only (/proc missing)")?;
    Ok(m.uid() == 0)
}

fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let dir = path.parent().context("path has no parent")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.rack-agent-tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    {
        let mut f =
            fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.set_permissions(fs::Permissions::from_mode(mode))?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

fn run(argv: &[&str]) -> Result<()> {
    let status = Command::new(argv[0])
        .args(&argv[1..])
        .status()
        .with_context(|| format!("running {}", argv[0]))?;
    if !status.success() {
        bail!("{} failed: {status}", argv.join(" "));
    }
    Ok(())
}

fn execute(step: &Step) -> Result<()> {
    match step {
        Step::CopyBinary { from } => {
            let bytes = fs::read(from).with_context(|| format!("reading {}", from.display()))?;
            write_atomic(Path::new(BIN_PATH), &bytes, 0o755)
        }
        Step::WriteFile { path, contents } => {
            write_atomic(Path::new(path), contents.as_bytes(), 0o644)
        }
        Step::CreateUser => run(&[
            USERADD,
            "--system",
            "--user-group",
            "--no-create-home",
            "--shell",
            "/usr/sbin/nologin",
            USER,
        ]),
        Step::CreateConfigDir => {
            fs::create_dir_all(CONFIG_DIR)?;
            std::os::unix::fs::chown(CONFIG_DIR, Some(0), Some(0))
                .with_context(|| format!("chown {CONFIG_DIR}"))?;
            fs::set_permissions(CONFIG_DIR, fs::Permissions::from_mode(0o755))?;
            Ok(())
        }
        Step::InstallConfig { src } => {
            let bytes = match src {
                ConfigSource::File(p) => {
                    fs::read(p).with_context(|| format!("reading {}", p.display()))?
                }
                ConfigSource::BuiltinExample => EXAMPLE_CONFIG.as_bytes().to_vec(),
            };
            let path = Path::new(DEFAULT_CONFIG_PATH);
            write_atomic(path, &bytes, 0o640)?;
            let gid = passwd_has("/etc/group", USER)?
                .with_context(|| format!("group {USER} not found in /etc/group"))?;
            std::os::unix::fs::chown(path, Some(0), Some(gid))
                .with_context(|| format!("chown {}", path.display()))
        }
        Step::KeepConfig => Ok(()),
        Step::Run(argv) => run(argv),
        Step::Warn(_) => Ok(()),
    }
}

pub fn install(dry_run: bool, config_src: Option<&Path>) -> Result<()> {
    if !dry_run && !is_root()? {
        bail!("install must run as root (or use --dry-run)");
    }
    let host = gather(config_src)?;
    let steps = plan(&host, config_src);
    if dry_run {
        println!("rack-agent install --dry-run: would perform:");
        for (i, s) in steps.iter().enumerate() {
            println!("  {}. {s}", i + 1);
        }
        return Ok(());
    }
    for s in &steps {
        println!("==> {s}");
        execute(s).with_context(|| format!("step failed: {s}"))?;
    }
    println!(
        "done. Put TLS material in /etc/rack/tls/ and the password in /etc/rack/<name>.password \
         (0640 root:{USER}); see README."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> HostState {
        HostState {
            exe: PathBuf::from("/root/rack-agent"),
            user_exists: false,
            config_exists: false,
            config_valid: false,
            polkitd_present: true,
        }
    }

    fn render(steps: &[Step]) -> Vec<String> {
        steps.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn fresh_host_dry_run_lists_all_actions() {
        let steps = plan(&host(), None);
        assert_eq!(
            render(&steps),
            vec![
                "copy /root/rack-agent -> /usr/local/bin/rack-agent (0755)",
                "write /etc/systemd/system/rack-agent.service (0644)",
                "write /etc/polkit-1/rules.d/10-rack-poweroff.rules (0644)",
                "run /usr/sbin/useradd --system --user-group --no-create-home --shell /usr/sbin/nologin rack-agent",
                "create /etc/rack-agent/ (0755 root:root)",
                "install built-in config.example.toml -> /etc/rack-agent/config.toml (0640 root:rack-agent)",
                "run /usr/bin/systemctl daemon-reload",
                "run /usr/bin/systemctl try-restart rack-agent",
                "run /usr/bin/systemctl enable rack-agent",
                "WARNING: /etc/rack-agent/config.toml is not valid yet (placeholder?): not starting. \
                 Edit it, then `systemctl start rack-agent`.",
            ]
        );
    }

    #[test]
    fn existing_user_and_valid_config_enables_now() {
        let h = HostState {
            exe: PathBuf::from(BIN_PATH),
            user_exists: true,
            config_exists: true,
            config_valid: true,
            polkitd_present: false,
        };
        let r = render(&plan(&h, Some(Path::new("/tmp/c.toml"))));
        assert!(!r.iter().any(|s| s.starts_with("copy ")), "{r:?}");
        assert!(!r.iter().any(|s| s.contains("useradd")));
        assert!(r
            .iter()
            .any(|s| s == "keep existing /etc/rack-agent/config.toml"));
        assert!(!r.iter().any(|s| s.starts_with("install ")));
        assert!(r.iter().any(|s| s.contains("apt install polkitd")));
        assert_eq!(
            r.last().unwrap(),
            "run /usr/bin/systemctl enable --now rack-agent"
        );
    }

    #[test]
    fn config_file_source_used_when_absent() {
        let h = HostState {
            config_valid: true,
            ..host()
        };
        let steps = plan(&h, Some(Path::new("/root/intel-1ecbc8.toml")));
        assert!(steps.contains(&Step::InstallConfig {
            src: ConfigSource::File("/root/intel-1ecbc8.toml".into())
        }));
    }

    #[test]
    fn embedded_files_are_the_shipped_ones() {
        assert!(UNIT.contains("User=rack-agent"));
        assert!(UNIT.contains("ExecStart=/usr/local/bin/rack-agent run"));
        assert!(POLKIT_RULE.contains("org.freedesktop.login1.power-off-multiple-sessions"));
    }

    #[test]
    fn passwd_lookup() {
        let dir = std::env::temp_dir().join(format!("rack-agent-passwd-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("passwd");
        fs::write(
            &p,
            "root:x:0:0::/root:/bin/sh\nrack-agentx:x:5:5::/:/x\nrack-agent:x:996:993::/:/usr/sbin/nologin\n",
        )
        .unwrap();
        assert_eq!(passwd_has(p.to_str().unwrap(), USER).unwrap(), Some(996));
        assert_eq!(passwd_has(p.to_str().unwrap(), "nobody").unwrap(), None);
        fs::remove_dir_all(&dir).unwrap();
    }
}
