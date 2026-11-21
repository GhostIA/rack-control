package main

import (
	"bytes"
	"crypto/hmac"
	"crypto/sha512"
	"crypto/x509"
	"encoding/base64"
	"encoding/binary"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"
)

// fakePasswords are TEST-ONLY values, never real credentials.
func fakeEnv() map[string]string {
	m := map[string]string{}
	for _, u := range Users {
		m[PasswordEnvVar(u)] = "fake-test-pw-" + u
	}
	return m
}

type harness struct {
	t      *testing.T
	out    string
	vars   map[string]string
	stdout bytes.Buffer
	stderr bytes.Buffer
}

func newHarness(t *testing.T) *harness {
	t.Helper()
	return &harness{t: t, out: filepath.Join(t.TempDir(), "pki"), vars: fakeEnv()}
}

func (h *harness) run(args ...string) int {
	h.stdout.Reset()
	h.stderr.Reset()
	e := &env{stdout: &h.stdout, stderr: &h.stderr, getenv: func(k string) string { return h.vars[k] }, cwd: "."}
	return e.run(append(args, "--out", h.out))
}

func (h *harness) must(args ...string) {
	h.t.Helper()
	if code := h.run(args...); code != 0 {
		h.t.Fatalf("rackpki %v: exit %d, stderr: %s", args, code, h.stderr.String())
	}
}

func (h *harness) cert(name string) *x509.Certificate {
	h.t.Helper()
	c, err := readCert(filepath.Join(h.out, name))
	if err != nil {
		h.t.Fatal(err)
	}
	return c
}

func (h *harness) pool() *x509.CertPool {
	p := x509.NewCertPool()
	p.AddCert(h.cert("ca.crt"))
	return p
}

// pbkdf2SHA512 is an independent RFC 8018 PBKDF2 using only crypto/hmac,
// to check golang.org/x/crypto/pbkdf2 as used by rackpki.
func pbkdf2SHA512(pw, salt []byte, iter, keyLen int) []byte {
	var out []byte
	for block := uint32(1); len(out) < keyLen; block++ {
		mac := hmac.New(sha512.New, pw)
		mac.Write(salt)
		var be [4]byte
		binary.BigEndian.PutUint32(be[:], block)
		mac.Write(be[:])
		u := mac.Sum(nil)
		t := append([]byte(nil), u...)
		for i := 1; i < iter; i++ {
			mac = hmac.New(sha512.New, pw)
			mac.Write(u)
			u = mac.Sum(nil)
			for j := range t {
				t[j] ^= u[j]
			}
		}
		out = append(out, t...)
	}
	return out[:keyLen]
}

type parsedLine struct {
	user       string
	hashType   int
	iterations int
	salt, hash []byte
}

func parseLine(t *testing.T, line string) parsedLine {
	t.Helper()
	user, rest, ok := strings.Cut(line, ":")
	if !ok {
		t.Fatalf("no colon in %q", line)
	}
	f := strings.Split(rest, "$")
	if len(f) != 5 || f[0] != "" {
		t.Fatalf("want $7$iter$salt$hash, got %q", rest)
	}
	ht, err1 := strconv.Atoi(f[1])
	it, err2 := strconv.Atoi(f[2])
	salt, err3 := base64.StdEncoding.Strict().DecodeString(f[3])
	hash, err4 := base64.StdEncoding.Strict().DecodeString(f[4])
	for _, err := range []error{err1, err2, err3, err4} {
		if err != nil {
			t.Fatalf("parse %q: %v", line, err)
		}
	}
	return parsedLine{user, ht, it, salt, hash}
}

func TestPasswdLineKnownVector(t *testing.T) {
	// Reference computed with Python hashlib.pbkdf2_hmac('sha512', ...).
	salt := []byte{1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12}
	got := formatPasswdLine("orchestrator", "fake-test-password", salt)
	want := "orchestrator:$7$101$AQIDBAUGBwgJCgsM$s7qGCy+7U8HwiNzpi8ChtaVqXEVdNx5Zb0Hu2ozK2nrEjiR29msf+3m6et45aSILLq7ksz7fXCUYeu7nSLys6Q=="
	if got != want {
		t.Fatalf("got  %s\nwant %s", got, want)
	}
}

