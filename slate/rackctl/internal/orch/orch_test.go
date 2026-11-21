package orch

import (
	"bytes"
	"context"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/GhostIA/rack-control/slate/rackctl/internal/config"
)

var order = []string{"intel-a163f8", "intel-91c45d", "intel-d1babf", "intel-177e78", "intel-1ecbc8"}

func TestShutdownAllOrderGatingAndCutDelay(t *testing.T) {
	s := newSim(t)
	o, out := s.orch()
	if err := o.Shutdown(context.Background(), []string{"all"}, ShutdownOpts{}); err != nil {
		t.Fatalf("shutdown: %v\n%s", err, out)
	}
	s.assertGated(t, 30*time.Second)

	pubs := s.pubs()
	if pubs[0].topic != TopicCephSet || pubs[0].payload != "on" {
		t.Fatalf("first publish must be ceph maintenance on, got %+v", pubs[0])
	}
	// Strict one-at-a-time sequence in config order: cmd, relay open, next cmd ...
	var seq []string
	for _, e := range pubs[1:] {
		seq = append(seq, e.topic+"="+e.payload)
	}
	var want []string
	for _, n := range order {
		p, _ := s.cfg.PC(n)
		want = append(want, TopicPCCmd(n)+"=shutdown", TopicRelaySet(p.Relay)+"=open")
	}
	if strings.Join(seq, "\n") != strings.Join(want, "\n") {
		t.Fatalf("publish sequence:\n%s\nwant:\n%s", strings.Join(seq, "\n"), strings.Join(want, "\n"))
	}
	if !strings.Contains(out.String(), "[5/5] intel-1ecbc8") || !strings.Contains(out.String(), "shutdown complete") {
		t.Fatalf("missing progress output:\n%s", out)
	}
}

func TestFlappingHostIsNotCutUntilSustainedDown(t *testing.T) {
	s := newSim(t)
	s.host("intel-a163f8").flapFor = 40 * time.Second
	o, out := s.orch()
	if err := o.Shutdown(context.Background(), []string{"all"}, ShutdownOpts{}); err != nil {
		t.Fatalf("shutdown: %v\n%s", err, out)
	}
	s.assertGated(t, 30*time.Second)
	open, _, ok := s.firstPub(TopicRelaySet(4), "open")
	h := s.host("intel-a163f8")
	if !ok {
		t.Fatal("relay 4 never opened")
	}
	if open.t.Before(h.downAt.Add(h.flapFor)) {
		t.Fatalf("relay opened at %s while host was still flapping (until %s)", open.t, h.downAt.Add(h.flapFor))
	}
}

func TestHostThatNeverGoesDownIsNotCutAndStopsSequence(t *testing.T) {
	for _, mode := range []string{"ignore", "flap-forever"} {
		t.Run(mode, func(t *testing.T) {
			s := newSim(t)
			h := s.host("intel-91c45d") // second in order
			if mode == "ignore" {
				h.ignore = true
			} else {
				h.flapAlways = true
			}
			o, out := s.orch()
			err := o.Shutdown(context.Background(), []string{"all"}, ShutdownOpts{})
			if err == nil {
				t.Fatalf("expected failure\n%s", out)
			}
			s.assertGated(t, 30*time.Second)
			opens := s.pubsMatching(isRelayOpen)
			if len(opens) != 1 || opens[0].topic != TopicRelaySet(4) {
				t.Fatalf("only the first host's relay (4) may open, got %+v", opens)
			}
			cmds := s.pubsMatching(isCmd)
			if len(cmds) != 2 {
				t.Fatalf("sequence must stop after the failing host; cmds=%+v", cmds)
			}
			if !strings.Contains(out.String(), "NOT cutting relay 0") || !strings.Contains(out.String(), "not attempted: intel-d1babf, intel-177e78, intel-1ecbc8") {
				t.Fatalf("missing failure report:\n%s", out)
			}
		})
	}
}

