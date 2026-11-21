package ping

import (
	"context"
	"fmt"
	"os"
	"syscall"
	"testing"
	"time"
)

func TestIsNoReply(t *testing.T) {
	for _, err := range []error{os.ErrDeadlineExceeded, syscall.EHOSTUNREACH, fmt.Errorf("sendto: %w", syscall.EHOSTDOWN)} {
		if !isNoReply(err) {
			t.Errorf("%v should mean no reply", err)
		}
	}
	for _, err := range []error{syscall.EPERM, syscall.EMFILE, syscall.ENETUNREACH} {
		if isNoReply(err) {
			t.Errorf("%v is a local failure, not a miss", err)
		}
	}
}

func TestBadAddressIsError(t *testing.T) {
	if ok, err := New().Ping(context.Background(), "not-an-ip", time.Millisecond); ok || err == nil {
		t.Fatalf("got %v %v", ok, err)
	}
}
