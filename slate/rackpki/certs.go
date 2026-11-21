package main

import (
	"bytes"
	"crypto"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/sha1"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"errors"
	"fmt"
	"math/big"
	"net"
	"os"
	"path/filepath"
	"time"
)

const (
	CACommonName     = "rack-control CA"
	BrokerCommonName = "rack-control broker"
	DefaultBrokerIP  = "192.168.8.1"

	caYears   = 10
	leafYears = 5

	// backdate keeps freshly issued certs valid on hosts whose clock is
	// slightly behind (the Slate and SBC4 may not have NTP yet at boot).
	backdate = time.Hour
)

// now is replaceable in tests.
var now = time.Now

type ca struct {
	cert *x509.Certificate
	key  *ecdsa.PrivateKey
}

func newKey() (*ecdsa.PrivateKey, error) {
	return ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
}

func serial() (*big.Int, error) {
	// 128 random bits, positive, never zero.
	n, err := rand.Int(rand.Reader, new(big.Int).Lsh(big.NewInt(1), 128))
	if err != nil {
		return nil, err
	}
	return n.Add(n, big.NewInt(1)), nil
}

func subjectKeyID(pub *ecdsa.PublicKey) ([]byte, error) {
	der, err := x509.MarshalPKIXPublicKey(pub)
	if err != nil {
		return nil, err
	}
	sum := sha1.Sum(der) // RFC 5280 method 1 style key identifier; not a security use.
	return sum[:], nil
}

func encodeCert(der []byte) []byte {
	return pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})
}

func encodeKey(k *ecdsa.PrivateKey) ([]byte, error) {
	der, err := x509.MarshalPKCS8PrivateKey(k)
	if err != nil {
		return nil, err
	}
	return pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: der}), nil
}

// makeCA builds a self-signed ECDSA P-256 CA valid for 10 years.
func makeCA() (certPEM, keyPEM []byte, err error) {
	key, err := newKey()
	if err != nil {
		return nil, nil, err
	}
	sn, err := serial()
	if err != nil {
		return nil, nil, err
	}
	skid, err := subjectKeyID(&key.PublicKey)
	if err != nil {
		return nil, nil, err
	}
	t := now()
	tmpl := &x509.Certificate{
		SerialNumber:          sn,
		Subject:               pkix.Name{CommonName: CACommonName},
		NotBefore:             t.Add(-backdate),
		NotAfter:              t.AddDate(caYears, 0, 0),
		IsCA:                  true,
		BasicConstraintsValid: true,
		MaxPathLen:            0,
		MaxPathLenZero:        true, // it signs leaves only, no intermediates
		KeyUsage:              x509.KeyUsageCertSign | x509.KeyUsageCRLSign,
		SubjectKeyId:          skid,
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, tmpl, &key.PublicKey, key)
	if err != nil {
		return nil, nil, err
	}
	keyPEM, err = encodeKey(key)
	if err != nil {
		return nil, nil, err
	}
	return encodeCert(der), keyPEM, nil
}

// issueLeaf signs a 5-year ECDSA P-256 leaf certificate.
func (c *ca) issueLeaf(cn string, ips []net.IP, eku x509.ExtKeyUsage) (certPEM, keyPEM []byte, err error) {
	key, err := newKey()
	if err != nil {
		return nil, nil, err
	}
	sn, err := serial()
	if err != nil {
		return nil, nil, err
	}
	t := now()
	notAfter := t.AddDate(leafYears, 0, 0)
	if notAfter.After(c.cert.NotAfter) {
		return nil, nil, fmt.Errorf("CA expires %s, before a %d-year leaf would; rotate the CA first",
			c.cert.NotAfter.Format(time.DateOnly), leafYears)
	}
	tmpl := &x509.Certificate{
		SerialNumber:          sn,
		Subject:               pkix.Name{CommonName: cn},
		NotBefore:             t.Add(-backdate),
		NotAfter:              notAfter,
		BasicConstraintsValid: true,
		IsCA:                  false,
		KeyUsage:              x509.KeyUsageDigitalSignature,
		ExtKeyUsage:           []x509.ExtKeyUsage{eku},
		IPAddresses:           ips,
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, c.cert, &key.PublicKey, c.key)
	if err != nil {
		return nil, nil, err
	}
	keyPEM, err = encodeKey(key)
	if err != nil {
		return nil, nil, err
	}
	return encodeCert(der), keyPEM, nil
}

func parseCertPEM(data []byte) (*x509.Certificate, error) {
	b, rest := pem.Decode(data)
	if b == nil || b.Type != "CERTIFICATE" {
		return nil, errors.New("no CERTIFICATE PEM block")
	}
	if len(bytes.TrimSpace(rest)) != 0 {
		return nil, errors.New("unexpected data after the certificate")
	}
	return x509.ParseCertificate(b.Bytes)
}

func parseKeyPEM(data []byte) (*ecdsa.PrivateKey, error) {
	b, _ := pem.Decode(data)
	if b == nil || b.Type != "PRIVATE KEY" {
		return nil, errors.New("no PKCS#8 PRIVATE KEY PEM block")
	}
	k, err := x509.ParsePKCS8PrivateKey(b.Bytes)
	if err != nil {
		return nil, err
	}
	ek, ok := k.(*ecdsa.PrivateKey)
	if !ok {
		return nil, errors.New("private key is not ECDSA")
	}
	return ek, nil
}

func readCert(path string) (*x509.Certificate, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	c, err := parseCertPEM(data)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	return c, nil
}

// keyMatches reports whether priv is the key for cert's public key.
func keyMatches(cert *x509.Certificate, priv crypto.Signer) bool {
	pub, ok := cert.PublicKey.(*ecdsa.PublicKey)
	return ok && pub.Equal(priv.Public())
}

// loadCA reads ca.crt and ca.key from dir and checks that they belong together.
func loadCA(dir string) (*ca, error) {
	cert, err := readCert(filepath.Join(dir, "ca.crt"))
	if err != nil {
		return nil, fmt.Errorf("load CA (run init-ca first?): %w", err)
	}
	keyData, err := os.ReadFile(filepath.Join(dir, "ca.key"))
	if err != nil {
		return nil, fmt.Errorf("load CA key (it lives in 1Password; restore it to %s for signing): %w", dir, err)
	}
	key, err := parseKeyPEM(keyData)
	if err != nil {
		return nil, fmt.Errorf("ca.key: %w", err)
	}
	if !cert.IsCA || !keyMatches(cert, key) {
		return nil, errors.New("ca.key does not match ca.crt, or ca.crt is not a CA")
	}
	return &ca{cert: cert, key: key}, nil
}

// verifyLeaf checks that cert chains to caCert for the given usage.
func verifyLeaf(caCert, cert *x509.Certificate, eku x509.ExtKeyUsage) error {
	pool := x509.NewCertPool()
	pool.AddCert(caCert)
	_, err := cert.Verify(x509.VerifyOptions{Roots: pool, KeyUsages: []x509.ExtKeyUsage{eku}})
	return err
}
