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
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
)

const usage = `usage: rackpki <command> [flags]

commands:
  init-ca                          create ca.crt / ca.key (ECDSA P-256, 10 years)
  server [--ip 192.168.8.1]        create broker.crt / broker.key (IP SAN only, 5 years)
  client <user>... | --all         create <user>.crt / <user>.key (CN = user, 5 years)
  passwd                           write passwd (Mosquitto $7$) from MQTT_<USER>_PASSWORD env vars
  bundle <user> | broker | --all   assemble deploy/<name>/ for one host (all files 0600)
  expiry [--warn-days 180]         list certificate expiry; exit 2 if any is within the window

common flags:
  --out DIR            PKI directory (default: <repo>/slate/pki)
  --force              overwrite existing files (refused otherwise)
  --mosquitto-dir DIR  bundle broker: dir with acl and mosquitto.conf (default: <repo>/slate/mosquitto)

users: orchestrator sbc4 intel-91c45d intel-d1babf intel-177e78 intel-a163f8 intel-1ecbc8
`

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
		os.Exit(1)
	}
	e := &env{stdout: os.Stdout, stderr: os.Stderr, getenv: os.Getenv, cwd: cwd}
	os.Exit(e.run(os.Args[1:]))
}

func (e *env) run(args []string) int {
	err := e.dispatch(args)
	switch {
	case err == nil:
		return 0
	case errors.Is(err, flag.ErrHelp):
		return 0
	case errors.Is(err, errExpiryWarn):
		fmt.Fprintln(e.stderr, "rackpki: WARNING:", err)
		return 2
	case errors.Is(err, errUsage):
		fmt.Fprintln(e.stderr, "rackpki:", err)
		fmt.Fprint(e.stderr, usage)
		return 64
	default:
		fmt.Fprintln(e.stderr, "rackpki:", err)
		return 1
	}
}

func (e *env) dispatch(args []string) error {
	if len(args) == 0 {
		return fmt.Errorf("%w: no command", errUsage)
	}
	cmd, rest := args[0], args[1:]
	switch cmd {
	case "init-ca":
		return e.cmdInitCA(rest)
	case "server":
		return e.cmdServer(rest)
	case "client":
		return e.cmdClient(rest)
	case "passwd":
		return e.cmdPasswd(rest)
	case "bundle":
		return e.cmdBundle(rest)
	case "expiry":
		return e.cmdExpiry(rest)
	case "help", "-h", "--help":
		fmt.Fprint(e.stdout, usage)
		return nil
	default:
		return fmt.Errorf("%w: unknown command %q", errUsage, cmd)
	}
}

// common holds the flags every command shares.
type common struct {
	out          string
	force        bool
	mosquittoDir string
}

func (e *env) newFlags(name string, c *common) *flag.FlagSet {
	fs := flag.NewFlagSet(name, flag.ContinueOnError)
	fs.SetOutput(e.stderr)
	fs.StringVar(&c.out, "out", "", "PKI directory (default <repo>/slate/pki)")
	fs.BoolVar(&c.force, "force", false, "overwrite existing files")
	return fs
}

// parse accepts flags before, between and after positional arguments.
// Everything after a literal "--" is positional.
func parse(fs *flag.FlagSet, args []string) ([]string, error) {
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
			if errors.Is(err, flag.ErrHelp) {
				return nil, err
			}
			return nil, fmt.Errorf("%w: %v", errUsage, err)
		}
		args = fs.Args()
		if len(args) == 0 {
			return append(pos, tail...), nil
		}
		pos = append(pos, args[0])
		args = args[1:]
	}
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
