// Package ping is a privileged (raw socket, ip4:icmp) ICMP echo pinger.
// rackctl runs as root on the Slate, or with CAP_NET_RAW in a container.
package ping

import (
	"context"
	"errors"
	"fmt"
	"net"
	"os"
	"sync/atomic"
	"syscall"
	"time"

	"golang.org/x/net/icmp"
	"golang.org/x/net/ipv4"
)

const protoICMP = 1

// ICMP implements orch.Pinger. Each Ping opens its own raw socket so calls
// are independent and safe to run concurrently; replies are matched by
// source address, echo ID and sequence number.
type ICMP struct {
	id  int
	seq atomic.Uint32
}

// New returns a pinger whose echo ID is derived from the process ID.
func New() *ICMP {
	return &ICMP{id: os.Getpid() & 0xffff}
}

// Ping sends one echo request to ip and waits up to timeout for the reply.
// It returns (false, nil) when no reply arrived (including "host
// unreachable" from neighbour resolution, which is what a powered-off host on
// the local subnet produces), and an error for local failures (bad address,
// no CAP_NET_RAW, out of descriptors, network down) so the caller never
// mistakes a broken pinger for a dead host.
func (p *ICMP) Ping(ctx context.Context, ip string, timeout time.Duration) (bool, error) {
	dst := net.ParseIP(ip).To4()
	if dst == nil {
		return false, fmt.Errorf("not an IPv4 address: %q", ip)
	}
	conn, err := icmp.ListenPacket("ip4:icmp", "0.0.0.0")
	if err != nil {
		return false, fmt.Errorf("open raw ICMP socket (needs root or CAP_NET_RAW): %w", err)
	}
	defer conn.Close()

	deadline := time.Now().Add(timeout)
	if d, ok := ctx.Deadline(); ok && d.Before(deadline) {
		deadline = d
	}
	if err := conn.SetDeadline(deadline); err != nil {
		return false, err
	}
	stop := context.AfterFunc(ctx, func() { _ = conn.SetDeadline(time.Now()) })
	defer stop()

	seq := int(p.seq.Add(1) & 0xffff)
	msg := icmp.Message{
		Type: ipv4.ICMPTypeEcho,
		Body: &icmp.Echo{ID: p.id, Seq: seq, Data: []byte("rackctl")},
	}
	wb, err := msg.Marshal(nil)
	if err != nil {
		return false, err
	}
	if _, err := conn.WriteTo(wb, &net.IPAddr{IP: dst}); err != nil {
		if isNoReply(err) {
			return false, nil
		}
		return false, fmt.Errorf("send echo: %w", err)
	}
	rb := make([]byte, 1500)
	for {
		n, peer, err := conn.ReadFrom(rb)
		if err != nil {
			if isNoReply(err) || ctx.Err() != nil {
				return false, nil
			}
			return false, fmt.Errorf("read reply: %w", err)
		}
		addr, ok := peer.(*net.IPAddr)
		if !ok || !addr.IP.Equal(dst) {
			continue
		}
		rm, err := icmp.ParseMessage(protoICMP, rb[:n])
		if err != nil || rm.Type != ipv4.ICMPTypeEchoReply {
			continue
		}
		if echo, ok := rm.Body.(*icmp.Echo); ok && echo.ID == p.id && echo.Seq == seq {
			return true, nil
		}
	}
}

// isNoReply: errors that mean "the host did not answer", not a local fault.
func isNoReply(err error) bool {
	var ne net.Error
	if errors.As(err, &ne) && ne.Timeout() {
		return true
	}
	return errors.Is(err, os.ErrDeadlineExceeded) || errors.Is(err, syscall.EHOSTUNREACH) || errors.Is(err, syscall.EHOSTDOWN)
}
