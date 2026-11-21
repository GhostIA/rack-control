package main

import (
	"bytes"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// runRaw runs rackpki without the harness's trailing --out.
func runRaw(t *testing.T, args ...string) (int, string, string) {
	t.Helper()
	var out, errb bytes.Buffer
	e := &env{stdout: &out, stderr: &errb, getenv: func(string) string { return "" }, cwd: t.TempDir()}
	code := e.run(args)
	return code, out.String(), errb.String()
}

func TestCLIUsageErrors(t *testing.T) {
	out := filepath.Join(t.TempDir(), "pki")
	for _, args := range [][]string{
		{}, {"frobnicate"}, {"init-ca", "extra"}, {"init-ca", "--frob"}, {"init-ca", "--ip", "1.2.3.4"},
		{"server", "x"}, {"server", "--ip"}, {"server", "--all"},
		{"client"}, {"client", "--all", "sbc4"}, {"client", "nobody"}, {"client", "sbc4", "sbc4"},
		{"client", "broker"}, {"client", "--mosquitto-dir", "x", "sbc4"},
		{"passwd", "x"}, {"passwd", "--all"},
		{"bundle"}, {"bundle", "--all", "broker"}, {"bundle", "nobody"}, {"bundle", "broker", "broker"},
		{"expiry", "x"}, {"expiry", "--warn-days", "-1"}, {"expiry", "--warn-days", "x"}, {"expiry", "--force"},
		{"--out"},
	} {
		argv := append(append([]string{}, args...), "--out", out)
		if len(args) == 1 && args[0] == "--out" {
			argv = args
		}
		code, stdout, stderr := runRaw(t, argv...)
		if code != exitUsage {
			t.Errorf("%v: exit %d, want %d: %s", args, code, exitUsage, stderr)
			continue
		}
		if strings.Count(stderr, "rackpki: ") != 1 || !strings.Contains(stderr, "Usage:") {
			t.Errorf("%v: want the error once plus usage:\n%s", args, stderr)
		}
		if stdout != "" {
			t.Errorf("%v: wrote to stdout: %s", args, stdout)
		}
	}
	if _, err := os.Stat(out); !os.IsNotExist(err) {
		t.Fatal("a usage error created the PKI directory")
	}
}

func TestCLIHelp(t *testing.T) {
	for _, args := range [][]string{
		{"--help"}, {"-h"}, {"help"}, {"help", "bundle"}, {"bundle", "--help"},
		{"server", "-h"}, {"expiry", "--help"}, {"completion", "zsh"},
	} {
		code, stdout, stderr := runRaw(t, args...)
		if code != 0 || stdout == "" {
			t.Errorf("%v: exit %d, stdout %d bytes, stderr %s", args, code, len(stdout), stderr)
		}
	}
	_, stdout, _ := runRaw(t, "--help")
	for _, want := range []string{"init-ca", "server", "client", "passwd", "bundle", "expiry", "--out", "orchestrator"} {
		if !strings.Contains(stdout, want) {
			t.Errorf("--help lacks %q", want)
		}
	}
	_, stdout, _ = runRaw(t, "bundle", "--help")
	for _, want := range []string{"--all", "--force", "--mosquitto-dir", "--out"} {
		if !strings.Contains(stdout, want) {
			t.Errorf("bundle --help lacks %q", want)
		}
	}
}

// Flags work before and after positionals and --out works before the
// command (persistent flag); runtime errors print no usage.
func TestCLIFlagPositions(t *testing.T) {
	h := newHarness(t)
	if code, _, stderr := runRaw(t, "--out", h.out, "init-ca"); code != 0 {
		t.Fatalf("--out before command: exit %d: %s", code, stderr)
	}
	if code, _, stderr := runRaw(t, "client", "--out", h.out, "sbc4", "--force"); code != 0 {
		t.Fatalf("interspersed flags: exit %d: %s", code, stderr)
	}
	code, _, stderr := runRaw(t, "init-ca", "--out="+h.out)
	if code != exitFailure || strings.Contains(stderr, "Usage:") || !strings.Contains(stderr, "--force") {
		t.Fatalf("overwrite refusal: exit %d:\n%s", code, stderr)
	}
}

// Without --out the PKI directory is found from the repo root.
func TestCLIDefaultOutDir(t *testing.T) {
	repo := t.TempDir()
	if err := os.MkdirAll(filepath.Join(repo, "slate", "mosquitto"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(repo, "slate", "mosquitto", "acl"), []byte("user x\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	sub := filepath.Join(repo, "slate", "rackpki")
	if err := os.MkdirAll(sub, 0o755); err != nil {
		t.Fatal(err)
	}
	var out, errb bytes.Buffer
	e := &env{stdout: &out, stderr: &errb, getenv: func(string) string { return "" }, cwd: sub}
	if code := e.run([]string{"init-ca"}); code != 0 {
		t.Fatalf("exit %d: %s", code, errb.String())
	}
	assertMode(t, filepath.Join(repo, "slate", "pki", "ca.key"), 0o600)
	e.cwd = t.TempDir()
	if code := e.run([]string{"init-ca"}); code != exitFailure {
		t.Fatalf("outside the repo: exit %d", code)
	}
}
