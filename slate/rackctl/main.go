// Command rackctl orchestrates the rack: graceful PC shutdown followed by a
// relay power cut, power-up, status, relay map verification and Ceph
// maintenance. Installed as rackctl with busybox-style symlinks
// rack-shutdown (= rackctl shutdown) and rack-up (= rackctl up).
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	mqtt "github.com/eclipse/paho.mqtt.golang"
	"github.com/hashicorp/go-hclog"
	"io"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"syscall"
	"time"

	"github.com/GhostIA/rack-control/slate/rackctl/internal/config"
	"github.com/GhostIA/rack-control/slate/rackctl/internal/mqttbus"
	"github.com/GhostIA/rack-control/slate/rackctl/internal/orch"
	"github.com/GhostIA/rack-control/slate/rackctl/internal/ping"
)

const (
	defaultConfig = "/etc/rack/rack.toml"
	settleTime    = 2 * time.Second
)

// lockPath is a var so tests can point it at a temp dir.
var lockPath = "/var/run/rackctl.lock"

const usage = `usage: rackctl [--config FILE] <command> [flags] [args]

commands:
  shutdown [--force] [--dry-run] [--no-relay] all|NAME...
      Gracefully shut PCs down one at a time in config order; cut each relay
      only after sustained ping loss + cut_delay. A host that does not go down
      stops the sequence and is never cut. Partial shutdowns are checked by the
      Ceph/PVE safety guard unless --force.
  up [--dry-run] all|NAME...
      Close relays in reverse order (5 s stagger), wait for ping, then turn
      Ceph maintenance off once every OSD host is up.
  status
      PCs (ping + retained status), relays, sbc4 and Ceph maintenance state.
  map-verify [--dry-run]
      With the whole rack off and all relays open, discover which relay powers
      which PC and print a [[pc]] TOML snippet.
  ceph-maintenance [--dry-run] on|off
  relay [--force] [--dry-run] open|close CH
      Set one relay channel (0-7) and wait for its state. Opening a channel
      whose configured PC still answers ping is refused unless --force.
      Use it to open unmapped channels (3, 5, 7) before map-verify.

Every command accepts --config FILE (default /etc/rack/rack.toml).
rack-shutdown and rack-up are aliases for "rackctl shutdown" and "rackctl up".
`

// dispatchArgs maps argv (including argv[0]) to subcommand arguments, using
// argv[0] for the busybox-style aliases.
func dispatchArgs(argv []string) []string {
	if len(argv) == 0 {
		return nil
	}
	switch filepath.Base(argv[0]) {
	case "rack-shutdown":
		return append([]string{"shutdown"}, argv[1:]...)
	case "rack-up":
		return append([]string{"up"}, argv[1:]...)
	}
	return argv[1:]
}

// parseInterspersed parses flags anywhere among positional arguments.
// Everything after a literal "--" is positional.
func parseInterspersed(fs *flag.FlagSet, args []string) ([]string, error) {
	var tail []string
	for i, a := range args {
		if a == "--" {
			args, tail = args[:i], args[i+1:]
			break
		}
	}
	var pos []string
	for {
		if err := fs.Parse(args); err != nil {
			return nil, err
		}
		rest := fs.Args()
		if len(rest) == 0 {
			return append(pos, tail...), nil
		}
		pos = append(pos, rest[0])
		args = rest[1:]
	}
}

type cmdOpts struct {
	cmd     string
	config  string
	dry     bool
	force   bool
	noRelay bool
	args    []string
	channel int
}

func parse(args []string, stderr io.Writer) (*cmdOpts, error) {
	g := flag.NewFlagSet("rackctl", flag.ContinueOnError)
	g.SetOutput(stderr)
	g.Usage = func() { fmt.Fprint(stderr, usage) }
	cfgPath := g.String("config", defaultConfig, "config file")
	if err := g.Parse(args); err != nil {
		return nil, err
	}
	rest := g.Args()
	if len(rest) == 0 {
		g.Usage()
		return nil, errors.New("no command given")
	}
	o := &cmdOpts{cmd: rest[0]}
	fs := flag.NewFlagSet(o.cmd, flag.ContinueOnError)
	fs.SetOutput(stderr)
	fs.Usage = func() { fmt.Fprint(stderr, usage) }
	fs.StringVar(&o.config, "config", *cfgPath, "config file")
	switch o.cmd {
	case "shutdown":
		fs.BoolVar(&o.force, "force", false, "skip the partial-shutdown guard and continue if ceph maintenance fails")
		fs.BoolVar(&o.noRelay, "no-relay", false, "never publish relay commands")
		fs.BoolVar(&o.dry, "dry-run", false, "print the sequence without connecting or pinging")
	case "relay":
		fs.BoolVar(&o.force, "force", false, "open even if the channel's PC answers ping")
		fs.BoolVar(&o.dry, "dry-run", false, "print the sequence without connecting or pinging")
	case "up", "map-verify", "ceph-maintenance":
		fs.BoolVar(&o.dry, "dry-run", false, "print the sequence without connecting or pinging")
	case "status":
	case "help", "-h", "--help":
		fmt.Fprint(stderr, usage)
		return nil, flag.ErrHelp
	default:
		fmt.Fprint(stderr, usage)
		return nil, fmt.Errorf("unknown command %q", o.cmd)
	}
	pos, err := parseInterspersed(fs, rest[1:])
	if err != nil {
		return nil, err
	}
	o.args = pos
	switch o.cmd {
	case "shutdown", "up":
		if len(pos) == 0 {
			return nil, fmt.Errorf("%s: name one or more PCs, or 'all'", o.cmd)
		}
	case "status", "map-verify":
		if len(pos) != 0 {
			return nil, fmt.Errorf("%s takes no arguments", o.cmd)
		}
	case "ceph-maintenance":
		if len(pos) != 1 || (pos[0] != "on" && pos[0] != "off") {
			return nil, errors.New("ceph-maintenance: want exactly 'on' or 'off'")
		}
	case "relay":
		if len(pos) != 2 || (pos[0] != "open" && pos[0] != "close") {
			return nil, errors.New("relay: want 'open' or 'close' and a channel 0-7")
		}
		ch, err := strconv.Atoi(pos[1])
		if err != nil || ch < 0 || ch > 7 || strconv.Itoa(ch) != pos[1] {
			return nil, fmt.Errorf("relay: channel %q is not 0-7", pos[1])
		}
		o.channel = ch
	}
	return o, nil
}

