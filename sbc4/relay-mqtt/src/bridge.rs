//! The transport-independent sidecar logic: MQTT command -> controller ->
//! retained state. The MQTT side and the controller side are both traits so
//! the logic is tested with fakes.

use std::future::Future;

use anyhow::Result;

use crate::controller::{Controller, ReplyKind};
use crate::protocol::{
    log_safe, parse_set_topic, parse_state, Channel, Command, RelayState, RELAY_CHANNELS,
    SET_FILTER, STATUS_ONLINE, STATUS_TOPIC,
};

/// The MQTT operations the bridge needs. Publishes are QoS 1.
pub trait Mqtt {
    fn publish_retained(
        &mut self,
        topic: &str,
        payload: &'static str,
    ) -> impl Future<Output = Result<()>> + Send;
    fn subscribe(&mut self, filter: &str) -> impl Future<Output = Result<()>> + Send;
}

/// What happened to an incoming message (for logs and tests).
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Retained,
    BadTopic,
    BadPayload,
    /// Controller acknowledged; state published.
    Applied(Channel, Command),
    /// Controller acknowledged, but queuing the state publish failed.
    AppliedUnpublished(Channel, Command),
    /// Controller replied something other than ok.
    Rejected(Channel, Command, String),
    /// Socket unreachable / timed out.
    ControllerDown(Channel, Command),
}

pub struct Bridge<C, M> {
    ctl: C,
    mqtt: M,
    need_sync: bool,
    /// Last state this process published per relay (None = unknown).
    published: [Option<RelayState>; RELAY_CHANNELS as usize],
}

impl<C: Controller, M: Mqtt> Bridge<C, M> {
    pub fn new(ctl: C, mqtt: M) -> Self {
        Bridge {
            ctl,
            mqtt,
            need_sync: true,
            published: [None; RELAY_CHANNELS as usize],
        }
    }

    /// True while the 8 retained states still have to be (re)published.
    pub fn needs_sync(&self) -> bool {
        self.need_sync
    }

    #[cfg(test)]
    pub fn parts(&self) -> (&C, &M) {
        (&self.ctl, &self.mqtt)
    }

    /// Called on every MQTT CONNACK. Clean session means the subscription
    /// must be renewed each time; status is published retained (LWT is the
    /// retained `offline`). Then try to publish all relay states.
    pub async fn on_connected(&mut self) {
        if let Err(e) = self.mqtt.subscribe(SET_FILTER).await {
            log::error!("subscribe {SET_FILTER}: {e:#}");
        }
        if let Err(e) = self
            .mqtt
            .publish_retained(STATUS_TOPIC, STATUS_ONLINE)
            .await
        {
            log::error!("publish {STATUS_TOPIC}: {e:#}");
        }
        self.published = [None; RELAY_CHANNELS as usize];
        self.need_sync = true;
        self.sync().await;
    }

    /// Query the controller's `state` and publish all 8 retained relay
    /// states. Returns true on success; on failure `needs_sync` stays true
    /// and the caller retries with backoff.
    pub async fn sync(&mut self) -> bool {
        self.need_sync = true;
        self.published = [None; RELAY_CHANNELS as usize];
        self.refresh().await
    }

    /// Periodic check: query `state` and publish only relays whose state
    /// differs from what was last published. Catches a controller restart
    /// (which resets every relay) even when no commands arrive.
    pub async fn audit(&mut self) -> bool {
        if self.need_sync {
            return self.sync().await;
        }
        self.refresh().await
    }

    async fn refresh(&mut self) -> bool {
        let result = self.try_sync().await;
        // This exchange may itself have (re)connected; that's the sync we want.
        self.ctl.take_reconnected();
        match result {
            Ok(()) => {
                self.need_sync = false;
                true
            }
            Err(e) => {
                log::warn!("relay state sync failed (will retry): {e:#}");
                self.need_sync = true;
                self.ctl.reset();
                false
            }
        }
    }

