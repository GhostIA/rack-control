//go:build e2e

// Command probe runs INSIDE the e2e compose network (it is cross-compiled for
// linux by e2e_test.go) and checks the real Mosquitto broker's TLS, password
// and ACL behaviour. It exits non-zero if any check fails. The passwords
// it uses are fake test values from the environment.
package main

import (
	"crypto/tls"
	"crypto/x509"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"time"

	mqtt "github.com/eclipse/paho.mqtt.golang"
)

var (
	brokerAddr = os.Getenv("BROKER_ADDR") // e.g. 172.31.250.10:8883
	brokerName = os.Getenv("BROKER_NAME") // the broker cert's IP SAN
	certDir    = os.Getenv("CERT_DIR")    // ca.crt, <user>.crt/.key, rogue/
)

const (
	quiet   = 2 * time.Second // how long "must not receive" waits
	timeout = 5 * time.Second
)

type client struct {
	user string
	c    mqtt.Client
	got  chan string // "topic payload"
}

type opts struct {
	user     string // MQTT username ("" = anonymous)
	password string
	certUser string // client cert to present ("" = none)
	certDir  string // where certUser's cert lives (default certDir)
	clientID string
}

func password(user string) string {
	return os.Getenv(envVar(user))
}

func envVar(user string) string {
	b := []byte("MQTT_")
	for _, r := range user {
		switch {
		case r == '-':
			b = append(b, '_')
		case r >= 'a' && r <= 'z':
			b = append(b, byte(r-'a'+'A'))
		default:
			b = append(b, byte(r))
		}
	}
	return string(append(b, "_PASSWORD"...))
}

func tlsConfig(o opts) (*tls.Config, error) {
	caPEM, err := os.ReadFile(filepath.Join(certDir, "ca.crt"))
	if err != nil {
		return nil, err
	}
	pool := x509.NewCertPool()
	if !pool.AppendCertsFromPEM(caPEM) {
		return nil, errors.New("bad ca.crt")
	}
	cfg := &tls.Config{RootCAs: pool, ServerName: brokerName, MinVersion: tls.VersionTLS12}
	if o.certUser != "" {
		dir := o.certDir
		if dir == "" {
			dir = certDir
		}
		kp, err := tls.LoadX509KeyPair(filepath.Join(dir, o.certUser+".crt"), filepath.Join(dir, o.certUser+".key"))
		if err != nil {
			return nil, err
		}
		cfg.Certificates = []tls.Certificate{kp}
	}
	return cfg, nil
}

func connect(o opts) (*client, error) {
	cfg, err := tlsConfig(o)
	if err != nil {
		return nil, err
	}
	id := o.clientID
	if id == "" {
		id = o.user
	}
	if id == "" {
		id = "anon-probe"
	}
	cl := &client{user: o.user, got: make(chan string, 16)}
	mo := mqtt.NewClientOptions().
		AddBroker("ssl://" + brokerAddr).
		SetClientID(id).
		SetUsername(o.user).
		SetPassword(o.password).
		SetTLSConfig(cfg).
		SetCleanSession(true).
		SetAutoReconnect(false).
		SetConnectRetry(false).
		SetConnectTimeout(timeout).
		SetDefaultPublishHandler(func(_ mqtt.Client, m mqtt.Message) {
			cl.got <- m.Topic() + " " + string(m.Payload())
		})
	cl.c = mqtt.NewClient(mo)
	tok := cl.c.Connect()
	if !tok.WaitTimeout(timeout + time.Second) {
		return nil, errors.New("connect timed out")
	}
	if err := tok.Error(); err != nil {
		return nil, err
	}
	return cl, nil
}

func full(user string) opts {
	return opts{user: user, password: password(user), certUser: user}
}

func (cl *client) sub(topic string) error {
	tok := cl.c.Subscribe(topic, 1, nil)
	if !tok.WaitTimeout(timeout) {
		return fmt.Errorf("%s: subscribe %s timed out", cl.user, topic)
	}
	return tok.Error()
}

func (cl *client) pub(topic, payload string) error {
	tok := cl.c.Publish(topic, 1, false, payload)
	if !tok.WaitTimeout(timeout) {
		return fmt.Errorf("%s: publish %s timed out", cl.user, topic)
	}
	return tok.Error()
}

func (cl *client) expect(want string) error {
	select {
	case m := <-cl.got:
		if m != want {
			return fmt.Errorf("%s got %q, want %q", cl.user, m, want)
		}
		return nil
	case <-time.After(timeout):
		return fmt.Errorf("%s did not receive %q", cl.user, want)
	}
}

func (cl *client) expectNothing() error {
	select {
	case m := <-cl.got:
		return fmt.Errorf("ACL breach: %s received %q", cl.user, m)
	case <-time.After(quiet):
		return nil
	}
}

var failures int

func check(name string, err error) {
	if err != nil {
		failures++
		fmt.Printf("FAIL %s: %v\n", name, err)
		return
	}
	fmt.Printf("ok   %s\n", name)
}

func refused(o opts) error {
	cl, err := connect(o)
	if err == nil {
		cl.c.Disconnect(100)
		return errors.New("connection was accepted")
	}
	fmt.Printf("     (refused: %v)\n", err)
	return nil
}