func TestPerPCTimeoutOverride(t *testing.T) {
	s := newSim(t)
	s.host("intel-a163f8").shutDelay = 200 * time.Second // > 120 s default, < 300 s override
	o, out := s.orch()
	if err := o.Shutdown(context.Background(), []string{"all"}, ShutdownOpts{}); err != nil {
		t.Fatalf("shutdown: %v\n%s", err, out)
	}
	s.assertGated(t, 30*time.Second)
	s2 := newSim(t)
	s2.host("intel-91c45d").shutDelay = 200 * time.Second
	o2, _ := s2.orch()
	if err := o2.Shutdown(context.Background(), []string{"all"}, ShutdownOpts{}); err == nil {
		t.Fatal("intel-91c45d uses the 120 s default and must time out")
	}
}

func TestNoRelayNeverPublishesRelay(t *testing.T) {
	s := newSim(t)
	o, out := s.orch()
	if err := o.Shutdown(context.Background(), []string{"intel-d1babf"}, ShutdownOpts{Force: true, NoRelay: true}); err != nil {
		t.Fatalf("%v\n%s", err, out)
	}
	for _, e := range s.pubs() {
		if strings.HasPrefix(e.topic, "rack/relay/") {
			t.Fatalf("relay publish with --no-relay: %+v", e)
		}
	}
	if _, _, ok := s.firstPub(TopicPCCmd("intel-d1babf"), "shutdown"); !ok {
		t.Fatal("shutdown cmd not sent")
	}
}

func TestCancelDuringPingWaitNeverCuts(t *testing.T) {
	s := newSim(t)
	s.host("intel-a163f8").shutDelay = time.Hour
	ctx, cancel := context.WithCancel(context.Background())
	o, _ := s.orch()
	o.Pinger = pingerFunc(func(ctx context.Context, ip string, d time.Duration) (bool, error) {
		if s.clk.Now().Sub(time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)) > 20*time.Second {
			cancel()
			return false, nil // a cancelled probe looks like a miss; it must not count
		}
		return s.Ping(ctx, ip, d)
	})
	err := o.Shutdown(ctx, []string{"all"}, ShutdownOpts{})
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("want context.Canceled, got %v", err)
	}
	if n := len(s.pubsMatching(isRelayOpen)); n != 0 {
		t.Fatalf("relay opened after cancel: %d", n)
	}
}

type pingerFunc func(context.Context, string, time.Duration) (bool, error)

func (f pingerFunc) Ping(ctx context.Context, ip string, d time.Duration) (bool, error) {
	return f(ctx, ip, d)
}

// A broken pinger (no CAP_NET_RAW, EMFILE, ...) must never make a running
// host look down.
func TestPingerErrorNeverCuts(t *testing.T) {
	s := newSim(t)
	o, out := s.orch()
	o.Pinger = pingerFunc(func(context.Context, string, time.Duration) (bool, error) {
		return false, errors.New("socket: operation not permitted")
	})
	err := o.Shutdown(context.Background(), []string{"all"}, ShutdownOpts{})
	if !errors.Is(err, ErrPinger) {
		t.Fatalf("want ErrPinger, got %v\n%s", err, out)
	}
	if n := len(s.pubsMatching(isRelayOpen)); n != 0 {
		t.Fatalf("relay opened with a broken pinger: %d", n)
	}
	if n := len(s.pubsMatching(isCmd)); n != 1 {
		t.Fatalf("sequence must stop at the first host, cmds=%d", n)
	}
	if !strings.Contains(out.String(), "not attempted: intel-91c45d") || !strings.Contains(out.String(), "intel-a163f8: shutdown was sent; relay 4 NOT cut") {
		t.Fatalf("stop report:\n%s", out)
	}
}

// --- relay command -----------------------------------------------------------

