package config

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// Strict decoding through viper: unknown keys anywhere, and values viper's
// default (weakly typed) decoding would have coerced, are all rejected.
func TestStrictDecoding(t *testing.T) {
	cases := map[string][2]string{
		"unknown table":        {`[broker]`, "[brokr]\nx = 1\n[broker]"},
		"unknown broker key":   {`host = "192.168.8.1"`, "host = \"192.168.8.1\"\nhots = \"x\""},
		"unknown pc key":       {`roles = ["osd"]`, "roles = [\"osd\"]\nrelais = 3"},
		"numeric duration":     {`timeout = "300s"`, `timeout = 300`},
		"numeric cut_delay":    {`[shutdown]`, "[shutdown]\ncut_delay = 30"},
		"duration no unit":     {`timeout = "300s"`, `timeout = "300"`},
		"empty duration":       {`timeout = "300s"`, `timeout = ""`},
		"relay as string":      {`relay = 1`, `relay = "1"`},
		"relay as float":       {`relay = 1`, `relay = 1.5`},
		"roles as string":      {`roles = ["mon", "vm"]`, `roles = "mon"`},
		"port as string":       {`host = "192.168.8.1"`, "host = \"192.168.8.1\"\nport = \"8883\""},
		"bad toml":             {`[broker]`, `[broker`},
		"order not a list":     {`order = ["a", "b", "c"]`, `order = "a,b,c"`},
		"pc not array of tbls": {`[shutdown]`, "pc = 1\n[shutdown]"},
		// viper lowercases keys and splits them on ".": both must be refused
		// before that can merge or relocate a value.
		"case duplicate":    {`host = "192.168.8.1"`, "host = \"192.168.8.1\"\nHOST = \"10.0.0.1\""},
		"uppercase key":     {`relay = 1`, `Relay = 1`},
		"uppercase table":   {`[broker]`, `[Broker]`},
		"dotted quoted key": {`[broker]`, "\"shutdown.cut_delay\" = \"0s\"\n[broker]"},
		"dotted key in pc":  {`roles = ["osd"]`, "roles = [\"osd\"]\n\"timeout.x\" = 1"},
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

func TestUnknownKeyIsNamed(t *testing.T) {
	_, err := Parse(strings.Replace(base, `[shutdown]`, "[shutdown]\ncutdelay = \"5s\"", 1))
	if err == nil || !strings.Contains(err.Error(), "cutdelay") {
		t.Fatalf("error does not name the key: %v", err)
	}
}

func TestLoadFile(t *testing.T) {
	dir := t.TempDir()
	// Any file name: the format is always TOML.
	p := filepath.Join(dir, "rack.conf")
	if err := os.WriteFile(p, []byte(base), 0o600); err != nil {
		t.Fatal(err)
	}
	c, err := Load(p)
	if err != nil {
		t.Fatal(err)
	}
	if len(c.PCs) != 3 || c.Broker.Host != "192.168.8.1" {
		t.Fatalf("%+v", c)
	}
	if _, err := Load(filepath.Join(dir, "missing.toml")); err == nil {
		t.Fatal("missing file accepted")
	}
}
