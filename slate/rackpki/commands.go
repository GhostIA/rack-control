package main

import (
	"bufio"
	"bytes"
	"crypto/x509"
	"errors"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"text/tabwriter"
	"time"
)

// ---- init-ca ---------------------------------------------------------------

func (e *env) cmdInitCA(c common) error {
	if err := e.resolve(&c, false); err != nil {
		return err
	}
	certPEM, keyPEM, err := makeCA()
	if err != nil {
		return err
	}
	if err := writeAll([]outFile{
		{filepath.Join(c.out, "ca.crt"), certPEM, modePublic},
		{filepath.Join(c.out, "ca.key"), keyPEM, modeSecret},
	}, c.force); err != nil {
		return err
	}
	fmt.Fprintf(e.stdout, "wrote %s/ca.crt and ca.key (0600)\n", c.out)
	fmt.Fprintln(e.stdout, "store ca.key as a 1Password Document item, then delete the local copy once all certs are issued")
	return nil
}

// ---- server ----------------------------------------------------------------

func (e *env) cmdServer(c common, ipStr string) error {
	ip := net.ParseIP(ipStr)
	if ip == nil {
		return fmt.Errorf("%w: --ip %q is not an IP address", errUsage, ipStr)
	}
	if v4 := ip.To4(); v4 != nil {
		ip = v4
	}
	if err := e.resolve(&c, false); err != nil {
		return err
	}
	authority, err := loadCA(c.out)
	if err != nil {
		return err
	}
	certPEM, keyPEM, err := authority.issueLeaf(BrokerCommonName, []net.IP{ip}, x509.ExtKeyUsageServerAuth)
	if err != nil {
		return err
	}
	if err := writeAll([]outFile{
		{filepath.Join(c.out, "broker.crt"), certPEM, modePublic},
		{filepath.Join(c.out, "broker.key"), keyPEM, modeSecret},
	}, c.force); err != nil {
		return err
	}
	fmt.Fprintf(e.stdout, "wrote %s/broker.crt (IP SAN %s) and broker.key (0600)\n", c.out, ip)
	return nil
}

// ---- client ----------------------------------------------------------------

func (e *env) cmdClient(c common, pos []string, all bool) error {
	users, err := pickUsers(pos, all, false)
	if err != nil {
		return err
	}
	if err := e.resolve(&c, false); err != nil {
		return err
	}
	authority, err := loadCA(c.out)
	if err != nil {
		return err
	}
	var files []outFile
	for _, u := range users {
		certPEM, keyPEM, err := authority.issueLeaf(u, nil, x509.ExtKeyUsageClientAuth)
		if err != nil {
			return err
		}
		files = append(files,
			outFile{filepath.Join(c.out, u+".crt"), certPEM, modePublic},
			outFile{filepath.Join(c.out, u+".key"), keyPEM, modeSecret})
	}
	if err := writeAll(files, c.force); err != nil {
		return err
	}
	for _, u := range users {
		fmt.Fprintf(e.stdout, "wrote %s/%s.crt and %s.key (0600)\n", c.out, u, u)
	}
	return nil
}

// pickUsers validates positional user names, or expands --all. allowBroker
// also accepts the "broker" pseudo-user (bundle only).
func pickUsers(pos []string, all, allowBroker bool) ([]string, error) {
	if all {
		if len(pos) != 0 {
			return nil, fmt.Errorf("%w: give user names or --all, not both", errUsage)
		}
		out := append([]string(nil), Users...)
		if allowBroker {
			out = append([]string{BrokerBundle}, out...)
		}
		return out, nil
	}
	if len(pos) == 0 {
		return nil, fmt.Errorf("%w: give a user name or --all", errUsage)
	}
	seen := map[string]bool{}
	for _, p := range pos {
		if seen[p] {
			return nil, fmt.Errorf("%w: %q given twice", errUsage, p)
		}
		seen[p] = true
		if allowBroker && p == BrokerBundle {
			continue
		}
		if err := validUser(p); err != nil {
			return nil, fmt.Errorf("%w: %v", errUsage, err)
		}
	}
	return pos, nil
}

