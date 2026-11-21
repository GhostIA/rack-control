package main

import (
	"bytes"
	"context"
	"errors"
	"io"
	"path/filepath"
	"reflect"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/GhostIA/rack-control/slate/rackctl/internal/config"
)

func TestDispatchArgs(t *testing.T) {
	cases := []struct {
		argv []string
		want []string
	}{
		{[]string{"/usr/bin/rack-shutdown", "all"}, []string{"shutdown", "all"}},
		{[]string{"rack-shutdown", "--force", "intel-d1babf"}, []string{"shutdown", "--force", "intel-d1babf"}},
		{[]string{"/usr/bin/rack-up", "all"}, []string{"up", "all"}},
		{[]string{"/usr/bin/rackctl", "status"}, []string{"status"}},
		{[]string{"./bin/rackctl"}, []string{}},
		{nil, []string{}},
	}
	for _, c := range cases {
		got := dispatchArgs(c.argv)
		if got == nil || !reflect.DeepEqual(got, c.want) {
			t.Errorf("%v: got %#v want %v", c.argv, got, c.want)
		}
	}
}

// parsed runs argv through the real command tree with a recording
// execFunc, returning what would have been executed and the exit code.
func parsed(t *testing.T, argv ...string) (*invocation, int, string) {
	t.Helper()
	var got *invocation
	rec := func(_ context.Context, inv *invocation, _, _ io.Writer) error {
		cp := *inv
		got = &cp
		return nil
	}
	var out, errb bytes.Buffer
	code := runWith(argv, &out, &errb, rec)
	return got, code, out.String() + errb.String()
}

func mustParse(t *testing.T, argv ...string) *invocation {
	t.Helper()
	inv, code, msg := parsed(t, argv...)
	if code != 0 || inv == nil {
		t.Fatalf("%v: exit %d\n%s", argv, code, msg)
	}
	return inv
}

func TestParseShutdownAndUp(t *testing.T) {
	o := mustParse(t, "rackctl", "--config", "/x.toml", "shutdown", "intel-d1babf", "--force", "--no-relay", "--dry-run")
	if o.cmd != "shutdown" || o.config != "/x.toml" || !o.force || !o.noRelay || !o.dry || !reflect.DeepEqual(o.args, []string{"intel-d1babf"}) {
		t.Fatalf("%+v", o)
	}
	o = mustParse(t, "rackctl", "shutdown", "a", "b")
	if o.config != defaultConfig || o.force || o.noRelay || o.dry || !reflect.DeepEqual(o.args, []string{"a", "b"}) {
		t.Fatalf("defaults: %+v", o)
	}
	o = mustParse(t, "rackctl", "up", "--config=/y.toml", "all")
	if o.cmd != "up" || o.config != "/y.toml" || o.args[0] != "all" || o.dry {
		t.Fatalf("up: %+v", o)
	}
	if o = mustParse(t, "rackctl", "up", "--dry-run", "all"); !o.dry {
		t.Fatalf("up --dry-run: %+v", o)
	}
	// "--" ends flag parsing: "--force" after it is positional, not a flag.
	o = mustParse(t, "rackctl", "shutdown", "--", "intel-d1babf", "--force")
	if o.force || !reflect.DeepEqual(o.args, []string{"intel-d1babf", "--force"}) {
		t.Fatalf("--: %+v", o)
	}
	// 'all' mixed with names is accepted here; orch rejects it.
	if o = mustParse(t, "rackctl", "shutdown", "all", "a"); len(o.args) != 2 {
		t.Fatalf("%+v", o)
	}
}

func TestParseAliases(t *testing.T) {
	o := mustParse(t, "/usr/bin/rack-shutdown", "--dry-run", "--config", "/z.toml", "all")
	if o.cmd != "shutdown" || !o.dry || o.config != "/z.toml" || o.args[0] != "all" {
		t.Fatalf("rack-shutdown: %+v", o)
	}
	o = mustParse(t, "rack-up", "intel-a163f8")
	if o.cmd != "up" || o.args[0] != "intel-a163f8" {
		t.Fatalf("rack-up: %+v", o)
	}
	// Bare alias: usage error, nothing runs.
	for _, alias := range []string{"rack-shutdown", "/usr/bin/rack-up"} {
		if inv, code, _ := parsed(t, alias); code != exitUsage || inv != nil {
			t.Errorf("bare %s: exit %d, ran %v", alias, code, inv)
		}
	}
	// rack-up has no --force.
	if _, code, _ := parsed(t, "rack-up", "--force", "all"); code != exitUsage {
		t.Errorf("rack-up --force: exit %d", code)
	}
}

