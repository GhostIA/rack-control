//go:build e2e

// End-to-end test: real Mosquitto 2.0.20 (mTLS + passwd + ACL), fake PCs
// that stop answering ping when they "power off", a fake SBC4 relay sidecar,
// and the real rackctl Linux binary, all on an internal Docker network
// (172.30.0.0/24) that cannot reach the real rack.
//
//	go test -tags e2e -count=1 -v ./e2e/      (or: make e2e)
package e2e

import (
	"bufio"
	"bytes"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/hex"
	"encoding/pem"
	"fmt"
	"math/big"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"testing"
	"time"
)

const brokerIP = "172.30.0.2"

type fakePC struct {
	name, ip, svc string
	relay         int
	roles         string
}

var pcs = []fakePC{
	{"e2e-a", "172.30.0.11", "pc-a", 0, `["osd"]`},
	{"e2e-b", "172.30.0.12", "pc-b", 1, `["osd"]`},
	{"e2e-c", "172.30.0.13", "pc-c", 2, `["osd", "mon"]`},
	{"e2e-d", "172.30.0.14", "pc-d", 4, `["mon", "vm"]`},
	{"e2e-e", "172.30.0.15", "pc-e", 6, `["osd", "mon"]`},
}

// Shutdown order, like production: the vm host first, the ceph node last.
var order = []string{"e2e-d", "e2e-a", "e2e-b", "e2e-c", "e2e-e"}

func pcByName(n string) fakePC {
	for _, p := range pcs {
		if p.name == n {
			return p
		}
	}
	panic(n)
}

func TestE2E(t *testing.T) {
	if err := exec.Command("docker", "info").Run(); err != nil {
		t.Skip("Docker daemon not running; skipping e2e")
	}
	dir, _ := os.Getwd()
	work := filepath.Join(dir, "work")
	os.RemoveAll(work)
	prepare(t, work)
	c := &compose{t: t, dir: dir}
	t.Cleanup(func() { c.run(nil, "down", "-v", "--remove-orphans") })

	t.Run("shutdown-all", func(t *testing.T) {
		c.t = t
		c.up(nil)
		out, code := c.rackctl("shutdown", "all")
		t.Logf("rackctl output:\n%s", out)
		if code != 0 {
			t.Fatalf("rackctl exit %d", code)
		}
		ev := c.events()
		opens := ev.filter("relay", "open")
		if len(opens) != len(order) {
			t.Fatalf("relay opens: %v", opens)
		}
		for i, n := range order {
			p := pcByName(n)
			if opens[i].fields[1] != strconv.Itoa(p.relay) {
				t.Fatalf("open #%d is relay %s, want %d (%s)", i, opens[i].fields[1], p.relay, n)
			}
			if opens[i].fields[3] != "reachable=false" {
				t.Fatalf("relay %d opened while %s still answered ping", p.relay, n)
			}
			sr := ev.filter("shutdown-received", n)
			ex := ev.filter("exiting", n)
			if len(sr) != 1 || len(ex) != 1 {
				t.Fatalf("%s: shutdown-received %d, exiting %d", n, len(sr), len(ex))
			}
			if !ex[0].t.Before(opens[i].t) {
				t.Fatalf("%s: relay opened before the PC exited", n)
			}
			if i > 0 && !sr[0].t.After(opens[i-1].t) {
				t.Fatalf("%s got shutdown before the previous host's relay was cut", n)
			}
		}
		ceph := ev.filter("ceph", "on")
		first := ev.filter("shutdown-received", order[0])
		if len(ceph) != 1 || !ceph[0].t.Before(first[0].t) {
			t.Fatalf("ceph maintenance on must precede the first shutdown: %v", ceph)
		}
		c.run(nil, "down", "-v")
	})

	t.Run("ignored-shutdown-stops-sequence", func(t *testing.T) {
		c.t = t
		env := []string{"IGNORE_A=true"} // second in order
		c.up(env)
		out, code := c.rackctl("shutdown", "all")
		t.Logf("rackctl output:\n%s", out)
		if code == 0 {
			t.Fatal("rackctl succeeded although e2e-a ignored shutdown")
		}
		if !strings.Contains(out, "NOT cutting relay 0") {
			t.Fatal("missing failure report")
		}
		ev := c.events()
		opens := ev.filter("relay", "open")
		if len(opens) != 1 || opens[0].fields[1] != "4" {
			t.Fatalf("only e2e-d's relay 4 may open, got %v", opens)
		}
		for _, n := range order[2:] {
			if len(ev.filter("shutdown-received", n)) != 0 {
				t.Fatalf("%s received shutdown after the sequence should have stopped", n)
			}
		}
		c.run(env, "down", "-v")
	})
}

// ---------------------------------------------------------------------------
// docker compose driver

type compose struct {
	t   *testing.T
	dir string
	env []string // IGNORE_* settings of the current scenario
}