// ---- passwd ----------------------------------------------------------------

func (e *env) cmdPasswd(c common) error {
	if err := e.resolve(&c, false); err != nil {
		return err
	}
	data, err := buildPasswd(e.getenv)
	if err != nil {
		return err
	}
	path := filepath.Join(c.out, "passwd")
	if err := writeAll([]outFile{{path, data, modeSecret}}, c.force); err != nil {
		return err
	}
	fmt.Fprintf(e.stdout, "wrote %s (0600) for %d users\n", path, len(Users))
	return nil
}

// ---- bundle ----------------------------------------------------------------

func (e *env) cmdBundle(c common, pos []string, all bool) error {
	names, err := pickUsers(pos, all, true)
	if err != nil {
		return err
	}
	needMosq := false
	for _, n := range names {
		if n == BrokerBundle {
			needMosq = true
		}
	}
	if err := e.resolve(&c, needMosq); err != nil {
		return err
	}
	caCert, err := readCert(filepath.Join(c.out, "ca.crt"))
	if err != nil {
		return err
	}
	caPEM := encodeCert(caCert.Raw)

	var files []outFile
	var errs []error
	for _, n := range names {
		var f []outFile
		var err error
		if n == BrokerBundle {
			f, err = brokerBundle(c, caCert, caPEM)
		} else {
			f, err = userBundle(c, n, caCert, caPEM, e.getenv)
		}
		if err != nil {
			errs = append(errs, fmt.Errorf("bundle %s: %w", n, err))
			continue
		}
		files = append(files, f...)
	}
	if len(errs) > 0 {
		return errors.Join(errs...)
	}
	if err := writeAll(files, c.force); err != nil {
		return err
	}
	for _, n := range names {
		fmt.Fprintf(e.stdout, "wrote %s (all files 0600)\n", filepath.Join(c.out, "deploy", n))
	}
	return nil
}

func readLeaf(dir, name string, caCert *x509.Certificate, eku x509.ExtKeyUsage) (certPEM, keyPEM []byte, cert *x509.Certificate, err error) {
	certPath := filepath.Join(dir, name+".crt")
	cert, err = readCert(certPath)
	if err != nil {
		return nil, nil, nil, err
	}
	if err := verifyLeaf(caCert, cert, eku); err != nil {
		return nil, nil, nil, fmt.Errorf("%s does not verify against ca.crt: %w", certPath, err)
	}
	keyPEM, err = os.ReadFile(filepath.Join(dir, name+".key"))
	if err != nil {
		return nil, nil, nil, err
	}
	key, err := parseKeyPEM(keyPEM)
	if err != nil {
		return nil, nil, nil, fmt.Errorf("%s.key: %w", name, err)
	}
	if !keyMatches(cert, key) {
		return nil, nil, nil, fmt.Errorf("%s.key does not match %s.crt", name, name)
	}
	return encodeCert(cert.Raw), keyPEM, cert, nil
}

func userBundle(c common, user string, caCert *x509.Certificate, caPEM []byte, getenv func(string) string) ([]outFile, error) {
	certPEM, keyPEM, cert, err := readLeaf(c.out, user, caCert, x509.ExtKeyUsageClientAuth)
	if err != nil {
		return nil, err
	}
	if cert.Subject.CommonName != user {
		return nil, fmt.Errorf("%s.crt has CN %q, want %q", user, cert.Subject.CommonName, user)
	}
	pw, err := passwordFor(user, getenv)
	if err != nil {
		return nil, err
	}
	d := filepath.Join(c.out, "deploy", user)
	return []outFile{
		{filepath.Join(d, "ca.crt"), caPEM, modeSecret},
		{filepath.Join(d, user+".crt"), certPEM, modeSecret},
		{filepath.Join(d, user+".key"), keyPEM, modeSecret},
		{filepath.Join(d, user+".password"), []byte(pw), modeSecret},
	}, nil
}

