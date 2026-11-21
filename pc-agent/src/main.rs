//! rack-agent: per-host MQTT agent for the rack. Its whole vocabulary is
//! "power off on the exact word `shutdown`" (plus, on the Ceph maintenance
//! node, setting/unsetting two fixed OSD flags). Message content is never
//! executed or used as an argument.

mod agent;
mod config;
mod install;
mod mqtt;
mod system;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use log::{error, info};
use rumqttc::{AsyncClient, QoS};
use tokio::signal::unix::{signal, SignalKind};

use agent::{Agent, Outcome};
use config::{Config, DEFAULT_CONFIG_PATH};
use mqtt::{LoopEvent, MqttPublisher};
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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = parse_args(&args).and_then(|cli| match cli {
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
            let cfg = Config::load(&config);
            cfg.and_then(|cfg| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("building tokio runtime")?
                    .block_on(run(cfg))
            })
        }
    });
    if let Err(e) = result {
        error!("{e:#}");
        std::process::exit(1);
    }
}

async fn disconnect(client: &AsyncClient, loop_task: tokio::task::JoinHandle<()>) {
    let _ = tokio::time::timeout(Duration::from_secs(2), client.disconnect()).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), loop_task).await;
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
    let opts = mqtt::options(&cfg)?;
    let (client, eventloop) = AsyncClient::new(opts, 16);
    let (mut events, acks, loop_task) = mqtt::spawn_event_loop(eventloop);
    let publisher = MqttPublisher::new(client.clone(), acks);
    // Two runs per command; keep the worst case (2 x 20 s) well under
    // systemd's 90 s stop timeout and the orchestrator's ping-down window.
    let ceph = CephCli {
        timeout: Duration::from_secs(20),
    };
    let mut agent = Agent::new(&cfg, publisher, LogindPowerOff, ceph);

    let mut sigterm = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("installing SIGINT handler")?;

    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                None => bail!("mqtt event loop ended unexpectedly"),
                Some(LoopEvent::Connected) => {
                    for t in agent.subscriptions() {
                        info!("subscribing {t}");
                        client.subscribe(t, QoS::AtLeastOnce).await.context("subscribe")?;
                    }
                    agent.on_connected().await;
                }
                Some(LoopEvent::Message(m)) => {
                    if agent.handle(&m).await == Outcome::Exit {
                        // Clean DISCONNECT: the broker keeps the retained `down`
                        // instead of firing the `offline` will.
                        disconnect(&client, loop_task).await;
                        info!("exiting (dry_run)");
                        return Ok(());
                    }
                }
            },
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }
    }
    info!("signal received; stopping");
    agent.on_terminate().await;
    disconnect(&client, loop_task).await;
    Ok(())
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
