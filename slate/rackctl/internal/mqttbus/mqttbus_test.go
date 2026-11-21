package mqttbus

import (
	"fmt"
	"github.com/hashicorp/go-hclog"
	"os"
	"path/filepath"
	"sync"
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

func newBus() *Bus { return &Bus{last: map[string]orch.Msg{}, logger: hclog.NewNullLogger()} }

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

// Race: a live reply and a retained redelivery (reconnect + resubscribe)
// interleave. The retained copy must not re-label a stale payload with the
// live message's freshness: a waiter that marked before the live "closed"
// arrived must not see the retained "open" as a fresh confirmation.
func TestRetainedAfterLiveCannotFakeFreshness(t *testing.T) {
	b := newBus()
	mark := b.Mark()                                          // orchestrator publishes "open", then waits
	b.handle(nil, msg{"rack/relay/1/state", "closed", false}) // unrelated live update
	b.handle(nil, msg{"rack/relay/1/state", "open", true})    // reconnect replays retained
	m, _ := b.Get("rack/relay/1/state")
	if m.Payload == "open" && m.Seq > mark {
		t.Fatalf("retained %q counted as fresh after mark %d: %+v", "open", mark, m)
	}
}

// Race detector target: paho delivers on its own goroutines while the
// orchestrator polls Get/Mark. Run with -race. Also checks that Mark is
// monotonic and Get never returns a Seq beyond the Mark read after it.
func TestConcurrentHandleGetMark(t *testing.T) {
	b := newBus()
	topics := []string{"rack/relay/0/state", "rack/relay/1/state", "rack/pc/intel-x/status", "rack/sbc4/status"}
	payloads := map[string][2]string{
		"rack/relay/0/state":     {"open", "closed"},
		"rack/relay/1/state":     {"open", "closed"},
		"rack/pc/intel-x/status": {"online", "down"},
		"rack/sbc4/status":       {"online", "offline"},
	}
	var wg sync.WaitGroup
	for w := 0; w < 8; w++ {
		wg.Add(1)
		go func(w int) {
			defer wg.Done()
			for i := 0; i < 2000; i++ {
				tp := topics[(w+i)%len(topics)]
				b.handle(nil, msg{tp, payloads[tp][i%2], (w+i)%3 == 0})
			}
		}(w)
	}
	errs := make(chan string, 16)
	for r := 0; r < 4; r++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			var last uint64
			for i := 0; i < 4000; i++ {
				m, _ := b.Get(topics[i%len(topics)])
				mk := b.Mark()
				if mk < last {
					errs <- fmt.Sprintf("Mark went backwards: %d -> %d", last, mk)
					return
				}
				if m.Seq > mk {
					errs <- fmt.Sprintf("Get Seq %d beyond later Mark %d", m.Seq, mk)
					return
				}
				last = mk
			}
		}()
	}
	wg.Wait()
	close(errs)
	for e := range errs {
		t.Error(e)
	}
}