    async fn try_sync(&mut self) -> Result<()> {
        let lines = self.ctl.exchange("state", ReplyKind::UntilEnd).await?;
        let reports = parse_state(&lines)?;
        let mut changed = false;
        for r in reports {
            if !r.attached {
                log::warn!("relay {} reported detached by controller", r.channel);
            }
            let slot = &mut self.published[r.channel.index()];
            if *slot == Some(r.state) {
                continue;
            }
            if slot.is_some() {
                log::warn!(
                    "relay {} is {} at the controller (was published {}); republishing",
                    r.channel,
                    r.state.payload(),
                    slot.map_or("?", RelayState::payload)
                );
            }
            // Forget it first so a failed publish is retried next time.
            *slot = None;
            self.mqtt
                .publish_retained(&r.channel.state_topic(), r.state.payload())
                .await?;
            self.published[r.channel.index()] = Some(r.state);
            changed = true;
        }
        if !changed {
            return Ok(());
        }
        log::info!(
            "published relay states: {}",
            reports
                .iter()
                .map(|r| format!("{}={}", r.channel, r.state.payload()))
                .collect::<Vec<_>>()
                .join(" ")
        );
        Ok(())
    }

    /// Handle one incoming PUBLISH.
    pub async fn on_message(&mut self, topic: &str, payload: &[u8], retain: bool) -> Outcome {
        if retain {
            log::warn!(
                "ignoring retained message on {} (commands are never retained)",
                log_safe(topic.as_bytes())
            );
            return Outcome::Retained;
        }
        let Some(ch) = parse_set_topic(topic) else {
            log::warn!(
                "ignoring message on invalid topic {}",
                log_safe(topic.as_bytes())
            );
            return Outcome::BadTopic;
        };
        let Some(cmd) = Command::parse(payload) else {
            log::warn!("ignoring invalid payload {} on {topic}", log_safe(payload));
            return Outcome::BadPayload;
        };
        let outcome = self.apply(ch, cmd).await;
        if self.ctl.take_reconnected() {
            // New socket connection: the controller may have restarted and
            // reset every relay, so republish everything.
            self.need_sync = true;
        }
        outcome
    }

