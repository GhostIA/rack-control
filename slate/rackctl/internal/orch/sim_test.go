package orch

import (
	"bytes"
	"context"
	"fmt"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/GhostIA/rack-control/slate/rackctl/internal/config"
)

// ---------------------------------------------------------------------------
// virtual clock

type fakeClock struct {
	mu  sync.Mutex
	now time.Time
}

func newClock() *fakeClock { return &fakeClock{now: time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)} }

func (c *fakeClock) Now() time.Time {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.now
}

func (c *fakeClock) Sleep(ctx context.Context, d time.Duration) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	c.mu.Lock()
	if d > 0 {
		c.now = c.now.Add(d)
	}
	c.mu.Unlock()
	return ctx.Err()
}

// ---------------------------------------------------------------------------
// simulated rack: hosts, relays, agents, sbc4 and the ceph agent

type event struct {
	t       time.Time
	kind    string // "pub" or "ping"
	topic   string
	payload string
	ip      string
	ok      bool
}

type simHost struct {
	pc         config.PC
	powered    bool          // relay closed and booted
	ignore     bool          // agent ignores shutdown
	shutDelay  time.Duration // cmd -> stops answering
	bootDelay  time.Duration // relay close -> answers ping
	flapFor    time.Duration // after going down, keep answering every 3rd ping for this long
	flapAlways bool          // never reach 5 consecutive misses
	downAt     time.Time     // zero = not shutting down
	bootAt     time.Time
	agentDead  bool // never publishes online
	nPing      int
}

type sim struct {
	t      *testing.T
	clk    *fakeClock
	cfg    *config.Config
	mu     sync.Mutex
	hosts  map[string]*simHost // by name
	wiring map[int]string      // physical relay ch -> host name
	seq    uint64
	last   map[string]Msg
	queue  []queued
	events []event

	cephMode   string // "ok", "dead", "error"
	relayDead  bool
	publishErr error
}

type queued struct {
	at      time.Time
	topic   string
	payload string
}

const testConfig = `
[broker]
host = "192.168.8.1"
ca = "/etc/rack/tls/ca.crt"
cert = "/etc/rack/tls/orchestrator.crt"
key = "/etc/rack/tls/orchestrator.key"
password_file = "/etc/rack/orchestrator.password"

[shutdown]
timeout = "120s"
cut_delay = "30s"
order = ["intel-a163f8", "intel-91c45d", "intel-d1babf", "intel-177e78", "intel-1ecbc8"]
pve_quorum = 3

[[pc]]
name = "intel-91c45d"
ip = "192.168.8.196"
relay = 0
roles = ["osd"]

[[pc]]
name = "intel-d1babf"
ip = "192.168.8.194"
relay = 1
roles = ["osd"]

[[pc]]
name = "intel-177e78"
ip = "192.168.8.162"
relay = 2
roles = ["osd", "mon"]

[[pc]]
name = "intel-a163f8"
ip = "192.168.8.123"
relay = 4
roles = ["mon", "vm"]
timeout = "300s"

[[pc]]
name = "intel-1ecbc8"
ip = "192.168.8.234"
relay = 6
roles = ["osd", "mon"]
`

func newSim(t *testing.T) *sim {
	t.Helper()
	cfg, err := config.Parse(testConfig)
	if err != nil {
		t.Fatal(err)
	}
	s := &sim{t: t, clk: newClock(), cfg: cfg, hosts: map[string]*simHost{}, wiring: map[int]string{}, last: map[string]Msg{}, cephMode: "ok"}
	for _, p := range cfg.PCs {
		s.hosts[p.Name] = &simHost{pc: p, powered: true, shutDelay: 20 * time.Second, bootDelay: 40 * time.Second}
		s.wiring[p.Relay] = p.Name
		s.setRetained(TopicPCStatus(p.Name), "online")
	}
	for ch := 0; ch < 8; ch++ {
		s.setRetained(TopicRelayState(ch), "closed")
	}
	s.setRetained(TopicSBC4Status, "online")
	s.setRetained(TopicCephState, "off")
	return s
}

// rackOff puts the whole rack in the powered-off, all-relays-open state.
func (s *sim) rackOff() {
	for _, h := range s.hosts {
		h.powered = false
		s.setRetained(TopicPCStatus(h.pc.Name), "offline")
	}
	for ch := 0; ch < 8; ch++ {
		s.setRetained(TopicRelayState(ch), "open")
	}
}

func (s *sim) host(name string) *simHost { return s.hosts[name] }

func (s *sim) byIP(ip string) *simHost {
	for _, h := range s.hosts {
		if h.pc.IP == ip {
			return h
		}
	}
	return nil
}

func (s *sim) setRetained(topic, payload string) {
	s.seq++
	s.last[topic] = Msg{Payload: payload, Seq: s.seq}
}

func (s *sim) deliverAt(at time.Time, topic, payload string) {
	s.queue = append(s.queue, queued{at, topic, payload})
}

func (s *sim) flush() {
	now := s.clk.Now()
	var rest []queued
	for _, q := range s.queue {
		if !q.at.After(now) {
			s.setRetained(q.topic, q.payload)
		} else {
			rest = append(rest, q)
		}
	}
	s.queue = rest
}

