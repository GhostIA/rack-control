package orch

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"text/tabwriter"
	"time"

	"github.com/GhostIA/rack-control/slate/rackctl/internal/config"
)

// ShutdownOpts are the `shutdown` flags.
type ShutdownOpts struct {
	Force   bool
	NoRelay bool
}

// Shutdown gracefully stops the targets one at a time, in config order, and
// cuts each relay only after sustained ping loss plus cut_delay. A host that
// does not go down stops the whole sequence and is never cut.
func (o *Orchestrator) Shutdown(ctx context.Context, targets []string, opt ShutdownOpts) error {
	pcs, all, err := o.resolve(targets)
	if err != nil {
		return err
	}
	if o.DryRun {
		o.logf("DRY RUN: no broker connection, no pings; printing the sequence only")
	}
	o.logf("shutdown targets (in order): %s", strings.Join(names(pcs), ", "))
	if opt.NoRelay {
		o.logf("--no-relay: relays will NOT be opened")
	}

	if !all && !opt.Force {
		if err := o.guard(ctx, pcs); err != nil {
			return err
		}
	} else if !all {
		o.logf("--force: skipping the partial-shutdown safety guard")
	}

	needCeph := all
	for _, p := range pcs {
		if p.HasRole(config.RoleOSD) {
			needCeph = true
		}
	}
	if needCeph {
		if err := o.setCeph(ctx, "on", o.CephTimeout); err != nil {
			if ctx.Err() != nil || !opt.Force {
				return fmt.Errorf("aborting before any shutdown: %w (use --force to continue without it)", err)
			}
			o.logf("WARNING: %v; continuing because of --force", err)
		}
	}

	var warnings []string
	for i, p := range pcs {
		timeout := o.Cfg.TimeoutFor(p)
		o.logf("[%d/%d] %s (%s, relay %d)", i+1, len(pcs), p.Name, p.IP, p.Relay)
		o.logf("  sending shutdown")
		if err := o.publish(ctx, TopicPCCmd(p.Name), "shutdown"); err != nil {
			o.logf("  %s: shutdown may or may not have been delivered; relay %d NOT cut", p.Name, p.Relay)
			return o.stopped(pcs[i+1:], err)
		}
		o.logf("  waiting up to %s for sustained ping loss", timeout)
		down, took, err := o.waitPingDown(ctx, p.IP, timeout)
		if err != nil {
			o.logf("  %s: shutdown was sent; relay %d NOT cut", p.Name, p.Relay)
			return o.stopped(pcs[i+1:], err)
		}
		if !down {
			o.logf("  FAILED: %s still answers ping after %s; NOT cutting relay %d", p.Name, round(took), p.Relay)
			return o.stopped(pcs[i+1:], fmt.Errorf("%s did not go down within %s; relay %d left closed", p.Name, timeout, p.Relay))
		}
		if !o.DryRun {
			o.logf("  down (sustained ping loss after %s)", round(took))
		}
		if opt.NoRelay {
			o.logf("  --no-relay: leaving relay %d closed", p.Relay)
			continue
		}
		o.logf("  waiting cut_delay %s before cutting relay %d", o.Cfg.Shutdown.CutDelay.Duration, p.Relay)
		if err := o.sleep(ctx, o.Cfg.Shutdown.CutDelay.Duration, "cut_delay"); err != nil {
			o.logf("  %s is down; relay %d NOT cut", p.Name, p.Relay)
			return o.stopped(pcs[i+1:], err)
		}
		mark := o.mark()
		o.logf("  opening relay %d", p.Relay)
		if err := o.publish(ctx, TopicRelaySet(p.Relay), "open"); err != nil {
			return o.stopped(pcs[i+1:], err)
		}
		if err := o.waitFresh(ctx, TopicRelayState(p.Relay), "open", mark, o.RelayTimeout); err != nil {
			if ctx.Err() != nil {
				return o.stopped(pcs[i+1:], err)
			}
			w := fmt.Sprintf("%s: relay %d open not confirmed: %v", p.Name, p.Relay, err)
			o.logf("  WARNING: %s", w)
			warnings = append(warnings, w)
			continue
		}
		if !o.DryRun {
			o.logf("  relay %d open confirmed; %s is powered off", p.Relay, p.Name)
		}
	}
	if len(warnings) > 0 {
		return fmt.Errorf("shutdown finished with warnings:\n  %s", strings.Join(warnings, "\n  "))
	}
	o.logf("shutdown complete")
	return nil
}

