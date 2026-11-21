#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use backon::Retryable;
use relay_mqtt::bridge::Bridge;
use relay_mqtt::config::{Config, Credentials, DEFAULT_CONFIG};
use relay_mqtt::controller::UnixController;
use relay_mqtt::mqtt::{self, ClientMqtt};
use relay_mqtt::worker;
use tokio::signal::unix::{signal, SignalKind};

const USAGE: &str = "usage: relay-mqtt [--config PATH]   (default /etc/rack/relay-mqtt.toml)";

fn parse_args() -> Result<PathBuf, String> {
    let mut args = std::env::args().skip(1);
    let mut path = PathBuf::from(DEFAULT_CONFIG);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" | "--config" => {
                path = args.next().ok_or("--config needs a path")?.into();
            }
            "-h" | "--help" => return Err(USAGE.into()),
            "-V" | "--version" => {
                return Err(format!("relay-mqtt {}", env!("CARGO_PKG_VERSION")));
            }
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    Ok(path)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let path = match parse_args() {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };
    // A broken config is a deploy error: exit and let systemd show it.
    let cfg = match Config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("{e:#}");
            return ExitCode::from(2);
        }
    };
    log::info!(
        "relay-mqtt {} broker={}:{} user={} socket={}",
        env!("CARGO_PKG_VERSION"),
        cfg.host,
        cfg.port,
        cfg.username,
        cfg.socket.display()
    );

    tokio::select! {
        () = run(cfg) => ExitCode::FAILURE,
        () = shutdown_signal() => {
            // Exit without DISCONNECT so the broker publishes the LWT `offline`.
            log::info!("signal received; exiting");
            ExitCode::SUCCESS
        }
    }
}

async fn run(cfg: Config) {
    // Credentials may be provisioned after the service starts; wait for them.
    let creds = (|| async { Credentials::load(&cfg) })
        .retry(
            backon::ExponentialBuilder::new()
                .with_min_delay(Duration::from_secs(2))
                .with_max_delay(Duration::from_secs(60))
                .without_max_times(),
        )
        .notify(|e, d| log::error!("{e:#}; retrying in {}s", d.as_secs()))
        .await
        .expect("retries forever");

    let opts = match mqtt::connect_options(&cfg, creds) {
        Ok(o) => o,
        Err(e) => {
            log::error!("{e:#}");
            return;
        }
    };
    let cli = match paho_mqtt::AsyncClient::new(mqtt::create_options(&cfg)) {
        Ok(c) => c,
        Err(e) => {
            log::error!("create MQTT client: {e}");
            return;
        }
    };
    // Never destroy the Paho client: MQTTAsync_destroy sends a DISCONNECT,
    // which would suppress the LWT. Leaking one handle means the process exit
    // (on SIGTERM) just closes the socket, and the broker publishes the
    // retained `offline` will.
    std::mem::forget(cli.clone());
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    // Callbacks first, so the first CONNACK is not missed.
    mqtt::install_callbacks(&cli, tx);
    mqtt::start(&cli, opts);
    mqtt::spawn_watchdog(cli.clone(), Duration::from_secs(60));

    let ctl = UnixController::new(cfg.socket.clone(), Duration::from_secs(3));
    let bridge = Bridge::new(ctl, ClientMqtt::new(cli));
    worker::run(bridge, rx, worker::Timing::default()).await;
    log::error!("internal channel closed");
}

async fn shutdown_signal() {
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("cannot install SIGTERM handler: {e}");
            return std::future::pending().await;
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}
