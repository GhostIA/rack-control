//! Client side of the rack_controller Unix socket.

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio::time::timeout;
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder, Framed, LinesCodec, LinesCodecError};

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

/// Line framing for the controller socket, in both directions.
///
/// It is `tokio_util`'s [`LinesCodec`] with a length limit (a longer line is
/// `MaxLineLengthExceeded`, a line that isn't UTF-8 is an `InvalidData` I/O
/// error), made byte-exact in two ways: a line ending in `\r\n` is an error
/// (LinesCodec would silently strip the `\r`), and a final line cut off by
/// EOF (no `\n`) is an error instead of being returned as a complete line,
/// so a malformed or half-written reply can never be taken for an `ok`.
#[derive(Debug, Clone)]
pub struct StrictLines(LinesCodec);

impl StrictLines {
    pub fn new(max_length: usize) -> Self {
        StrictLines(LinesCodec::new_with_max_length(max_length))
    }
}

impl Decoder for StrictLines {
    type Item = String;
    type Error = LinesCodecError;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<String>, LinesCodecError> {
        if let Some(i) = buf.iter().position(|b| *b == b'\n') {
            if i > 0 && buf[i - 1] == b'\r' {
                // The caller drops the connection on any framing error.
                return Err(LinesCodecError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "line ends in \\r\\n",
                )));
            }
        }
        self.0.decode(buf)
    }

    fn decode_eof(&mut self, buf: &mut BytesMut) -> Result<Option<String>, LinesCodecError> {
        match self.decode(buf)? {
            Some(line) => Ok(Some(line)),
            None if buf.is_empty() => Ok(None),
            None => {
                buf.clear();
                Err(LinesCodecError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed mid-line",
                )))
            }
        }
    }
}

impl<T: AsRef<str>> Encoder<T> for StrictLines {
    type Error = LinesCodecError;

    fn encode(&mut self, line: T, buf: &mut BytesMut) -> Result<(), LinesCodecError> {
        self.0.encode(line, buf)
    }
}

/// A line-framed controller connection.
pub type LineStream<S> = Framed<S, StrictLines>;

/// Real controller client: keeps one connection open and reconnects lazily.
pub struct UnixController {
    path: PathBuf,
    io_timeout: Duration,
    conn: Option<LineStream<UnixStream>>,
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
            self.conn = Some(Framed::new(stream, StrictLines::new(MAX_LINE)));
            self.reconnected = true;
        }
        let conn = self.conn.as_mut().expect("connection just ensured");
        let fut = async {
            if let Err(e) = conn.send(request).await {
                return Err((true, anyhow!("write: {e}")));
            }
            let mut out = Vec::new();
            loop {
                let l = match conn.next().await {
                    Some(Ok(l)) => l,
                    None => {
                        return Err((out.is_empty(), anyhow!("controller closed the connection")))
                    }
                    Some(Err(e)) => {
                        let reset = matches!(&e, LinesCodecError::Io(io)
                            if io.kind() == std::io::ErrorKind::ConnectionReset);
                        return Err((out.is_empty() && reset, anyhow!("read reply: {e}")));
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
        if !conn.read_buffer().is_empty() {
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

    fn decode_all(input: &[u8], max: usize) -> Vec<Result<String, String>> {
        let mut c = StrictLines::new(max);
        let mut buf = BytesMut::from(input);
        let mut out = Vec::new();
        loop {
            match c.decode_eof(&mut buf) {
                Ok(Some(l)) => out.push(Ok(l)),
                Ok(None) => return out,
                Err(e) => {
                    out.push(Err(e.to_string()));
                    return out;
                }
            }
        }
    }

    #[test]
    fn strict_lines_framing() {
        assert_eq!(
            decode_all(b"ok open 3\nsecond\n", 16),
            vec![Ok("ok open 3".to_string()), Ok("second".to_string())]
        );
        assert_eq!(decode_all(b"abcd\n", 4), vec![Ok("abcd".to_string())]);
        assert!(decode_all(b"abcdefgh\n", 4)[0].is_err(), "too long");
        assert!(
            decode_all(b"abcdefgh", 4)[0].is_err(),
            "too long, no newline"
        );
        assert!(decode_all(b"ok\xff\n", 16)[0].is_err(), "not UTF-8");
        assert!(
            decode_all(b"ok open 3\r\n", 16)[0].is_err(),
            "CR is not stripped"
        );
        assert_eq!(
            decode_all(b"a\rb\n", 16),
            vec![Ok("a\rb".to_string())],
            "inner CR is kept (and fails the exact match later)"
        );
        let cut = decode_all(b"ok open 3", 16);
        assert_eq!(cut.len(), 1);
        assert!(cut[0].as_ref().unwrap_err().contains("mid-line"), "{cut:?}");
        assert!(decode_all(b"", 16).is_empty());
    }

    #[test]
    fn strict_lines_encodes_one_line() {
        let mut c = StrictLines::new(16);
        let mut buf = BytesMut::new();
        c.encode("open 3", &mut buf).unwrap();
        assert_eq!(&buf[..], b"open 3\n");
    }
}
