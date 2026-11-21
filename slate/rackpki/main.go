// Command rackpki generates the rack-control TLS PKI and Mosquitto password
// file on the dev Mac. Everything goes to the gitignored slate/pki/ (or
// --out). It never prints passwords or keys.
//
//	rackpki init-ca                      CA (ECDSA P-256, 10 y)
//	rackpki server [--ip 192.168.8.1]    broker.crt/broker.key (IP SAN only, 5 y)
//	rackpki client <user>...|--all       <user>.crt/<user>.key (CN = user, 5 y)
//	rackpki passwd                       passwd ($7$ PBKDF2-SHA512) from MQTT_*_PASSWORD
//	rackpki bundle <user>|broker|--all   per-host deploy dir under deploy/
//	rackpki expiry [--warn-days 180]     list certificate expiry dates
package main

import (
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"github.com/spf13/cobra"
)

const (
	exitFailure = 1
	exitExpiry  = 2  // expiry: a certificate is expired or close to it
	exitUsage   = 64 // EX_USAGE
)

// errUsage marks errors caused by bad invocation (exit 64).
var errUsage = errors.New("usage error")

// errExpiryWarn is returned by expiry when a certificate is close to expiring.
var errExpiryWarn = errors.New("one or more certificates expire soon or have expired")

type env struct {
	stdout io.Writer
	stderr io.Writer
	getenv func(string) string
	cwd    string
}

func main() {
	cwd, err := os.Getwd()
	if err != nil {
		fmt.Fprintln(os.Stderr, "rackpki:", err)
		os.Exit(exitFailure)
	}
	e := &env{stdout: os.Stdout, stderr: os.Stderr, getenv: os.Getenv, cwd: cwd}
	os.Exit(e.run(os.Args[1:]))
}

// cmdError marks an error returned by a command's own code (as opposed to
// one from cobra's flag or argument checks, which are always usage errors).
type cmdError struct{ err error }

func (c *cmdError) Error() string { return c.err.Error() }
func (c *cmdError) Unwrap() error { return c.err }

// run executes args and maps the outcome to an exit code. Errors are printed
// once, here; usage is printed only for usage errors.
func (e *env) run(args []string) int {
	root := e.newRootCmd()
	if args == nil {
		args = []string{} // cobra would fall back to os.Args
	}
	root.SetArgs(args)
	cmd, err := root.ExecuteC()
	var ce *cmdError
	switch {
	case err == nil:
		return 0
	case errors.Is(err, errExpiryWarn):
		fmt.Fprintln(e.stderr, "rackpki: WARNING:", err)
		return exitExpiry
	case errors.As(err, &ce) && !errors.Is(err, errUsage):
		fmt.Fprintln(e.stderr, "rackpki:", err)
		return exitFailure
	default:
		fmt.Fprintln(e.stderr, "rackpki:", err)
		if cmd != nil {
			fmt.Fprint(e.stderr, cmd.UsageString())
		}
		return exitUsage
	}
}

// common holds the flags the commands share.
type common struct {
	out          string
	force        bool
	mosquittoDir string
}

func (e *env) newRootCmd() *cobra.Command {
	var c common
	root := &cobra.Command{
		Use:   "rackpki",
		Short: "Generate the rack-control TLS PKI and Mosquitto password file",
		Long: `rackpki generates the rack-control TLS PKI and Mosquitto password file on the
dev Mac. Everything goes to the gitignored slate/pki/ (or --out). Existing
files are never overwritten without --force. It never prints passwords or keys.

users: ` + strings.Join(Users, " "),
		SilenceUsage:  true,
		SilenceErrors: true,
		RunE: func(*cobra.Command, []string) error {
			return fmt.Errorf("%w: no command", errUsage)
		},
	}
	root.SetOut(e.stdout)
	root.SetErr(e.stderr)
	root.PersistentFlags().StringVar(&c.out, "out", "", "PKI directory (default <repo>/slate/pki)")

	// do wraps a command body so its errors are told apart from cobra's.
	do := func(fn func(args []string) error) func(*cobra.Command, []string) error {
		return func(_ *cobra.Command, args []string) error {
			if err := fn(args); err != nil {
				return &cmdError{err}
			}
			return nil
		}
	}
	force := func(cmd *cobra.Command) {
		cmd.Flags().BoolVar(&c.force, "force", false, "overwrite existing files (refused otherwise)")
	}

	initCA := &cobra.Command{
		Use:   "init-ca",
		Short: "Create ca.crt / ca.key (ECDSA P-256, 10 years)",
		Args:  cobra.NoArgs,
		RunE:  do(func([]string) error { return e.cmdInitCA(c) }),
	}
	force(initCA)

	var ip string
	server := &cobra.Command{
		Use:   "server",
		Short: "Create broker.crt / broker.key (IP SAN only, 5 years)",
		Args:  cobra.NoArgs,
		RunE:  do(func([]string) error { return e.cmdServer(c, ip) }),
	}
	force(server)
	server.Flags().StringVar(&ip, "ip", DefaultBrokerIP, "broker IP address (the only SAN)")

	var clientAll bool
	client := &cobra.Command{
		Use:       "client <user>... | --all",
		Short:     "Create <user>.crt / <user>.key (CN = user, 5 years)",
		ValidArgs: Users,
		RunE:      do(func(args []string) error { return e.cmdClient(c, args, clientAll) }),
	}
	force(client)
	client.Flags().BoolVar(&clientAll, "all", false, "issue a cert for every user")

	passwd := &cobra.Command{
		Use:   "passwd",
		Short: "Write passwd (Mosquitto $7$) from MQTT_<USER>_PASSWORD env vars",
		Args:  cobra.NoArgs,
		RunE:  do(func([]string) error { return e.cmdPasswd(c) }),
	}
	force(passwd)

	var bundleAll bool
	bundle := &cobra.Command{
		Use:       "bundle <user> | broker | --all",
		Short:     "Assemble deploy/<name>/ for one host (all files 0600)",
		ValidArgs: append([]string{BrokerBundle}, Users...),
		RunE:      do(func(args []string) error { return e.cmdBundle(c, args, bundleAll) }),
	}
	force(bundle)
	bundle.Flags().BoolVar(&bundleAll, "all", false, "bundle the broker and every user")
	bundle.Flags().StringVar(&c.mosquittoDir, "mosquitto-dir", "", "bundle broker: dir with the committed acl and mosquitto.conf (default <repo>/slate/mosquitto)")

	var warnDays int
	expiry := &cobra.Command{
		Use:   "expiry",
		Short: "List certificate expiry; exit 2 if any is within the window",
		Args:  cobra.NoArgs,
		RunE:  do(func([]string) error { return e.cmdExpiry(c, warnDays) }),
	}
	expiry.Flags().IntVar(&warnDays, "warn-days", 180, "warn when a cert expires within this many days")

	root.AddCommand(initCA, server, client, passwd, bundle, expiry)
	return root
}

// resolve fills in the default directories from the repo root.
func (e *env) resolve(c *common, needMosquitto bool) error {
	if c.out != "" && (!needMosquitto || c.mosquittoDir != "") {
		return nil
	}
	root, err := findRepoRoot(e.cwd)
	if err != nil {
		return err
	}
	if c.out == "" {
		c.out = filepath.Join(root, "slate", "pki")
	}
	if needMosquitto && c.mosquittoDir == "" {
		c.mosquittoDir = filepath.Join(root, "slate", "mosquitto")
	}
	return nil
}
