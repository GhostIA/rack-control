// Command fakenode is an e2e-only stand-in for the real pc-agent and the
// SBC4 relay sidecar. It speaks the same MQTT contract over mTLS + password.
//
//	-mode pc:   publishes retained "online"; on exactly "shutdown" waits
//	            -shutdown-delay, publishes retained "down" and EXITS, so the
//	            container stops and its IP stops answering ping. -ignore makes
//	            it ignore shutdown. -ceph also serves rack/ceph/maintenance/set.
//	-mode sbc4: publishes retained sbc4 "online" and all 8 relays "closed";
//	            on rack/relay/<ch>/set it checks (by ICMP) whether the PC on
//	            that channel still answers, logs an EVENT line, and publishes
//	            the retained state.
//
// Every observable action is printed as "EVENT <unix-nanos> ..." for the
// e2e test to assert on.
package main

import (
	"context"
	"crypto/tls"
	"crypto/x509"
	"flag"
	"fmt"
	"log"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"

	mqtt "github.com/eclipse/paho.mqtt.golang"

	"github.com/GhostIA/rack-control/slate/rackctl/internal/ping"
)

var (
	mode      = flag.String("mode", "pc", "pc or sbc4")
	broker    = flag.String("broker", "ssl://172.30.0.2:8883", "broker URL")
	host      = flag.String("servername", "172.30.0.2", "TLS server name")
	ca        = flag.String("ca", "/work/certs/ca.crt", "CA file")
	user      = flag.String("user", "", "MQTT username (also client id and cert basename)")
	pwFile    = flag.String("password-file", "", "password file")
	name      = flag.String("name", "", "pc name (mode pc)")
	ignore    = flag.Bool("ignore", false, "ignore shutdown (mode pc)")
	delay     = flag.Duration("shutdown-delay", 3*time.Second, "time between shutdown and exit (mode pc)")
	ceph      = flag.Bool("ceph", false, "serve ceph maintenance (mode pc)")
	relayMap  = flag.String("map", "", "ch=ip,... for reachability checks (mode sbc4)")
	certDir   = flag.String("certs", "/work/certs", "directory with <user>.crt/.key")
	eventLock sync.Mutex
)

func event(format string, a ...any) {
	eventLock.Lock()
	defer eventLock.Unlock()
	fmt.Printf("EVENT %d %s\n", time.Now().UnixNano(), fmt.Sprintf(format, a...))
}

func tlsConfig() *tls.Config {
	pem, err := os.ReadFile(*ca)
	if err != nil {
		log.Fatal(err)
	}
	pool := x509.NewCertPool()
	pool.AppendCertsFromPEM(pem)
	cert, err := tls.LoadX509KeyPair(*certDir+"/"+*user+".crt", *certDir+"/"+*user+".key")
	if err != nil {
		log.Fatal(err)
	}
	return &tls.Config{MinVersion: tls.VersionTLS12, RootCAs: pool, Certificates: []tls.Certificate{cert}, ServerName: *host}
}

func main() {
	flag.Parse()
	pw, err := os.ReadFile(*pwFile)
	if err != nil {
		log.Fatal(err)
	}
	opts := mqtt.NewClientOptions().AddBroker(*broker).SetClientID(*user).SetUsername(*user).
		SetPassword(strings.TrimSpace(string(pw))).SetTLSConfig(tlsConfig()).SetCleanSession(true).
		SetAutoReconnect(true).SetConnectRetry(true).SetConnectRetryInterval(500 * time.Millisecond)
	switch *mode {
	case "pc":
		runPC(opts)
	case "sbc4":
		runSBC4(opts)
	default:
		log.Fatalf("unknown mode %q", *mode)
	}
}

func wait(t mqtt.Token) {
	if !t.WaitTimeout(10*time.Second) || t.Error() != nil {
		log.Printf("mqtt token: %v", t.Error())
	}
}

func runPC(opts *mqtt.ClientOptions) {
	status := "rack/pc/" + *name + "/status"
	opts.SetWill(status, "offline", 1, true)
	done := make(chan struct{})
	var once sync.Once
	opts.SetOnConnectHandler(func(c mqtt.Client) {
		wait(c.Publish(status, 1, true, "online"))
		event("online %s", *name)
		wait(c.Subscribe("rack/pc/"+*name+"/cmd", 1, func(c mqtt.Client, m mqtt.Message) {
			if m.Retained() || string(m.Payload()) != "shutdown" {
				return
			}
			event("shutdown-received %s ignore=%v", *name, *ignore)
			if *ignore {
				return
			}
			once.Do(func() {
				go func() {
					time.Sleep(*delay)
					wait(c.Publish(status, 1, true, "down"))
					event("exiting %s", *name)
					close(done)
				}()
			})
		}))
		if *ceph {
			wait(c.Subscribe("rack/ceph/maintenance/set", 1, func(c mqtt.Client, m mqtt.Message) {
				p := string(m.Payload())
				if m.Retained() || (p != "on" && p != "off") {
					return
				}
				event("ceph %s", p)
				wait(c.Publish("rack/ceph/maintenance/state", 1, true, p))
			}))
		}
	})
	c := mqtt.NewClient(opts)
	wait(c.Connect())
	<-done
	c.Disconnect(250)
	os.Exit(0)
}

func runSBC4(opts *mqtt.ClientOptions) {
	ips := map[int]string{}
	for _, kv := range strings.Split(*relayMap, ",") {
		if k, v, ok := strings.Cut(kv, "="); ok {
			ch, _ := strconv.Atoi(k)
			ips[ch] = v
		}
	}
	pinger := ping.New()
	opts.SetWill("rack/sbc4/status", "offline", 1, true)
	opts.SetOnConnectHandler(func(c mqtt.Client) {
		for ch := 0; ch < 8; ch++ {
			wait(c.Publish(fmt.Sprintf("rack/relay/%d/state", ch), 1, true, "closed"))
		}
		wait(c.Publish("rack/sbc4/status", 1, true, "online"))
		event("sbc4 online")
		wait(c.Subscribe("rack/relay/+/set", 1, func(c mqtt.Client, m mqtt.Message) {
			parts := strings.Split(m.Topic(), "/")
			if m.Retained() || len(parts) != 4 {
				return
			}
			ch, err := strconv.Atoi(parts[2])
			p := string(m.Payload())
			if err != nil || ch < 0 || ch > 7 || (p != "open" && p != "close") {
				return
			}
			reachable := false
			if ip, ok := ips[ch]; ok {
				for i := 0; i < 2 && !reachable; i++ {
					reachable, _ = pinger.Ping(context.Background(), ip, 500*time.Millisecond)
				}
			}
			event("relay %d %s reachable=%v", ch, p, reachable)
			state := map[string]string{"open": "open", "close": "closed"}[p]
			wait(c.Publish(fmt.Sprintf("rack/relay/%d/state", ch), 1, true, state))
		}))
	})
	c := mqtt.NewClient(opts)
	wait(c.Connect())
	select {}
}