func (o *Orchestrator) stopped(untouched []config.PC, err error) error {
	if len(untouched) > 0 {
		o.logf("sequence STOPPED; not attempted: %s", strings.Join(names(untouched), ", "))
	} else {
		o.logf("sequence STOPPED")
	}
	return err
}

// guard pings every non-target PC and applies CheckGuard to the down set.
func (o *Orchestrator) guard(ctx context.Context, targets []config.PC) error {
	down := map[string]bool{}
	for _, p := range targets {
		down[p.Name] = true
	}
	if o.DryRun {
		o.dryf("ping every PC to find hosts already down (assuming all up for this dry run)")
	} else {
		var others []config.PC
		for _, p := range o.Cfg.PCs {
			if !down[p.Name] {
				others = append(others, p)
			}
		}
		up, err := o.pingAll(ctx, others)
		if err != nil {
			return err
		}
		var already []string
		for _, p := range others {
			if !up[p.Name] {
				down[p.Name] = true
				already = append(already, p.Name)
			}
		}
		if len(already) > 0 {
			o.logf("already down (no ping reply): %s", strings.Join(already, ", "))
		}
	}
	if err := CheckGuard(o.Cfg, down); err != nil {
		return err
	}
	o.logf("guard: OK (down afterwards: %s)", strings.Join(sortedKeys(down), ", "))
	return nil
}

// Up closes relays in reverse config order with a stagger, waits for each
// host to answer ping, then turns Ceph maintenance off once every OSD host
// is up.
func (o *Orchestrator) Up(ctx context.Context, targets []string) error {
	pcs, _, err := o.resolve(targets)
	if err != nil {
		return err
	}
	for i, j := 0, len(pcs)-1; i < j; i, j = i+1, j-1 {
		pcs[i], pcs[j] = pcs[j], pcs[i]
	}
	if o.DryRun {
		o.logf("DRY RUN: no broker connection, no pings; printing the sequence only")
	}
	o.logf("up targets (in order): %s", strings.Join(names(pcs), ", "))

	var warnings []string
	closedAt := map[string]time.Time{}
	for i, p := range pcs {
		o.logf("[%d/%d] %s: closing relay %d", i+1, len(pcs), p.Name, p.Relay)
		mark := o.mark()
		if err := o.publish(ctx, TopicRelaySet(p.Relay), "close"); err != nil {
			return err
		}
		if err := o.waitFresh(ctx, TopicRelayState(p.Relay), "closed", mark, o.RelayTimeout); err != nil {
			if ctx.Err() != nil {
				return err
			}
			w := fmt.Sprintf("%s: relay %d closed not confirmed: %v", p.Name, p.Relay, err)
			o.logf("  WARNING: %s", w)
			warnings = append(warnings, w)
		}
		if !o.DryRun {
			closedAt[p.Name] = o.Clock.Now()
		}
		if i < len(pcs)-1 {
			if err := o.sleep(ctx, o.UpStagger, "stagger"); err != nil {
				return err
			}
		}
	}

	o.logf("waiting up to %s per host for ping replies", o.UpTimeout)
	up := map[string]bool{}
	var never []string
	if o.DryRun {
		for _, p := range pcs {
			_, _, _ = o.waitPingUp(ctx, p.IP, o.UpTimeout)
			up[p.Name] = true
		}
	} else {
		pending := append([]config.PC(nil), pcs...)
		for len(pending) > 0 {
			var next []config.PC
			for _, p := range pending {
				ok, err := o.pingOnce(ctx, p.IP)
				if err != nil {
					return err
				}
				switch {
				case ok:
					up[p.Name] = true
					o.logf("  %s is up (%s after relay close)", p.Name, round(o.Clock.Now().Sub(closedAt[p.Name])))
				case !o.Clock.Now().Before(closedAt[p.Name].Add(o.UpTimeout)):
					never = append(never, p.Name)
					o.logf("  %s did NOT answer ping within %s", p.Name, o.UpTimeout)
				default:
					next = append(next, p)
				}
			}
			pending = next
			if len(pending) > 0 {
				if err := o.Clock.Sleep(ctx, o.PingInterval); err != nil {
					return err
				}
			}
		}
	}

	// Ceph maintenance off only when every OSD host in the rack is up.
	var osdDown []string
	for _, p := range o.Cfg.Ordered() {
		if !p.HasRole(config.RoleOSD) || up[p.Name] {
			continue
		}
		if o.DryRun {
			o.dryf("check %s (osd) answers ping", p.Name)
			continue
		}
		ok, err := o.quickUp(ctx, p.IP)
		if err != nil {
			return err
		}
		if !ok {
			osdDown = append(osdDown, p.Name)
		}
	}
	if len(osdDown) > 0 {
		o.logf("leaving ceph maintenance ON: OSD hosts not up: %s", strings.Join(osdDown, ", "))
	} else if err := o.setCeph(ctx, "off", o.CephOffTimeout); err != nil {
		if ctx.Err() != nil {
			return err
		}
		warnings = append(warnings, err.Error())
		o.logf("WARNING: %v", err)
	}

	if len(never) > 0 {
		warnings = append(warnings, "never came up: "+strings.Join(never, ", "))
	}
	if len(warnings) > 0 {
		return fmt.Errorf("up finished with problems:\n  %s", strings.Join(warnings, "\n  "))
	}
	o.logf("up complete")
	return nil
}

