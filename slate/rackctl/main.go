// Command rackctl orchestrates the rack: graceful PC shutdown followed by a
// relay power cut, power-up, status, relay map verification and Ceph
// maintenance. Installed as rackctl with busybox-style symlinks
// rack-shutdown (= rackctl shutdown) and rack-up (= rackctl up).
package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"syscall"
	"time"

	mqtt "github.com/eclipse/paho.mqtt.golang"
	"github.com/hashicorp/go-hclog"
	"github.com/spf13/cobra"
	"github.com/spf13/viper"

	"github.com/GhostIA/rack-control/slate/rackctl/internal/config"
	"github.com/GhostIA/rack-control/slate/rackctl/internal/mqttbus"
	"github.com/GhostIA/rack-control/slate/rackctl/internal/orch"
	"github.com/GhostIA/rack-control/slate/rackctl/internal/ping"
)

const (
	defaultConfig  = "/etc/rack/rack.toml"
	settleTime     = 2 * time.Second
	minBootTimeout = 30 * time.Second

	exitFailure = 1 // runtime failure or refusal
	exitUsage   = 2 // bad invocation or config
)

// lockPath is a var so tests can point it at a temp dir.
var lockPath = "/var/run/rackctl.lock"

// aliases maps busybox-style argv[0] names to the subcommand they run.
var aliases = map[string]string{
	"rack-shutdown": "shutdown",
	"rack-up":       "up",
}

// dispatchArgs maps argv (including argv[0]) to the arguments for the root
// command, prepending the subcommand when invoked through an alias. The
// result is never nil: cobra would fall back to os.Args for a nil slice.
func dispatchArgs(argv []string) []string {
	if len(argv) == 0 {
		return []string{}
	}
	if sub, ok := aliases[filepath.Base(argv[0])]; ok {
		return append([]string{sub}, argv[1:]...)
	}
	return append([]string{}, argv[1:]...)
}

// invocation is one fully parsed and validated command line.
type invocation struct {
	cmd         string
	config      string
	dry         bool
	force       bool
	noRelay     bool
	args        []string
	channel     int           // relay
	channels    []int         // map-verify; nil = all 0-7
	bootTimeout time.Duration // map-verify
}

// execFunc carries out an invocation. Tests substitute a recorder.
type execFunc func(ctx context.Context, inv *invocation, stdout, stderr io.Writer) error

// exitError carries the exit code of an error raised after the command line
// was accepted. Any other error from cobra is a usage error.
type exitError struct {
	code int
	err  error
}

func (e *exitError) Error() string { return e.err.Error() }
func (e *exitError) Unwrap() error { return e.err }

func failure(err error) error { return &exitError{code: exitFailure, err: err} }

// defaultMapUpTimeout is orch's own default, so the help text matches it.
var defaultMapUpTimeout = orch.New(nil, nil, nil, nil, nil, false).MapUpTimeout

