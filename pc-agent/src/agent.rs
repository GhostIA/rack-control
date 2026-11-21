//! Transport-independent agent logic. Everything with side effects sits behind
//! a trait (`Publisher`, `PowerOff`, `CephRunner`) so tests use fakes and need
//! neither a broker nor a D-Bus system bus.

use std::time::Duration;

use log::{error, info, warn};

use crate::config::Config;

/// How long to wait for the broker to acknowledge a retained status publish.
pub const ACK_TIMEOUT: Duration = Duration::from_secs(5);

pub const CEPH_SET_TOPIC: &str = "rack/ceph/maintenance/set";
pub const CEPH_STATE_TOPIC: &str = "rack/ceph/maintenance/state";
pub const CEPH_BIN: &str = "/usr/bin/ceph";

/// The whole vocabulary of `rack/pc/<name>/cmd`. Byte-exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcCommand {
    Shutdown,
}

impl PcCommand {
    pub fn parse(payload: &[u8]) -> Option<Self> {
        match payload {
            b"shutdown" => Some(Self::Shutdown),
            _ => None,
        }
    }
}

/// The whole vocabulary of `rack/ceph/maintenance/set`. Byte-exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CephCommand {
    On,
    Off,
}

impl CephCommand {
    pub fn parse(payload: &[u8]) -> Option<Self> {
        match payload {
            b"on" => Some(Self::On),
            b"off" => Some(Self::Off),
            _ => None,
        }
    }
}

/// A received MQTT PUBLISH.
#[derive(Debug, Clone)]
pub struct Message {
    pub topic: String,
    pub payload: Vec<u8>,
    pub retain: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    /// Leave the process with status 0 (dry run after a handled shutdown).
    Exit,
}

/// Publishes a retained QoS 1 message and waits up to `timeout` for the PUBACK.
/// `Ok(true)` = acknowledged, `Ok(false)` = timed out.
pub trait Publisher {
    async fn publish_retained(
        &mut self,
        topic: &str,
        payload: &'static str,
        timeout: Duration,
    ) -> anyhow::Result<bool>;
}

/// Powers the host off (logind `PowerOff(false)` in production).
pub trait PowerOff {
    async fn power_off(&mut self) -> anyhow::Result<()>;
}

/// Runs `/usr/bin/ceph` with the given fixed argv (program excluded).
pub trait CephRunner {
    async fn run(&mut self, args: &[String]) -> anyhow::Result<()>;
}

/// Escaped, truncated rendering of a payload for logs. Never interpreted.
pub fn describe(payload: &[u8]) -> String {
    const MAX: usize = 64;
    let shown = &payload[..payload.len().min(MAX)];
    let mut s: String = String::from_utf8_lossy(shown).escape_debug().collect();
    if payload.len() > MAX {
        s.push_str(&format!("... ({} bytes)", payload.len()));
    }
    format!("\"{s}\"")
}

/// The two ceph invocations for `cmd`, as argv after the program name:
/// `[--conf <ceph_conf>] --id <ceph_id> osd set|unset noout|norebalance`.
/// Both values come from the validated config only, never from a message.
pub fn ceph_argv(ceph_id: &str, ceph_conf: Option<&str>, cmd: CephCommand) -> Vec<Vec<String>> {
    let verb = match cmd {
        CephCommand::On => "set",
        CephCommand::Off => "unset",
    };
    ["noout", "norebalance"]
        .iter()
        .map(|flag| {
            let mut argv: Vec<String> = Vec::new();
            if let Some(conf) = ceph_conf {
                argv.push("--conf".into());
                argv.push(conf.into());
            }
            argv.extend(
                ["--id", ceph_id, "osd", verb, flag]
                    .iter()
                    .map(|s| s.to_string()),
            );
            argv
        })
        .collect()
}

pub struct Agent<P, O, C> {
    name: String,
    cmd_topic: String,
    status_topic: String,
    ceph_maintenance: bool,
    ceph_id: String,
    ceph_conf: Option<String>,
    dry_run: bool,
    publisher: P,
    power: O,
    ceph: C,
    /// Set once `shutdown` was accepted; duplicates are ignored while set.
    shutting_down: bool,
}

impl<P: Publisher, O: PowerOff, C: CephRunner> Agent<P, O, C> {
    pub fn new(cfg: &Config, publisher: P, power: O, ceph: C) -> Self {
        Self {
            name: cfg.name.clone(),
            cmd_topic: cfg.cmd_topic(),
            status_topic: cfg.status_topic(),
            ceph_maintenance: cfg.ceph_maintenance,
            ceph_id: cfg.ceph_id.clone(),
            // Validated as UTF-8 in Config::parse.
            ceph_conf: cfg
                .ceph_conf
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            dry_run: cfg.dry_run,
            publisher,
            power,
            ceph,
            shutting_down: false,
        }
    }