// lock takes an exclusive, non-blocking flock so two orchestrations (which
// would also share the MQTT client ID) never run at once.
func lock() (func(), error) {
	if err := os.MkdirAll(filepath.Dir(lockPath), 0o755); err != nil {
		return nil, err
	}
	f, err := os.OpenFile(lockPath, os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return nil, err
	}
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		f.Close()
		return nil, fmt.Errorf("another rackctl is running (%s is locked)", lockPath)
	}
	return func() { f.Close() }, nil
}

func run(argv []string, stdout, stderr io.Writer) int {
	o, err := parse(dispatchArgs(argv), stderr)
	if err != nil {
		if errors.Is(err, flag.ErrHelp) {
			return 0
		}
		fmt.Fprintln(stderr, "rackctl:", err)
		return 2
	}
	cfg, err := config.Load(o.config)
	if err != nil {
		fmt.Fprintf(stderr, "rackctl: config %s: %v\n", o.config, err)
		return 2
	}

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	logger := newLogger(stderr)
	var bus orch.Bus
	var pinger orch.Pinger
	if !o.dry {
		if o.cmd != "status" {
			unlock, err := lock()
			if err != nil {
				fmt.Fprintln(stderr, "rackctl:", err)
				return 1
			}
			defer unlock()
		}
		// status is read-only and may run during an orchestration, so it
		// uses its own client ID instead of evicting the running one.
		clientID := cfg.Broker.Username
		if o.cmd == "status" {
			clientID = fmt.Sprintf("%s-status-%d", cfg.Broker.Username, os.Getpid())
		}
		b, err := mqttbus.Connect(ctx, cfg.Broker, clientID, settleTime, logger)
		if err != nil {
			fmt.Fprintln(stderr, "rackctl: mqtt:", err)
			return 1
		}
		defer b.Close()
		bus = b
		pinger = ping.New()
	}
	or := orch.New(cfg, bus, pinger, orch.RealClock{}, stdout, o.dry)

	switch o.cmd {
	case "shutdown":
		err = or.Shutdown(ctx, o.args, orch.ShutdownOpts{Force: o.force, NoRelay: o.noRelay})
	case "up":
		err = or.Up(ctx, o.args)
	case "status":
		err = or.Status(ctx)
	case "map-verify":
		_, err = or.MapVerify(ctx)
	case "ceph-maintenance":
		err = or.CephMaintenance(ctx, o.args[0])
	case "relay":
		err = or.Relay(ctx, o.args[0], o.channel, o.force)
	}
	if err != nil {
		fmt.Fprintln(stderr, "rackctl:", err)
		return 1
	}
	return 0
}

func main() {
	os.Exit(run(os.Args, os.Stdout, os.Stderr))
}

// newLogger builds the diagnostic logger (stderr; progress output stays on
// stdout). RACKCTL_LOG_LEVEL overrides the level (trace, debug, info, warn,
// error). The paho MQTT library's own warnings and errors go through it too.
func newLogger(w io.Writer) hclog.Logger {
	level := hclog.LevelFromString(os.Getenv("RACKCTL_LOG_LEVEL"))
	if level == hclog.NoLevel {
		level = hclog.Info
	}
	l := hclog.New(&hclog.LoggerOptions{Name: "rackctl", Output: w, Level: level})
	p := l.Named("paho")
	mqtt.ERROR = p.StandardLogger(&hclog.StandardLoggerOptions{ForceLevel: hclog.Error})
	mqtt.CRITICAL = p.StandardLogger(&hclog.StandardLoggerOptions{ForceLevel: hclog.Error})
	mqtt.WARN = p.StandardLogger(&hclog.StandardLoggerOptions{ForceLevel: hclog.Warn})
	return l
}
