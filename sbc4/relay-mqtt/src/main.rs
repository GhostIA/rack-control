#![forbid(unsafe_code)]

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use backon::Retryable;
use relay_mqtt::bridge::Bridge;
use relay_mqtt::config::{Config, Credentials, DEFAULT_CONFIG};
use relay_mqtt::controller::UnixController;
use relay_mqtt::logging;
use relay_mqtt::mqtt::{self, ClientMqtt};
use relay_mqtt::worker;
use tokio::signal::unix::{signal, SignalKind};

const USAGE: &str = "usage: relay-mqtt [--config PATH]   (default /etc/rack/relay-mqtt.toml)";

#[derive(Debug, PartialEq, Eq)]
enum Cli {
    Run(PathBuf),
    Help,
    Version,
}

fn parse_args(args: &[String]) -> Result<Cli> {
    let mut it = args.iter();
    let mut path = PathBuf::from(DEFAULT_CONFIG);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-c" | "--config" => path = it.next().context("--config needs a path")?.into(),
            "-h" | "--help" => return Ok(Cli::Help),
            "-V" | "--version" => return Ok(Cli::Version),
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    Ok(Cli::Run(path))
}

fn main() -> ExitCode {
    install_panic_hook();
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(debug_assertions)]
    if args.first().map(String::as_str) == Some("--panic-test") {
        panic_test();
    }
    let code = real_main(&args);
    // fast_log writes on its own thread: flush so the last lines are never
    // lost, whichever way we exit.
    logging::flush();
    code
}

fn real_main(args: &[String]) -> ExitCode {
    let path = match parse_args(args) {
        Ok(Cli::Run(p)) => p,
        Ok(Cli::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Cli::Version) => {
            println!("relay-mqtt {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            fatal(&e);
            return ExitCode::from(2);
        }
    };
    // A broken config is a deploy error: exit 2 and let the operator see it.
    let cfg = match Config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            fatal(&e);
            return ExitCode::from(2);
        }
    };
    if let Err(e) = logging::init(&logging::directives(cfg.log_level.as_deref())) {
        fatal(&e);
        return ExitCode::FAILURE;
    }
    log::info!(
        "relay-mqtt {} broker={}:{} user={} socket={}",
        env!("CARGO_PKG_VERSION"),
        cfg.host,
        cfg.port,
        cfg.username,
        cfg.socket.display()
    );
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")
    {
        Ok(rt) => rt,
        Err(e) => {
            fatal(&e);
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async {
        tokio::select! {
            r = run(cfg) => {
                if let Err(e) = r {
                    log::error!("{e:#}");
                }
                ExitCode::FAILURE
            }
            () = shutdown_signal() => {
                // Exit without DISCONNECT so the broker publishes the LWT `offline`.
                log::info!("signal received; exiting");
                ExitCode::SUCCESS
            }
        }
    })
}

/// Logs a fatal error with its whole context chain (`{:#}`), installing the
/// default logger first if the config never got that far.
fn fatal(e: &anyhow::Error) {
    if !logging::installed() {
        let _ = logging::init(&logging::directives(None));
    }
    if logging::installed() {
        log::error!("{e:#}");
    } else {
        let _ = writeln!(std::io::stderr(), "error: {e:#}");
    }
}

/// human-panic, plus two things it does not do for a daemon: flush the log
/// first, and put the panic message itself on stderr. In release builds
/// human-panic writes the message only to its report file in the temp dir.
fn install_panic_hook() {
    human_panic::setup_panic!(human_panic::metadata!().support(
        "- The panic message is also on stderr: /tmp/<app_name>/stderr under the \
         Phidget launcher, or journalctl -u relay-mqtt under systemd"
    ));
    let human = human_panic::PanicStyle::default() == human_panic::PanicStyle::Human;
    let next = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        logging::flush();
        if human {
            let _ = writeln!(
                std::io::stderr(),
                "relay-mqtt {}: {info}",
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
    log::info!("panic-test: logged just before the panic");
    panic!("--panic-test (debug builds only)");
}

/// Runs the bridge; it only returns on a fatal error.
async fn run(cfg: Config) -> Result<()> {
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
        .context("loading credentials")?;

    let opts = mqtt::connect_options(&cfg, creds)?;
    let cli =
        paho_mqtt::AsyncClient::new(mqtt::create_options(&cfg)).context("create MQTT client")?;
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
    Err(anyhow!("internal channel closed"))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cli_parsing() {
        assert_eq!(
            parse_args(&a(&[])).unwrap(),
            Cli::Run(PathBuf::from(DEFAULT_CONFIG))
        );
        assert_eq!(
            parse_args(&a(&["-c", "/x.toml"])).unwrap(),
            Cli::Run("/x.toml".into())
        );
        assert_eq!(parse_args(&a(&["--help"])).unwrap(), Cli::Help);
        assert_eq!(parse_args(&a(&["-V"])).unwrap(), Cli::Version);
        let e = format!("{:#}", parse_args(&a(&["--config"])).unwrap_err());
        assert_eq!(e, "--config needs a path");
        let e = format!("{:#}", parse_args(&a(&["--bogus"])).unwrap_err());
        assert!(e.starts_with("unknown argument \"--bogus\""), "{e}");
    }
}
