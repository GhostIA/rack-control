#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use relay_mqtt::backoff::Backoff;
use relay_mqtt::bridge::Bridge;
use relay_mqtt::config::{Config, Credentials, DEFAULT_CONFIG};
use relay_mqtt::controller::UnixController;
use relay_mqtt::mqtt::{options, run_eventloop, ClientMqtt};
use relay_mqtt::worker;
use rumqttc::AsyncClient;
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
        () = run(cfg) => ExitCode::SUCCESS,
        () = shutdown_signal() => {
            // Exit without DISCONNECT so the broker publishes the LWT `offline`.
            log::info!("signal received; exiting");
            ExitCode::SUCCESS
        }
    }
}

async fn run(cfg: Config) {
    // Credentials may be provisioned after the service starts; wait for them.
    let mut backoff = Backoff::new(Duration::from_secs(2), Duration::from_secs(60));
    let creds = loop {
        match Credentials::load(&cfg) {
            Ok(c) => break c,
            Err(e) => {
                let d = backoff.next_delay();
                log::error!("{e:#}; retrying in {}s", d.as_secs());
                tokio::time::sleep(d).await;
            }
        }
    };

    let (client, eventloop) = AsyncClient::new(options(&cfg, creds), 64);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let ctl = UnixController::new(cfg.socket.clone(), Duration::from_secs(3));
    let bridge = Bridge::new(ctl, ClientMqtt(client));

    tokio::join!(
        run_eventloop(eventloop, tx),
        worker::run(bridge, rx, worker::Timing::default()),
    );
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