// Status prints PCs (ping + retained status), relays, sbc4 and ceph state.
func (o *Orchestrator) Status(ctx context.Context) error {
	if o.DryRun {
		o.dryf("connect, read retained states, ping every PC and print a table")
		return nil
	}
	up, err := o.pingAll(ctx, o.Cfg.Ordered())
	if err != nil {
		return err
	}
	get := func(topic string) string {
		if m, ok := o.Bus.Get(topic); ok {
			return m.Payload
		}
		return "-"
	}
	tw := tabwriter.NewWriter(o.Out, 0, 0, 2, ' ', 0)
	fmt.Fprintln(tw, "PC\tIP\tRELAY\tROLES\tPING\tSTATUS\tRELAY STATE")
	pcByRelay := map[int]string{}
	for _, p := range o.Cfg.Ordered() {
		pcByRelay[p.Relay] = p.Name
		ping := "down"
		if up[p.Name] {
			ping = "up"
		}
		fmt.Fprintf(tw, "%s\t%s\t%d\t%s\t%s\t%s\t%s\n", p.Name, p.IP, p.Relay, strings.Join(p.Roles, ","), ping,
			get(TopicPCStatus(p.Name)), get(TopicRelayState(p.Relay)))
	}
	fmt.Fprintln(tw)
	fmt.Fprintln(tw, "RELAY\tSTATE\tPC")
	for ch := 0; ch < 8; ch++ {
		pc := pcByRelay[ch]
		if pc == "" {
			pc = "-"
		}
		fmt.Fprintf(tw, "%d\t%s\t%s\n", ch, get(TopicRelayState(ch)), pc)
	}
	fmt.Fprintln(tw)
	fmt.Fprintf(tw, "sbc4\t%s\n", get(TopicSBC4Status))
	fmt.Fprintf(tw, "ceph maintenance\t%s\n", get(TopicCephState))
	return tw.Flush()
}

