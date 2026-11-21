//! Production implementations of `PowerOff` and `CephRunner`.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

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
        run_with_timeout(CEPH_BIN, args, self.timeout).await
    }
}

async fn run_with_timeout(program: &str, args: &[String], limit: Duration) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning {program}"))?;
    match tokio::time::timeout(limit, child.wait()).await {
        Ok(status) => {
            let status = status.context("waiting for ceph")?;
            if !status.success() {
                bail!("{program} exited with {status}");
            }
            Ok(())
        }
        Err(_) => {
            let _ = child.kill().await;
            bail!("{program} timed out after {limit:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn run_with_timeout_reports_status_and_timeout() {
        let t = Duration::from_secs(5);
        assert!(run_with_timeout("/usr/bin/true", &[], t).await.is_ok());
        assert!(run_with_timeout("/usr/bin/false", &[], t).await.is_err());
        let e = run_with_timeout("/bin/sleep", &["5".into()], Duration::from_millis(200))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("timed out"), "{e}");
        assert!(
            run_with_timeout("/nonexistent/ceph", &[], Duration::from_secs(1))
                .await
                .is_err()
        );
    }
}
