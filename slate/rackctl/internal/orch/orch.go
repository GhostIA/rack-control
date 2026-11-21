package orch

import (
	"context"
	"errors"
	"fmt"
	"io"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/GhostIA/rack-control/slate/rackctl/internal/config"
)

// Orchestrator runs the rack procedures. In DryRun mode Bus and Pinger are
// never touched (they may be nil): every publish and wait is printed instead.
type Orchestrator struct {
	Cfg    *config.Config
	Bus    Bus
	Pinger Pinger
	Clock  Clock
	Out    io.Writer
	DryRun bool

	PingInterval     time.Duration // time between probes (1 s)
	PingTimeout      time.Duration // per-probe reply timeout (1 s)
	DownMisses       int           // consecutive misses = sustained down (5)
	PollInterval     time.Duration // retained-state poll period
	CephTimeout      time.Duration // ceph maintenance on: wait for state
	CephOffTimeout   time.Duration // ceph maintenance off after up (agent may still be booting)
	CephRepublish    time.Duration // re-send the ceph command this often while waiting
	RelayTimeout     time.Duration // wait for relay state after set
	UpStagger        time.Duration // between relay closes in up
	UpTimeout        time.Duration // per host, close -> first ping reply
	MapUpTimeout     time.Duration // map-verify: close -> any IP answers
	MapOnlineTimeout time.Duration // map-verify: wait for agent "online"
	// MapChannels limits map-verify to these relay channels (nil = 0-7).
	MapChannels []int
}

// New returns an Orchestrator with the production tunables.
func New(cfg *config.Config, bus Bus, p Pinger, clk Clock, out io.Writer, dry bool) *Orchestrator {
	return &Orchestrator{
		Cfg: cfg, Bus: bus, Pinger: p, Clock: clk, Out: out, DryRun: dry,
		PingInterval:     time.Second,
		PingTimeout:      time.Second,
		DownMisses:       5,
		PollInterval:     200 * time.Millisecond,
		CephTimeout:      30 * time.Second,
		CephOffTimeout:   180 * time.Second,
		CephRepublish:    10 * time.Second,
		RelayTimeout:     10 * time.Second,
		UpStagger:        5 * time.Second,
		UpTimeout:        300 * time.Second,
		MapUpTimeout:     300 * time.Second,
		MapOnlineTimeout: 120 * time.Second,
	}
}

// errReported marks a wait that ended on an explicit failure payload.
var errReported = errors.New("failure state")

// ErrPinger marks a local ICMP failure; the procedure stops without cutting.
var ErrPinger = errors.New("pinger failed (not treated as host down)")

// ErrRefused is returned when the partial-shutdown guard refuses.
var ErrRefused = errors.New("refused by safety guard")

func (o *Orchestrator) logf(format string, a ...any) {
	fmt.Fprintf(o.Out, format+"\n", a...)
}

func (o *Orchestrator) dryf(format string, a ...any) {
	fmt.Fprintf(o.Out, "  [dry-run] "+format+"\n", a...)
}

// ---------------------------------------------------------------------------
// primitives

func (o *Orchestrator) publish(ctx context.Context, topic, payload string) error {
	if o.DryRun {
		o.dryf("publish %s = %q (QoS 1, not retained)", topic, payload)
		return nil
	}
	if err := o.Bus.Publish(ctx, topic, payload); err != nil {
		return fmt.Errorf("publish %s=%s: %w", topic, payload, err)
	}
	return nil
}

func (o *Orchestrator) mark() uint64 {
	if o.DryRun {
		return 0
	}
	return o.Bus.Mark()
}

func (o *Orchestrator) sleep(ctx context.Context, d time.Duration, why string) error {
	if o.DryRun {
		o.dryf("sleep %s (%s)", d, why)
		return nil
	}
	return o.Clock.Sleep(ctx, d)
}

// waitFresh waits until a message newer than `after` arrives on topic with
// payload want. A fresh payload listed in failOn ends the wait with an error.
func (o *Orchestrator) waitFresh(ctx context.Context, topic, want string, after uint64, timeout time.Duration, failOn ...string) error {
	if o.DryRun {
		o.dryf("wait <=%s for %s = %q", timeout, topic, want)
		return nil
	}
	deadline := o.Clock.Now().Add(timeout)
	for {
		if m, ok := o.Bus.Get(topic); ok && m.Seq > after {
			if m.Payload == want {
				return nil
			}
			for _, f := range failOn {
				if m.Payload == f {
					return fmt.Errorf("%w: %s reported %q", errReported, topic, m.Payload)
				}
			}
		}
		if !o.Clock.Now().Before(deadline) {
			if m, ok := o.Bus.Get(topic); ok {
				return fmt.Errorf("timed out after %s waiting for %s = %q (last: %q)", timeout, topic, want, m.Payload)
			}
			return fmt.Errorf("timed out after %s waiting for %s = %q (nothing received)", timeout, topic, want)
		}
		if err := o.Clock.Sleep(ctx, o.PollInterval); err != nil {
			return err
		}
	}
}