func TestPasswdRoundTrip(t *testing.T) {
	h := newHarness(t)
	h.must("passwd")
	data, err := os.ReadFile(filepath.Join(h.out, "passwd"))
	if err != nil {
		t.Fatal(err)
	}
	if bytes.Contains(h.stdout.Bytes(), []byte("fake-test-pw")) || bytes.Contains(h.stderr.Bytes(), []byte("fake-test-pw")) {
		t.Fatal("a password was printed")
	}
	lines := strings.Split(strings.TrimSuffix(string(data), "\n"), "\n")
	if len(lines) != len(Users) {
		t.Fatalf("want %d lines, got %d", len(Users), len(lines))
	}
	salts := map[string]bool{}
	for i, line := range lines {
		p := parseLine(t, line)
		if p.user != Users[i] || p.hashType != 7 || p.iterations != 101 || len(p.salt) != 12 || len(p.hash) != 64 {
			t.Fatalf("bad line %q: %+v", line, p)
		}
		if salts[string(p.salt)] {
			t.Fatal("salt reused")
		}
		salts[string(p.salt)] = true
		want := pbkdf2SHA512([]byte(h.vars[PasswordEnvVar(p.user)]), p.salt, p.iterations, 64)
		if !bytes.Equal(p.hash, want) {
			t.Fatalf("%s: hash mismatch with independent PBKDF2", p.user)
		}
		wrong := pbkdf2SHA512([]byte("wrong"), p.salt, p.iterations, 64)
		if bytes.Equal(p.hash, wrong) {
			t.Fatal("wrong password matched")
		}
	}
	if err := checkPasswdFile(data); err != nil {
		t.Fatal(err)
	}
	assertMode(t, filepath.Join(h.out, "passwd"), 0o600)
}

func TestPasswdMissingEnvFails(t *testing.T) {
	h := newHarness(t)
	delete(h.vars, "MQTT_INTEL_D1BABF_PASSWORD")
	h.vars["MQTT_SBC4_PASSWORD"] = ""
	if code := h.run("passwd"); code != 1 {
		t.Fatalf("exit %d, want 1", code)
	}
	msg := h.stderr.String()
	for _, v := range []string{"MQTT_INTEL_D1BABF_PASSWORD", "MQTT_SBC4_PASSWORD"} {
		if !strings.Contains(msg, v) {
			t.Errorf("error does not name %s: %s", v, msg)
		}
	}
	if strings.Contains(msg, "fake-test-pw") {
		t.Fatal("error leaked a password")
	}
	if _, err := os.Stat(filepath.Join(h.out, "passwd")); !os.IsNotExist(err) {
		t.Fatal("passwd written despite missing variable")
	}
}

func TestPasswdRejectsWhitespace(t *testing.T) {
	h := newHarness(t)
	h.vars["MQTT_ORCHESTRATOR_PASSWORD"] = "fake-test-pw \n"
	if code := h.run("passwd"); code != 1 {
		t.Fatalf("exit %d, want 1", code)
	}
}

func TestEnvVarNames(t *testing.T) {
	cases := map[string]string{
		"orchestrator": "MQTT_ORCHESTRATOR_PASSWORD",
		"sbc4":         "MQTT_SBC4_PASSWORD",
		"intel-91c45d": "MQTT_INTEL_91C45D_PASSWORD",
		"intel-1ecbc8": "MQTT_INTEL_1ECBC8_PASSWORD",
	}
	for u, want := range cases {
		if got := PasswordEnvVar(u); got != want {
			t.Errorf("%s: got %s want %s", u, got, want)
		}
	}
}

func TestCAProperties(t *testing.T) {
	h := newHarness(t)
	h.must("init-ca")
	c := h.cert("ca.crt")
	if c.Subject.CommonName != "rack-control CA" || !c.IsCA || !c.BasicConstraintsValid ||
		c.MaxPathLen != 0 || !c.MaxPathLenZero ||
		c.KeyUsage != x509.KeyUsageCertSign|x509.KeyUsageCRLSign {
		t.Fatalf("bad CA: %+v", c)
	}
	if c.PublicKeyAlgorithm != x509.ECDSA || c.SignatureAlgorithm != x509.ECDSAWithSHA256 {
		t.Fatal("CA not ECDSA P-256/SHA-256")
	}
	if y := c.NotAfter.Year() - time.Now().Year(); y != 10 {
		t.Fatalf("CA validity %d years", y)
	}
	assertMode(t, filepath.Join(h.out, "ca.key"), 0o600)
}

