package main

import (
	"bufio"
	"os"
	"path/filepath"
	"reflect"
	"sort"
	"strings"
	"testing"
)

const mosqDir = "../mosquitto"

// parseACL is a tiny parser for the subset of Mosquitto's acl_file syntax
// we allow: comments, "user <name>", "topic read|write <topic>". Anything
// else (pattern lines, readwrite, deny, global topic lines) fails the test.
func parseACL(t *testing.T, path string) map[string][]string {
	t.Helper()
	f, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	acl := map[string][]string{}
	cur := ""
	sc := bufio.NewScanner(f)
	for n := 1; sc.Scan(); n++ {
		line := strings.TrimSpace(sc.Text())
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		fields := strings.Fields(line)
		switch {
		case len(fields) == 2 && fields[0] == "user":
			cur = fields[1]
			if _, dup := acl[cur]; dup {
				t.Fatalf("line %d: user %s declared twice", n, cur)
			}
			acl[cur] = []string{}
		case len(fields) == 3 && fields[0] == "topic" && (fields[1] == "read" || fields[1] == "write"):
			if cur == "" {
				t.Fatalf("line %d: global topic line (applies to everyone): %q", n, line)
			}
			acl[cur] = append(acl[cur], fields[1]+" "+fields[2])
		default:
			t.Fatalf("line %d: not allowed in acl: %q", n, line)
		}
	}
	if err := sc.Err(); err != nil {
		t.Fatal(err)
	}
	for u := range acl {
		sort.Strings(acl[u])
	}
	return acl
}

func sorted(s ...string) []string {
	sort.Strings(s)
	return s
}

func TestACL(t *testing.T) {
	acl := parseACL(t, filepath.Join(mosqDir, "acl"))

	want := map[string][]string{
		"orchestrator": sorted(
			"write rack/pc/+/cmd",
			"write rack/relay/+/set",
			"write rack/ceph/maintenance/set",
			"read rack/pc/+/status",
			"read rack/relay/+/state",
			"read rack/sbc4/status",
			"read rack/ceph/maintenance/state",
		),
		"sbc4": sorted(
			"read rack/relay/+/set",
			"write rack/relay/+/state",
			"write rack/sbc4/status",
		),
	}
	for _, u := range Users {
		if strings.HasPrefix(u, "intel-") {
			want[u] = sorted("read rack/pc/"+u+"/cmd", "write rack/pc/"+u+"/status")
		}
	}
	want["intel-1ecbc8"] = sorted(append(want["intel-1ecbc8"],
		"read rack/ceph/maintenance/set", "write rack/ceph/maintenance/state")...)

	if !reflect.DeepEqual(acl, want) {
		t.Fatalf("acl mismatch\n got: %v\nwant: %v", acl, want)
	}

	// Each PC has exactly its own two pc topics, and no wildcards at all.
	for _, u := range Users {
		if !strings.HasPrefix(u, "intel-") {
			continue
		}
		var pcTopics []string
		for _, rule := range acl[u] {
			topic := strings.Fields(rule)[1]
			if strings.ContainsAny(topic, "+#") {
				t.Errorf("%s has a wildcard rule %q", u, rule)
			}
			if strings.HasPrefix(topic, "rack/pc/") {
				pcTopics = append(pcTopics, rule)
			}
			if strings.HasPrefix(topic, "rack/relay/") || strings.HasPrefix(topic, "rack/sbc4/") {
				t.Errorf("%s can touch %s", u, topic)
			}
		}
		if !reflect.DeepEqual(pcTopics, sorted("read rack/pc/"+u+"/cmd", "write rack/pc/"+u+"/status")) {
			t.Errorf("%s pc rules: %v", u, pcTopics)
		}
	}
}

