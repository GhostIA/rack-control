//! A stand-in for rack_controller's socket server, implementing the socket
//! contract with in-memory state. Used by the tests and the docker e2e only;
//! it never touches hardware.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::codec::Framed;

use crate::controller::StrictLines;

/// Requests longer than this are rejected, per the contract.
pub const MAX_REQUEST: usize = 128;

#[derive(Debug, Clone)]
pub struct FakeState {
    /// true = commanded open.
    pub relay_open: [bool; 8],
    pub fan_duty: [f64; 4],
    pub ssr_on: [bool; 4],
    /// Channels that reply `err` to open/close (failure injection).
    pub fail_relays: Vec<u8>,
    /// Every request line received, in order.
    pub log: Vec<String>,
}

impl Default for FakeState {
    fn default() -> Self {
        FakeState {
            relay_open: [false; 8],
            fan_duty: [0.0; 4],
            ssr_on: [false; 4],
            fail_relays: Vec::new(),
            log: Vec::new(),
        }
    }
}

pub type Shared = Arc<Mutex<FakeState>>;

fn channel(tok: Option<&str>, max: u8) -> Option<usize> {
    match tok?.as_bytes() {
        [d @ b'0'..=b'9'] if d - b'0' < max => Some(usize::from(d - b'0')),
        _ => None,
    }
}

/// Handle one request line; returns the reply lines.
pub fn handle(state: &Shared, line: &str) -> Vec<String> {
    let mut s = state.lock().expect("fake state poisoned");
    s.log.push(line.to_string());
    let mut it = line.split(' ');
    let verb = it.next().unwrap_or("");
    let a = it.next();
    let b = it.next();
    let extra = it.next().is_some();
    let err = |m: &str| vec![format!("err {m}")];
    match (verb, extra) {
        ("open" | "close", false) if b.is_none() => {
            let Some(ch) = channel(a, 8) else {
                return err("bad channel");
            };
            if s.fail_relays.contains(&(ch as u8)) {
                return err("injected failure");
            }
            s.relay_open[ch] = verb == "open";
            vec![format!("ok {verb} {ch}")]
        }
        ("fan", false) => {
            let Some(ch) = channel(a, 4) else {
                return err("bad channel");
            };
            match b.and_then(|d| d.parse::<f64>().ok()) {
                Some(d) if (0.0..=1.0).contains(&d) => {
                    s.fan_duty[ch] = d;
                    vec![format!("ok fan {ch} {d:.2}")]
                }
                _ => err("bad duty"),
            }
        }
        ("ssr", false) => {
            let Some(ch) = channel(a, 4) else {
                return err("bad channel");
            };
            match b {
                Some(v @ ("on" | "off")) => {
                    s.ssr_on[ch] = v == "on";
                    vec![format!("ok ssr {ch} {v}")]
                }
                _ => err("bad value"),
            }
        }
        ("state", false) if a.is_none() => {
            let mut out = Vec::new();
            for (i, open) in s.relay_open.iter().enumerate() {
                let cmd = if *open { "open" } else { "closed" };
                out.push(format!("relay {i} cmd={cmd} attached=1"));
            }
            for (i, d) in s.fan_duty.iter().enumerate() {
                out.push(format!("fan {i} duty={d:.2} attached=1"));
            }
            for (i, on) in s.ssr_on.iter().enumerate() {
                let cmd = if *on { "on" } else { "off" };
                out.push(format!("ssr {i} cmd={cmd} attached=1"));
            }
            out.push("end".into());
            out
        }
        _ => err("unknown command"),
    }
}

async fn serve_conn(state: Shared, stream: UnixStream) -> Result<()> {
    let mut conn = Framed::new(stream, StrictLines::new(MAX_REQUEST));
    loop {
        let line = match conn.next().await {
            Some(Ok(l)) => l,
            None => return Ok(()),
            Some(Err(e)) => {
                let _ = conn.send("err bad request").await;
                return Err(e.into());
            }
        };
        // All reply lines go out in one write, like the real controller.
        for l in handle(&state, &line) {
            conn.feed(l).await?;
        }
        SinkExt::<String>::flush(&mut conn).await?;
    }
}

/// Bind `path` (replacing a stale socket file) and serve forever.
pub async fn serve(path: &Path, state: Shared) -> Result<()> {
    let listener = bind(path)?;
    run(listener, state).await
}

pub fn bind(path: &Path) -> Result<UnixListener> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        use std::os::unix::fs::FileTypeExt;
        if meta.file_type().is_socket() {
            std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))
}

/// Serve connections until cancelled. Connection tasks live in a JoinSet, so
/// dropping this future (e.g. aborting its task) closes every connection,
/// like a controller restart would.
pub async fn run(listener: UnixListener, state: Shared) -> Result<()> {
    let mut conns = tokio::task::JoinSet::new();
    loop {
        let (stream, _) = listener.accept().await?;
        while conns.try_join_next().is_some() {}
        let st = state.clone();
        conns.spawn(async move {
            if let Err(e) = serve_conn(st, stream).await {
                log::warn!("fake-controller connection: {e:#}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_contract() {
        let s: Shared = Default::default();
        assert_eq!(handle(&s, "open 3"), vec!["ok open 3"]);
        assert_eq!(handle(&s, "close 3"), vec!["ok close 3"]);
        assert!(handle(&s, "open 8")[0].starts_with("err "));
        assert!(handle(&s, "open")[0].starts_with("err "));
        assert!(handle(&s, "open 3 x")[0].starts_with("err "));
        assert!(handle(&s, "OPEN 3")[0].starts_with("err "));
        assert_eq!(handle(&s, "fan 1 0.5"), vec!["ok fan 1 0.50"]);
        assert!(handle(&s, "fan 1 1.5")[0].starts_with("err "));
        assert_eq!(handle(&s, "ssr 0 on"), vec!["ok ssr 0 on"]);
        handle(&s, "open 5");
        let st = handle(&s, "state");
        assert_eq!(st.len(), 8 + 4 + 4 + 1);
        assert_eq!(st[5], "relay 5 cmd=open attached=1");
        assert_eq!(st.last().unwrap(), "end");
        s.lock().unwrap().fail_relays.push(2);
        assert!(handle(&s, "open 2")[0].starts_with("err "));
    }
}
