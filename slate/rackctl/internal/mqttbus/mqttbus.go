// Package mqttbus implements orch.Bus on top of paho.mqtt.golang with mTLS
// plus password authentication.
package mqttbus

import (
	"context"
	"crypto/tls"
	"crypto/x509"
	"errors"
	"fmt"
	"log"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"

	mqtt "github.com/eclipse/paho.mqtt.golang"

	"github.com/GhostIA/rack-control/slate/rackctl/internal/config"
	"github.com/GhostIA/rack-control/slate/rackctl/internal/orch"
)

// Bus is a connected MQTT client that tracks the latest valid payload of
// every state/status topic.
type Bus struct {
	client mqtt.Client
	mu     sync.Mutex
	seq    uint64
	last   map[string]orch.Msg
	logger *log.Logger
}

// TLSConfig builds the client TLS config: TLS >= 1.2, the rack CA as the only
// root, the client certificate, ServerName = broker host, verification on.
func TLSConfig(b config.Broker) (*tls.Config, error) {
	caPEM, err := os.ReadFile(b.CA)
	if err != nil {
		return nil, fmt.Errorf("read CA: %w", err)
	}
	pool := x509.NewCertPool()
	if !pool.AppendCertsFromPEM(caPEM) {
		return nil, fmt.Errorf("no certificates in CA file %s", b.CA)
	}
	cert, err := tls.LoadX509KeyPair(b.Cert, b.Key)
	if err != nil {
		return nil, fmt.Errorf("load client cert/key: %w", err)
	}
	return &tls.Config{
		MinVersion:   tls.VersionTLS12,
		RootCAs:      pool,
		Certificates: []tls.Certificate{cert},
		ServerName:   b.Host,
	}, nil
}

// ReadPassword reads the password file, dropping one trailing newline.
func ReadPassword(path string) (string, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return "", fmt.Errorf("read password file: %w", err)
	}
	s := strings.TrimRight(string(b), "\r\n")
	if s == "" {
		return "", errors.New("password file is empty")
	}
	return s, nil
}

// Connect dials the broker, subscribes to the state topics and waits settle
// for retained messages to arrive. clientID is normally the username; the
// read-only `status` command uses a distinct one so it never evicts a
// running orchestration from the broker.
func Connect(ctx context.Context, b config.Broker, clientID string, settle time.Duration, logger *log.Logger) (*Bus, error) {
	for name, v := range map[string]string{"ca": b.CA, "cert": b.Cert, "key": b.Key, "password_file": b.PasswordFile} {
		if v == "" {
			return nil, fmt.Errorf("broker.%s is required", name)
		}
	}
	tc, err := TLSConfig(b)
	if err != nil {
		return nil, err
	}
	pw, err := ReadPassword(b.PasswordFile)
	if err != nil {
		return nil, err
	}
	bus := &Bus{last: map[string]orch.Msg{}, logger: logger}

	subscribed := make(chan error, 1)
	opts := mqtt.NewClientOptions().
		AddBroker("ssl://" + b.Host + ":" + strconv.Itoa(b.Port)).
		SetClientID(clientID).
		SetUsername(b.Username).
		SetPassword(pw).
		SetTLSConfig(tc).
		SetCleanSession(true).
		SetResumeSubs(false).
		SetAutoReconnect(true).
		SetConnectRetry(false).
		SetConnectTimeout(10 * time.Second).
		SetKeepAlive(30 * time.Second).
		SetMaxReconnectInterval(5 * time.Second).
		SetOrderMatters(true).
		SetConnectionLostHandler(func(_ mqtt.Client, err error) {
			logger.Printf("mqtt: connection lost: %v (reconnecting)", err)
		})
	opts.SetOnConnectHandler(func(c mqtt.Client) {
		// Clean session: (re)subscribe on every connect.
		filters := map[string]byte{}
		for _, f := range orch.SubscribeFilters {
			filters[f] = 1
		}
		t := c.SubscribeMultiple(filters, bus.handle)
		go func() {
			var err error
			if !t.WaitTimeout(10 * time.Second) {
				err = errors.New("subscribe timed out")
			} else {
				err = t.Error()
			}
			if err != nil {
				logger.Printf("mqtt: subscribe: %v", err)
			}
			select {
			case subscribed <- err:
			default:
			}
		}()
	})
	bus.client = mqtt.NewClient(opts)
	t := bus.client.Connect()
	if !t.WaitTimeout(15 * time.Second) {
		bus.client.Disconnect(0)
		return nil, fmt.Errorf("connect to %s:%d timed out", b.Host, b.Port)
	}
	if err := t.Error(); err != nil {
		return nil, fmt.Errorf("connect to %s:%d: %w", b.Host, b.Port, err)
	}
	select {
	case err := <-subscribed:
		if err != nil {
			bus.Close()
			return nil, err
		}
	case <-time.After(15 * time.Second):
		bus.Close()
		return nil, errors.New("subscribe timed out")
	case <-ctx.Done():
		bus.Close()
		return nil, ctx.Err()
	}
	// Retained messages arrive right after SUBACK; give them a moment.
	t2 := time.NewTimer(settle)
	defer t2.Stop()
	select {
	case <-t2.C:
	case <-ctx.Done():
		bus.Close()
		return nil, ctx.Err()
	}
	return bus, nil
}

func (b *Bus) handle(_ mqtt.Client, m mqtt.Message) {
	topic, payload := m.Topic(), string(m.Payload())
	if !orch.ValidState(topic, payload) {
		b.logger.Printf("mqtt: ignoring unexpected %s = %q", topic, payload)
		return
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	if m.Retained() {
		// A retained message is history (sent on (re)subscribe), never a
		// fresh reply to a command: store it without advancing Seq, so
		// waits that need "arrived after the command" ignore it. Live
		// messages are forwarded with the retain flag cleared (MQTT 3.1.1).
		prev := b.last[topic]
		b.last[topic] = orch.Msg{Payload: payload, Seq: prev.Seq}
		return
	}
	b.seq++
	b.last[topic] = orch.Msg{Payload: payload, Seq: b.seq}
}

// Publish sends a QoS 1, non-retained message and waits for the PUBACK.
func (b *Bus) Publish(ctx context.Context, topic, payload string) error {
	t := b.client.Publish(topic, 1, false, payload)
	select {
	case <-t.Done():
		return t.Error()
	case <-time.After(10 * time.Second):
		return errors.New("publish not acknowledged within 10s")
	case <-ctx.Done():
		return ctx.Err()
	}
}

// Get implements orch.Bus.
func (b *Bus) Get(topic string) (orch.Msg, bool) {
	b.mu.Lock()
	defer b.mu.Unlock()
	m, ok := b.last[topic]
	return m, ok
}

// Mark implements orch.Bus.
func (b *Bus) Mark() uint64 {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.seq
}

// Close disconnects, letting in-flight messages drain briefly.
func (b *Bus) Close() {
	b.client.Disconnect(250)
}
