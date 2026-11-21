//! The worker task: applies broker events to the bridge in order, retries the
//! state sync with backoff until the controller socket is reachable, and
//! periodically audits the controller's state.

use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{sleep_until, Instant};

use backon::{BackoffBuilder, ExponentialBackoff, ExponentialBuilder};

use crate::bridge::{Bridge, Mqtt};
use crate::controller::Controller;
use crate::mqtt::BrokerEvent;

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Resync retry backoff bounds while the controller is unreachable.
    pub min_retry: Duration,
    pub max_retry: Duration,
    /// How often to compare the controller's `state` with what was published.
    pub audit_every: Duration,
    /// Commands that waited longer than this (behind a slow controller) are
    /// dropped instead of being applied late.
    pub max_command_age: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            min_retry: Duration::from_secs(1),
            max_retry: Duration::from_secs(30),
            audit_every: Duration::from_secs(60),
            max_command_age: Duration::from_secs(10),
        }
    }
}

/// Exponential backoff (x2 per attempt, capped, never gives up) from `backon`.
pub fn backoff(min: Duration, max: Duration) -> ExponentialBackoff {
    ExponentialBuilder::new()
        .with_min_delay(min)
        .with_max_delay(max)
        .without_max_times()
        .build()
}

/// Run until the event channel closes (it never does in production).
pub async fn run<C: Controller, M: Mqtt>(
    mut bridge: Bridge<C, M>,
    mut rx: UnboundedReceiver<BrokerEvent>,
    t: Timing,
) {
    let new_backoff = || backoff(t.min_retry, t.max_retry);
    let mut retry_delays = new_backoff();
    let mut next_sync: Option<Instant> = None;
    let mut next_audit = Instant::now() + t.audit_every;
    loop {
        let retry = async {
            match next_sync {
                Some(at) => sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            ev = rx.recv() => match ev {
                None => return,
                Some(BrokerEvent::Connected) => {
                    retry_delays = new_backoff();
                    next_sync = None;
                    bridge.on_connected().await;
                    next_audit = Instant::now() + t.audit_every;
                }
                Some(BrokerEvent::Message { topic, payload, retain, received }) => {
                    let age = received.elapsed();
                    if age > t.max_command_age {
                        log::error!(
                            "dropping message on {} queued for {}ms (> {}ms)",
                            crate::protocol::log_safe(topic.as_bytes()),
                            age.as_millis(),
                            t.max_command_age.as_millis()
                        );
                    } else {
                        bridge.on_message(&topic, &payload, retain).await;
                    }
                }
            },
            () = retry => {
                next_sync = None;
                if bridge.retry().await {
                    retry_delays = new_backoff();
                }
            }
            () = sleep_until(next_audit) => {
                next_audit = Instant::now() + t.audit_every;
                bridge.audit().await;
            }
        }
        if !bridge.needs_sync() {
            next_sync = None;
        } else if next_sync.is_none() {
            // Don't push an already-armed timer back, or a steady stream of
            // messages could starve the resync.
            let delay = retry_delays.next().unwrap_or(t.max_retry);
            next_sync = Some(Instant::now() + delay);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let s = Duration::from_secs;
        let got: Vec<_> = backoff(s(1), s(5)).take(6).collect();
        assert_eq!(got, vec![s(1), s(2), s(4), s(5), s(5), s(5)]);
    }
}
