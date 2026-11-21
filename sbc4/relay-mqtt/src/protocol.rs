//! Pure parsing/formatting for the MQTT topics and the rack_controller
//! Unix-socket protocol. Everything here is byte-exact: anything not matching
//! the contract is rejected, never "fixed up".

use std::fmt;

/// Number of relay channels on the controller (port 3 ch0-3 = 0-3, port 4 ch0-3 = 4-7).
pub const RELAY_CHANNELS: u8 = 8;

/// MQTT subscription filter for relay commands.
pub const SET_FILTER: &str = "rack/relay/+/set";
/// Retained availability topic (LWT = `offline`).
pub const STATUS_TOPIC: &str = "rack/sbc4/status";
pub const STATUS_ONLINE: &str = "online";
pub const STATUS_OFFLINE: &str = "offline";

/// A validated relay channel, 0..=7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Channel(u8);

impl Channel {
    pub fn new(n: u8) -> Option<Self> {
        (n < RELAY_CHANNELS).then_some(Channel(n))
    }

    pub fn index(self) -> usize {
        usize::from(self.0)
    }

    pub fn all() -> impl Iterator<Item = Channel> {
        (0..RELAY_CHANNELS).map(Channel)
    }

    /// Parse a channel token exactly: one ASCII digit `0`..=`7`. Rejects
    /// signs, whitespace, leading zeros and anything else.
    pub fn parse(s: &str) -> Option<Self> {
        match s.as_bytes() {
            [d @ b'0'..=b'7'] => Some(Channel(d - b'0')),
            _ => None,
        }
    }

    pub fn state_topic(self) -> String {
        format!("rack/relay/{}/state", self.0)
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A relay command received on `rack/relay/<ch>/set`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Open,
    Close,
}

impl Command {
    /// Byte-exact payload match: only `open` or `close`.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        match payload {
            b"open" => Some(Command::Open),
            b"close" => Some(Command::Close),
            _ => None,
        }
    }

    pub fn verb(self) -> &'static str {
        match self {
            Command::Open => "open",
            Command::Close => "close",
        }
    }

    /// The relay state that results from this command once acknowledged.
    pub fn resulting_state(self) -> RelayState {
        match self {
            Command::Open => RelayState::Open,
            Command::Close => RelayState::Closed,
        }
    }

    /// Socket request line (without the trailing newline).
    pub fn request(self, ch: Channel) -> String {
        format!("{} {}", self.verb(), ch)
    }

    /// True only for an exact success reply: `ok <verb> <ch>` per the
    /// contract, or a bare `ok`. Everything else (including `err ...` and an
    /// `ok` naming a different command/channel) is a failure.
    pub fn is_ok_reply(self, ch: Channel, reply: &str) -> bool {
        reply == "ok" || reply == format!("ok {}", self.request(ch))
    }
}

/// Commanded relay state as published on `rack/relay/<ch>/state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayState {
    Open,
    Closed,
}

impl RelayState {
    pub fn payload(self) -> &'static str {
        match self {
            RelayState::Open => "open",
            RelayState::Closed => "closed",
        }
    }
}

/// Parse `rack/relay/<ch>/set` into a channel. `None` for any other topic or
/// an invalid channel.
pub fn parse_set_topic(topic: &str) -> Option<Channel> {
    let mut parts = topic.split('/');
    match (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) {
        (Some("rack"), Some("relay"), Some(ch), Some("set"), None) => Channel::parse(ch),
        _ => None,
    }
}

/// One relay line from the controller's `state` reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayReport {
    pub channel: Channel,
    pub state: RelayState,
    pub attached: bool,
}

