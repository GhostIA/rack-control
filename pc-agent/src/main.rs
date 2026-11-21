//! rack-agent: per-host MQTT agent for the rack. Its whole vocabulary is
//! "power off on the exact word `shutdown`" (plus, on the Ceph maintenance
//! node, setting/unsetting two fixed OSD flags). Message content is never
//! executed or used as an argument.

mod agent;
mod config;
mod install;
mod logging;
mod mqtt;
#[cfg(test)]
mod stub_broker;
mod system;

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use log::{error, info};
use tokio::signal::unix::{signal, SignalKind};

use agent::Agent;
use config::{read_password, Config, DEFAULT_CONFIG_PATH};
use mqtt::{MqttPublisher, Served};
use system::{CephCli, LogindPowerOff};

const USAGE: &str = "\
usage:
  rack-agent [run] [--config <path>]                 run the agent (default config /etc/rack-agent/config.toml)
  rack-agent check [--config <path>]                 validate the config and exit
  rack-agent install [--dry-run] [--config <path>] [--bundle <dir>]
                                                     install binary, unit, polkit rule, user, config, and
                                                     a `rackpki bundle <name>` dir into /etc/rack (root)
  rack-agent --version | --help";

#[derive(Debug, PartialEq, Eq)]
enum Cli {
    Run {
        config: PathBuf,
    },
    Check {
        config: PathBuf,
    },
    Install {
        dry_run: bool,
        config: Option<PathBuf>,
        bundle: Option<PathBuf>,
    },
    Help,
    Version,
}

fn parse_args(args: &[String]) -> Result<Cli> {
    let mut it = args.iter().peekable();
    let sub = match it.peek().map(|s| s.as_str()) {
        Some("run") | Some("check") | Some("install") => it.next().map(|s| s.as_str()),
        Some("-h") | Some("--help") => return Ok(Cli::Help),
        Some("-V") | Some("--version") => return Ok(Cli::Version),
        _ => Some("run"),
    };
    let mut config: Option<PathBuf> = None;
    let mut dry_run = false;
    let mut bundle: Option<PathBuf> = None;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" | "-c" => {
                let v = it.next().context("--config needs a path")?;
                config = Some(PathBuf::from(v));
            }
            "--dry-run" if sub == Some("install") => dry_run = true,
            "--bundle" if sub == Some("install") => {
                let v = it.next().context("--bundle needs a directory")?;
                bundle = Some(PathBuf::from(v));
            }
            "-h" | "--help" => return Ok(Cli::Help),
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    let default = || PathBuf::from(DEFAULT_CONFIG_PATH);
    Ok(match sub {
        Some("install") => Cli::Install {
            dry_run,
            config,
            bundle,
        },
        Some("check") => Cli::Check {
            config: config.unwrap_or_else(default),
        },
        _ => Cli::Run {
            config: config.unwrap_or_else(default),
        },
    })
}

fn main() {
    install_panic_hook();
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(debug_assertions)]
    if args.first().map(String::as_str) == Some("--panic-test") {
        panic_test();
    }
    let code = match real_main(&args) {
        Ok(()) => 0,
        Err(e) => {
            if !logging::installed() {
                let _ = logging::init(&logging::directives(None));
            }
            if logging::installed() {
                error!("{e:#}");
            } else {
                let _ = writeln!(std::io::stderr(), "error: {e:#}");
            }
            1
        }
    };
    // fast_log writes on its own thread: flush so the last lines (e.g.
    // "dry_run: would power off") are never lost.
    logging::flush();
    std::process::exit(code);
}

/// human-panic, plus two things it does not do for a daemon: flush the log
/// first, and put the panic message itself on stderr (journald). In release
/// builds human-panic writes the message only to its report file, which is
/// in the unit's PrivateTmp and gone once systemd restarts the agent.
fn install_panic_hook() {
    human_panic::setup_panic!(human_panic::metadata!()
        .support("- The panic message is also in the journal: journalctl -u rack-agent"));
    let human = human_panic::PanicStyle::default() == human_panic::PanicStyle::Human;
    let next = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        logging::flush();
        if human {
            let _ = writeln!(
                std::io::stderr(),
                "rack-agent {}: {info}",
                env!("CARGO_PKG_VERSION")
            );
        }
        next(info);
    }));
}