// MapVerify discovers which relay channel powers which PC. It requires the
// whole rack to be off with every relay open, then closes one channel at a
// time, sees who boots, shuts it down gracefully and opens the relay again.
func (o *Orchestrator) MapVerify(ctx context.Context) (map[int]string, error) {
	pcs := o.Cfg.Ordered()
	if o.DryRun {
		o.logf("DRY RUN: no broker connection, no pings; printing the sequence only")
		o.dryf("require every PC sustained-down and every relay 0-7 retained state = \"open\"")
	} else if err := o.mapPreconditions(ctx, pcs); err != nil {
		return nil, err
	}

	found := map[int]string{}
	for ch := 0; ch < 8; ch++ {
		o.logf("channel %d: closing relay", ch)
		mark := o.mark()
		if err := o.publish(ctx, TopicRelaySet(ch), "close"); err != nil {
			// The close may still have been delivered: don't leave a PC
			// booting with nobody watching.
			o.reopen(ch)
			return found, err
		}
		if err := o.waitFresh(ctx, TopicRelayState(ch), "closed", mark, o.RelayTimeout); err != nil {
			o.reopen(ch)
			return found, fmt.Errorf("channel %d: %w", ch, err)
		}
		o.logf("  waiting up to %s for any configured PC to answer ping", o.MapUpTimeout)
		var who *config.PC
		if o.DryRun {
			o.dryf("round-robin ping %s", strings.Join(ipList(pcs), ", "))
			o.dryf("if a PC answers: wait <=%s for its status \"online\", send shutdown, wait for sustained ping loss, cut_delay, then open", o.MapOnlineTimeout)
		} else {
			p, err := o.waitAnyUp(ctx, pcs)
			if err != nil {
				// No PC has answered yet, so at most one is in early boot.
				o.reopen(ch)
				return found, err
			}
			who = p
		}
		if who == nil {
			if !o.DryRun {
				o.logf("  channel %d: no PC answered", ch)
			}
			if err := o.openAndConfirm(ctx, ch); err != nil {
				return found, err
			}
			continue
		}
		p := *who
		o.logf("  channel %d powers %s (%s)", ch, p.Name, p.IP)
		for _, other := range pcs {
			if other.Name == p.Name {
				continue
			}
			ok, err := o.quickUp(ctx, other.IP)
			if err != nil {
				return found, err
			}
			if ok {
				return found, fmt.Errorf("channel %d: both %s and %s answer ping; relay %d left CLOSED, resolve by hand", ch, p.Name, other.Name, ch)
			}
		}
		o.logf("  waiting up to %s for %s agent to report online", o.MapOnlineTimeout, p.Name)
		if err := o.waitFresh(ctx, TopicPCStatus(p.Name), "online", mark, o.MapOnlineTimeout); err != nil {
			return found, fmt.Errorf("channel %d: %s is running but its agent never came online (%w); relay %d left CLOSED", ch, p.Name, err, ch)
		}
		o.logf("  shutting %s down again", p.Name)
		if err := o.publish(ctx, TopicPCCmd(p.Name), "shutdown"); err != nil {
			return found, err
		}
		timeout := o.Cfg.TimeoutFor(p)
		down, took, err := o.waitPingDown(ctx, p.IP, timeout)
		if err != nil {
			return found, err
		}
		if !down {
			return found, fmt.Errorf("channel %d: %s did not go down within %s; relay %d left CLOSED", ch, p.Name, timeout, ch)
		}
		o.logf("  %s down after %s; waiting cut_delay %s", p.Name, round(took), o.Cfg.Shutdown.CutDelay.Duration)
		if err := o.sleep(ctx, o.Cfg.Shutdown.CutDelay.Duration, "cut_delay"); err != nil {
			return found, err
		}
		if err := o.openAndConfirm(ctx, ch); err != nil {
			return found, err
		}
		found[ch] = p.Name
	}
	if o.DryRun {
		return found, nil
	}
	return found, o.printMapping(found)
}

func ipList(pcs []config.PC) []string {
	out := make([]string, len(pcs))
	for i, p := range pcs {
		out[i] = p.IP
	}
	return out
}

func (o *Orchestrator) mapPreconditions(ctx context.Context, pcs []config.PC) error {
	o.logf("checking preconditions: every PC down, every relay open")
	type res struct {
		name string
		down bool
		err  error
	}
	ch := make(chan res, len(pcs))
	for _, p := range pcs {
		go func(p config.PC) {
			d, err := o.sustainedDown(ctx, p.IP)
			ch <- res{p.Name, d, err}
		}(p)
	}
	var problems []string
	for range pcs {
		r := <-ch
		if r.err != nil {
			return r.err
		}
		if !r.down {
			problems = append(problems, r.name+" answers ping")
		}
	}
	byRelay := map[int]string{}
	for _, p := range pcs {
		byRelay[p.Relay] = p.Name
	}
	for c := 0; c < 8; c++ {
		m, ok := o.Bus.Get(TopicRelayState(c))
		if ok && m.Payload == "open" {
			continue
		}
		state := "unknown (no retained state)"
		if ok {
			state = m.Payload
		}
		if pc := byRelay[c]; pc != "" {
			problems = append(problems, fmt.Sprintf("relay %d is %s (configured for %s: run 'rackctl shutdown %s' or 'rackctl shutdown all')", c, state, pc, pc))
		} else {
			problems = append(problems, fmt.Sprintf("relay %d is %s (no PC configured: run 'rackctl relay open %d')", c, state, c))
		}
	}
	if len(problems) > 0 {
		return fmt.Errorf("%w: map-verify needs the whole rack off and all 8 relays open:\n  %s",
			ErrRefused, strings.Join(problems, "\n  "))
	}
	return nil
}

// waitAnyUp round-robins pings over every PC until one answers.
func (o *Orchestrator) waitAnyUp(ctx context.Context, pcs []config.PC) (*config.PC, error) {
	deadline := o.Clock.Now().Add(o.MapUpTimeout)
	for {
		start := o.Clock.Now()
		for i := range pcs {
			ok, err := o.pingOnce(ctx, pcs[i].IP)
			if err != nil {
				return nil, err
			}
			if ok {
				p := pcs[i]
				return &p, nil
			}
		}
		if !o.Clock.Now().Before(deadline) {
			return nil, nil
		}
		if rest := o.PingInterval - o.Clock.Now().Sub(start); rest > 0 {
			if err := o.Clock.Sleep(ctx, rest); err != nil {
				return nil, err
			}
		}
	}
}

