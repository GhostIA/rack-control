//! Runs the real (debug) binary: the panic hook is installed, and the async
//! log is flushed on the error-exit and panic paths.
//!
//! Cargo.toml builds human-panic without debug assertions in dev/test, so the
//! hook here behaves as in release (human message + report file).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_relay-mqtt");

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("relay-mqtt-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(args: &[&str], tmp: &Path) -> Output {
    Command::new(BIN)
        .args(args)
        .env_remove("RUST_BACKTRACE")
        .env_remove("RUST_LOG")
        .env("TMPDIR", tmp)
        .output()
        .unwrap()
}

#[test]
fn panic_hook_is_installed_and_logs_the_message() {
    let tmp = scratch("panic");
    let out = run(&["--panic-test"], &tmp);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    // The line logged right before the panic was flushed first.
    let logged = err
        .find("INFO  relay_mqtt] panic-test: logged just before the panic")
        .unwrap_or_else(|| panic!("log line missing: {err}"));
    // Our addition: the panic message itself, on stderr.
    let msg = err
        .find("panicked at src/main.rs")
        .unwrap_or_else(|| panic!("panic message missing: {err}"));
    assert!(logged < msg, "{err}");
    assert!(err.contains("--panic-test (debug builds only)"), "{err}");
    // human-panic's message, naming the report it wrote into TMPDIR.
    assert!(err.contains("Well, this is embarrassing."), "{err}");
    assert!(err.contains("/tmp/<app_name>/stderr"), "{err}");
    let reports: Vec<_> = std::fs::read_dir(&tmp)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    assert_eq!(reports.len(), 1, "{err}");
    assert!(err.contains(&reports[0].display().to_string()), "{err}");
    let report = std::fs::read_to_string(&reports[0]).unwrap();
    assert!(
        report.contains("--panic-test (debug builds only)"),
        "{report}"
    );
    std::fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn broken_config_exits_2_with_the_chained_error() {
    let tmp = scratch("exit");
    let cfg = tmp.join("relay-mqtt.toml");
    std::fs::write(&cfg, "hots = \"10.0.0.1\"\n").unwrap();
    let out = run(&["--config", cfg.to_str().unwrap()], &tmp);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    let line = err
        .lines()
        .find(|l| l.contains(" ERROR relay_mqtt] "))
        .unwrap_or_else(|| panic!("error line missing: {err}"));
    // `{:#}`: the whole chain, outermost context first, on one line.
    assert!(
        line.contains(&format!(
            "in {}: parse config: unknown field",
            cfg.display()
        )),
        "{line}"
    );
    assert!(line.contains("`hots`"), "{line}");

    // A real RELAY_MQTT_* override is applied and validated (and an
    // unrelated one is ignored, not rejected as an unknown key).
    std::fs::write(&cfg, "").unwrap();
    let out = Command::new(BIN)
        .args(["--config", cfg.to_str().unwrap()])
        .env("RELAY_MQTT_KEEP_ALIVE_SECS", "1")
        .env("RELAY_MQTT_UNRELATED", "x")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(
        err.contains("(with RELAY_MQTT_* environment overrides): keep_alive_secs must be 5..=600"),
        "{err}"
    );

    let missing = tmp.join("missing.toml");
    let out = run(&["--config", missing.to_str().unwrap()], &tmp);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(
        err.contains(&format!("read {}: ", missing.display())),
        "{err}"
    );

    let out = run(&["--bogus"], &tmp);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("unknown argument \"--bogus\""), "{err}");
    std::fs::remove_dir_all(&tmp).unwrap();
}
