package mqttbus

import (
	"io"
	"log"
	"os"
	"path/filepath"
	"testing"

	"github.com/GhostIA/rack-control/slate/rackctl/internal/orch"
)

type msg struct {
	topic, payload string
	retained       bool
}

func (m msg) Duplicate() bool   { return false }
func (m msg) Qos() byte         { return 1 }
func (m msg) Retained() bool    { return m.retained }
func (m msg) Topic() string     { return m.topic }
func (m msg) MessageID() uint16 { return 0 }
func (m msg) Payload() []byte   { return []byte(m.payload) }
func (m msg) Ack()              {}

func newBus() *Bus { return &Bus{last: map[string]orch.Msg{}, logger: log.New(io.Discard, "", 0)} }

func TestHandleKeepsOnlyValidStates(t *testing.T) {
	b := newBus()
	b.handle(nil, msg{"rack/relay/1/state", "open", false})
	b.handle(nil, msg{"rack/relay/1/state", "OPEN", false})
	b.handle(nil, msg{"rack/pc/x/cmd", "shutdown", false})
	m, ok := b.Get("rack/relay/1/state")
	if !ok || m.Payload != "open" || m.Seq != 1 || b.Mark() != 1 {
		t.Fatalf("%+v %v mark=%d", m, ok, b.Mark())
	}
	if _, ok := b.Get("rack/pc/x/cmd"); ok {
		t.Fatal("command topic stored")
	}
	b.handle(nil, msg{"rack/relay/1/state", "closed", false})
	if m, _ := b.Get("rack/relay/1/state"); m.Payload != "closed" || m.Seq != 2 {
		t.Fatalf("%+v", m)
	}
}

// A retained redelivery (e.g. after a reconnect) must never look fresh.
func TestRetainedNeverFresh(t *testing.T) {
	b := newBus()
	b.handle(nil, msg{"rack/ceph/maintenance/state", "on", true})
	m, ok := b.Get("rack/ceph/maintenance/state")
	if !ok || m.Payload != "on" || m.Seq != 0 {
		t.Fatalf("%+v", m)
	}
	mark := b.Mark()
	b.handle(nil, msg{"rack/ceph/maintenance/state", "on", true}) // reconnect
	if m, _ := b.Get("rack/ceph/maintenance/state"); m.Seq > mark {
		t.Fatalf("retained redelivery counted as fresh: %+v mark=%d", m, mark)
	}
	b.handle(nil, msg{"rack/ceph/maintenance/state", "on", false}) // live reply
	if m, _ := b.Get("rack/ceph/maintenance/state"); m.Seq <= mark {
		t.Fatalf("live message not fresh: %+v", m)
	}
}

func TestReadPassword(t *testing.T) {
	d := t.TempDir()
	p := filepath.Join(d, "pw")
	if err := os.WriteFile(p, []byte("s3cret\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if s, err := ReadPassword(p); err != nil || s != "s3cret" {
		t.Fatalf("%q %v", s, err)
	}
	if err := os.WriteFile(p, []byte("\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := ReadPassword(p); err == nil {
		t.Fatal("empty accepted")
	}
}