func TestParseRelayAndCeph(t *testing.T) {
	o := mustParse(t, "rackctl", "relay", "open", "3", "--dry-run")
	if o.cmd != "relay" || o.channel != 3 || o.args[0] != "open" || !o.dry || o.force {
		t.Fatalf("relay: %+v", o)
	}
	o = mustParse(t, "rackctl", "relay", "--force", "close", "0")
	if o.channel != 0 || o.args[0] != "close" || !o.force {
		t.Fatalf("relay close: %+v", o)
	}
	o = mustParse(t, "rackctl", "ceph-maintenance", "on")
	if o.cmd != "ceph-maintenance" || o.args[0] != "on" || o.dry {
		t.Fatalf("ceph: %+v", o)
	}
	if o = mustParse(t, "rackctl", "ceph-maintenance", "--dry-run", "off"); !o.dry || o.args[0] != "off" {
		t.Fatalf("ceph off: %+v", o)
	}
	if o = mustParse(t, "rackctl", "status"); o.cmd != "status" || len(o.args) != 0 {
		t.Fatalf("status: %+v", o)
	}
}

func TestParseMapVerify(t *testing.T) {
	o := mustParse(t, "rackctl", "map-verify")
	if o.cmd != "map-verify" || o.channels != nil || o.bootTimeout != 5*time.Minute || o.dry {
		t.Fatalf("defaults: %+v", o)
	}
	if defaultMapUpTimeout != 5*time.Minute {
		t.Fatalf("orch default %s", defaultMapUpTimeout)
	}
	for _, c := range []struct {
		args []string
		want []int
	}{
		{[]string{"--channels", "3,4,5"}, []int{3, 4, 5}},
		{[]string{"--channels", "7,3,3"}, []int{3, 7}},
		{[]string{"--channels=3,4,5,6,7"}, []int{3, 4, 5, 6, 7}},
		{[]string{"--channels", "5", "--channels", "0"}, []int{0, 5}},
		{[]string{"--channels", "0,1,2,3,4,5,6,7"}, []int{0, 1, 2, 3, 4, 5, 6, 7}},
	} {
		o := mustParse(t, append([]string{"rackctl", "map-verify"}, c.args...)...)
		if !reflect.DeepEqual(o.channels, c.want) {
			t.Errorf("%v: got %v want %v", c.args, o.channels, c.want)
		}
	}
	o = mustParse(t, "rackctl", "map-verify", "--dry-run", "--boot-timeout", "90s")
	if !o.dry || o.bootTimeout != 90*time.Second {
		t.Fatalf("boot-timeout: %+v", o)
	}
	if o = mustParse(t, "rackctl", "map-verify", "--boot-timeout=30s"); o.bootTimeout != 30*time.Second {
		t.Fatalf("boot-timeout minimum: %+v", o)
	}
}

func TestUsageErrors(t *testing.T) {
	for _, bad := range [][]string{
		{}, {"frobnicate"}, {"--config"},
		{"shutdown"}, {"shutdown", "--frob", "all"}, {"shutdown", "--force=maybe", "all"},
		{"up"}, {"up", "--force", "all"}, {"up", "--no-relay", "all"},
		{"status", "x"}, {"status", "--force"}, {"status", "--dry-run"},
		{"map-verify", "x"}, {"map-verify", "--force"},
		{"map-verify", "--channels", "8"}, {"map-verify", "--channels", "-1"},
		{"map-verify", "--channels", "x"}, {"map-verify", "--channels", "3,x"},
		{"map-verify", "--channels="}, {"map-verify", "--channels", "3,,4"},
		{"map-verify", "--boot-timeout", "29s"}, {"map-verify", "--boot-timeout", "0"},
		{"map-verify", "--boot-timeout", "-1m"}, {"map-verify", "--boot-timeout", "5"},
		{"ceph-maintenance"}, {"ceph-maintenance", "maybe"}, {"ceph-maintenance", "on", "off"},
		{"ceph-maintenance", "--force", "on"},
		{"relay"}, {"relay", "open"}, {"relay", "open", "8"}, {"relay", "open", "-1"},
		{"relay", "open", "03"}, {"relay", "open", "x"}, {"relay", "flip", "1"},
		{"relay", "open", "1", "2"}, {"relay", "--no-relay", "open", "1"},
	} {
		argv := append([]string{"rackctl"}, bad...)
		inv, code, msg := parsed(t, argv...)
		if code != exitUsage || inv != nil {
			t.Errorf("%v: exit %d (want %d), ran %v", bad, code, exitUsage, inv != nil)
			continue
		}
		if strings.Count(msg, "rackctl: ") != 1 {
			t.Errorf("%v: error not printed exactly once:\n%s", bad, msg)
		}
		if !strings.Contains(msg, "Usage:") {
			t.Errorf("%v: no usage on a usage error:\n%s", bad, msg)
		}
	}
}