func (c *compose) run(env []string, args ...string) (string, int) {
	cmd := exec.Command("docker", append([]string{"compose", "-f", filepath.Join(c.dir, "docker-compose.yml")}, args...)...)
	cmd.Dir = c.dir
	cmd.Env = append(append(os.Environ(), c.env...), env...)
	var b bytes.Buffer
	cmd.Stdout, cmd.Stderr = &b, &b
	err := cmd.Run()
	code := 0
	if err != nil {
		code = 1
		if ee, ok := err.(*exec.ExitError); ok {
			code = ee.ExitCode()
		}
	}
	return b.String(), code
}

func (c *compose) up(env []string) {
	c.t.Helper()
	c.env = env
	if out, code := c.run(env, "up", "-d", "--build", "--force-recreate"); code != 0 {
		c.t.Fatalf("compose up: %s", out)
	}
	deadline := time.Now().Add(60 * time.Second)
	for {
		out, _ := c.run(env, "run", "--rm", "rackctl", "status")
		ready := false
		n := 0
		for _, l := range strings.Split(out, "\n") {
			f := strings.Fields(l)
			if len(f) == 2 && f[0] == "sbc4" && f[1] == "online" {
				ready = true
			}
			if len(f) >= 7 && strings.HasPrefix(f[0], "e2e-") && f[4] == "up" && f[5] == "online" {
				n++
			}
		}
		if n == len(pcs) && ready {
			return
		}
		if time.Now().After(deadline) {
			logs, _ := c.run(env, "logs")
			c.t.Fatalf("rig not ready:\n%s\n%s", out, logs)
		}
		time.Sleep(2 * time.Second)
	}
}

func (c *compose) rackctl(args ...string) (string, int) {
	return c.run(nil, append([]string{"run", "--rm", "rackctl"}, args...)...)
}

type ev struct {
	t      time.Time
	fields []string
}

type events []ev

func (c *compose) events() events {
	out, _ := c.run(nil, "logs", "--no-color", "--no-log-prefix", "sbc4", "pc-a", "pc-b", "pc-c", "pc-d", "pc-e")
	var es events
	sc := bufio.NewScanner(strings.NewReader(out))
	for sc.Scan() {
		f := strings.Fields(sc.Text())
		if len(f) < 3 || f[0] != "EVENT" {
			continue
		}
		ns, err := strconv.ParseInt(f[1], 10, 64)
		if err != nil {
			continue
		}
		es = append(es, ev{time.Unix(0, ns), f[2:]})
	}
	sort.Slice(es, func(i, j int) bool { return es[i].t.Before(es[j].t) })
	return es
}

// filter matches events by kind and, for "relay", the payload (field 2);
// otherwise by field 1.
func (es events) filter(kind, arg string) events {
	var out events
	for _, e := range es {
		if e.fields[0] != kind || len(e.fields) < 2 {
			continue
		}
		if kind == "relay" {
			if len(e.fields) >= 3 && e.fields[2] == arg {
				out = append(out, e)
			}
		} else if e.fields[1] == arg {
			out = append(out, e)
		}
	}
	return out
}

// ---------------------------------------------------------------------------
// rig generation: certs, passwords, mosquitto config, binaries, rack.toml

func prepare(t *testing.T, work string) {
	t.Helper()
	for _, d := range []string{"certs", "pw", "mosquitto", "bin"} {
		must(t, os.MkdirAll(filepath.Join(work, d), 0o755))
	}
	users := []string{"orchestrator", "sbc4"}
	for _, p := range pcs {
		users = append(users, p.name)
	}
	genPKI(t, filepath.Join(work, "certs"), users)

	var passwd strings.Builder
	for _, u := range users {
		pw := randHex(16)
		must(t, os.WriteFile(filepath.Join(work, "pw", u), []byte(pw+"\n"), 0o644))
		passwd.WriteString(u + ":" + mosquittoHash(pw) + "\n")
	}
	must(t, os.WriteFile(filepath.Join(work, "mosquitto", "passwd"), []byte(passwd.String()), 0o644))
	must(t, os.WriteFile(filepath.Join(work, "mosquitto", "acl"), []byte(acl()), 0o644))
	must(t, os.WriteFile(filepath.Join(work, "mosquitto", "mosquitto.conf"), []byte(`listener 8883
allow_anonymous false
password_file /work/mosquitto/passwd
acl_file /work/mosquitto/acl
cafile /work/certs/ca.crt
certfile /work/certs/server.crt
keyfile /work/certs/server.key
require_certificate true
use_identity_as_username false
tls_version tlsv1.2
persistence false
log_dest stdout
log_type error
log_type warning
log_type notice
`), 0o644))

	arch := strings.TrimSpace(output(t, "docker", "version", "--format", "{{.Server.Arch}}"))
	for _, b := range [][2]string{{"rackctl", ".."}, {"fakenode", "./fakenode"}} {
		cmd := exec.Command("go", "build", "-trimpath", "-o", filepath.Join(work, "bin", b[0]), b[1])
		cmd.Env = append(os.Environ(), "CGO_ENABLED=0", "GOOS=linux", "GOARCH="+arch)
		if out, err := cmd.CombinedOutput(); err != nil {
			t.Fatalf("build %s: %v\n%s", b[0], err, out)
		}
	}

	var cfg strings.Builder
	fmt.Fprintf(&cfg, `[broker]
host = %q
port = 8883
ca = "/work/certs/ca.crt"
cert = "/work/certs/orchestrator.crt"
key = "/work/certs/orchestrator.key"
username = "orchestrator"
password_file = "/work/pw/orchestrator"

[shutdown]
timeout = "25s"
cut_delay = "2s"
pve_quorum = 3
order = ["%s"]
`, brokerIP, strings.Join(order, `", "`))
	for _, p := range pcs {
		fmt.Fprintf(&cfg, "\n[[pc]]\nname = %q\nip = %q\nrelay = %d\nroles = %s\n", p.name, p.ip, p.relay, p.roles)
	}
	must(t, os.WriteFile(filepath.Join(work, "rack.toml"), []byte(cfg.String()), 0o644))
}