func TestServerCertIPSAN(t *testing.T) {
	h := newHarness(t)
	h.must("init-ca")
	h.must("server", "--ip", "192.168.8.1")
	c := h.cert("broker.crt")
	opts := x509.VerifyOptions{Roots: h.pool(), KeyUsages: []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth}}

	opts.DNSName = "192.168.8.1"
	if _, err := c.Verify(opts); err != nil {
		t.Fatalf("should verify for 192.168.8.1: %v", err)
	}
	for _, other := range []string{"192.168.8.2", "10.0.0.1", "broker.rack.lan"} {
		opts.DNSName = other
		if _, err := c.Verify(opts); err == nil {
			t.Fatalf("verified for %s", other)
		}
	}
	if len(c.DNSNames) != 0 || len(c.IPAddresses) != 1 || len(c.EmailAddresses) != 0 || len(c.URIs) != 0 {
		t.Fatalf("SANs should be the one IP only: %v %v", c.DNSNames, c.IPAddresses)
	}
	if c.IsCA || len(c.ExtKeyUsage) != 1 || c.ExtKeyUsage[0] != x509.ExtKeyUsageServerAuth {
		t.Fatal("broker cert must be a ServerAuth-only leaf")
	}
	if y := c.NotAfter.Year() - time.Now().Year(); y != 5 {
		t.Fatalf("broker validity %d years", y)
	}
	assertMode(t, filepath.Join(h.out, "broker.key"), 0o600)
}

func TestServerRejectsBadIP(t *testing.T) {
	h := newHarness(t)
	h.must("init-ca")
	if code := h.run("server", "--ip", "slate.lan"); code != 64 {
		t.Fatalf("exit %d, want 64", code)
	}
}

func TestClientCerts(t *testing.T) {
	h := newHarness(t)
	h.must("init-ca")
	h.must("client", "--all")
	for _, u := range Users {
		c := h.cert(u + ".crt")
		if c.Subject.CommonName != u {
			t.Errorf("%s: CN %q", u, c.Subject.CommonName)
		}
		if len(c.ExtKeyUsage) != 1 || c.ExtKeyUsage[0] != x509.ExtKeyUsageClientAuth {
			t.Errorf("%s: EKU %v", u, c.ExtKeyUsage)
		}
		if _, err := c.Verify(x509.VerifyOptions{Roots: h.pool(), KeyUsages: []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth}}); err != nil {
			t.Errorf("%s: %v", u, err)
		}
		if _, err := c.Verify(x509.VerifyOptions{Roots: h.pool(), KeyUsages: []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth}}); err == nil {
			t.Errorf("%s: client cert usable as a server cert", u)
		}
		assertMode(t, filepath.Join(h.out, u+".key"), 0o600)
	}
}

func TestClientRejectsUnknownUser(t *testing.T) {
	h := newHarness(t)
	h.must("init-ca")
	for _, args := range [][]string{{"client", "intel-ffffff"}, {"client", "../ca"}, {"client"}, {"client", "sbc4", "--all"}} {
		if code := h.run(args...); code != 64 {
			t.Errorf("%v: exit %d, want 64", args, code)
		}
	}
}

func TestOverwriteRefused(t *testing.T) {
	h := newHarness(t)
	h.must("init-ca")
	before, _ := os.ReadFile(filepath.Join(h.out, "ca.key"))
	if code := h.run("init-ca"); code != 1 || !strings.Contains(h.stderr.String(), "--force") {
		t.Fatalf("second init-ca: exit %d, %s", code, h.stderr.String())
	}
	after, _ := os.ReadFile(filepath.Join(h.out, "ca.key"))
	if !bytes.Equal(before, after) {
		t.Fatal("ca.key changed without --force")
	}

	h.must("client", "sbc4")
	if code := h.run("client", "sbc4"); code != 1 {
		t.Fatalf("client overwrite: exit %d", code)
	}
	h.must("passwd")
	if code := h.run("passwd"); code != 1 {
		t.Fatalf("passwd overwrite: exit %d", code)
	}

	h.must("init-ca", "--force")
	after, _ = os.ReadFile(filepath.Join(h.out, "ca.key"))
	if bytes.Equal(before, after) {
		t.Fatal("--force did not replace ca.key")
	}
	assertMode(t, filepath.Join(h.out, "ca.key"), 0o600)
	h.must("passwd", "--force")
}