func TestHelpExitsZero(t *testing.T) {
	for _, argv := range [][]string{
		{"rackctl", "--help"}, {"rackctl", "-h"}, {"rackctl", "help"},
		{"rackctl", "help", "map-verify"}, {"rackctl", "map-verify", "--help"},
		{"rackctl", "shutdown", "--help"}, {"rack-shutdown", "--help"}, {"rack-up", "-h"},
		{"rackctl", "relay", "--help"}, {"rackctl", "completion", "bash"},
	} {
		var out, errb bytes.Buffer
		ran := false
		rec := func(context.Context, *invocation, io.Writer, io.Writer) error { ran = true; return nil }
		if code := runWith(argv, &out, &errb, rec); code != 0 || ran {
			t.Errorf("%v: exit %d ran=%v\n%s", argv, code, ran, errb.String())
		}
		if out.Len() == 0 {
			t.Errorf("%v: nothing on stdout", argv)
		}
	}
	var out, errb bytes.Buffer
	runWith([]string{"rackctl", "--help"}, &out, &errb, nil)
	for _, sub := range []string{"shutdown", "up", "status", "map-verify", "ceph-maintenance", "relay", "--config"} {
		if !strings.Contains(out.String(), sub) {
			t.Errorf("--help does not mention %s", sub)
		}
	}
	out.Reset()
	runWith([]string{"rackctl", "map-verify", "--help"}, &out, &errb, nil)
	if !strings.Contains(out.String(), "--channels") || !strings.Contains(out.String(), "(default 5m0s)") {
		t.Errorf("map-verify --help:\n%s", out.String())
	}
}

func TestRuntimeErrorExitCodes(t *testing.T) {
	fail := func(code int) execFunc {
		return func(context.Context, *invocation, io.Writer, io.Writer) error {
			if code == 0 {
				return errors.New("boom")
			}
			return &exitError{code: code, err: errors.New("boom")}
		}
	}
	for _, c := range []struct {
		exec execFunc
		want int
	}{{fail(exitFailure), 1}, {fail(exitUsage), 2}} {
		var out, errb bytes.Buffer
		code := runWith([]string{"rackctl", "shutdown", "all"}, &out, &errb, c.exec)
		if code != c.want {
			t.Errorf("exit %d want %d", code, c.want)
		}
		if errb.String() != "rackctl: boom\n" || out.Len() != 0 {
			t.Errorf("runtime error output: stdout %q stderr %q (want the error once, no usage)", out.String(), errb.String())
		}
	}
}

func TestExampleConfigIsValid(t *testing.T) {
	c, err := config.Load("rack.example.toml")
	if err != nil {
		t.Fatal(err)
	}
	var got []string
	for _, p := range c.Ordered() {
		got = append(got, p.Name)
	}
	want := "intel-a163f8,intel-91c45d,intel-d1babf,intel-177e78,intel-1ecbc8"
	if strings.Join(got, ",") != want {
		t.Fatalf("order %v", got)
	}
	a, _ := c.PC("intel-a163f8")
	if c.TimeoutFor(a) != 300*time.Second || c.Shutdown.CutDelay != 30*time.Second {
		t.Fatalf("durations: %s %s", c.TimeoutFor(a), c.Shutdown.CutDelay)
	}
}

