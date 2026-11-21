package main

import (
	"bytes"
	"reflect"
	"strings"
	"testing"

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
	}
	for _, c := range cases {
		if got := dispatchArgs(c.argv); !reflect.DeepEqual(got, c.want) {
			t.Errorf("%v: got %v want %v", c.argv, got, c.want)
		}
	}
}

func TestParseFlags(t *testing.T) {
	var e bytes.Buffer
	o, err := parse([]string{"--config", "/x.toml", "shutdown", "intel-d1babf", "--force", "--no-relay", "--dry-run"}, &e)
	if err != nil {
		t.Fatal(err)
	}
	if o.config != "/x.toml" || !o.force || !o.noRelay || !o.dry || !reflect.DeepEqual(o.args, []string{"intel-d1babf"}) {
		t.Fatalf("%+v", o)
	}
	o, err = parse([]string{"up", "--config=/y.toml", "all"}, &e)
	if err != nil || o.config != "/y.toml" || o.args[0] != "all" {
		t.Fatalf("%+v %v", o, err)
	}
	o, err = parse([]string{"relay", "open", "3", "--dry-run"}, &e)
	if err != nil || o.channel != 3 || o.args[0] != "open" || !o.dry {
		t.Fatalf("relay: %+v %v", o, err)
	}
	// "--" ends flag parsing: "--force" after it is positional, not a flag.
	o, err = parse([]string{"shutdown", "--", "intel-d1babf", "--force"}, &e)
	if err != nil || o.force || !reflect.DeepEqual(o.args, []string{"intel-d1babf", "--force"}) {
		t.Fatalf("--: %+v %v", o, err)
	}
	for _, bad := range [][]string{
		{"relay", "open"}, {"relay", "open", "8"}, {"relay", "open", "03"}, {"relay", "flip", "1"}, {"relay", "open", "1", "2"},
		{"shutdown"}, {"up"}, {"status", "x"}, {"map-verify", "x"}, {"ceph-maintenance"},
		{"ceph-maintenance", "maybe"}, {"frobnicate"}, {"status", "--force"}, {"up", "--force", "all"}, {},
	} {
		if _, err := parse(bad, &e); err == nil {
			t.Errorf("%v accepted", bad)
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
}

// Dry runs through the real entry point: the example config's cert and
// password files don't exist here, so any broker connection would fail.
func TestRunDryRunViaAlias(t *testing.T) {
	for _, argv := range [][]string{
		{"/usr/bin/rack-shutdown", "--config", "rack.example.toml", "--dry-run", "all"},
		{"rack-up", "--config", "rack.example.toml", "--dry-run", "all"},
		{"rackctl", "--config", "rack.example.toml", "map-verify", "--dry-run"},
		{"rackctl", "--config", "rack.example.toml", "ceph-maintenance", "--dry-run", "off"},
		{"rackctl", "--config", "rack.example.toml", "relay", "--dry-run", "open", "5"},
	} {
		var out, errb bytes.Buffer
		if code := run(argv, &out, &errb); code != 0 {
			t.Fatalf("%v: exit %d\n%s%s", argv, code, out.String(), errb.String())
		}
		if !strings.Contains(out.String(), "[dry-run]") {
			t.Fatalf("%v: no dry-run output", argv)
		}
	}
}

func TestRunBadConfig(t *testing.T) {
	var out, errb bytes.Buffer
	if code := run([]string{"rackctl", "--config", "/nonexistent.toml", "status"}, &out, &errb); code != 2 {
		t.Fatalf("exit %d", code)
	}
}