func newRootCmd(stdout, stderr io.Writer, exec execFunc) *cobra.Command {
	v := viper.New()
	root := &cobra.Command{
		Use:   "rackctl",
		Short: "Rack orchestrator: graceful shutdown, power-up and relay control",
		Long: `rackctl shuts the rack's PCs down gracefully over MQTT, cuts each PC's relay
only after it has stopped answering ping, and brings the rack back up.

rack-shutdown and rack-up are aliases for "rackctl shutdown" and "rackctl up".
Exit codes: 0 ok, 1 failure or refusal, 2 usage or config error.`,
		SilenceUsage:  true,
		SilenceErrors: true,
		RunE: func(*cobra.Command, []string) error {
			return errors.New("no command given")
		},
	}
	root.SetOut(stdout)
	root.SetErr(stderr)
	root.PersistentFlags().String("config", defaultConfig, "config file")
	if err := v.BindPFlag("config", root.PersistentFlags().Lookup("config")); err != nil {
		panic(err) // the flag was registered just above
	}

	// runner fills in the shared fields and hands the invocation over.
	runner := func(inv *invocation) func(*cobra.Command, []string) error {
		return func(cmd *cobra.Command, args []string) error {
			inv.config = v.GetString("config")
			inv.args = args
			return exec(cmd.Context(), inv, stdout, stderr)
		}
	}
	dryRun := func(cmd *cobra.Command, p *bool) {
		cmd.Flags().BoolVar(p, "dry-run", false, "print the sequence without connecting or pinging")
	}
	targets := func(cmd *cobra.Command, args []string) error {
		if len(args) == 0 {
			return fmt.Errorf("%s: name one or more PCs, or 'all'", cmd.Name())
		}
		return nil
	}

	sd := &invocation{cmd: "shutdown"}
	shutdown := &cobra.Command{
		Use:   "shutdown [flags] all|NAME...",
		Short: "Gracefully shut PCs down, then cut their relays",
		Long: `Gracefully shut PCs down one at a time in config order; cut each relay only
after sustained ping loss + cut_delay. A host that does not go down stops the
sequence and is never cut. Partial shutdowns are checked by the Ceph/PVE
safety guard unless --force.`,
		Args: targets,
		RunE: runner(sd),
	}
	dryRun(shutdown, &sd.dry)
	shutdown.Flags().BoolVar(&sd.force, "force", false, "skip the partial-shutdown guard and continue if ceph maintenance fails")
	shutdown.Flags().BoolVar(&sd.noRelay, "no-relay", false, "never publish relay commands")

	upInv := &invocation{cmd: "up"}
	up := &cobra.Command{
		Use:   "up [flags] all|NAME...",
		Short: "Power PCs back on",
		Long: `Close relays in reverse order (5 s stagger), wait for ping, then turn Ceph
maintenance off once every OSD host is up.`,
		Args: targets,
		RunE: runner(upInv),
	}
	dryRun(up, &upInv.dry)

	status := &cobra.Command{
		Use:   "status",
		Short: "Show PCs, relays, sbc4 and Ceph maintenance state",
		Long:  "PCs (ping + retained status), relays, sbc4 and Ceph maintenance state.",
		Args:  cobra.NoArgs,
		RunE:  runner(&invocation{cmd: "status"}),
	}

	mv := &invocation{cmd: "map-verify"}
	mapVerify := &cobra.Command{
		Use:   "map-verify [flags]",
		Short: "Discover which relay powers which PC",
		Long: `With the whole rack off and all relays open, close one relay channel at a
time, see which PC boots, shut it down gracefully and open the relay again.
Prints the mapping and a [[pc]] TOML snippet. --channels limits the test to
some channels (e.g. the unmapped ones).`,
		Args: cobra.NoArgs,
		PreRunE: func(cmd *cobra.Command, _ []string) error {
			ch, err := normalizeChannels(mv.channels)
			if err != nil {
				return err
			}
			mv.channels = ch
			if mv.bootTimeout < minBootTimeout {
				return fmt.Errorf("--boot-timeout %s is below the minimum %s", mv.bootTimeout, minBootTimeout)
			}
			return nil
		},
		RunE: runner(mv),
	}
	dryRun(mapVerify, &mv.dry)
	mapVerify.Flags().IntSliceVar(&mv.channels, "channels", nil, "relay channels to test, e.g. 3,5,7 (default 0-7)")
	mapVerify.Flags().DurationVar(&mv.bootTimeout, "boot-timeout", defaultMapUpTimeout, "per channel: relay close -> any PC answers ping (min 30s)")

	cm := &invocation{cmd: "ceph-maintenance"}
	ceph := &cobra.Command{
		Use:       "ceph-maintenance [flags] on|off",
		Short:     "Set Ceph maintenance mode on or off",
		ValidArgs: []string{"on", "off"},
		Args:      cobra.MatchAll(cobra.ExactArgs(1), cobra.OnlyValidArgs),
		RunE:      runner(cm),
	}
	dryRun(ceph, &cm.dry)

	rl := &invocation{cmd: "relay"}
	relay := &cobra.Command{
		Use:   "relay [flags] open|close CH",
		Short: "Open or close one relay channel (0-7)",
		Long: `Set one relay channel (0-7) and wait for its state. Opening a channel whose
configured PC still answers ping is refused unless --force. Use it to open
unmapped channels (3, 5, 7) before map-verify.`,
		Args: cobra.MatchAll(cobra.ExactArgs(2), func(_ *cobra.Command, args []string) error {
			if args[0] != "open" && args[0] != "close" {
				return fmt.Errorf("relay: want 'open' or 'close', got %q", args[0])
			}
			ch, err := parseChannel(args[1])
			if err != nil {
				return fmt.Errorf("relay: %w", err)
			}
			rl.channel = ch
			return nil
		}),
		ValidArgsFunction: func(_ *cobra.Command, args []string, _ string) ([]string, cobra.ShellCompDirective) {
			switch len(args) {
			case 0:
				return []string{"open", "close"}, cobra.ShellCompDirectiveNoFileComp
			case 1:
				return []string{"0", "1", "2", "3", "4", "5", "6", "7"}, cobra.ShellCompDirectiveNoFileComp
			}
			return nil, cobra.ShellCompDirectiveNoFileComp
		},
		RunE: runner(rl),
	}
	dryRun(relay, &rl.dry)
	relay.Flags().BoolVar(&rl.force, "force", false, "open even if the channel's PC answers ping")

	root.AddCommand(shutdown, up, status, mapVerify, ceph, relay)
	return root
}

