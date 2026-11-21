//! Test-only fake of rack_controller's control socket. Never deploy this.
//!
//! usage: fake-controller <socket-path> [--fail CH]... [--mode OCTAL]
#![forbid(unsafe_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::ExitCode;

use relay_mqtt::fake_controller::{bind, run, FakeState, Shared};
use relay_mqtt::logging;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let _ = logging::init(&logging::directives(None));
    let code = serve().await;
    logging::flush();
    code
}

async fn serve() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut path: Option<PathBuf> = None;
    let mut state = FakeState::default();
    let mut mode: u32 = 0o660;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fail" => match args.next().and_then(|c| c.parse::<u8>().ok()) {
                Some(c) if c < 8 => state.fail_relays.push(c),
                _ => return usage(),
            },
            "--mode" => match args.next().and_then(|m| u32::from_str_radix(&m, 8).ok()) {
                Some(m) if m <= 0o777 => mode = m,
                _ => return usage(),
            },
            _ if path.is_none() && !a.starts_with('-') => path = Some(a.into()),
            _ => return usage(),
        }
    }
    let Some(path) = path else { return usage() };

    let listener = match bind(&path) {
        Ok(l) => l,
        Err(e) => {
            log::error!("{e:#}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)) {
        log::error!("chmod {}: {e}", path.display());
        return ExitCode::FAILURE;
    }
    log::info!("fake-controller listening on {}", path.display());
    let shared: Shared = std::sync::Arc::new(std::sync::Mutex::new(state));
    match run(listener, shared).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: fake-controller <socket-path> [--fail CH]... [--mode OCTAL]");
    ExitCode::from(2)
}