func TestRelayOpenRefusesLiveHost(t *testing.T) {
	s := newSim(t) // every PC is up
	o, _ := s.orch()
	err := o.Relay(context.Background(), "open", 1, false)
	if !errors.Is(err, ErrRefused) || !strings.Contains(err.Error(), "intel-d1babf") {
		t.Fatalf("want refusal naming intel-d1babf, got %v", err)
	}
	if n := len(s.pubs()); n != 0 {
		t.Fatalf("refused relay open published %d messages", n)
	}
}

func TestRelayOpenForceBypassesLiveCheck(t *testing.T) {
	s := newSim(t)
	o, _ := s.orch()
	if err := o.Relay(context.Background(), "open", 1, true); err != nil {
		t.Fatal(err)
	}
	if _, _, ok := s.firstPub(TopicRelaySet(1), "open"); !ok {
		t.Fatal("forced open not published")
	}
	for _, e := range s.events {
		if e.kind == "ping" {
			t.Fatal("--force should not ping")
		}
	}
	if m, _ := s.Get(TopicRelayState(1)); m.Payload != "open" {
		t.Fatalf("state %q", m.Payload)
	}
}

func TestRelayOpenDownHostAndUnmappedChannel(t *testing.T) {
	s := newSim(t)
	s.host("intel-d1babf").powered = false
	o, _ := s.orch()
	if err := o.Relay(context.Background(), "open", 1, false); err != nil {
		t.Fatalf("down host: %v", err)
	}
	for _, ch := range []int{3, 5, 7} {
		if err := o.Relay(context.Background(), "open", ch, false); err != nil {
			t.Fatalf("unmapped ch %d: %v", ch, err)
		}
	}
	// close is always allowed, and a dead sidecar is reported
	if err := o.Relay(context.Background(), "close", 2, false); err != nil {
		t.Fatal(err)
	}
	s.relayDead = true
	if err := o.Relay(context.Background(), "close", 3, false); err == nil {
		t.Fatal("unconfirmed relay state accepted")
	}
	for _, bad := range []struct {
		a  string
		ch int
	}{{"toggle", 1}, {"open", 8}, {"open", -1}} {
		if err := o.Relay(context.Background(), bad.a, bad.ch, false); err == nil {
			t.Fatalf("%v accepted", bad)
		}
	}
}

func TestMapVerifyRefusalSuggestsRelayOpen(t *testing.T) {
	s := newSim(t)
	s.rackOff()
	s.setRetained(TopicRelayState(5), "closed")
	o, _ := s.orch()
	_, err := o.MapVerify(context.Background())
	if err == nil || !strings.Contains(err.Error(), "rackctl relay open 5") {
		t.Fatalf("got %v", err)
	}
}

func TestRelayConfirmTimeoutWarnsButContinues(t *testing.T) {
	s := newSim(t)
	s.relayDead = true
	o, out := s.orch()
	err := o.Shutdown(context.Background(), []string{"all"}, ShutdownOpts{})
	if err == nil || !strings.Contains(err.Error(), "not confirmed") {
		t.Fatalf("want warning error, got %v\n%s", err, out)
	}
	if n := len(s.pubsMatching(isCmd)); n != 5 {
		t.Fatalf("all hosts should still be shut down gracefully, cmds=%d", n)
	}
	s.assertGated(t, 30*time.Second)
}

// --- guard ---------------------------------------------------------------