func TestDoubleDashEndsFlags(t *testing.T) {
	h := newHarness(t)
	h.must("init-ca")
	h.must("client", "sbc4")
	// "--force" after "--" is a positional (invalid user), not the flag.
	e := &env{stdout: &h.stdout, stderr: &h.stderr, getenv: func(string) string { return "" }, cwd: "."}
	if code := e.run([]string{"client", "--out", h.out, "--", "sbc4", "--force"}); code != 64 {
		t.Fatalf("exit %d, want 64: %s", code, h.stderr.String())
	}
}

func TestBundles(t *testing.T) {
	h := newHarness(t)
	mosq := t.TempDir()
	os.WriteFile(filepath.Join(mosq, "acl"), []byte("user x\n"), 0o644)
	os.WriteFile(filepath.Join(mosq, "mosquitto.conf"), []byte("listener 8883 192.168.8.1\n"), 0o644)
	h.must("init-ca")
	h.must("server")
	h.must("client", "--all")
	h.must("passwd")
	h.must("bundle", "intel-d1babf", "broker", "--mosquitto-dir", mosq)

	d := filepath.Join(h.out, "deploy", "intel-d1babf")
	for _, f := range []string{"ca.crt", "intel-d1babf.crt", "intel-d1babf.key", "intel-d1babf.password"} {
		assertMode(t, filepath.Join(d, f), 0o600)
	}
	pw, _ := os.ReadFile(filepath.Join(d, "intel-d1babf.password"))
	if string(pw) != "fake-test-pw-intel-d1babf" {
		t.Fatal("password file content wrong")
	}
	b := filepath.Join(h.out, "deploy", "broker")
	for _, f := range []string{"ca.crt", "broker.crt", "broker.key", "passwd", "acl", "mosquitto.conf"} {
		assertMode(t, filepath.Join(b, f), 0o600)
	}
	if code := h.run("bundle", "intel-d1babf"); code != 1 {
		t.Fatalf("bundle overwrite: exit %d", code)
	}
	delete(h.vars, "MQTT_SBC4_PASSWORD")
	if code := h.run("bundle", "sbc4"); code != 1 || !strings.Contains(h.stderr.String(), "MQTT_SBC4_PASSWORD") {
		t.Fatalf("bundle without password: exit %d %s", code, h.stderr.String())
	}
}

func TestBundleRejectsForeignCert(t *testing.T) {
	h := newHarness(t)
	h.must("init-ca")
	h.must("client", "sbc4")
	h.must("init-ca", "--force") // new CA; sbc4.crt is now from the old one
	if code := h.run("bundle", "sbc4"); code != 1 || !strings.Contains(h.stderr.String(), "verify") {
		t.Fatalf("exit %d: %s", code, h.stderr.String())
	}
}

func TestExpiryWarns(t *testing.T) {
	h := newHarness(t)
	h.must("init-ca")
	h.must("server")
	h.must("expiry")
	if !strings.Contains(h.stdout.String(), "broker.crt") {
		t.Fatal(h.stdout.String())
	}
	real := now
	defer func() { now = real }()
	now = func() time.Time { return real().AddDate(4, 9, 0) }
	if code := h.run("expiry"); code != 2 || !strings.Contains(h.stdout.String(), "RENEW SOON") {
		t.Fatalf("exit %d: %s", code, h.stdout.String())
	}
	now = func() time.Time { return real().AddDate(6, 0, 0) }
	if code := h.run("expiry"); code != 2 || !strings.Contains(h.stdout.String(), "EXPIRED") {
		t.Fatalf("exit %d: %s", code, h.stdout.String())
	}
}

func TestLeafCannotOutliveCA(t *testing.T) {
	h := newHarness(t)
	h.must("init-ca")
	real := now
	defer func() { now = real }()
	now = func() time.Time { return real().AddDate(6, 0, 0) }
	if code := h.run("client", "sbc4"); code != 1 {
		t.Fatalf("exit %d", code)
	}
}

func assertMode(t *testing.T, path string, want os.FileMode) {
	t.Helper()
	st, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := st.Mode().Perm(); got != want {
		t.Fatalf("%s: mode %o, want %o", path, got, want)
	}
}
