//! Production implementations of `PowerOff` and `CephRunner`.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::agent::{CephRunner, PowerOff, CEPH_BIN};

/// logind `org.freedesktop.login1.Manager.PowerOff(interactive=false)` on the
/// system bus. As user `rack-agent` this is authorised by the polkit rule.
pub struct LogindPowerOff;

impl PowerOff for LogindPowerOff {
    async fn power_off(&mut self) -> Result<()> {
        let conn = zbus::Connection::system()
            .await
            .context("connecting to the D-Bus system bus")?;
        conn.call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "PowerOff",
            &(false,),
        )
        .await
        .context("calling org.freedesktop.login1.Manager.PowerOff(false)")?;
        Ok(())
    }
}

/// Runs `/usr/bin/ceph <args>` directly (no shell), killing it after a timeout
/// so an unreachable monitor cannot wedge the agent forever.
pub struct CephCli {
    pub timeout: Duration,
}

impl CephRunner for CephCli {
    async fn run(&mut self, args: &[String]) -> Result<()> {
        let args = args.to_vec();
        let limit = self.timeout;
        tokio::task::spawn_blocking(move || run_blocking(CEPH_BIN, &args, limit))
            .await
            .context("ceph task panicked")?
    }
}

fn run_blocking(program: &str, args: &[String], limit: Duration) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning {program}"))?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().context("waiting for ceph")? {
            if status.success() {
                return Ok(());
            }
            bail!("{program} exited with {status}");
        }
        if start.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{program} timed out after {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_blocking_reports_status_and_timeout() {
        assert!(run_blocking("/usr/bin/true", &[], Duration::from_secs(5)).is_ok());
        assert!(run_blocking("/usr/bin/false", &[], Duration::from_secs(5)).is_err());
        let e = run_blocking("/bin/sleep", &["5".into()], Duration::from_millis(200))
            .unwrap_err()
            .to_string();
        assert!(e.contains("timed out"), "{e}");
        assert!(run_blocking("/nonexistent/ceph", &[], Duration::from_secs(1)).is_err());
    }
}