func confDirectives(t *testing.T) (raw string, d map[string][]string) {
	t.Helper()
	b, err := os.ReadFile(filepath.Join(mosqDir, "mosquitto.conf"))
	if err != nil {
		t.Fatal(err)
	}
	d = map[string][]string{}
	for _, line := range strings.Split(string(b), "\n") {
		line = strings.TrimSpace(line)
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		k, v, _ := strings.Cut(line, " ")
		d[k] = append(d[k], strings.TrimSpace(v))
	}
	return string(b), d
}

func TestMosquittoConf(t *testing.T) {
	raw, d := confDirectives(t)
	if strings.Contains(raw, "1883") {
		t.Fatal("mosquitto.conf mentions 1883")
	}
	for _, v := range d["allow_anonymous"] {
		if v != "false" {
			t.Fatalf("allow_anonymous %s", v)
		}
	}
	want := map[string]string{
		"per_listener_settings":    "false",
		"listener":                 "8883 192.168.8.1",
		"allow_anonymous":          "false",
		"password_file":            "/etc/mosquitto/passwd",
		"acl_file":                 "/etc/mosquitto/acl",
		"cafile":                   "/etc/mosquitto/certs/ca.crt",
		"certfile":                 "/etc/mosquitto/certs/broker.crt",
		"keyfile":                  "/etc/mosquitto/certs/broker.key",
		"require_certificate":      "true",
		"use_identity_as_username": "false",
		"use_username_as_clientid": "true",
		"tls_version":              "tlsv1.2",
		"persistence":              "false",
		"log_dest":                 "syslog",
	}
	for k, v := range want {
		if got := d[k]; len(got) != 1 || got[0] != v {
			t.Errorf("%s = %v, want exactly [%s]", k, got, v)
		}
	}
	for _, forbidden := range []string{"port", "bind_address", "psk_hint", "allow_duplicate_messages", "use_subject_as_username", "plugin", "auth_plugin"} {
		if _, ok := d[forbidden]; ok {
			t.Errorf("unexpected directive %s", forbidden)
		}
	}
}

// TestListenerScopedAfterListener: a listener-scoped option placed before the
// first `listener` line makes Mosquitto create an implicit default listener on
// 0.0.0.0:1883 / [::]:1883 (plaintext, all interfaces). Observed on the Slate
// with 2.0.18 when use_username_as_clientid sat above the listener.
func TestListenerScopedAfterListener(t *testing.T) {
	raw, _ := confDirectives(t)
	scoped := map[string]bool{
		"use_username_as_clientid": true, "cafile": true, "capath": true,
		"certfile": true, "keyfile": true, "require_certificate": true,
		"use_identity_as_username": true, "tls_version": true, "ciphers": true,
		"ciphers_tls1.3": true, "crlfile": true, "max_connections": true,
		"protocol": true, "socket_domain": true, "bind_interface": true,
		"mount_point": true, "http_dir": true, "max_qos": true,
		"max_topic_alias": true, "use_subject_as_username": true,
	}
	seenListener := false
	for i, line := range strings.Split(raw, "\n") {
		f := strings.Fields(line)
		if len(f) == 0 || strings.HasPrefix(f[0], "#") {
			continue
		}
		if f[0] == "listener" {
			seenListener = true
			continue
		}
		if scoped[f[0]] && !seenListener {
			t.Errorf("line %d: listener-scoped %q appears before any listener (opens default 1883)", i+1, f[0])
		}
	}
	if !seenListener {
		t.Fatal("no listener line")
	}
}

func TestPasswdExampleHasEveryUserOnly(t *testing.T) {
	b, err := os.ReadFile(filepath.Join(mosqDir, "passwd.example"))
	if err != nil {
		t.Fatal(err)
	}
	var users []string
	for _, line := range strings.Split(string(b), "\n") {
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		u, _, _ := strings.Cut(line, ":")
		users = append(users, u)
	}
	if !reflect.DeepEqual(users, Users) {
		t.Fatalf("passwd.example users %v, want %v", users, Users)
	}
}