// pingOnce sends one ping. A cancelled context or a local pinger failure is
// an error and never a "miss".
func (o *Orchestrator) pingOnce(ctx context.Context, ip string) (bool, error) {
	ok, err := o.Pinger.Ping(ctx, ip, o.PingTimeout)
	if cerr := ctx.Err(); cerr != nil {
		return false, cerr
	}
	if err != nil {
		return false, fmt.Errorf("%w: ping %s: %v", ErrPinger, ip, err)
	}
	return ok, nil
}

// probe sends one ping and pads to PingInterval so probes run at 1 Hz.
// A cancelled context is reported as an error and never counts as a miss.
func (o *Orchestrator) probe(ctx context.Context, ip string) (bool, error) {
	start := o.Clock.Now()
	ok, err := o.pingOnce(ctx, ip)
	if err != nil {
		return false, err
	}
	if rest := o.PingInterval - o.Clock.Now().Sub(start); rest > 0 {
		if err := o.Clock.Sleep(ctx, rest); err != nil {
			return false, err
		}
	}
	return ok, nil
}

// waitPingDown returns true once ip missed DownMisses consecutive probes.
// Any reply resets the count (a flapping host is never "down").
func (o *Orchestrator) waitPingDown(ctx context.Context, ip string, timeout time.Duration) (bool, time.Duration, error) {
	if o.DryRun {
		o.dryf("wait <=%s for sustained ping loss from %s (%d consecutive misses at %s)", timeout, ip, o.DownMisses, o.PingInterval)
		return true, 0, nil
	}
	start := o.Clock.Now()
	deadline := start.Add(timeout)
	misses := 0
	for {
		ok, err := o.probe(ctx, ip)
		if err != nil {
			return false, o.Clock.Now().Sub(start), err
		}
		if ok {
			misses = 0
		} else {
			misses++
			if misses >= o.DownMisses {
				return true, o.Clock.Now().Sub(start), nil
			}
		}
		if !o.Clock.Now().Before(deadline) {
			return false, o.Clock.Now().Sub(start), nil
		}
	}
}

// sustainedDown probes DownMisses times; any reply means "up".
func (o *Orchestrator) sustainedDown(ctx context.Context, ip string) (bool, error) {
	for i := 0; i < o.DownMisses; i++ {
		ok, err := o.probe(ctx, ip)
		if err != nil || ok {
			return false, err
		}
	}
	return true, nil
}

// waitPingUp returns true on the first reply.
func (o *Orchestrator) waitPingUp(ctx context.Context, ip string, timeout time.Duration) (bool, time.Duration, error) {
	if o.DryRun {
		o.dryf("wait <=%s for %s to answer ping", timeout, ip)
		return true, 0, nil
	}
	start := o.Clock.Now()
	deadline := start.Add(timeout)
	for {
		ok, err := o.probe(ctx, ip)
		if err != nil {
			return false, o.Clock.Now().Sub(start), err
		}
		if ok {
			return true, o.Clock.Now().Sub(start), nil
		}
		if !o.Clock.Now().Before(deadline) {
			return false, o.Clock.Now().Sub(start), nil
		}
	}
}

// quickUp: up if any of three probes answers.
func (o *Orchestrator) quickUp(ctx context.Context, ip string) (bool, error) {
	for i := 0; i < 3; i++ {
		ok, err := o.probe(ctx, ip)
		if err != nil || ok {
			return ok, err
		}
	}
	return false, nil
}

// pingAll runs quickUp for every PC concurrently.
func (o *Orchestrator) pingAll(ctx context.Context, pcs []config.PC) (map[string]bool, error) {
	up := make(map[string]bool, len(pcs))
	var mu sync.Mutex
	var wg sync.WaitGroup
	var firstErr error
	for _, p := range pcs {
		wg.Add(1)
		go func(p config.PC) {
			defer wg.Done()
			ok, err := o.quickUp(ctx, p.IP)
			mu.Lock()
			defer mu.Unlock()
			if err != nil && firstErr == nil {
				firstErr = err
			}
			up[p.Name] = ok
		}(p)
	}
	wg.Wait()
	return up, firstErr
}