// Dry runs through the real entry point: the example config's cert and
// password files don't exist here, so any broker connection would fail.
func TestRunDryRunViaAlias(t *testing.T) {
	for _, c := range []struct {
		argv []string
		want string
	}{
		{[]string{"/usr/bin/rack-shutdown", "--config", "rack.example.toml", "--dry-run", "all"}, "[dry-run]"},
		{[]string{"rack-up", "--config", "rack.example.toml", "--dry-run", "all"}, "[dry-run]"},
		{[]string{"rackctl", "--config", "rack.example.toml", "map-verify", "--dry-run"}, "channel 7: closing relay"},
		{[]string{"rackctl", "map-verify", "--dry-run", "--config", "rack.example.toml", "--channels", "7,3,3", "--boot-timeout", "45s"}, "testing only channels [3 7]"},
		{[]string{"rackctl", "map-verify", "--dry-run", "--config", "rack.example.toml", "--boot-timeout", "45s"}, "waiting up to 45s"},
		{[]string{"rackctl", "--config", "rack.example.toml", "ceph-maintenance", "--dry-run", "off"}, "[dry-run]"},
		{[]string{"rackctl", "--config", "rack.example.toml", "relay", "--dry-run", "open", "5"}, "[dry-run]"},
	} {
		var out, errb bytes.Buffer
		if code := run(c.argv, &out, &errb); code != 0 {
			t.Fatalf("%v: exit %d\n%s%s", c.argv, code, out.String(), errb.String())
		}
		if !strings.Contains(out.String(), c.want) {
			t.Fatalf("%v: output lacks %q:\n%s", c.argv, c.want, out.String())
		}
	}
	var out, errb bytes.Buffer
	run([]string{"rackctl", "map-verify", "--dry-run", "--config", "rack.example.toml", "--channels", "4"}, &out, &errb)
	if strings.Contains(out.String(), "channel 3:") || !strings.Contains(out.String(), "channel 4:") {
		t.Fatalf("--channels 4 not honoured:\n%s", out.String())
	}
}

func TestRunErrors(t *testing.T) {
	var out, errb bytes.Buffer
	if code := run([]string{"rackctl", "--config", "/nonexistent.toml", "status"}, &out, &errb); code != exitUsage {
		t.Fatalf("bad config: exit %d", code)
	}
	if strings.Contains(errb.String(), "Usage:") {
		t.Errorf("usage printed for a config error:\n%s", errb.String())
	}
	// Orchestration-level refusal ('all' mixed with names): exit 1, no usage.
	out.Reset()
	errb.Reset()
	code := run([]string{"rackctl", "--config", "rack.example.toml", "shutdown", "--dry-run", "all", "intel-a163f8"}, &out, &errb)
	if code != exitFailure || strings.Contains(errb.String(), "Usage:") || strings.Count(errb.String(), "rackctl: ") != 1 {
		t.Fatalf("exit %d:\n%s", code, errb.String())
	}
}

// Race: several rackctl processes start at once (e.g. a double-typed
// rack-shutdown). Exactly one may hold the orchestration lock; the rest must
// be refused immediately, and the lock must be free again after release.
func TestLockIsExclusiveUnderConcurrency(t *testing.T) {
	old := lockPath
	lockPath = filepath.Join(t.TempDir(), "run", "rackctl.lock")
	defer func() { lockPath = old }()

	const n = 16
	var (
		wg       sync.WaitGroup
		mu       sync.Mutex
		releases []func()
		refused  int
	)
	start := make(chan struct{})
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			<-start
			rel, err := lock()
			mu.Lock()
			defer mu.Unlock()
			if err != nil {
				refused++
				return
			}
			releases = append(releases, rel)
		}()
	}
	close(start)
	wg.Wait()
	if len(releases) != 1 || refused != n-1 {
		t.Fatalf("holders=%d refused=%d, want 1 and %d", len(releases), refused, n-1)
	}
	releases[0]()
	rel, err := lock()
	if err != nil {
		t.Fatalf("lock not reusable after release: %v", err)
	}
	rel()
}

// A non-dry command must take the lock before connecting: with the lock
// held elsewhere it is refused (exit 1) without touching the broker.
func TestNonDryRunTakesLock(t *testing.T) {
	old := lockPath
	lockPath = filepath.Join(t.TempDir(), "rackctl.lock")
	defer func() { lockPath = old }()
	rel, err := lock()
	if err != nil {
		t.Fatal(err)
	}
	defer rel()
	for _, argv := range [][]string{
		{"rackctl", "--config", "rack.example.toml", "relay", "close", "3"},
		{"rackctl", "--config", "rack.example.toml", "map-verify", "--channels", "3"},
	} {
		var out, errb bytes.Buffer
		if code := run(argv, &out, &errb); code != exitFailure || !strings.Contains(errb.String(), "another rackctl is running") {
			t.Errorf("%v: exit %d: %s", argv, code, errb.String())
		}
	}
}