/// Parse one `relay <ch> cmd=<open|closed> attached=<0|1>` line.
pub fn parse_relay_line(line: &str) -> Option<RelayReport> {
    let mut it = line.split(' ');
    let (kind, ch, cmd, att, rest) = (it.next(), it.next(), it.next(), it.next(), it.next());
    if kind != Some("relay") || rest.is_some() {
        return None;
    }
    let channel = Channel::parse(ch?)?;
    let state = match cmd? {
        "cmd=open" => RelayState::Open,
        "cmd=closed" => RelayState::Closed,
        _ => return None,
    };
    let attached = match att? {
        "attached=1" => true,
        "attached=0" => false,
        _ => return None,
    };
    Some(RelayReport {
        channel,
        state,
        attached,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum StateError {
    Malformed(String),
    Duplicate(Channel),
    Missing(Channel),
    MissingEnd,
    ControllerError(String),
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StateError::Malformed(l) => write!(f, "malformed relay line {l:?}"),
            StateError::Duplicate(c) => write!(f, "relay {c} reported twice"),
            StateError::Missing(c) => write!(f, "relay {c} missing from state"),
            StateError::MissingEnd => write!(f, "state reply not terminated by `end`"),
            StateError::ControllerError(l) => write!(f, "controller replied {l:?}"),
        }
    }
}

impl std::error::Error for StateError {}

/// Parse the full `state` reply (lines up to and including `end`). Requires
/// exactly one well-formed line for each of the 8 relays; `fan`/`ssr` and
/// other non-relay lines are ignored.
pub fn parse_state(lines: &[String]) -> Result<[RelayReport; RELAY_CHANNELS as usize], StateError> {
    let mut found: [Option<RelayReport>; RELAY_CHANNELS as usize] = [None; RELAY_CHANNELS as usize];
    let mut ended = false;
    for line in lines {
        if line == "end" {
            ended = true;
            break;
        }
        if line == "err" || line.starts_with("err ") {
            return Err(StateError::ControllerError(line.clone()));
        }
        if line == "relay" || line.starts_with("relay ") {
            let r = parse_relay_line(line).ok_or_else(|| StateError::Malformed(line.clone()))?;
            let slot = &mut found[r.channel.index()];
            if slot.is_some() {
                return Err(StateError::Duplicate(r.channel));
            }
            *slot = Some(r);
        }
    }
    if !ended {
        return Err(StateError::MissingEnd);
    }
    let mut out = [RelayReport {
        channel: Channel(0),
        state: RelayState::Closed,
        attached: false,
    }; RELAY_CHANNELS as usize];
    for ch in Channel::all() {
        out[ch.index()] = found[ch.index()].ok_or(StateError::Missing(ch))?;
    }
    Ok(out)
}

/// Render untrusted bytes for a log line: bounded length, escaped.
pub fn log_safe(bytes: &[u8]) -> String {
    const MAX: usize = 64;
    let shown = &bytes[..bytes.len().min(MAX)];
    let mut s = format!("{:?}", String::from_utf8_lossy(shown));
    if bytes.len() > MAX {
        s.push_str(&format!("...(+{} bytes)", bytes.len() - MAX));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_topic_accepts_all_valid_channels() {
        for n in 0..8u8 {
            let ch = parse_set_topic(&format!("rack/relay/{n}/set")).unwrap();
            assert_eq!(ch.index(), usize::from(n));
        }
    }

    #[test]
    fn set_topic_rejects_bad_channels() {
        for t in [
            "rack/relay/8/set",
            "rack/relay/9/set",
            "rack/relay/-1/set",
            "rack/relay/x/set",
            "rack/relay//set",
            "rack/relay/set",
            "rack/relay/03/set",
            "rack/relay/+3/set",
            "rack/relay/ 3/set",
            "rack/relay/3 /set",
            "rack/relay/10/set",
            "rack/relay/3/set/extra",
            "rack/relay/3/state",
            "rack/relay/3/SET",
            "Rack/relay/3/set",
            "/rack/relay/3/set",
            "rack/relay/\u{0663}/set",
            "",
        ] {
            assert_eq!(parse_set_topic(t), None, "{t:?} must be rejected");
        }
    }

    #[test]
    fn payload_is_byte_exact() {
        assert_eq!(Command::parse(b"open"), Some(Command::Open));
        assert_eq!(Command::parse(b"close"), Some(Command::Close));
        for p in [
            &b"OPEN"[..],
            b"Open",
            b"open\n",
            b"open\r\n",
            b" open",
            b"open ",
            b"open\0",
            b"closed",
            b"close\n",
            b"",
            b"opn",
            b"shutdown",
        ] {
            assert_eq!(Command::parse(p), None, "{p:?} must be rejected");
        }
    }

    #[test]
    fn request_and_ok_reply() {
        let ch = Channel::new(3).unwrap();
        assert_eq!(Command::Open.request(ch), "open 3");
        assert_eq!(Command::Close.request(ch), "close 3");
        assert!(Command::Open.is_ok_reply(ch, "ok open 3"));
        assert!(Command::Open.is_ok_reply(ch, "ok"));
        assert!(Command::Close.is_ok_reply(ch, "ok close 3"));
        assert!(!Command::Open.is_ok_reply(ch, "ok close 3"));
        assert!(!Command::Open.is_ok_reply(ch, "ok open 4"));
        assert!(!Command::Open.is_ok_reply(ch, "err busy"));
        assert!(!Command::Open.is_ok_reply(ch, "okay"));
        assert!(!Command::Open.is_ok_reply(ch, ""));
    }

    fn lines(s: &[&str]) -> Vec<String> {
        s.iter().map(|l| l.to_string()).collect()
    }

    fn full_state() -> Vec<String> {
        lines(&[
            "relay 0 cmd=closed attached=1",
            "relay 1 cmd=open attached=1",
            "relay 2 cmd=closed attached=1",
            "relay 3 cmd=open attached=0",
            "relay 4 cmd=closed attached=1",
            "relay 5 cmd=closed attached=1",
            "relay 6 cmd=closed attached=1",
            "relay 7 cmd=closed attached=1",
            "fan 0 duty=0.50 attached=1",
            "ssr 0 cmd=on attached=1",
            "end",
        ])
    }

    #[test]
    fn parses_state_output() {
        let s = parse_state(&full_state()).unwrap();
        assert_eq!(s[1].state, RelayState::Open);
        assert_eq!(s[3].state, RelayState::Open);
        assert!(!s[3].attached);
        assert_eq!(s[0].state, RelayState::Closed);
        for (i, r) in s.iter().enumerate() {
            assert_eq!(r.channel.index(), i);
        }
    }

    #[test]
    fn state_rejects_incomplete_or_bad() {
        let mut missing = full_state();
        missing.remove(7);
        assert_eq!(
            parse_state(&missing),
            Err(StateError::Missing(Channel::new(7).unwrap()))
        );

        let mut no_end = full_state();
        no_end.pop();
        assert_eq!(parse_state(&no_end), Err(StateError::MissingEnd));

        let mut dup = full_state();
        dup.insert(0, "relay 1 cmd=open attached=1".into());
        assert_eq!(
            parse_state(&dup),
            Err(StateError::Duplicate(Channel::new(1).unwrap()))
        );

        for bad in [
            "relay 8 cmd=open attached=1",
            "relay 1 cmd=OPEN attached=1",
            "relay 1 cmd=open attached=2",
            "relay 1 cmd=open",
            "relay 1 cmd=open attached=1 extra",
            "relay  1 cmd=open attached=1",
        ] {
            let mut s = full_state();
            s.insert(0, bad.into());
            assert!(
                matches!(parse_state(&s), Err(StateError::Malformed(_))),
                "{bad:?}"
            );
        }

        assert!(matches!(
            parse_state(&lines(&["err not ready"])),
            Err(StateError::ControllerError(_))
        ));
    }

    #[test]
    fn log_safe_is_bounded() {
        let long = vec![b'a'; 200];
        let s = log_safe(&long);
        assert!(s.len() < 100);
        assert!(s.contains("+136 bytes"));
        assert_eq!(log_safe(b"open\n"), "\"open\\n\"");
    }
}