// parseChannel accepts exactly "0".."7" (no sign, no leading zero).
func parseChannel(s string) (int, error) {
	ch, err := strconv.Atoi(s)
	if err != nil || ch < 0 || ch > 7 || strconv.Itoa(ch) != s {
		return 0, fmt.Errorf("channel %q is not 0-7", s)
	}
	return ch, nil
}

// normalizeChannels validates --channels and returns them de-duplicated in
// ascending order; nil means every channel.
func normalizeChannels(in []int) ([]int, error) {
	if len(in) == 0 {
		return nil, nil
	}
	var seen [8]bool
	for _, c := range in {
		if c < 0 || c > 7 {
			return nil, fmt.Errorf("--channels: %d is not a relay channel 0-7", c)
		}
		seen[c] = true
	}
	var out []int
	for c, ok := range seen {
		if ok {
			out = append(out, c)
		}
	}
	return out, nil
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

// execute is the production execFunc: load the config, take the lock,
// connect, and run the procedure.
func execute(ctx context.Context, inv *invocation, stdout, stderr io.Writer) error {
	cfg, err := config.Load(inv.config)
	if err != nil {
		return &exitError{code: exitUsage, err: fmt.Errorf("config %s: %w", inv.config, err)}
	}

	logger := newLogger(stderr)
	var bus orch.Bus
	var pinger orch.Pinger
	if !inv.dry {
		if inv.cmd != "status" {
			unlock, err := lock()
			if err != nil {
				return failure(err)
			}
			defer unlock()
		}
		// status is read-only and may run during an orchestration, so it
		// uses its own client ID instead of evicting the running one.
		clientID := cfg.Broker.Username
		if inv.cmd == "status" {
			clientID = fmt.Sprintf("%s-status-%d", cfg.Broker.Username, os.Getpid())
		}
		b, err := mqttbus.Connect(ctx, cfg.Broker, clientID, settleTime, logger)
		if err != nil {
			return failure(fmt.Errorf("mqtt: %w", err))
		}
		defer b.Close()
		bus = b
		pinger = ping.New()
	}
	or := orch.New(cfg, bus, pinger, orch.RealClock{}, stdout, inv.dry)
	or.MapChannels = inv.channels
	if inv.bootTimeout > 0 {
		or.MapUpTimeout = inv.bootTimeout
	}

	switch inv.cmd {
	case "shutdown":
		err = or.Shutdown(ctx, inv.args, orch.ShutdownOpts{Force: inv.force, NoRelay: inv.noRelay})
	case "up":
		err = or.Up(ctx, inv.args)
	case "status":
		err = or.Status(ctx)
	case "map-verify":
		_, err = or.MapVerify(ctx)
	case "ceph-maintenance":
		err = or.CephMaintenance(ctx, inv.args[0])
	case "relay":
		err = or.Relay(ctx, inv.args[0], inv.channel, inv.force)
	default:
		err = fmt.Errorf("no handler for command %q", inv.cmd)
	}
	if err != nil {
		return failure(err)
	}
	return nil
}

func run(argv []string, stdout, stderr io.Writer) int {
	return runWith(argv, stdout, stderr, execute)
}

// runWith parses argv, runs exec and maps the outcome to an exit code. Errors
// are printed once, here; usage is printed only for usage errors.
func runWith(argv []string, stdout, stderr io.Writer, exec execFunc) int {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	root := newRootCmd(stdout, stderr, exec)
	root.SetArgs(dispatchArgs(argv))
	cmd, err := root.ExecuteContextC(ctx)
	if err == nil {
		return 0
	}
	fmt.Fprintln(stderr, "rackctl:", err)
	var ee *exitError
	if errors.As(err, &ee) {
		return ee.code
	}
	if cmd != nil {
		fmt.Fprint(stderr, cmd.UsageString())
	}
	return exitUsage
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