func TestCheckGuard(t *testing.T) {
	cfg, err := config.Parse(testConfig)
	if err != nil {
		t.Fatal(err)
	}
	set := func(n ...string) map[string]bool {
		m := map[string]bool{}
		for _, x := range n {
			m[x] = true
		}
		return m
	}
	cases := []struct {
		name string
		down map[string]bool
		ok   bool
		why  string
	}{
		{"single osd", set("intel-d1babf"), true, ""},
		{"single mon/vm", set("intel-a163f8"), true, ""},
		{"vm + one osd", set("intel-a163f8", "intel-91c45d"), true, ""},
		{"two osd", set("intel-91c45d", "intel-d1babf"), false, "OSD"},
		{"two mon", set("intel-a163f8", "intel-177e78"), false, "mon"},
		{"three down", set("intel-a163f8", "intel-91c45d", "intel-d1babf"), false, "PVE"},
	}
	for _, c := range cases {
		err := CheckGuard(cfg, c.down)
		if c.ok != (err == nil) {
			t.Errorf("%s: got %v", c.name, err)
			continue
		}
		if !c.ok && (!errors.Is(err, ErrRefused) || !strings.Contains(err.Error(), c.why)) {
			t.Errorf("%s: want %q refusal, got %v", c.name, c.why, err)
		}
	}
	// quorum on its own: raise pve_quorum so one vm-only host down is refused.
	cfg.Shutdown.PVEQuorum = 5
	if err := CheckGuard(cfg, set("intel-a163f8")); err == nil || !strings.Contains(err.Error(), "quorum") {
		t.Errorf("quorum: got %v", err)
	}
}

func TestPartialShutdownGuardUsesAlreadyDownHosts(t *testing.T) {
	s := newSim(t)
	s.host("intel-91c45d").powered = false // already down
	o, out := s.orch()
	err := o.Shutdown(context.Background(), []string{"intel-d1babf"}, ShutdownOpts{})
	if !errors.Is(err, ErrRefused) {
		t.Fatalf("want refusal, got %v\n%s", err, out)
	}
	if n := len(s.pubs()); n != 0 {
		t.Fatalf("refused shutdown published %d messages", n)
	}

	// --force bypasses the guard.
	s2 := newSim(t)
	s2.host("intel-91c45d").powered = false
	o2, out2 := s2.orch()
	if err := o2.Shutdown(context.Background(), []string{"intel-d1babf"}, ShutdownOpts{Force: true}); err != nil {
		t.Fatalf("force: %v\n%s", err, out2)
	}
	if _, _, ok := s2.firstPub(TopicRelaySet(1), "open"); !ok {
		t.Fatal("forced shutdown did not complete")
	}
	s2.assertGated(t, 30*time.Second)
}

func TestPartialShutdownRefusedTwoMon(t *testing.T) {
	s := newSim(t)
	o, _ := s.orch()
	err := o.Shutdown(context.Background(), []string{"intel-a163f8", "intel-177e78"}, ShutdownOpts{})
	if !errors.Is(err, ErrRefused) || len(s.pubs()) != 0 {
		t.Fatalf("want refusal with no publishes, got %v / %d pubs", err, len(s.pubs()))
	}
}

func TestAllSkipsGuard(t *testing.T) {
	s := newSim(t)
	s.host("intel-91c45d").powered = false
	s.host("intel-d1babf").powered = false
	o, out := s.orch()
	if err := o.Shutdown(context.Background(), []string{"all"}, ShutdownOpts{}); err != nil {
		t.Fatalf("%v\n%s", err, out)
	}
}

// --- ceph ------------------------------------------------------------------

func TestCephOnBeforeFirstOSDHost(t *testing.T) {
	s := newSim(t)
	o, out := s.orch()
	if err := o.Shutdown(context.Background(), []string{"intel-a163f8", "intel-91c45d"}, ShutdownOpts{}); err != nil {
		t.Fatalf("%v\n%s", err, out)
	}
	_, ci, ok := s.firstPub(TopicCephSet, "on")
	_, oi, _ := s.firstPub(TopicPCCmd("intel-a163f8"), "shutdown")
	if !ok || ci > oi {
		t.Fatalf("ceph on (idx %d) must precede the first shutdown (idx %d)", ci, oi)
	}

	// No osd target -> no ceph publish.
	s2 := newSim(t)
	o2, _ := s2.orch()
	if err := o2.Shutdown(context.Background(), []string{"intel-a163f8"}, ShutdownOpts{}); err != nil {
		t.Fatal(err)
	}
	if _, _, ok := s2.firstPub(TopicCephSet, "on"); ok {
		t.Fatal("ceph maintenance set for a non-osd target")
	}
}

