// Package orch holds the rack orchestration logic: graceful shutdown with
// sustained ping-down gating before any relay cut, power-up, status, relay
// map verification and Ceph maintenance. All I/O goes through the Bus,
// Pinger and Clock interfaces so the logic is tested with fakes.
package orch

import (
	"context"
	"fmt"
	"strconv"
	"strings"
	"time"
)

// Msg is the latest message seen on a topic. Seq is a bus-wide counter that
// increases with every accepted message, so "Seq > mark" means "arrived after
// the mark was taken".
type Msg struct {
	Payload string
	Seq     uint64
}

// Bus is the MQTT side. Publish is QoS 1, never retained, and returns once
// the broker acknowledged it. Get returns the latest valid message received
// on a subscribed state/status topic (retained or live).
type Bus interface {
	Publish(ctx context.Context, topic, payload string) error
	Get(topic string) (Msg, bool)
	Mark() uint64
}

// Pinger sends one ICMP echo and reports whether a reply arrived in time.
// (false, nil) means "no reply". A non-nil error means the probe itself
// failed locally (no raw-socket permission, out of file descriptors, ...):
// the orchestrator aborts instead of counting that as a miss, because a
// broken pinger would otherwise make every running host look "down".
// It must be safe for concurrent use.
type Pinger interface {
	Ping(ctx context.Context, ip string, timeout time.Duration) (bool, error)
}

// Clock is injectable so tests run in virtual time.
type Clock interface {
	Now() time.Time
	// Sleep blocks for d or until ctx is done (then it returns ctx.Err()).
	Sleep(ctx context.Context, d time.Duration) error
}

// RealClock is the wall clock.
type RealClock struct{}

// Now implements Clock.
func (RealClock) Now() time.Time { return time.Now() }

// Sleep implements Clock.
func (RealClock) Sleep(ctx context.Context, d time.Duration) error {
	if d <= 0 {
		return ctx.Err()
	}
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-t.C:
		return nil
	}
}

// Topic helpers (see the shared MQTT contract).
func TopicPCCmd(name string) string    { return "rack/pc/" + name + "/cmd" }
func TopicPCStatus(name string) string { return "rack/pc/" + name + "/status" }
func TopicRelaySet(ch int) string      { return fmt.Sprintf("rack/relay/%d/set", ch) }
func TopicRelayState(ch int) string    { return fmt.Sprintf("rack/relay/%d/state", ch) }

const (
	TopicSBC4Status = "rack/sbc4/status"
	TopicCephSet    = "rack/ceph/maintenance/set"
	TopicCephState  = "rack/ceph/maintenance/state"
)

// SubscribeFilters are the state/status topics the orchestrator reads.
// Command topics are never subscribed.
var SubscribeFilters = []string{
	"rack/pc/+/status",
	"rack/relay/+/state",
	TopicSBC4Status,
	TopicCephState,
}

// ValidState reports whether payload is an allowed, byte-exact value for a
// state/status topic. Anything else (including command topics) is rejected.
func ValidState(topic, payload string) bool {
	switch {
	case topic == TopicSBC4Status:
		return payload == "online" || payload == "offline"
	case topic == TopicCephState:
		return payload == "on" || payload == "off" || payload == "error"
	}
	parts := strings.Split(topic, "/")
	if len(parts) == 4 && parts[0] == "rack" && parts[1] == "pc" && parts[3] == "status" && parts[2] != "" {
		return payload == "online" || payload == "offline" || payload == "down"
	}
	if len(parts) == 4 && parts[0] == "rack" && parts[1] == "relay" && parts[3] == "state" {
		ch, err := strconv.Atoi(parts[2])
		if err != nil || ch < 0 || ch > 7 || strconv.Itoa(ch) != parts[2] {
			return false
		}
		return payload == "open" || payload == "closed"
	}
	return false
}