// setCeph publishes the maintenance command and waits for the matching
// state, re-sending every CephRepublish until timeout.
func (o *Orchestrator) setCeph(ctx context.Context, val string, timeout time.Duration) error {
	o.logf("ceph maintenance %s: publishing %s", val, TopicCephSet)
	if o.DryRun {
		_ = o.publish(ctx, TopicCephSet, val)
		return o.waitFresh(ctx, TopicCephState, val, 0, timeout)
	}
	mark := o.mark()
	deadline := o.Clock.Now().Add(timeout)
	for {
		if err := o.publish(ctx, TopicCephSet, val); err != nil {
			return err
		}
		remaining := deadline.Sub(o.Clock.Now())
		step := o.CephRepublish
		if remaining < step {
			step = remaining
		}
		err := o.waitFresh(ctx, TopicCephState, val, mark, step, "error")
		if err == nil {
			o.logf("ceph maintenance %s: confirmed", val)
			return nil
		}
		if ctx.Err() != nil || errors.Is(err, errReported) || !o.Clock.Now().Before(deadline) {
			return fmt.Errorf("ceph maintenance %s not confirmed within %s (re-sent every %s): %w", val, timeout, o.CephRepublish, err)
		}
	}
}

// CephMaintenance implements `rackctl ceph-maintenance on|off`.
func (o *Orchestrator) CephMaintenance(ctx context.Context, val string) error {
	if val != "on" && val != "off" {
		return fmt.Errorf("ceph-maintenance: want on or off, got %q", val)
	}
	return o.setCeph(ctx, val, o.CephTimeout)
}

// ---------------------------------------------------------------------------
// target resolution and guard

// resolve maps CLI targets to PCs in config shutdown order. It returns
// all=true for the single word "all".
func (o *Orchestrator) resolve(targets []string) ([]config.PC, bool, error) {
	if len(targets) == 0 {
		return nil, false, errors.New("no targets: name one or more PCs, or say 'all'")
	}
	for _, t := range targets {
		if t == "all" {
			if len(targets) != 1 {
				return nil, false, errors.New("'all' cannot be combined with PC names")
			}
			return o.Cfg.Ordered(), true, nil
		}
	}
	want := map[string]bool{}
	for _, t := range targets {
		if _, ok := o.Cfg.PC(t); !ok {
			return nil, false, fmt.Errorf("unknown PC %q", t)
		}
		want[t] = true
	}
	var out []config.PC
	for _, p := range o.Cfg.Ordered() {
		if want[p.Name] {
			out = append(out, p)
		}
	}
	return out, false, nil
}

// CheckGuard evaluates the partial-shutdown safety rules for the set of PCs
// that would be down afterwards.
func CheckGuard(cfg *config.Config, down map[string]bool) error {
	var osd, mon []string
	nDown := 0
	for _, p := range cfg.PCs {
		if !down[p.Name] {
			continue
		}
		nDown++
		if p.HasRole(config.RoleOSD) {
			osd = append(osd, p.Name)
		}
		if p.HasRole(config.RoleMon) {
			mon = append(mon, p.Name)
		}
	}
	var reasons []string
	if len(osd) >= 2 {
		reasons = append(reasons, fmt.Sprintf("%d OSD hosts would be down (%s); Ceph EC 2+1 tolerates only 1", len(osd), strings.Join(osd, ", ")))
	}
	if len(mon) >= 2 {
		reasons = append(reasons, fmt.Sprintf("%d mon hosts would be down (%s); Ceph mon quorum would be lost", len(mon), strings.Join(mon, ", ")))
	}
	if up := len(cfg.PCs) - nDown; up < cfg.Shutdown.PVEQuorum {
		reasons = append(reasons, fmt.Sprintf("only %d PVE nodes would stay up; quorum needs %d", up, cfg.Shutdown.PVEQuorum))
	}
	if len(reasons) > 0 {
		return fmt.Errorf("%w: %s (use --force to override, or 'all' for a full rack shutdown)", ErrRefused, strings.Join(reasons, "; "))
	}
	return nil
}

func names(pcs []config.PC) []string {
	out := make([]string, len(pcs))
	for i, p := range pcs {
		out[i] = p.Name
	}
	return out
}

func sortedKeys(m map[string]bool) []string {
	var out []string
	for k, v := range m {
		if v {
			out = append(out, k)
		}
	}
	sort.Strings(out)
	return out
}

func round(d time.Duration) time.Duration { return d.Round(100 * time.Millisecond) }