func TestCephFailureAbortsUnlessForce(t *testing.T) {
	for _, mode := range []string{"dead", "error"} {
		s := newSim(t)
		s.cephMode = mode
		o, _ := s.orch()
		err := o.Shutdown(context.Background(), []string{"all"}, ShutdownOpts{})
		if err == nil || len(s.pubsMatching(isCmd)) != 0 {
			t.Fatalf("%s: want abort before any cmd, got %v", mode, err)
		}
		if mode == "dead" {
			// re-published while waiting, within 30 s
			if n := len(s.pubsMatching(func(e event) bool { return e.topic == TopicCephSet })); n < 2 {
				t.Fatalf("expected ceph republish, got %d", n)
			}
		}
	}
	s := newSim(t)
	s.cephMode = "dead"
	o, out := s.orch()
	if err := o.Shutdown(context.Background(), []string{"all"}, ShutdownOpts{Force: true}); err != nil {
		t.Fatalf("force: %v\n%s", err, out)
	}
	if len(s.pubsMatching(isCmd)) != 5 {
		t.Fatal("force should continue")
	}
}

func TestCephMaintenanceCommand(t *testing.T) {
	s := newSim(t)
	o, _ := s.orch()
	if err := o.CephMaintenance(context.Background(), "on"); err != nil {
		t.Fatal(err)
	}
	if err := o.CephMaintenance(context.Background(), "bogus"); err == nil {
		t.Fatal("bogus accepted")
	}
	s.cephMode = "error"
	if err := o.CephMaintenance(context.Background(), "off"); err == nil {
		t.Fatal("error state accepted")
	}
}

// --- up ----------------------------------------------------------------------

func TestUpReverseOrderStaggerAndCephOffLast(t *testing.T) {
	s := newSim(t)
	s.rackOff()
	s.host("intel-91c45d").bootDelay = 90 * time.Second // slowest osd host
	o, out := s.orch()
	if err := o.Up(context.Background(), []string{"all"}); err != nil {
		t.Fatalf("%v\n%s", err, out)
	}
	closes := s.pubsMatching(func(e event) bool { return strings.HasSuffix(e.topic, "/set") && e.payload == "close" })
	if len(closes) != 5 {
		t.Fatalf("closes: %+v", closes)
	}
	for i, e := range closes {
		p, _ := s.cfg.PC(order[len(order)-1-i])
		if e.topic != TopicRelaySet(p.Relay) {
			t.Fatalf("close %d = %s, want relay %d (%s)", i, e.topic, p.Relay, p.Name)
		}
		if i > 0 && e.t.Sub(closes[i-1].t) < 5*time.Second {
			t.Fatalf("stagger %s < 5s", e.t.Sub(closes[i-1].t))
		}
	}
	off, _, ok := s.firstPub(TopicCephSet, "off")
	if !ok {
		t.Fatalf("ceph off never published\n%s", out)
	}
	// every osd host must have answered a ping before ceph off
	for _, p := range s.cfg.PCs {
		if !p.HasRole(config.RoleOSD) {
			continue
		}
		answered := false
		for _, e := range s.events {
			if e.kind == "ping" && e.ip == p.IP && e.ok && !e.t.After(off.t) {
				answered = true
			}
		}
		if !answered {
			t.Fatalf("ceph off at %s before osd host %s answered ping", off.t, p.Name)
		}
	}
}