    /// Topics to (re)subscribe after every CONNACK (clean session).
    pub fn subscriptions(&self) -> Vec<String> {
        let mut v = vec![self.cmd_topic.clone()];
        if self.ceph_maintenance {
            v.push(CEPH_SET_TOPIC.to_string());
        }
        v
    }

    async fn publish_status(&mut self, payload: &'static str) -> bool {
        match self
            .publisher
            .publish_retained(&self.status_topic, payload, ACK_TIMEOUT)
            .await
        {
            Ok(true) => true,
            Ok(false) => {
                warn!(
                    "no PUBACK for {}={payload} within {:?}",
                    self.status_topic, ACK_TIMEOUT
                );
                false
            }
            Err(e) => {
                warn!("publishing {}={payload} failed: {e:#}", self.status_topic);
                false
            }
        }
    }

    /// Called on every successful CONNACK, after subscribing.
    pub async fn on_connected(&mut self) {
        let status = if self.shutting_down { "down" } else { "online" };
        info!("connected as {}; publishing {status}", self.name);
        self.publish_status(status).await;
    }

    /// Called on SIGTERM/SIGINT before disconnecting. A plain stop marks the
    /// host `offline`; during shutdown the retained `down` is left in place.
    pub async fn on_terminate(&mut self) {
        if !self.shutting_down {
            self.publish_status("offline").await;
        }
    }

    pub async fn handle(&mut self, msg: &Message) -> Outcome {
        if msg.retain {
            // A stale retained command must never act (a retained `shutdown`
            // would power the host off at every boot).
            warn!(
                "ignoring retained message on {} payload={}",
                msg.topic,
                describe(&msg.payload)
            );
            return Outcome::Continue;
        }
        if msg.topic == self.cmd_topic {
            return self.handle_cmd(&msg.payload).await;
        }
        if msg.topic == CEPH_SET_TOPIC && self.ceph_maintenance {
            if self.shutting_down {
                // The host is powering off: don't start ceph commands that
                // systemd would kill mid-run, and don't report state for a
                // node that is going away.
                warn!("ignoring ceph maintenance command: power-off in progress");
                return Outcome::Continue;
            }
            self.handle_ceph(&msg.payload).await;
            return Outcome::Continue;
        }
        warn!(
            "ignoring message on unexpected topic {:?} payload={}",
            msg.topic,
            describe(&msg.payload)
        );
        Outcome::Continue
    }

    async fn handle_cmd(&mut self, payload: &[u8]) -> Outcome {
        let Some(PcCommand::Shutdown) = PcCommand::parse(payload) else {
            warn!(
                "ignoring unknown payload on {}: {}",
                self.cmd_topic,
                describe(payload)
            );
            return Outcome::Continue;
        };
        if self.shutting_down {
            info!("duplicate shutdown ignored; power-off already requested");
            return Outcome::Continue;
        }
        self.shutting_down = true;
        info!("shutdown received; publishing down");
        // Best effort: the orchestrator gates the relay cut on ping-down, so a
        // missing ack must not block the power-off.
        if self.publish_status("down").await {
            info!("down acknowledged by broker");
        }
        if self.dry_run {
            info!("dry_run: would power off (logind PowerOff(false)); exiting 0");
            return Outcome::Exit;
        }
        match self.power.power_off().await {
            Ok(()) => {
                info!("logind PowerOff requested; waiting for systemd to stop us");
                Outcome::Continue
            }
            Err(e) => {
                error!("PowerOff failed: {e:#}; staying up and accepting a new shutdown");
                self.shutting_down = false;
                self.publish_status("online").await;
                Outcome::Continue
            }
        }
    }

