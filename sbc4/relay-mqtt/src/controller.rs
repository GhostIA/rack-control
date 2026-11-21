//! Client side of the rack_controller Unix socket.

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::timeout;

/// Longest reply line accepted from the controller (requests are <=128 bytes;
/// replies are short too). Longer lines are a protocol error.
pub const MAX_LINE: usize = 512;
/// Upper bound on lines in a `state` reply before `end`.
pub const MAX_STATE_LINES: usize = 64;

/// How many reply lines a request expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyKind {
    /// Exactly one line (`ok ...` / `err ...`).
    Single,
    /// Lines up to and including `end` (the `state` request).
    UntilEnd,
}

/// One request/response exchange with the controller. Implemented by the real
/// Unix-socket client and by test fakes.
pub trait Controller {
    /// Send one request line (no newline) and collect the reply lines
    /// (without newlines). For `UntilEnd` the last line is `end`.
    fn exchange(
        &mut self,
        request: &str,
        kind: ReplyKind,
    ) -> impl Future<Output = Result<Vec<String>>> + Send;

    /// Returns true (once) if a new socket connection was opened since the
    /// last call. A fresh connection may mean the controller restarted, which
    /// resets its commanded state, so the caller should re-sync.
    fn take_reconnected(&mut self) -> bool;

    /// Drop the current connection (after an unexpected reply), so the next
    /// request starts on a fresh, in-step stream.
    fn reset(&mut self);
}

/// Read one `\n`-terminated line of at most `max` bytes. `Ok(None)` on clean
/// EOF before any byte. The newline is stripped; a line that is not valid
/// UTF-8, too long, or cut off by EOF is an error.
pub async fn read_line_bounded<R>(reader: &mut R, max: usize) -> Result<Option<String>>
where
    R: AsyncBufRead + Unpin,
{
    let mut buf = Vec::new();
    let n = reader
        .take(max as u64 + 1)
        .read_until(b'\n', &mut buf)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        if buf.len() > max {
            bail!("line longer than {max} bytes");
        }
        bail!("connection closed mid-line");
    }
    buf.pop();
    if buf.len() > max {
        bail!("line longer than {max} bytes");
    }
    String::from_utf8(buf)
        .map(Some)
        .context("line is not UTF-8")
}

/// Real controller client: keeps one connection open and reconnects lazily.
pub struct UnixController {
    path: PathBuf,
    io_timeout: Duration,
    conn: Option<BufReader<UnixStream>>,
    reconnected: bool,
}

impl UnixController {
    pub fn new(path: impl Into<PathBuf>, io_timeout: Duration) -> Self {
        UnixController {
            path: path.into(),
            io_timeout,
            conn: None,
            reconnected: false,
        }
    }

    /// One attempt. On error, the bool is true when the failure means the
    /// kept-open connection was stale (write failed, or EOF/reset before any
    /// reply byte): the request cannot have been executed from a reply we
    /// lost, so one retry on a fresh connection is safe. Timeouts are never
    /// "stale": a slow controller must not get the command a second time.
    async fn once(
        &mut self,
        request: &str,
        kind: ReplyKind,
    ) -> std::result::Result<Vec<String>, (bool, anyhow::Error)> {
        if self.conn.is_none() {
            let stream = timeout(self.io_timeout, UnixStream::connect(&self.path))
                .await
                .map_err(|_| (false, anyhow!("connect {} timed out", self.path.display())))?
                .map_err(|e| (false, anyhow!("connect {}: {e}", self.path.display())))?;
            self.conn = Some(BufReader::new(stream));
            self.reconnected = true;
        }
        let conn = self.conn.as_mut().expect("connection just ensured");
        let fut = async {
            let mut line = Vec::with_capacity(request.len() + 1);
            line.extend_from_slice(request.as_bytes());
            line.push(b'\n');
            if let Err(e) = conn.get_mut().write_all(&line).await {
                return Err((true, anyhow!("write: {e}")));
            }
            let mut out = Vec::new();
            loop {
                let l = match read_line_bounded(conn, MAX_LINE).await {
                    Ok(Some(l)) => l,
                    Ok(None) => {
                        return Err((out.is_empty(), anyhow!("controller closed the connection")))
                    }
                    Err(e) => {
                        let reset = e
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|io| io.kind() == std::io::ErrorKind::ConnectionReset);
                        return Err((out.is_empty() && reset, e));
                    }
                };
                let done = match kind {
                    ReplyKind::Single => true,
                    // An `err` reply to `state` is a single line, no `end`.
                    ReplyKind::UntilEnd => {
                        l == "end" || (out.is_empty() && (l == "err" || l.starts_with("err ")))
                    }
                };
                out.push(l);
                if done {
                    return Ok(out);
                }
                if out.len() > MAX_STATE_LINES {
                    return Err((
                        false,
                        anyhow!("state reply exceeded {MAX_STATE_LINES} lines"),
                    ));
                }
            }
        };
        let out = timeout(self.io_timeout, fut)
            .await
            .map_err(|_| (false, anyhow!("controller reply timed out")))??;
        if !conn.buffer().is_empty() {
            // More bytes than one reply: the stream is out of step. Drop it so
            // a leftover line is never read as the reply to the next request.
            log::warn!("controller sent unexpected extra data; reconnecting");
            self.conn = None;
        }
        Ok(out)
    }
}

impl Controller for UnixController {
    async fn exchange(&mut self, request: &str, kind: ReplyKind) -> Result<Vec<String>> {
        let reused = self.conn.is_some();
        match self.once(request, kind).await {
            Ok(v) => Ok(v),
            Err((stale, e)) => {
                // Any error leaves the stream in an unknown position: drop it.
                self.conn = None;
                if !(reused && stale) {
                    return Err(e);
                }
                // The kept-open connection was dead (controller restarted).
                // Retry once on a fresh one.
                log::info!("controller connection was stale ({e:#}); reconnecting");
                self.once(request, kind).await.map_err(|(_, e)| {
                    self.conn = None;
                    e
                })
            }
        }
    }

    fn take_reconnected(&mut self) -> bool {
        std::mem::take(&mut self.reconnected)
    }

    fn reset(&mut self) {
        self.conn = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounded_reader() {
        let data: &[u8] = b"ok open 3\nsecond\n";
        let mut r = BufReader::new(data);
        assert_eq!(
            read_line_bounded(&mut r, 16).await.unwrap().as_deref(),
            Some("ok open 3")
        );
        assert_eq!(
            read_line_bounded(&mut r, 16).await.unwrap().as_deref(),
            Some("second")
        );
        assert_eq!(read_line_bounded(&mut r, 16).await.unwrap(), None);

        let mut exact = BufReader::new(&b"abcd\n"[..]);
        assert_eq!(
            read_line_bounded(&mut exact, 4).await.unwrap().as_deref(),
            Some("abcd")
        );

        let mut long = BufReader::new(&b"abcdefgh\n"[..]);
        assert!(read_line_bounded(&mut long, 4).await.is_err());

        let mut cut = BufReader::new(&b"abc"[..]);
        assert!(read_line_bounded(&mut cut, 16).await.is_err());
    }
}