func TestUpReportsHostsThatNeverCameUp(t *testing.T) {
	s := newSim(t)
	s.rackOff()
	s.host("intel-d1babf").bootDelay = time.Hour
	o, out := s.orch()
	err := o.Up(context.Background(), []string{"all"})
	if err == nil || !strings.Contains(err.Error(), "never came up: intel-d1babf") {
		t.Fatalf("got %v\n%s", err, out)
	}
	if _, _, ok := s.firstPub(TopicCephSet, "off"); ok {
		t.Fatal("ceph off with an osd host down")
	}
}

func TestUpPartialNonOSDStillChecksOSDHosts(t *testing.T) {
	s := newSim(t)
	s.host("intel-a163f8").powered = false
	s.host("intel-91c45d").powered = false // osd host down, not a target
	o, _ := s.orch()
	if err := o.Up(context.Background(), []string{"intel-a163f8"}); err != nil {
		t.Fatal(err)
	}
	if _, _, ok := s.firstPub(TopicCephSet, "off"); ok {
		t.Fatal("ceph off while an osd host is down")
	}
}

// --- map-verify -----------------------------------------------------------------

func TestMapVerifyPreconditions(t *testing.T) {
	cases := map[string]func(s *sim){
		"pc up":         func(s *sim) { s.host("intel-d1babf").powered = true },
		"relay closed":  func(s *sim) { s.setRetained(TopicRelayState(5), "closed") },
		"relay unknown": func(s *sim) { delete(s.last, TopicRelayState(7)) },
	}
	for name, mut := range cases {
		t.Run(name, func(t *testing.T) {
			s := newSim(t)
			s.rackOff()
			mut(s)
			o, _ := s.orch()
			_, err := o.MapVerify(context.Background())
			if !errors.Is(err, ErrRefused) {
				t.Fatalf("want refusal, got %v", err)
			}
			if n := len(s.pubs()); n != 0 {
				t.Fatalf("refused map-verify published %d messages", n)
			}
		})
	}
}

func TestMapVerifyDiscoversWiring(t *testing.T) {
	s := newSim(t)
	s.rackOff()
	// Physical wiring differs from config: d1babf is really on 3, 91c45d on 1.
	s.wiring = map[int]string{1: "intel-91c45d", 2: "intel-177e78", 3: "intel-d1babf", 4: "intel-a163f8", 6: "intel-1ecbc8"}
	o, out := s.orch()
	found, err := o.MapVerify(context.Background())
	if err == nil || !strings.Contains(err.Error(), "differs") {
		t.Fatalf("want mismatch error, got %v\n%s", err, out)
	}
	for ch, n := range s.wiring {
		if found[ch] != n {
			t.Fatalf("ch %d: found %q want %q\n%s", ch, found[ch], n, out)
		}
	}
	if len(found) != 5 {
		t.Fatalf("found %v", found)
	}
	s.assertGated(t, 30*time.Second)
	if !strings.Contains(out.String(), "name = \"intel-d1babf\"\nip = \"192.168.8.194\"\nrelay = 3") {
		t.Fatalf("snippet missing:\n%s", out)
	}
	// every channel ends open
	for ch := 0; ch < 8; ch++ {
		if m, _ := s.Get(TopicRelayState(ch)); m.Payload != "open" {
			t.Fatalf("relay %d left %s", ch, m.Payload)
		}
	}
}

func TestMapVerifyAgentNeverOnlineLeavesRelayClosed(t *testing.T) {
	s := newSim(t)
	s.rackOff()
	s.host("intel-91c45d").agentDead = true
	o, _ := s.orch()
	_, err := o.MapVerify(context.Background())
	if err == nil || !strings.Contains(err.Error(), "never came online") {
		t.Fatalf("got %v", err)
	}
	if len(s.pubsMatching(isRelayOpen)) != 0 {
		t.Fatal("relay opened under a running PC")
	}
}

// --- dry run --------------------------------------------------------------------

type countingBus struct{ n int }

func (b *countingBus) Publish(context.Context, string, string) error { b.n++; return nil }
func (b *countingBus) Get(string) (Msg, bool)                        { b.n++; return Msg{}, false }
func (b *countingBus) Mark() uint64                                  { b.n++; return 0 }