func acl() string {
	var b strings.Builder
	b.WriteString(`user orchestrator
topic write rack/pc/+/cmd
topic write rack/relay/+/set
topic write rack/ceph/maintenance/set
topic read rack/pc/+/status
topic read rack/relay/+/state
topic read rack/sbc4/status
topic read rack/ceph/maintenance/state

user sbc4
topic read rack/relay/+/set
topic write rack/relay/+/state
topic write rack/sbc4/status
`)
	for _, p := range pcs {
		fmt.Fprintf(&b, "\nuser %s\ntopic read rack/pc/%s/cmd\ntopic write rack/pc/%s/status\n", p.name, p.name, p.name)
		if p.name == "e2e-e" {
			b.WriteString("topic read rack/ceph/maintenance/set\ntopic write rack/ceph/maintenance/state\n")
		}
	}
	return b.String()
}

func genPKI(t *testing.T, dir string, users []string) {
	t.Helper()
	caKey, _ := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	caTmpl := &x509.Certificate{
		SerialNumber: serial(), Subject: pkix.Name{CommonName: "rackctl e2e test CA"},
		NotBefore: time.Now().Add(-time.Hour), NotAfter: time.Now().Add(24 * time.Hour),
		IsCA: true, BasicConstraintsValid: true, KeyUsage: x509.KeyUsageCertSign | x509.KeyUsageCRLSign,
	}
	caDER, err := x509.CreateCertificate(rand.Reader, caTmpl, caTmpl, &caKey.PublicKey, caKey)
	must(t, err)
	caCert, _ := x509.ParseCertificate(caDER)
	writePEM(t, filepath.Join(dir, "ca.crt"), "CERTIFICATE", caDER)

	leaf := func(name string, server bool) {
		k, _ := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
		tmpl := &x509.Certificate{
			SerialNumber: serial(), Subject: pkix.Name{CommonName: name},
			NotBefore: time.Now().Add(-time.Hour), NotAfter: time.Now().Add(24 * time.Hour),
			KeyUsage: x509.KeyUsageDigitalSignature,
		}
		if server {
			tmpl.ExtKeyUsage = []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth}
			tmpl.IPAddresses = []net.IP{net.ParseIP(brokerIP)}
		} else {
			tmpl.ExtKeyUsage = []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth}
		}
		der, err := x509.CreateCertificate(rand.Reader, tmpl, caCert, &k.PublicKey, caKey)
		must(t, err)
		writePEM(t, filepath.Join(dir, name+".crt"), "CERTIFICATE", der)
		kd, err := x509.MarshalECPrivateKey(k)
		must(t, err)
		writePEM(t, filepath.Join(dir, name+".key"), "EC PRIVATE KEY", kd)
	}
	leaf("server", true)
	for _, u := range users {
		leaf(u, false)
	}
}

func writePEM(t *testing.T, path, typ string, der []byte) {
	// 0644: read by the non-root mosquitto user through a bind mount; test-only material.
	must(t, os.WriteFile(path, pem.EncodeToMemory(&pem.Block{Type: typ, Bytes: der}), 0o644))
}

func serial() *big.Int {
	n, _ := rand.Int(rand.Reader, new(big.Int).Lsh(big.NewInt(1), 120))
	return n
}

func randHex(n int) string {
	b := make([]byte, n)
	rand.Read(b)
	return hex.EncodeToString(b)
}

func output(t *testing.T, name string, args ...string) string {
	out, err := exec.Command(name, args...).Output()
	must(t, err)
	return string(out)
}

func must(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}
