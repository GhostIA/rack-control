package config

import (
	"strings"
	"testing"
	"time"
)

const base = `
[broker]
host = "192.168.8.1"
[shutdown]
order = ["a", "b", "c"]
[[pc]]
name = "a"
ip = "10.0.0.1"
relay = 0
roles = ["osd"]
[[pc]]
name = "b"
ip = "10.0.0.2"
relay = 1
roles = ["mon", "vm"]
timeout = "300s"
[[pc]]
name = "c"
ip = "10.0.0.3"
relay = 7
roles = []
`

func TestDefaultsAndOrder(t *testing.T) {
	c, err := Parse(base)
	if err != nil {
		t.Fatal(err)
	}
	if c.Broker.Port != 8883 || c.Broker.Username != "orchestrator" {
		t.Errorf("broker defaults: %+v", c.Broker)
	}
	if c.Shutdown.Timeout != 120*time.Second || c.Shutdown.CutDelay != 30*time.Second || c.Shutdown.PVEQuorum != 3 {
		t.Errorf("shutdown defaults: %+v", c.Shutdown)
	}
	got := []string{}
	for _, p := range c.Ordered() {
		got = append(got, p.Name)
	}
	if strings.Join(got, ",") != "a,b,c" {
		t.Errorf("order %v", got)
	}
	b, _ := c.PC("b")
	a, _ := c.PC("a")
	if c.TimeoutFor(b) != 300*time.Second || c.TimeoutFor(a) != 120*time.Second {
		t.Error("TimeoutFor")
	}
	if !b.HasRole(RoleVM) || b.HasRole(RoleOSD) {
		t.Error("HasRole")
	}
}

func TestExplicitZeroAndQuorumDefault(t *testing.T) {
	c, err := Parse(strings.Replace(base, "[shutdown]", "[shutdown]\ncut_delay = \"0s\"", 1))
	if err != nil {
		t.Fatal(err)
	}
	if c.Shutdown.CutDelay != 0 {
		t.Errorf("explicit cut_delay 0s became %s", c.Shutdown.CutDelay)
	}
	two := strings.Replace(base, `order = ["a", "b", "c"]`, `order = ["a", "b"]`, 1)
	two = two[:strings.Index(two, "[[pc]]\nname = \"c\"")]
	c, err = Parse(two)
	if err != nil {
		t.Fatal(err)
	}
	if c.Shutdown.PVEQuorum != 2 {
		t.Errorf("default quorum for 2 PCs = %d", c.Shutdown.PVEQuorum)
	}
}

func TestValidation(t *testing.T) {
	cases := map[string][2]string{
		"dup name":       {`name = "b"`, `name = "a"`},
		"dup ip":         {`ip = "10.0.0.2"`, `ip = "10.0.0.1"`},
		"bad ip":         {`ip = "10.0.0.2"`, `ip = "nope"`},
		"ipv6":           {`ip = "10.0.0.2"`, `ip = "::1"`},
		"dup relay":      {`relay = 1`, `relay = 0`},
		"relay 8":        {`relay = 1`, `relay = 8`},
		"relay -1":       {`relay = 1`, `relay = -1`},
		"bad role":       {`roles = ["mon", "vm"]`, `roles = ["mds"]`},
		"order missing":  {`order = ["a", "b", "c"]`, `order = ["a", "b"]`},
		"order twice":    {`order = ["a", "b", "c"]`, `order = ["a", "b", "c", "a"]`},
		"order unknown":  {`order = ["a", "b", "c"]`, `order = ["a", "b", "c", "z"]`},
		"bad name":       {`name = "a"`, `name = "a/+"`},
		"unknown key":    {`[shutdown]`, "[shutdown]\ncutdelay = \"5s\""},
		"bad duration":   {`timeout = "300s"`, `timeout = "5 minutes"`},
		"quorum too big": {`[shutdown]`, "[shutdown]\npve_quorum = 4"},
		"no host":        {`host = "192.168.8.1"`, `host = ""`},
		"missing relay":  {"relay = 7\n", ""},
		"missing roles":  {"roles = []\n", ""},
		"missing ip":     {`ip = "10.0.0.3"`, ""},
		"short timeout":  {`timeout = "300s"`, `timeout = "3s"`},
		"short global":   {`[shutdown]`, "[shutdown]\ntimeout = \"5s\""},
		"empty username": {`host = "192.168.8.1"`, "host = \"192.168.8.1\"\nusername = \"\""},
	}
	for name, r := range cases {
		text := strings.Replace(base, r[0], r[1], 1)
		if text == base {
			t.Fatalf("%s: replacement did not apply", name)
		}
		if _, err := Parse(text); err == nil {
			t.Errorf("%s: accepted", name)
		}
	}
}