func brokerBundle(c common, caCert *x509.Certificate, caPEM []byte) ([]outFile, error) {
	certPEM, keyPEM, _, err := readLeaf(c.out, "broker", caCert, x509.ExtKeyUsageServerAuth)
	if err != nil {
		return nil, err
	}
	passwd, err := os.ReadFile(filepath.Join(c.out, "passwd"))
	if err != nil {
		return nil, fmt.Errorf("%w (run rackpki passwd first)", err)
	}
	if err := checkPasswdFile(passwd); err != nil {
		return nil, err
	}
	acl, err := os.ReadFile(filepath.Join(c.mosquittoDir, "acl"))
	if err != nil {
		return nil, err
	}
	conf, err := os.ReadFile(filepath.Join(c.mosquittoDir, "mosquitto.conf"))
	if err != nil {
		return nil, err
	}
	d := filepath.Join(c.out, "deploy", BrokerBundle)
	return []outFile{
		{filepath.Join(d, "ca.crt"), caPEM, modeSecret},
		{filepath.Join(d, "broker.crt"), certPEM, modeSecret},
		{filepath.Join(d, "broker.key"), keyPEM, modeSecret},
		{filepath.Join(d, "passwd"), passwd, modeSecret},
		{filepath.Join(d, "acl"), acl, modeSecret},
		{filepath.Join(d, "mosquitto.conf"), conf, modeSecret},
	}, nil
}

// checkPasswdFile makes sure every user has exactly one $7$ line.
func checkPasswdFile(data []byte) error {
	count := map[string]int{}
	sc := bufio.NewScanner(bytes.NewReader(data))
	for sc.Scan() {
		line := sc.Text()
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		user, hash, ok := strings.Cut(line, ":")
		if !ok || !strings.HasPrefix(hash, fmt.Sprintf("$%d$%d$", pwHashType, pwIterations)) {
			return errors.New("passwd has a line that is not user:$7$101$...")
		}
		count[user]++
	}
	if err := sc.Err(); err != nil {
		return err
	}
	for _, u := range Users {
		if count[u] != 1 {
			return fmt.Errorf("passwd has %d entries for %s, want 1 (regenerate with rackpki passwd --force)", count[u], u)
		}
	}
	if len(count) != len(Users) {
		return errors.New("passwd has entries for unknown users")
	}
	return nil
}

// ---- expiry ----------------------------------------------------------------

func (e *env) cmdExpiry(c common, warnDays int) error {
	if warnDays < 0 {
		return fmt.Errorf("%w: --warn-days must be >= 0", errUsage)
	}
	if err := e.resolve(&c, false); err != nil {
		return err
	}
	entries, err := os.ReadDir(c.out)
	if err != nil {
		return err
	}
	var names []string
	for _, ent := range entries {
		if ent.Type().IsRegular() && strings.HasSuffix(ent.Name(), ".crt") {
			names = append(names, ent.Name())
		}
	}
	if len(names) == 0 {
		return fmt.Errorf("no .crt files in %s", c.out)
	}
	sort.Strings(names)
	t := now()
	horizon := t.Add(time.Duration(warnDays) * 24 * time.Hour)
	tw := tabwriter.NewWriter(e.stdout, 0, 4, 2, ' ', 0)
	fmt.Fprintln(tw, "FILE\tCN\tNOT AFTER (UTC)\tDAYS LEFT\tSTATUS")
	warn := false
	for _, n := range names {
		cert, err := readCert(filepath.Join(c.out, n))
		if err != nil {
			return err
		}
		days := int(cert.NotAfter.Sub(t).Hours() / 24)
		status := "ok"
		switch {
		case !t.Before(cert.NotAfter):
			status, warn = "EXPIRED", true
		case cert.NotAfter.Before(horizon):
			status, warn = "RENEW SOON", true
		}
		fmt.Fprintf(tw, "%s\t%s\t%s\t%d\t%s\n", n, cert.Subject.CommonName,
			cert.NotAfter.UTC().Format(time.DateTime), days, status)
	}
	if err := tw.Flush(); err != nil {
		return err
	}
	if warn {
		fmt.Fprintln(e.stderr, "note: an expired ca.crt or broker.crt stops every client from connecting, which breaks the shutdown path; renew before it lapses (see slate/README.md)")
		return errExpiryWarn
	}
	return nil
}
