//go:build e2e

// Package e2e checks the rackpki output and the committed broker config
// against a real eclipse-mosquitto:2.0.20 broker in Docker:
//
//	go test -tags e2e -count=1 -v ./e2e/      (or: make e2e)
//
// All passwords here are FAKE test values. Docker commands run with fixed
// argv (no shell). The compose network is internal-only (see
// docker-compose.yml).
package e2e

import (
	"bytes"
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"
)

const (
	brokerIP   = "172.31.250.10"
	prodListen = "listener 8883 192.168.8.1"
	testListen = "listener 8883 " + brokerIP
	project    = "rackpki-e2e"
)

var users = []string{"orchestrator", "sbc4", "intel-91c45d", "intel-d1babf", "intel-177e78", "intel-a163f8", "intel-1ecbc8"}

func envVar(u string) string {
	return "MQTT_" + strings.ToUpper(strings.ReplaceAll(u, "-", "_")) + "_PASSWORD"
}

// fakeEnv: test-only passwords, never real credentials.
func fakeEnv() []string {
	var out []string
	for _, u := range users {
		out = append(out, envVar(u)+"=fake-e2e-pw-"+u)
	}
	return out
}

func run(t *testing.T, ctx context.Context, env []string, dir, name string, args ...string) string {
	t.Helper()
	cmd := exec.CommandContext(ctx, name, args...)
	cmd.Dir = dir
	cmd.Env = append(os.Environ(), env...)
	var out bytes.Buffer
	cmd.Stdout, cmd.Stderr = &out, &out
	if err := cmd.Run(); err != nil {
		t.Fatalf("%s %s: %v\n%s", name, strings.Join(args, " "), err, out.String())
	}
	return out.String()
}

func copyFile(t *testing.T, src, dst string, mode os.FileMode) {
	t.Helper()
	b, err := os.ReadFile(src)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(dst, b, mode); err != nil {
		t.Fatal(err)
	}
}

func TestBrokerE2E(t *testing.T) {
	if _, err := exec.LookPath("docker"); err != nil {
		t.Skip("docker not installed")
	}
	if err := exec.Command("docker", "info").Run(); err != nil {
		t.Skip("docker daemon not running")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
	defer cancel()

	e2eDir, err := filepath.Abs(".")
	if err != nil {
		t.Fatal(err)
	}
	moduleDir := filepath.Dir(e2eDir)
	repoMosq := filepath.Join(moduleDir, "..", "mosquitto")
	work := t.TempDir()
	env := fakeEnv()

	// 1. Build rackpki and the in-network probe (linux, static).
	rackpki := filepath.Join(work, "rackpki")
	run(t, ctx, nil, moduleDir, "go", "build", "-o", rackpki, ".")
	probeDir := filepath.Join(work, "probe")
	run(t, ctx, []string{"CGO_ENABLED=0", "GOOS=linux", "GOARCH=" + runtime.GOARCH},
		moduleDir, "go", "build", "-tags", "e2e", "-o", filepath.Join(probeDir, "probe"), "./e2e/probe")

	// 2. Test copy of mosquitto.conf: identical except the listener address.
	conf, err := os.ReadFile(filepath.Join(repoMosq, "mosquitto.conf"))
	if err != nil {
		t.Fatal(err)
	}
	if strings.Count(string(conf), prodListen+"\n") != 1 {
		t.Fatalf("committed mosquitto.conf must contain exactly one %q line", prodListen)
	}
	testConf := strings.Replace(string(conf), prodListen+"\n", testListen+"\n", 1)
	mosq := filepath.Join(work, "mosquitto")
	if err := os.MkdirAll(mosq, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(mosq, "mosquitto.conf"), []byte(testConf), 0o600); err != nil {
		t.Fatal(err)
	}
	copyFile(t, filepath.Join(repoMosq, "acl"), filepath.Join(mosq, "acl"), 0o600) // committed acl, unchanged

	// 3. PKI + passwd with rackpki, exactly as an operator would.
	pki := filepath.Join(work, "pki")
	rp := func(args ...string) { run(t, ctx, env, work, rackpki, append(args, "--out", pki)...) }
	rp("init-ca")
	rp("server", "--ip", brokerIP)
	rp("client", "--all")
	rp("passwd")
	rp("bundle", "broker", "--mosquitto-dir", mosq)
	rp("expiry")

	// A second, unrelated CA for the "rogue cert" check.
	rogue := filepath.Join(work, "rogue")
	run(t, ctx, env, work, rackpki, "init-ca", "--out", rogue)
	run(t, ctx, env, work, rackpki, "client", "orchestrator", "--out", rogue)

	broker := filepath.Join(pki, "deploy", "broker")
	copyFile(t, filepath.Join(e2eDir, "Dockerfile.broker"), filepath.Join(broker, "Dockerfile.broker"), 0o600)

	certs := filepath.Join(probeDir, "certs")
	if err := os.MkdirAll(filepath.Join(certs, "rogue"), 0o700); err != nil {
		t.Fatal(err)
	}
	copyFile(t, filepath.Join(pki, "ca.crt"), filepath.Join(certs, "ca.crt"), 0o600)
	for _, u := range users {
		copyFile(t, filepath.Join(pki, u+".crt"), filepath.Join(certs, u+".crt"), 0o600)
		copyFile(t, filepath.Join(pki, u+".key"), filepath.Join(certs, u+".key"), 0o600)
	}
	copyFile(t, filepath.Join(rogue, "orchestrator.crt"), filepath.Join(certs, "rogue", "orchestrator.crt"), 0o600)
	copyFile(t, filepath.Join(rogue, "orchestrator.key"), filepath.Join(certs, "rogue", "orchestrator.key"), 0o600)

	// 4. docker compose up; the probe's exit code is the verdict.
	composeEnv := append([]string{"E2E_BROKER_CONTEXT=" + broker, "E2E_PROBE_DIR=" + probeDir}, env...)
	compose := func(args ...string) []string {
		return append([]string{"compose", "-p", project, "-f", filepath.Join(e2eDir, "docker-compose.yml")}, args...)
	}
	t.Cleanup(func() {
		ctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
		defer cancel()
		cmd := exec.CommandContext(ctx, "docker", compose("down", "--volumes", "--remove-orphans", "--rmi", "local")...)
		cmd.Env = append(os.Environ(), composeEnv...)
		if out, err := cmd.CombinedOutput(); err != nil {
			t.Logf("compose down: %v\n%s", err, out)
		}
	})
	out := run(t, ctx, composeEnv, e2eDir, "docker",
		compose("up", "--build", "--abort-on-container-exit", "--exit-code-from", "probe")...)
	t.Log(out)
	if !strings.Contains(out, "all broker checks passed") {
		t.Fatal("probe did not report success")
	}
	for _, line := range strings.Split(out, "\n") {
		if strings.Contains(line, "fake-e2e-pw") {
			t.Fatalf("a password appeared in container output: %q", line)
		}
	}
}