// Bus implementation ---------------------------------------------------------

func (s *sim) Publish(_ context.Context, topic, payload string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	now := s.clk.Now()
	s.events = append(s.events, event{t: now, kind: "pub", topic: topic, payload: payload})
	if s.publishErr != nil {
		return s.publishErr
	}
	var ch int
	var name string
	switch {
	case strings.HasSuffix(topic, "/cmd"):
		name = strings.TrimSuffix(strings.TrimPrefix(topic, "rack/pc/"), "/cmd")
		h := s.hosts[name]
		if h != nil && payload == "shutdown" && !h.ignore && h.powered && h.downAt.IsZero() {
			h.downAt = now.Add(h.shutDelay)
			s.deliverAt(now.Add(time.Second), TopicPCStatus(name), "down")
		}
	case topic == TopicCephSet:
		switch s.cephMode {
		case "ok":
			s.deliverAt(now.Add(2*time.Second), TopicCephState, payload)
		case "error":
			s.deliverAt(now.Add(2*time.Second), TopicCephState, "error")
		}
	default:
		if _, err := fmt.Sscanf(topic, "rack/relay/%d/set", &ch); err == nil && !s.relayDead {
			state := map[string]string{"open": "open", "close": "closed"}[payload]
			s.deliverAt(now.Add(100*time.Millisecond), TopicRelayState(ch), state)
			if h := s.hosts[s.wiring[ch]]; h != nil {
				if payload == "open" {
					h.powered = false
					h.downAt = time.Time{}
				} else if !h.powered {
					h.powered = true
					h.downAt = time.Time{}
					h.bootAt = now.Add(h.bootDelay)
					if !h.agentDead {
						s.deliverAt(h.bootAt.Add(5*time.Second), TopicPCStatus(h.pc.Name), "online")
					}
				}
			}
		}
	}
	return nil
}

func (s *sim) Get(topic string) (Msg, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.flush()
	m, ok := s.last[topic]
	return m, ok
}

func (s *sim) Mark() uint64 {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.flush()
	return s.seq
}

// Pinger implementation -------------------------------------------------------

func (s *sim) Ping(_ context.Context, ip string, _ time.Duration) (bool, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	now := s.clk.Now()
	h := s.byIP(ip)
	ok := false
	if h != nil && h.powered && !now.Before(h.bootAt) {
		h.nPing++
		switch {
		case h.downAt.IsZero() || now.Before(h.downAt):
			ok = true
		case h.flapAlways:
			ok = h.nPing%3 == 0
		case now.Before(h.downAt.Add(h.flapFor)):
			ok = h.nPing%3 == 0
		}
	}
	s.events = append(s.events, event{t: now, kind: "ping", ip: ip, ok: ok})
	return ok, nil
}

// helpers ----------------------------------------------------------------------

func (s *sim) orch() (*Orchestrator, *bytes.Buffer) {
	var out bytes.Buffer
	o := New(s.cfg, s, s, s.clk, &out, false)
	return o, &out
}

func (s *sim) pubs() []event {
	var out []event
	for _, e := range s.events {
		if e.kind == "pub" {
			out = append(out, e)
		}
	}
	return out
}

func (s *sim) pubsMatching(pred func(event) bool) []event {
	var out []event
	for _, e := range s.pubs() {
		if pred(e) {
			out = append(out, e)
		}
	}
	return out
}

func (s *sim) firstPub(topic, payload string) (event, int, bool) {
	for i, e := range s.pubs() {
		if e.topic == topic && e.payload == payload {
			return e, i, true
		}
	}
	return event{}, -1, false
}

func isRelayOpen(e event) bool {
	return strings.HasPrefix(e.topic, "rack/relay/") && strings.HasSuffix(e.topic, "/set") && e.payload == "open"
}

func isCmd(e event) bool { return strings.HasSuffix(e.topic, "/cmd") }

// assertGated checks the core safety invariant: every relay open is preceded
// by >= 5 consecutive ping misses for the host physically wired to that
// channel, the last of which happened at least cut_delay before the open.
func (s *sim) assertGated(t *testing.T, cutDelay time.Duration) {
	t.Helper()
	for i, e := range s.events {
		if e.kind != "pub" || !isRelayOpen(e) {
			continue
		}
		var ch int
		fmt.Sscanf(e.topic, "rack/relay/%d/set", &ch)
		h := s.hosts[s.wiring[ch]] // the host physically on this channel
		if h == nil {
			continue
		}
		pc := h.pc.IP
		misses := 0
		var lastMiss time.Time
		for j := i - 1; j >= 0; j-- {
			p := s.events[j]
			if p.kind != "ping" || p.ip != pc {
				continue
			}
			if p.ok {
				break
			}
			if misses == 0 {
				lastMiss = p.t
			}
			misses++
		}
		if misses < 5 {
			t.Fatalf("relay %d opened at %s with only %d consecutive ping misses for %s", ch, e.t, misses, pc)
		}
		if e.t.Sub(lastMiss) < cutDelay {
			t.Fatalf("relay %d opened %s after sustained ping loss; cut_delay is %s", ch, e.t.Sub(lastMiss), cutDelay)
		}
	}
}
