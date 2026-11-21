//! Logging: the `log` facade, written asynchronously by fast_log (a worker
//! thread formats and writes each batch) to stderr: the journal under
//! systemd, `/tmp/<app_name>/stderr` under the SBC4's Phidget launcher.
//!
//! Filtering uses `RUST_LOG`-style directives (`info`,
//! `info,relay_mqtt::worker=debug`), parsed by `env_filter`, the parser
//! env_logger uses. Because writes are asynchronous, every exit path must call
//! [`flush`], or its last lines can be lost.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use fast_log::appender::{Command, FastLogRecord, LogAppender, RecordFormat};

/// Used when neither the config (`log_level`) nor `RUST_LOG` sets a filter.
pub const DEFAULT_DIRECTIVES: &str = "info";

/// Records queued for the writer thread. A full queue blocks the caller
/// (as a blocking stderr write did with env_logger) instead of growing
/// without bound.
const QUEUE_LEN: usize = 4096;

/// [`flush`] gives up after this long, so a wedged stderr can never hang an
/// exit (or the panic hook).
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// The filter in effect: the config's `log_level` if set, else `RUST_LOG`,
/// else [`DEFAULT_DIRECTIVES`].
pub fn directives(configured: Option<&str>) -> String {
    match configured {
        Some(d) => d.to_string(),
        None => std::env::var("RUST_LOG").unwrap_or_else(|_| DEFAULT_DIRECTIVES.to_string()),
    }
}

/// Checks a `log_level` value strictly (`RUST_LOG` itself stays lenient, as
/// with env_logger: bad parts are warned about and ignored).
pub fn check_directives(d: &str) -> Result<()> {
    // env_filter reads "" as "errors only", which would silently hide the
    // info and warn lines.
    if d.trim().is_empty() {
        return Err(anyhow!("must not be empty (use e.g. \"info\")"));
    }
    env_filter::Builder::new()
        .try_parse(d)
        .map(|_| ())
        .map_err(|e| anyhow!("{e}"))
}

/// Installs the stderr logger. Errors if a logger is already installed.
pub fn init(directives: &str) -> Result<()> {
    init_with(directives, Stderr)
}

fn init_with(directives: &str, appender: impl LogAppender + 'static) -> Result<()> {
    let filter = env_filter::Builder::new().parse(directives).build();
    let level = filter.filter();
    fast_log::init(
        fast_log::Config::new()
            .custom(appender)
            .format(Format)
            .add_filter(Directives(filter))
            .level(level)
            .chan_len(Some(QUEUE_LEN)),
    )
    .map_err(|e| anyhow!("{e}"))
    .context("installing the logger")?;
    INSTALLED.store(true, Ordering::SeqCst);
    Ok(())
}

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Whether [`init`] has succeeded in this process.
pub fn installed() -> bool {
    INSTALLED.load(Ordering::SeqCst)
}

/// Waits (at most [`FLUSH_TIMEOUT`]) until everything logged so far has been
/// written. A no-op before [`init`].
pub fn flush() {
    let (tx, rx) = mpsc::channel();
    // On a helper thread, so the wait is bounded even if the writer thread is
    // stuck (or is the thread that panicked).
    let spawned = std::thread::Builder::new()
        .name("log-flush".into())
        .spawn(move || {
            log::logger().flush();
            let _ = tx.send(());
        });
    if spawned.is_ok() {
        let _ = rx.recv_timeout(FLUSH_TIMEOUT);
    }
}

/// `[2026-11-21T12:34:56Z INFO  relay_mqtt::worker] message`, env_logger's
/// default layout: UTC timestamp, level, target (the module path).
struct Format;

impl RecordFormat for Format {
    fn do_format(&self, r: &mut FastLogRecord) {
        if r.command == Command::CommandRecord {
            r.formated = format_line(&r.now, r.level, &r.target, &r.args);
        }
    }
}

fn format_line(now: &std::time::SystemTime, level: log::Level, target: &str, msg: &str) -> String {
    // "YYYY-MM-DDThh:mm:ss.nnnnnnnnn": keep whole seconds, like env_logger.
    let ts = fastdate::DateTime::from(*now).display(false);
    let secs = ts.get(..19).unwrap_or(&ts);
    format!("[{secs}Z {level:<5} {target}] {msg}\n")
}

struct Directives(env_filter::Filter);

impl fast_log::filter::Filter for Directives {
    fn do_log(&self, record: &log::Record) -> bool {
        self.0.matches(record)
    }
}

/// One `write_all` per batch. Write errors are ignored (as env_logger
/// does): fast_log's own console appender panics on them, which with
/// `panic = "abort"` would kill the bridge.
struct Stderr;

impl LogAppender for Stderr {
    fn do_logs(&mut self, records: &[FastLogRecord]) {
        let buf: String = records.iter().map(|r| r.formated.as_str()).collect();
        if !buf.is_empty() {
            let mut err = std::io::stderr().lock();
            let _ = err.write_all(buf.as_bytes());
            let _ = err.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::UNIX_EPOCH;

    const MARK: &str = "last line before exit";

    /// A deliberately slow sink for this test's line: without a working
    /// flush, a check right after logging would find nothing written yet.
    /// (Other tests' records, if any get through, are not slowed down.)
    struct Slow(Arc<Mutex<String>>);

    impl LogAppender for Slow {
        fn do_logs(&mut self, records: &[FastLogRecord]) {
            if records.iter().any(|r| r.formated.contains(MARK)) {
                std::thread::sleep(Duration::from_millis(200));
            }
            let mut out = self.0.lock().unwrap();
            for r in records {
                out.push_str(&r.formated);
            }
        }
    }

    // The only test that installs the (process-global) logger.
    #[test]
    fn flush_writes_the_last_line_before_returning() {
        let out = Arc::new(Mutex::new(String::new()));
        // Only this module logs, so the other tests stay quiet and fast.
        init_with("off,relay_mqtt::logging::tests=debug", Slow(out.clone())).unwrap();
        log::debug!("{MARK}");
        log::trace!("filtered out");
        log::error!(target: "other", "filtered out");
        flush();
        let got = out.lock().unwrap().clone();
        assert!(
            got.contains(&format!(" DEBUG relay_mqtt::logging::tests] {MARK}\n")),
            "{got:?}"
        );
        assert!(!got.contains("filtered out"), "{got:?}");
        // A second logger cannot be installed.
        assert!(init("info").is_err());
    }

    #[test]
    fn line_layout_matches_env_logger() {
        let t = UNIX_EPOCH + Duration::from_millis(1_795_000_000_123);
        assert_eq!(
            format_line(&t, log::Level::Info, "relay_mqtt::worker", "hi"),
            "[2026-11-18T11:06:40Z INFO  relay_mqtt::worker] hi\n"
        );
    }

    #[test]
    fn directives_precedence_and_check() {
        assert_eq!(directives(Some("debug")), "debug");
        check_directives("info,paho_mqtt::c_trace=debug").unwrap();
        check_directives("warn").unwrap();
        assert!(check_directives("info,x=nope").is_err());
        assert!(check_directives("a=b=c").is_err());
        assert!(check_directives("").is_err());
        assert!(check_directives("  ").is_err());
    }
}