    async fn apply(&mut self, ch: Channel, cmd: Command) -> Outcome {
        let request = cmd.request(ch);
        let reply = match self.ctl.exchange(&request, ReplyKind::Single).await {
            Ok(lines) => lines.into_iter().next().unwrap_or_default(),
            Err(e) => {
                log::error!("controller unreachable for {request:?}: {e:#}");
                self.need_sync = true;
                return Outcome::ControllerDown(ch, cmd);
            }
        };
        if !cmd.is_ok_reply(ch, &reply) {
            log::error!(
                "controller refused {request:?}: {}; state NOT published",
                log_safe(reply.as_bytes())
            );
            if !(reply == "err" || reply.starts_with("err ")) {
                // Not even an `err` line: the stream may be out of step.
                self.ctl.reset();
                self.need_sync = true;
            }
            return Outcome::Rejected(ch, cmd, reply);
        }
        let state = cmd.resulting_state();
        self.published[ch.index()] = None;
        if let Err(e) = self
            .mqtt
            .publish_retained(&ch.state_topic(), state.payload())
            .await
        {
            log::error!(
                "relay {ch}: {} acknowledged but publishing {} failed: {e:#}",
                cmd.verb(),
                ch.state_topic()
            );
            self.need_sync = true;
            return Outcome::AppliedUnpublished(ch, cmd);
        }
        self.published[ch.index()] = Some(state);
        log::info!(
            "relay {ch}: {} acknowledged -> {}",
            cmd.verb(),
            state.payload()
        );
        Outcome::Applied(ch, cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct FakeCtl {
        replies: VecDeque<Result<Vec<String>>>,
        requests: Vec<String>,
        reconnected: bool,
        resets: usize,
    }

    impl FakeCtl {
        fn reply(mut self, lines: &[&str]) -> Self {
            self.replies
                .push_back(Ok(lines.iter().map(|s| s.to_string()).collect()));
            self
        }
        fn fail(mut self) -> Self {
            self.replies.push_back(Err(anyhow::anyhow!("socket down")));
            self
        }
    }

    impl Controller for FakeCtl {
        async fn exchange(&mut self, request: &str, _kind: ReplyKind) -> Result<Vec<String>> {
            self.requests.push(request.to_string());
            self.replies
                .pop_front()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no scripted reply")))
        }
        fn take_reconnected(&mut self) -> bool {
            std::mem::take(&mut self.reconnected)
        }
        fn reset(&mut self) {
            self.resets += 1;
        }
    }

    #[derive(Default)]
    struct FakeMqtt {
        published: Vec<(String, String)>,
        subscribed: Vec<String>,
        fail_publish: bool,
    }

    impl Mqtt for FakeMqtt {
        async fn publish_retained(&mut self, topic: &str, payload: &'static str) -> Result<()> {
            if self.fail_publish {
                anyhow::bail!("client channel closed");
            }
            self.published
                .push((topic.to_string(), payload.to_string()));
            Ok(())
        }
        async fn subscribe(&mut self, filter: &str) -> Result<()> {
            self.subscribed.push(filter.to_string());
            Ok(())
        }
    }

    fn state_lines() -> Vec<&'static str> {
        vec![
            "relay 0 cmd=closed attached=1",
            "relay 1 cmd=closed attached=1",
            "relay 2 cmd=closed attached=1",
            "relay 3 cmd=open attached=1",
            "relay 4 cmd=closed attached=1",
            "relay 5 cmd=closed attached=1",
            "relay 6 cmd=closed attached=1",
            "relay 7 cmd=closed attached=0",
            "fan 0 duty=0.40 attached=1",
            "end",
        ]
    }

    fn pubs(m: &FakeMqtt) -> Vec<(&str, &str)> {
        m.published
            .iter()
            .map(|(t, p)| (t.as_str(), p.as_str()))
            .collect()
    }

    #[tokio::test]
    async fn publishes_state_after_ok() {
        let mut b = Bridge::new(
            FakeCtl::default().reply(&["ok open 3"]),
            FakeMqtt::default(),
        );
        let o = b.on_message("rack/relay/3/set", b"open", false).await;
        assert_eq!(o, Outcome::Applied(Channel::new(3).unwrap(), Command::Open));
        let (ctl, m) = b.parts();
        assert_eq!(ctl.requests, vec!["open 3"]);
        assert_eq!(pubs(m), vec![("rack/relay/3/state", "open")]);
    }

    #[tokio::test]
    async fn close_publishes_closed() {
        let mut b = Bridge::new(
            FakeCtl::default().reply(&["ok close 6"]),
            FakeMqtt::default(),
        );
        b.on_message("rack/relay/6/set", b"close", false).await;
        assert_eq!(pubs(b.parts().1), vec![("rack/relay/6/state", "closed")]);
    }

    #[tokio::test]
    async fn no_state_after_err() {
        let mut b = Bridge::new(
            FakeCtl::default().reply(&["err relay 3 not attached"]),
            FakeMqtt::default(),
        );
        let o = b.on_message("rack/relay/3/set", b"open", false).await;
        assert!(matches!(o, Outcome::Rejected(_, Command::Open, _)));
        assert!(b.parts().1.published.is_empty());
    }

    #[tokio::test]
    async fn no_state_after_mismatched_ok() {
        let mut b = Bridge::new(
            FakeCtl::default().reply(&["ok close 3"]),
            FakeMqtt::default(),
        );
        let o = b.on_message("rack/relay/3/set", b"open", false).await;
        assert!(matches!(o, Outcome::Rejected(..)));
        assert!(b.parts().1.published.is_empty());
        assert_eq!(
            b.parts().0.resets,
            1,
            "out-of-step reply must drop the connection"
        );
        assert!(b.needs_sync());
    }

    #[tokio::test]
    async fn err_reply_keeps_connection() {
        let mut b = Bridge::new(FakeCtl::default().reply(&["err busy"]), FakeMqtt::default());
        b.need_sync = false;
        b.on_message("rack/relay/3/set", b"open", false).await;
        assert_eq!(b.parts().0.resets, 0);
        assert!(!b.needs_sync());
    }

    #[tokio::test]
    async fn publish_failure_is_reported() {
        let mqtt = FakeMqtt {
            fail_publish: true,
            ..Default::default()
        };
        let mut b = Bridge::new(FakeCtl::default().reply(&["ok open 3"]), mqtt);
        b.need_sync = false;
        let o = b.on_message("rack/relay/3/set", b"open", false).await;
        assert!(matches!(o, Outcome::AppliedUnpublished(..)), "{o:?}");
        assert!(b.needs_sync());
    }

    #[tokio::test]
    async fn audit_publishes_only_changes() {
        let mut changed = state_lines();
        changed[3] = "relay 3 cmd=closed attached=1";
        let mut b = Bridge::new(
            FakeCtl::default()
                .reply(&state_lines())
                .reply(&state_lines())
                .reply(&changed),
            FakeMqtt::default(),
        );
        b.on_connected().await;
        assert_eq!(b.parts().1.published.len(), 9);
        assert!(b.audit().await);
        assert_eq!(b.parts().1.published.len(), 9, "no change, no publish");
        assert!(b.audit().await);
        assert_eq!(pubs(b.parts().1)[9..], [("rack/relay/3/state", "closed")]);
    }

    #[tokio::test]
    async fn no_state_when_controller_down() {
        let mut b = Bridge::new(FakeCtl::default().fail(), FakeMqtt::default());
        b.need_sync = false;
        let o = b.on_message("rack/relay/3/set", b"open", false).await;
        assert!(matches!(o, Outcome::ControllerDown(..)));
        assert!(b.parts().1.published.is_empty());
        assert!(b.needs_sync(), "a socket failure must trigger a resync");
    }

    #[tokio::test]
    async fn ignores_retained() {
        let mut b = Bridge::new(
            FakeCtl::default().reply(&["ok open 3"]),
            FakeMqtt::default(),
        );
        let o = b.on_message("rack/relay/3/set", b"open", true).await;
        assert_eq!(o, Outcome::Retained);
        let (ctl, m) = b.parts();
        assert!(
            ctl.requests.is_empty(),
            "retained must never reach the controller"
        );
        assert!(m.published.is_empty());
    }

    #[tokio::test]
    async fn rejects_bad_channels_and_payloads() {
        let mut b = Bridge::new(FakeCtl::default(), FakeMqtt::default());
        for t in [
            "rack/relay/8/set",
            "rack/relay/-1/set",
            "rack/relay/x/set",
            "rack/relay//set",
            "rack/relay/9/set",
        ] {
            assert_eq!(
                b.on_message(t, b"open", false).await,
                Outcome::BadTopic,
                "{t}"
            );
        }
        for p in [&b"OPEN"[..], b"open\n", b" open", b"close ", b""] {
            assert_eq!(
                b.on_message("rack/relay/3/set", p, false).await,
                Outcome::BadPayload
            );
        }
        let (ctl, m) = b.parts();
        assert!(ctl.requests.is_empty());
        assert!(m.published.is_empty());
    }

    #[tokio::test]
    async fn on_connected_subscribes_publishes_online_and_all_states() {
        let mut b = Bridge::new(
            FakeCtl::default().reply(&state_lines()),
            FakeMqtt::default(),
        );
        b.on_connected().await;
        assert!(!b.needs_sync());
        let (ctl, m) = b.parts();
        assert_eq!(ctl.requests, vec!["state"]);
        assert_eq!(m.subscribed, vec!["rack/relay/+/set"]);
        let p = pubs(m);
        assert_eq!(p[0], ("rack/sbc4/status", "online"));
        assert_eq!(p.len(), 9);
        for n in 0..8 {
            let want = if n == 3 { "open" } else { "closed" };
            assert_eq!(p[1 + n], (format!("rack/relay/{n}/state").as_str(), want));
        }
    }

    #[tokio::test]
    async fn sync_failure_keeps_retrying_state() {
        let mut b = Bridge::new(
            FakeCtl::default()
                .fail()
                .reply(&["relay 0 cmd=open attached=1", "end"]),
            FakeMqtt::default(),
        );
        b.on_connected().await;
        assert!(b.needs_sync());
        // Incomplete state (only relay 0) must not publish anything.
        assert!(!b.sync().await);
        assert_eq!(pubs(b.parts().1), vec![("rack/sbc4/status", "online")]);
        b.ctl = FakeCtl::default().reply(&state_lines());
        assert!(b.sync().await);
        assert!(!b.needs_sync());
        assert_eq!(b.parts().1.published.len(), 9);
    }

    #[tokio::test]
    async fn socket_reconnect_triggers_resync() {
        let mut ctl = FakeCtl::default().reply(&["ok open 1"]);
        ctl.reconnected = true;
        let mut b = Bridge::new(ctl, FakeMqtt::default());
        b.need_sync = false;
        b.on_message("rack/relay/1/set", b"open", false).await;
        assert!(b.needs_sync());
    }
}