/// Debug builds only (tests/panic_hook.rs): log a line, then panic.
#[cfg(debug_assertions)]
fn panic_test() -> ! {
    let _ = logging::init(&logging::directives(None));
    info!("panic-test: logged just before the panic");
    panic!("--panic-test (debug builds only)");
}

fn real_main(args: &[String]) -> Result<()> {
    match parse_args(args)? {
        Cli::Help => {
            println!("{USAGE}");
            Ok(())
        }
        Cli::Version => {
            println!("rack-agent {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Cli::Check { config } => {
            let c = Config::load(&config)?;
            println!("ok: {} -> {}:{}", c.name, c.broker_host, c.broker_port);
            Ok(())
        }
        Cli::Install {
            dry_run,
            config,
            bundle,
        } => install::install(dry_run, config.as_deref(), bundle.as_deref()),
        Cli::Run { config } => {
            let cfg = Config::load(&config)?;
            logging::init(&logging::directives(cfg.log_level.as_deref()))?;
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("building tokio runtime")?
                .block_on(run(cfg))
        }
    }
}

async fn run(cfg: Config) -> Result<()> {
    info!(
        "rack-agent {} starting as {} (broker {}:{}, ceph_maintenance={}, dry_run={})",
        env!("CARGO_PKG_VERSION"),
        cfg.name,
        cfg.broker_host,
        cfg.broker_port,
        cfg.ceph_maintenance,
        cfg.dry_run
    );
    let ssl = mqtt::tls_options(&cfg)?;
    let password = read_password(&cfg.password_file)?;
    let client = mqtt::create_client(&mqtt::broker_uri(&cfg), &cfg.name)?;
    // Callbacks must be in place before the first connect.
    let mut events = mqtt::event_channel(&client);
    let publisher = MqttPublisher::new(client.clone());
    // Two runs per command; keep the worst case (2 x 20 s) well under
    // systemd's 90 s stop timeout and the orchestrator's ping-down window.
    let ceph = CephCli {
        timeout: Duration::from_secs(20),
    };
    let mut agent = Agent::new(&cfg, publisher, LogindPowerOff, ceph);

    let mut sigterm = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("installing SIGINT handler")?;
    let stop = async {
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
    };

    mqtt::start_connect(
        &client,
        mqtt::connect_options(&cfg.name, &password, &cfg.status_topic(), Some(ssl)),
    );
    match mqtt::serve(&client, &mut agent, &mut events, stop).await? {
        Served::Exit => {
            // Clean DISCONNECT: the broker keeps the retained `down` instead
            // of firing the `offline` will.
            mqtt::disconnect(&client).await;
            info!("exiting (dry_run)");
            Ok(())
        }
        Served::Stopped => {
            info!("signal received; stopping");
            agent.on_terminate().await;
            mqtt::disconnect(&client).await;
            Ok(())
        }
        Served::Closed => bail!("mqtt callbacks ended unexpectedly"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cli_parsing() {
        let def = PathBuf::from(DEFAULT_CONFIG_PATH);
        assert_eq!(
            parse_args(&a(&[])).unwrap(),
            Cli::Run {
                config: def.clone()
            }
        );
        assert_eq!(
            parse_args(&a(&["run", "--config", "/x.toml"])).unwrap(),
            Cli::Run {
                config: "/x.toml".into()
            }
        );
        assert_eq!(
            parse_args(&a(&["--config", "/x.toml"])).unwrap(),
            Cli::Run {
                config: "/x.toml".into()
            }
        );
        assert_eq!(
            parse_args(&a(&["install", "--dry-run"])).unwrap(),
            Cli::Install {
                dry_run: true,
                config: None,
                bundle: None
            }
        );
        assert_eq!(
            parse_args(&a(&["install", "--config", "/c.toml", "--bundle", "/b"])).unwrap(),
            Cli::Install {
                dry_run: false,
                config: Some("/c.toml".into()),
                bundle: Some("/b".into())
            }
        );
        assert!(parse_args(&a(&["run", "--bundle", "/b"])).is_err());
        assert!(parse_args(&a(&["install", "--bundle"])).is_err());
        assert_eq!(
            parse_args(&a(&["check"])).unwrap(),
            Cli::Check { config: def }
        );
        assert!(parse_args(&a(&["run", "--dry-run"])).is_err());
        assert!(parse_args(&a(&["install", "--bogus"])).is_err());
        assert!(parse_args(&a(&["--config"])).is_err());
    }
}