func main() {
	if brokerAddr == "" || brokerName == "" || certDir == "" {
		fmt.Println("BROKER_ADDR, BROKER_NAME and CERT_DIR must be set")
		os.Exit(2)
	}

	// Wait for the broker: a valid client must eventually connect.
	var orch *client
	var err error
	deadline := time.Now().Add(30 * time.Second)
	for {
		orch, err = connect(full("orchestrator"))
		if err == nil || time.Now().After(deadline) {
			break
		}
		time.Sleep(500 * time.Millisecond)
	}
	check("mTLS + correct password connects (orchestrator)", err)
	if err != nil {
		os.Exit(1)
	}

	// --- authentication ---
	check("correct cert + wrong password is refused",
		refused(opts{user: "orchestrator", password: "fake-wrong-password", certUser: "orchestrator"}))
	check("correct password + no client cert is refused",
		refused(opts{user: "orchestrator", password: password("orchestrator")}))
	check("anonymous (valid cert, no username) is refused",
		refused(opts{certUser: "orchestrator", clientID: "anon-probe"}))
	check("anonymous (no cert, no username) is refused",
		refused(opts{clientID: "anon-probe-2"}))
	check("cert from a rogue CA + correct password is refused",
		refused(opts{user: "orchestrator", password: password("orchestrator"), certUser: "orchestrator",
			certDir: filepath.Join(certDir, "rogue"), clientID: "rogue-probe"}))
	check("unknown user is refused",
		refused(opts{user: "intel-ffffff", password: "fake-test-pw", certUser: "orchestrator", clientID: "unknown-probe"}))

	// --- ACL ---
	pc1, err := connect(full("intel-91c45d"))
	check("intel-91c45d connects", err)
	pc2, err2 := connect(full("intel-d1babf"))
	check("intel-d1babf connects", err2)
	sbc, err3 := connect(full("sbc4"))
	check("sbc4 connects", err3)
	if err != nil || err2 != nil || err3 != nil {
		os.Exit(1)
	}

	check("intel-91c45d subscribes to its own cmd", pc1.sub("rack/pc/intel-91c45d/cmd"))
	check("intel-d1babf subscribes to its own cmd", pc2.sub("rack/pc/intel-d1babf/cmd"))
	check("sbc4 subscribes to rack/relay/+/set", sbc.sub("rack/relay/+/set"))
	// Mosquitto 2.x accepts every SUBSCRIBE and checks read access on each
	// delivered message, so the forbidden subscription must just never
	// receive anything.
	check("intel-91c45d subscribes to intel-d1babf/cmd (accepted, must deliver nothing)", pc1.sub("rack/pc/intel-d1babf/cmd"))

	// orchestrator -> PC: allowed, and only that PC receives it.
	check("orchestrator publishes rack/pc/intel-d1babf/cmd", orch.pub("rack/pc/intel-d1babf/cmd", "shutdown"))
	check("intel-d1babf receives its shutdown", pc2.expect("rack/pc/intel-d1babf/cmd shutdown"))
	check("intel-91c45d cannot read intel-d1babf/cmd", pc1.expectNothing())

	check("orchestrator publishes rack/pc/intel-91c45d/cmd", orch.pub("rack/pc/intel-91c45d/cmd", "shutdown"))
	check("intel-91c45d receives its shutdown", pc1.expect("rack/pc/intel-91c45d/cmd shutdown"))

	// PC -> relay: denied (publish is dropped by the broker).
	_ = pc1.pub("rack/relay/0/set", "open")
	check("intel-91c45d cannot write rack/relay/0/set", sbc.expectNothing())
	// Positive control: orchestrator -> relay reaches sbc4.
	check("orchestrator publishes rack/relay/0/set", orch.pub("rack/relay/0/set", "open"))
	check("sbc4 receives rack/relay/0/set", sbc.expect("rack/relay/0/set open"))

	// sbc4 -> PC cmd: denied.
	_ = sbc.pub("rack/pc/intel-91c45d/cmd", "shutdown")
	check("sbc4 cannot write rack/pc/intel-91c45d/cmd", pc1.expectNothing())

	// PC -> another PC's cmd: denied.
	_ = pc1.pub("rack/pc/intel-d1babf/cmd", "shutdown")
	check("intel-91c45d cannot write intel-d1babf/cmd", pc2.expectNothing())

	// Client-ID takeover: with use_username_as_clientid the broker replaces
	// "orchestrator" with the username, so the real orchestrator session
	// must survive.
	hijack, err := connect(opts{user: "intel-177e78", password: password("intel-177e78"), certUser: "intel-177e78", clientID: "orchestrator"})
	check("intel-177e78 connects (client ID \"orchestrator\" requested)", err)
	time.Sleep(time.Second)
	check("orchestrator session survives a client-ID takeover attempt", func() error {
		if !orch.c.IsConnectionOpen() {
			return errors.New("orchestrator was disconnected")
		}
		return orch.pub("rack/pc/intel-91c45d/cmd", "shutdown")
	}())
	check("intel-91c45d still receives from orchestrator", pc1.expect("rack/pc/intel-91c45d/cmd shutdown"))
	if hijack != nil {
		hijack.c.Disconnect(100)
	}

	for _, c := range []*client{orch, pc1, pc2, sbc} {
		c.c.Disconnect(100)
	}
	if failures > 0 {
		fmt.Printf("%d check(s) FAILED\n", failures)
		os.Exit(1)
	}
	fmt.Println("all broker checks passed")
}