type countingPinger struct{ n int }

func (p *countingPinger) Ping(context.Context, string, time.Duration) (bool, error) {
	p.n++
	return false, nil
}

func TestDryRunTouchesNothing(t *testing.T) {
	cfg, _ := config.Parse(testConfig)
	bus, pinger := &countingBus{}, &countingPinger{}
	var out bytes.Buffer
	o := New(cfg, bus, pinger, newClock(), &out, true)
	ctx := context.Background()
	steps := []func() error{
		func() error { return o.Shutdown(ctx, []string{"all"}, ShutdownOpts{}) },
		func() error { return o.Shutdown(ctx, []string{"intel-d1babf"}, ShutdownOpts{}) },
		func() error { return o.Up(ctx, []string{"all"}) },
		func() error { _, err := o.MapVerify(ctx); return err },
		func() error { return o.CephMaintenance(ctx, "on") },
		func() error { return o.Status(ctx) },
		func() error { return o.Relay(ctx, "open", 1, false) },
	}
	for i, f := range steps {
		if err := f(); err != nil {
			t.Fatalf("step %d: %v", i, err)
		}
	}
	if bus.n != 0 || pinger.n != 0 {
		t.Fatalf("dry run touched bus %d times, pinger %d times", bus.n, pinger.n)
	}
	s := out.String()
	for _, want := range []string{
		"publish rack/ceph/maintenance/set = \"on\"",
		"publish rack/pc/intel-a163f8/cmd = \"shutdown\"",
		"wait <=2m0s for sustained ping loss from 192.168.8.196",
		"sleep 30s (cut_delay)",
		"publish rack/relay/4/set = \"open\"",
		"publish rack/relay/6/set = \"close\"",
	} {
		if !strings.Contains(s, want) {
			t.Errorf("dry-run output missing %q", want)
		}
	}
}

func TestResolve(t *testing.T) {
	s := newSim(t)
	o, _ := s.orch()
	if _, _, err := o.resolve(nil); err == nil {
		t.Error("empty targets accepted")
	}
	if _, _, err := o.resolve([]string{"all", "intel-d1babf"}); err == nil {
		t.Error("all + name accepted")
	}
	if _, _, err := o.resolve([]string{"nope"}); err == nil {
		t.Error("unknown accepted")
	}
	pcs, all, err := o.resolve([]string{"intel-1ecbc8", "intel-a163f8", "intel-1ecbc8"})
	if err != nil || all || len(pcs) != 2 || pcs[0].Name != "intel-a163f8" || pcs[1].Name != "intel-1ecbc8" {
		t.Errorf("resolve order/dedupe: %v %v %v", pcs, all, err)
	}
}

func TestValidState(t *testing.T) {
	good := [][2]string{
		{"rack/pc/intel-d1babf/status", "online"}, {"rack/pc/x/status", "down"},
		{"rack/relay/0/state", "open"}, {"rack/relay/7/state", "closed"},
		{"rack/sbc4/status", "offline"}, {"rack/ceph/maintenance/state", "error"},
	}
	bad := [][2]string{
		{"rack/pc/x/status", "Online"}, {"rack/pc/x/status", "online\n"}, {"rack/relay/8/state", "open"},
		{"rack/relay/01/state", "open"}, {"rack/relay/-1/state", "open"}, {"rack/relay/0/state", "close"},
		{"rack/pc/x/cmd", "shutdown"}, {"rack/relay/0/set", "open"}, {"rack/ceph/maintenance/state", "ON"},
		{"rack/pc//status", "online"},
	}
	for _, g := range good {
		if !ValidState(g[0], g[1]) {
			t.Errorf("rejected %v", g)
		}
	}
	for _, b := range bad {
		if ValidState(b[0], b[1]) {
			t.Errorf("accepted %v", b)
		}
	}
}
