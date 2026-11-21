package mqttbus

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"math/big"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/GhostIA/rack-control/slate/rackctl/internal/config"
)

func writePEM(t *testing.T, path, typ string, der []byte) {
	t.Helper()
	if err := os.WriteFile(path, pem.EncodeToMemory(&pem.Block{Type: typ, Bytes: der}), 0o600); err != nil {
		t.Fatal(err)
	}
}

func TestTLSConfig(t *testing.T) {
	d := t.TempDir()
	k, _ := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	tmpl := &x509.Certificate{SerialNumber: big.NewInt(1), Subject: pkix.Name{CommonName: "orchestrator"},
		NotBefore: time.Now().Add(-time.Hour), NotAfter: time.Now().Add(time.Hour), IsCA: true, BasicConstraintsValid: true,
		KeyUsage: x509.KeyUsageCertSign | x509.KeyUsageDigitalSignature}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, tmpl, &k.PublicKey, k)
	if err != nil {
		t.Fatal(err)
	}
	kd, _ := x509.MarshalECPrivateKey(k)
	writePEM(t, filepath.Join(d, "ca.crt"), "CERTIFICATE", der)
	writePEM(t, filepath.Join(d, "c.crt"), "CERTIFICATE", der)
	writePEM(t, filepath.Join(d, "c.key"), "EC PRIVATE KEY", kd)
	b := config.Broker{Host: "192.168.8.1", CA: filepath.Join(d, "ca.crt"), Cert: filepath.Join(d, "c.crt"), Key: filepath.Join(d, "c.key")}
	tc, err := TLSConfig(b)
	if err != nil {
		t.Fatal(err)
	}
	if tc.MinVersion != tls.VersionTLS12 || tc.ServerName != "192.168.8.1" || tc.InsecureSkipVerify || tc.RootCAs == nil || len(tc.Certificates) != 1 {
		t.Fatalf("bad tls config: %+v", tc)
	}
	b.CA = filepath.Join(d, "c.key")
	if _, err := TLSConfig(b); err == nil {
		t.Fatal("CA file without certificates accepted")
	}
}