func (o *Orchestrator) openAndConfirm(ctx context.Context, ch int) error {
	mark := o.mark()
	o.logf("  opening relay %d", ch)
	if err := o.publish(ctx, TopicRelaySet(ch), "open"); err != nil {
		return err
	}
	if err := o.waitFresh(ctx, TopicRelayState(ch), "open", mark, o.RelayTimeout); err != nil {
		return fmt.Errorf("channel %d: %w", ch, err)
	}
	return nil
}

// reopen is a best-effort open after a failed close confirmation, so a
// relay in an unknown state does not boot a PC unattended.
func (o *Orchestrator) reopen(ch int) {
	if o.DryRun {
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if err := o.Bus.Publish(ctx, TopicRelaySet(ch), "open"); err != nil {
		o.logf("  WARNING: could not re-open relay %d: %v", ch, err)
	}
}

func (o *Orchestrator) printMapping(found map[int]string) error {
	byName := map[string]int{}
	for ch, n := range found {
		byName[n] = ch
	}
	o.logf("")
	o.logf("verified relay mapping:")
	for ch := 0; ch < 8; ch++ {
		n := found[ch]
		if n == "" {
			n = "(none)"
		}
		o.logf("  ch %d -> %s", ch, n)
	}
	var problems []string
	var b strings.Builder
	b.WriteString("# ---- verified by rackctl map-verify; paste over the [[pc]] tables ----\n")
	for _, p := range o.Cfg.Ordered() {
		ch, ok := byName[p.Name]
		if !ok {
			fmt.Fprintf(&b, "# %s (%s): did NOT come up on any channel\n\n", p.Name, p.IP)
			problems = append(problems, p.Name+" not found on any channel")
			continue
		}
		if ch != p.Relay {
			problems = append(problems, fmt.Sprintf("%s: config says relay %d, verified %d", p.Name, p.Relay, ch))
		}
		roles := make([]string, len(p.Roles))
		for i, r := range p.Roles {
			roles[i] = fmt.Sprintf("%q", r)
		}
		fmt.Fprintf(&b, "[[pc]]\nname = %q\nip = %q\nrelay = %d\nroles = [%s]\n", p.Name, p.IP, ch, strings.Join(roles, ", "))
		if p.Timeout.Duration > 0 {
			fmt.Fprintf(&b, "timeout = %q\n", p.Timeout.Duration.String())
		}
		b.WriteString("\n")
	}
	o.logf("")
	fmt.Fprint(o.Out, b.String())
	if len(problems) > 0 {
		return errors.New("mapping differs from config:\n  " + strings.Join(problems, "\n  "))
	}
	o.logf("mapping matches config")
	return nil
}

// Relay implements `rackctl relay open|close <ch>`. Opening a channel whose
// configured PC still answers ping is refused unless force, so a live host
// is never cut by hand by accident. Closing is always allowed (it only
// powers a host on).
func (o *Orchestrator) Relay(ctx context.Context, action string, ch int, force bool) error {
	if action != "open" && action != "close" {
		return fmt.Errorf("relay: want open or close, got %q", action)
	}
	if ch < 0 || ch > 7 {
		return fmt.Errorf("relay: channel %d out of range 0-7", ch)
	}
	var pc *config.PC
	for _, p := range o.Cfg.PCs {
		if p.Relay == ch {
			p := p
			pc = &p
		}
	}
	if action == "open" && pc != nil {
		switch {
		case force:
			o.logf("--force: not checking whether %s (relay %d) is still running", pc.Name, ch)
		case o.DryRun:
			o.dryf("ping %s (%s); refuse if it answers", pc.Name, pc.IP)
		default:
			up, err := o.quickUp(ctx, pc.IP)
			if err != nil {
				return err
			}
			if up {
				return fmt.Errorf("%w: relay %d powers %s, which answers ping; shut it down first ('rackctl shutdown %s') or use --force",
					ErrRefused, ch, pc.Name, pc.Name)
			}
		}
	}
	want := map[string]string{"open": "open", "close": "closed"}[action]
	mark := o.mark()
	o.logf("relay %d: publishing %s", ch, action)
	if err := o.publish(ctx, TopicRelaySet(ch), action); err != nil {
		return err
	}
	if err := o.waitFresh(ctx, TopicRelayState(ch), want, mark, o.RelayTimeout); err != nil {
		return fmt.Errorf("relay %d: %w", ch, err)
	}
	if !o.DryRun {
		o.logf("relay %d: %s confirmed", ch, want)
	}
	return nil
}
