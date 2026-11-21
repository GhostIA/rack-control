//! Runs the real (debug) binary: the panic hook is installed, and the async
//! log is flushed on the error-exit and panic paths.
//!
//! Cargo.toml builds human-panic without debug assertions in dev/test, so the
//! hook here behaves as in release (human message + report file).

use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_rack-agent");

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rack-agent-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(args: &[&str], tmp: &PathBuf) -> Output {
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
        .find("INFO  rack_agent] panic-test: logged just before the panic")
        .unwrap_or_else(|| panic!("log line missing: {err}"));
    // Our addition: the panic message itself, for journald.
    let msg = err
        .find("panicked at src/main.rs")
        .unwrap_or_else(|| panic!("panic message missing: {err}"));
    assert!(logged < msg, "{err}");
    assert!(err.contains("--panic-test (debug builds only)"), "{err}");
    // human-panic's message, naming the report it wrote into TMPDIR.
    assert!(err.contains("Well, this is embarrassing."), "{err}");
    assert!(err.contains("journalctl -u rack-agent"), "{err}");
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
fn error_exit_flushes_the_chained_error() {
    let tmp = scratch("exit");
    let cfg = tmp.join("config.toml");
    std::fs::write(&cfg, "name = \"pc\"\nbroker_hots = \"x\"\n").unwrap();
    let out = run(&["check", "--config", cfg.to_str().unwrap()], &tmp);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    let line = err
        .lines()
        .find(|l| l.contains(" ERROR rack_agent] "))
        .unwrap_or_else(|| panic!("error line missing: {err}"));
    // `{:#}`: the whole chain, outermost context first, on one line.
    assert!(
        line.contains(&format!("in config {}: parsing config: ", cfg.display())),
        "{line}"
    );
    assert!(line.contains("broker_hots"), "{line}");

    // Real RACK_AGENT_* overrides; an unrelated one is ignored.
    std::fs::write(&cfg, "name = \"pc\"\n").unwrap();
    let out = Command::new(BIN)
        .args(["check", "--config", cfg.to_str().unwrap()])
        .env("RACK_AGENT_BROKER_PORT", "18883")
        .env("RACK_AGENT_NAME", "intel-x")
        .env("RACK_AGENT_UNRELATED", "x")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "ok: intel-x -> 192.168.8.1:18883\n"
    );

    let missing = tmp.join("missing.toml");
    let out = run(&["run", "--config", missing.to_str().unwrap()], &tmp);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains(&format!("reading config {}", missing.display())),
        "{err}"
    );
    std::fs::remove_dir_all(&tmp).unwrap();
}