    async fn handle_ceph(&mut self, payload: &[u8]) {
        let Some(cmd) = CephCommand::parse(payload) else {
            warn!(
                "ignoring unknown payload on {CEPH_SET_TOPIC}: {}",
                describe(payload)
            );
            return;
        };
        let mut ok = true;
        for argv in ceph_argv(&self.ceph_id, self.ceph_conf.as_deref(), cmd) {
            let shown = format!("{CEPH_BIN} {}", argv.join(" "));
            if self.dry_run {
                info!("dry_run: would run {shown}");
                continue;
            }
            info!("running {shown}");
            if let Err(e) = self.ceph.run(&argv).await {
                error!("{shown} failed: {e:#}");
                ok = false;
            }
        }
        if self.dry_run {
            // Publishing on/off here would falsely claim the flags changed.
            info!("dry_run: not publishing {CEPH_STATE_TOPIC}");
            return;
        }
        let state = match (ok, cmd) {
            (false, _) => "error",
            (true, CephCommand::On) => "on",
            (true, CephCommand::Off) => "off",
        };
        match self
            .publisher
            .publish_retained(CEPH_STATE_TOPIC, state, ACK_TIMEOUT)
            .await
        {
            Ok(true) => info!("{CEPH_STATE_TOPIC}={state}"),
            Ok(false) => warn!("no PUBACK for {CEPH_STATE_TOPIC}={state}"),
            Err(e) => warn!("publishing {CEPH_STATE_TOPIC}={state} failed: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    type Log = Rc<RefCell<Vec<String>>>;

    struct FakePub {
        log: Log,
        ack: bool,
    }
    impl Publisher for FakePub {
        async fn publish_retained(
            &mut self,
            topic: &str,
            payload: &'static str,
            _timeout: Duration,
        ) -> anyhow::Result<bool> {
            self.log.borrow_mut().push(format!("pub {topic} {payload}"));
            Ok(self.ack)
        }
    }

    struct FakePower {
        log: Log,
        fail: bool,
    }
    impl PowerOff for FakePower {
        async fn power_off(&mut self) -> anyhow::Result<()> {
            self.log.borrow_mut().push("poweroff".into());
            if self.fail {
                anyhow::bail!("denied")
            }
            Ok(())
        }
    }

    struct FakeCeph {
        log: Log,
        fail_on: Option<&'static str>,
    }
    impl CephRunner for FakeCeph {
        async fn run(&mut self, args: &[String]) -> anyhow::Result<()> {
            let line = args.join(" ");
            self.log.borrow_mut().push(format!("ceph {line}"));
            if let Some(f) = self.fail_on {
                if line.contains(f) {
                    anyhow::bail!("exit 1")
                }
            }
            Ok(())
        }
    }

    fn cfg(extra: &str) -> Config {
        Config::parse(&format!("name = \"intel-test\"\n{extra}")).unwrap()
    }

    fn agent(cfg: &Config, ack: bool) -> (Agent<FakePub, FakePower, FakeCeph>, Log) {
        let log: Log = Rc::default();
        let a = Agent::new(
            cfg,
            FakePub {
                log: log.clone(),
                ack,
            },
            FakePower {
                log: log.clone(),
                fail: false,
            },
            FakeCeph {
                log: log.clone(),
                fail_on: None,
            },
        );
        (a, log)
    }

    fn msg(topic: &str, payload: &[u8], retain: bool) -> Message {
        Message {
            topic: topic.into(),
            payload: payload.to_vec(),
            retain,
        }
    }

    const CMD: &str = "rack/pc/intel-test/cmd";

    /// Race: many producers on worker threads flood the agent's inbox (as the
    /// MQTT event loop would after a reconnect replay) with interleaved
    /// shutdown duplicates, retained copies, junk, and ceph commands. The
    /// agent must power off exactly once, publish `down` exactly once and
    /// before the power-off, and act on nothing after it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_message_storm_powers_off_once() {
        for round in 0..20 {
            let c = cfg("ceph_maintenance = true\n");
            let (mut a, log) = agent(&c, true);
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Message>(1024);
            let mut producers = Vec::new();
            for p in 0..8u32 {
                let tx = tx.clone();
                producers.push(tokio::spawn(async move {
                    for i in 0..50u32 {
                        let m = match (p + i + round) % 5 {
                            0 => msg(CMD, b"shutdown", false),
                            1 => msg(CMD, b"shutdown", true),
                            2 => msg(CMD, b"Shutdown", false),
                            3 => msg("rack/ceph/maintenance/set", b"on", false),
                            _ => msg("rack/pc/intel-other/cmd", b"shutdown", false),
                        };
                        if tx.send(m).await.is_err() {
                            return; // agent stopped consuming: fine
                        }
                        tokio::task::yield_now().await;
                    }
                }));
            }
            drop(tx);
            // The real agent keeps consuming after PowerOff until systemd
            // stops it, so drain the whole storm.
            while let Some(m) = rx.recv().await {
                a.handle(&m).await;
            }
            for p in producers {
                p.await.unwrap();
            }
            let log = log.borrow();
            let offs = log.iter().filter(|l| *l == "poweroff").count();
            let downs: Vec<_> = log
                .iter()
                .enumerate()
                .filter(|(_, l)| l.ends_with("/status down"))
                .collect();
            assert_eq!(offs, 1, "round {round}: {log:?}");
            assert_eq!(downs.len(), 1, "round {round}: {log:?}");
            let off_at = log.iter().position(|l| l == "poweroff").unwrap();
            assert!(downs[0].0 < off_at, "round {round}: down after poweroff");
            assert_eq!(
                off_at,
                log.len() - 1,
                "round {round}: acted after poweroff: {log:?}"
            );
        }
    }

    #[test]
    fn exact_payload_parsing() {
        assert_eq!(PcCommand::parse(b"shutdown"), Some(PcCommand::Shutdown));
        for p in [
            &b"Shutdown"[..],
            b"shutdown\n",
            b"shutdown now",
            b"",
            b" shutdown",
            b"SHUTDOWN",
        ] {
            assert_eq!(PcCommand::parse(p), None);
        }
        assert_eq!(CephCommand::parse(b"on"), Some(CephCommand::On));
        assert_eq!(CephCommand::parse(b"off"), Some(CephCommand::Off));
        assert_eq!(CephCommand::parse(b"ON"), None);
        assert_eq!(CephCommand::parse(b"on\n"), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retained_shutdown_ignored() {
        let (mut a, log) = agent(&cfg(""), true);
        assert_eq!(
            a.handle(&msg(CMD, b"shutdown", true)).await,
            Outcome::Continue
        );
        assert!(log.borrow().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn non_exact_payloads_ignored() {
        let (mut a, log) = agent(&cfg(""), true);
        for p in [&b"Shutdown"[..], b"shutdown\n", b"shutdown now", b""] {
            assert_eq!(a.handle(&msg(CMD, p, false)).await, Outcome::Continue);
        }
        assert!(log.borrow().is_empty(), "{:?}", log.borrow());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn down_published_before_poweroff() {
        let (mut a, log) = agent(&cfg(""), true);
        assert_eq!(
            a.handle(&msg(CMD, b"shutdown", false)).await,
            Outcome::Continue
        );
        assert_eq!(
            *log.borrow(),
            vec!["pub rack/pc/intel-test/status down", "poweroff"]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn poweroff_still_happens_on_ack_timeout() {
        let (mut a, log) = agent(&cfg(""), false);
        a.handle(&msg(CMD, b"shutdown", false)).await;
        assert_eq!(
            *log.borrow(),
            vec!["pub rack/pc/intel-test/status down", "poweroff"]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn poweroff_still_happens_on_publish_error() {
        struct ErrPub;
        impl Publisher for ErrPub {
            async fn publish_retained(
                &mut self,
                _: &str,
                _: &'static str,
                _: Duration,
            ) -> anyhow::Result<bool> {
                anyhow::bail!("client gone")
            }
        }
        let log: Log = Rc::default();
        let mut a = Agent::new(
            &cfg(""),
            ErrPub,
            FakePower {
                log: log.clone(),
                fail: false,
            },
            FakeCeph {
                log: log.clone(),
                fail_on: None,
            },
        );
        a.handle(&msg(CMD, b"shutdown", false)).await;
        assert_eq!(*log.borrow(), vec!["poweroff"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplicate_shutdown_powers_off_once() {
        let (mut a, log) = agent(&cfg(""), true);
        for _ in 0..3 {
            a.handle(&msg(CMD, b"shutdown", false)).await;
        }
        let n = log.borrow().iter().filter(|l| *l == "poweroff").count();
        assert_eq!(n, 1);
        // A reconnect during shutdown re-asserts `down`, not `online`.
        a.on_connected().await;
        assert_eq!(
            log.borrow().last().unwrap(),
            "pub rack/pc/intel-test/status down"
        );
        // And stopping during shutdown does not overwrite `down`.
        let before = log.borrow().len();
        a.on_terminate().await;
        assert_eq!(log.borrow().len(), before);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_poweroff_reverts_to_online_and_allows_retry() {
        let log: Log = Rc::default();
        let mut a = Agent::new(
            &cfg(""),
            FakePub {
                log: log.clone(),
                ack: true,
            },
            FakePower {
                log: log.clone(),
                fail: true,
            },
            FakeCeph {
                log: log.clone(),
                fail_on: None,
            },
        );
        a.handle(&msg(CMD, b"shutdown", false)).await;
        a.handle(&msg(CMD, b"shutdown", false)).await;
        assert_eq!(
            *log.borrow(),
            vec![
                "pub rack/pc/intel-test/status down",
                "poweroff",
                "pub rack/pc/intel-test/status online",
                "pub rack/pc/intel-test/status down",
                "poweroff",
                "pub rack/pc/intel-test/status online",
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dry_run_exits_without_poweroff() {
        let (mut a, log) = agent(&cfg("dry_run = true\n"), true);
        assert_eq!(a.handle(&msg(CMD, b"shutdown", false)).await, Outcome::Exit);
        assert_eq!(*log.borrow(), vec!["pub rack/pc/intel-test/status down"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn other_pc_topic_ignored() {
        let (mut a, log) = agent(&cfg(""), true);
        a.handle(&msg("rack/pc/intel-other/cmd", b"shutdown", false))
            .await;
        assert!(log.borrow().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn connect_publishes_online_and_stop_publishes_offline() {
        let (mut a, log) = agent(&cfg(""), true);
        a.on_connected().await;
        a.on_terminate().await;
        assert_eq!(
            *log.borrow(),
            vec![
                "pub rack/pc/intel-test/status online",
                "pub rack/pc/intel-test/status offline"
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ceph_topic_ignored_when_disabled() {
        let (mut a, log) = agent(&cfg(""), true);
        assert_eq!(a.subscriptions(), vec![CMD.to_string()]);
        a.handle(&msg(CEPH_SET_TOPIC, b"on", false)).await;
        assert!(log.borrow().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ceph_exact_argv() {
        let (mut a, log) = agent(&cfg("ceph_maintenance = true\n"), true);
        assert_eq!(
            a.subscriptions(),
            vec![CMD.to_string(), CEPH_SET_TOPIC.to_string()]
        );
        a.handle(&msg(CEPH_SET_TOPIC, b"on", false)).await;
        a.handle(&msg(CEPH_SET_TOPIC, b"off", false)).await;
        a.handle(&msg(CEPH_SET_TOPIC, b"on\n", false)).await;
        a.handle(&msg(CEPH_SET_TOPIC, b"on", true)).await;
        assert_eq!(
            *log.borrow(),
            vec![
                "ceph --id rack-maint osd set noout",
                "ceph --id rack-maint osd set norebalance",
                "pub rack/ceph/maintenance/state on",
                "ceph --id rack-maint osd unset noout",
                "ceph --id rack-maint osd unset norebalance",
                "pub rack/ceph/maintenance/state off",
            ]
        );
        assert_eq!(
            ceph_argv("rack-maint", None, CephCommand::On)[0],
            vec!["--id", "rack-maint", "osd", "set", "noout"]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ceph_conf_prepended_to_argv() {
        let (mut a, log) = agent(
            &cfg("ceph_maintenance = true\nceph_conf = \"/etc/rack-agent/ceph.conf\"\n"),
            true,
        );
        a.handle(&msg(CEPH_SET_TOPIC, b"off", false)).await;
        assert_eq!(
            *log.borrow(),
            vec![
                "ceph --conf /etc/rack-agent/ceph.conf --id rack-maint osd unset noout",
                "ceph --conf /etc/rack-agent/ceph.conf --id rack-maint osd unset norebalance",
                "pub rack/ceph/maintenance/state off",
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ceph_failure_publishes_error() {
        let log: Log = Rc::default();
        let mut a = Agent::new(
            &cfg("ceph_maintenance = true\nceph_id = \"maint-2\"\n"),
            FakePub {
                log: log.clone(),
                ack: true,
            },
            FakePower {
                log: log.clone(),
                fail: false,
            },
            FakeCeph {
                log: log.clone(),
                fail_on: Some("noout"),
            },
        );
        a.handle(&msg(CEPH_SET_TOPIC, b"on", false)).await;
        assert_eq!(
            *log.borrow(),
            vec![
                "ceph --id maint-2 osd set noout",
                "ceph --id maint-2 osd set norebalance",
                "pub rack/ceph/maintenance/state error",
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ceph_never_runs_in_dry_run() {
        let (mut a, log) = agent(&cfg("ceph_maintenance = true\ndry_run = true\n"), true);
        a.handle(&msg(CEPH_SET_TOPIC, b"on", false)).await;
        assert!(log.borrow().is_empty(), "{:?}", log.borrow());
    }

    #[test]
    fn describe_escapes_and_truncates() {
        assert_eq!(describe(b"shutdown\n"), "\"shutdown\\n\"");
        let long = vec![b'a'; 100];
        assert!(describe(&long).ends_with("... (100 bytes)\""));
    }
}
