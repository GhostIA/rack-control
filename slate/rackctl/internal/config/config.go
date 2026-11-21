// Package config loads and validates /etc/rack/rack.toml.
package config

import (
	"errors"
	"fmt"
	"net"
	"os"
	"sort"
	"strings"
	"time"

	"github.com/BurntSushi/toml"
)

// Roles a PC may carry.
const (
	RoleOSD = "osd"
	RoleMon = "mon"
	RoleVM  = "vm"
)

// Duration is a time.Duration that decodes from a TOML string like "120s".
type Duration struct{ time.Duration }

// UnmarshalText implements encoding.TextUnmarshaler.
func (d *Duration) UnmarshalText(b []byte) error {
	v, err := time.ParseDuration(string(b))
	if err != nil {
		return err
	}
	d.Duration = v
	return nil
}

// Broker is the [broker] table.
type Broker struct {
	Host         string `toml:"host"`
	Port         int    `toml:"port"`
	CA           string `toml:"ca"`
	Cert         string `toml:"cert"`
	Key          string `toml:"key"`
	Username     string `toml:"username"`
	PasswordFile string `toml:"password_file"`
}

// Shutdown is the [shutdown] table.
type Shutdown struct {
	Timeout   Duration `toml:"timeout"`
	CutDelay  Duration `toml:"cut_delay"`
	Order     []string `toml:"order"`
	PVEQuorum int      `toml:"pve_quorum"`
}

// PC is one [[pc]] entry.
type PC struct {
	Name    string   `toml:"name"`
	IP      string   `toml:"ip"`
	Relay   int      `toml:"relay"`
	Roles   []string `toml:"roles"`
	Timeout Duration `toml:"timeout"`
}

// HasRole reports whether the PC carries role r.
func (p PC) HasRole(r string) bool {
	for _, x := range p.Roles {
		if x == r {
			return true
		}
	}
	return false
}

// Config is the whole file.
type Config struct {
	Broker   Broker   `toml:"broker"`
	Shutdown Shutdown `toml:"shutdown"`
	PCs      []PC     `toml:"pc"`
}

// Load reads, defaults and validates a config file.
func Load(path string) (*Config, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	return Parse(string(b))
}

// MinTimeout is the shortest accepted shutdown timeout: sustained ping loss
// needs 5 consecutive 1 s misses, so anything shorter could never succeed.
const MinTimeout = 10 * time.Second

// rawPC mirrors PC with pointers so missing required keys are detectable
// (a missing relay must not silently become relay 0).
type rawPC struct {
	Name    *string   `toml:"name"`
	IP      *string   `toml:"ip"`
	Relay   *int      `toml:"relay"`
	Roles   *[]string `toml:"roles"`
	Timeout Duration  `toml:"timeout"`
}

type rawConfig struct {
	Broker   Broker   `toml:"broker"`
	Shutdown Shutdown `toml:"shutdown"`
	PCs      []rawPC  `toml:"pc"`
}

// Parse decodes, defaults and validates TOML text. Unknown keys are rejected
// so a typo (e.g. "cutdelay") never silently falls back to a default, and
// name/ip/relay/roles are required in every [[pc]].
func Parse(text string) (*Config, error) {
	var raw rawConfig
	md, err := toml.Decode(text, &raw)
	if err != nil {
		return nil, fmt.Errorf("parse config: %w", err)
	}
	if und := md.Undecoded(); len(und) > 0 {
		keys := make([]string, len(und))
		for i, k := range und {
			keys[i] = k.String()
		}
		return nil, fmt.Errorf("unknown config keys: %s", strings.Join(keys, ", "))
	}
	c := Config{Broker: raw.Broker, Shutdown: raw.Shutdown}
	var errs []error
	for i, r := range raw.PCs {
		var missing []string
		if r.Name == nil {
			missing = append(missing, "name")
		}
		if r.IP == nil {
			missing = append(missing, "ip")
		}
		if r.Relay == nil {
			missing = append(missing, "relay")
		}
		if r.Roles == nil {
			missing = append(missing, "roles (use [] for none)")
		}
		if len(missing) > 0 {
			errs = append(errs, fmt.Errorf("pc[%d]: missing required %s", i, strings.Join(missing, ", ")))
			continue
		}
		c.PCs = append(c.PCs, PC{Name: *r.Name, IP: *r.IP, Relay: *r.Relay, Roles: *r.Roles, Timeout: r.Timeout})
	}
	if len(errs) > 0 {
		return nil, errors.Join(errs...)
	}
	c.applyDefaults(md)
	if err := c.Validate(); err != nil {
		return nil, err
	}
	return &c, nil
}

// applyDefaults fills only keys that are absent, so explicit values such
// as cut_delay = "0s" are honoured.
func (c *Config) applyDefaults(md toml.MetaData) {
	if !md.IsDefined("broker", "port") {
		c.Broker.Port = 8883
	}
	if !md.IsDefined("broker", "username") {
		c.Broker.Username = "orchestrator"
	}
	if !md.IsDefined("shutdown", "timeout") {
		c.Shutdown.Timeout.Duration = 120 * time.Second
	}
	if !md.IsDefined("shutdown", "cut_delay") {
		c.Shutdown.CutDelay.Duration = 30 * time.Second
	}
	if !md.IsDefined("shutdown", "pve_quorum") {
		c.Shutdown.PVEQuorum = min(3, len(c.PCs))
	}
}

func validName(s string) bool {
	if s == "" || len(s) > 64 {
		return false
	}
	for _, r := range s {
		ok := (r >= 'a' && r <= 'z') || (r >= 'A' && r <= 'Z') || (r >= '0' && r <= '9') || r == '-' || r == '_' || r == '.'
		if !ok {
			return false
		}
	}
	return true
}

// Validate checks every invariant the orchestrator relies on.
func (c *Config) Validate() error {
	var errs []error
	add := func(f string, a ...any) { errs = append(errs, fmt.Errorf(f, a...)) }

	if c.Broker.Host == "" {
		add("broker.host is required")
	}
	if c.Broker.Port < 1 || c.Broker.Port > 65535 {
		add("broker.port %d out of range", c.Broker.Port)
	}
	if c.Shutdown.Timeout.Duration < MinTimeout {
		add("shutdown.timeout %s is below the minimum %s", c.Shutdown.Timeout.Duration, MinTimeout)
	}
	if c.Shutdown.CutDelay.Duration < 0 {
		add("shutdown.cut_delay must not be negative")
	}
	if c.Broker.Username == "" {
		add("broker.username must not be empty")
	}
	if len(c.PCs) == 0 {
		add("at least one [[pc]] is required")
	}
	if c.Shutdown.PVEQuorum < 1 || c.Shutdown.PVEQuorum > len(c.PCs) {
		add("shutdown.pve_quorum %d must be between 1 and the number of PCs (%d)", c.Shutdown.PVEQuorum, len(c.PCs))
	}

	names := map[string]bool{}
	ips := map[string]string{}
	relays := map[int]string{}
	for i, p := range c.PCs {
		if !validName(p.Name) {
			add("pc[%d]: invalid name %q (use letters, digits, '-', '_', '.')", i, p.Name)
		}
		if names[p.Name] {
			add("pc %q: duplicate name", p.Name)
		}
		names[p.Name] = true
		ip := net.ParseIP(p.IP)
		if ip == nil || ip.To4() == nil {
			add("pc %q: ip %q is not an IPv4 address", p.Name, p.IP)
		} else {
			key := ip.To4().String()
			if other, ok := ips[key]; ok {
				add("pc %q: ip %s already used by %q", p.Name, key, other)
			}
			ips[key] = p.Name
		}
		if p.Relay < 0 || p.Relay > 7 {
			add("pc %q: relay %d out of range 0-7", p.Name, p.Relay)
		} else if other, ok := relays[p.Relay]; ok {
			add("pc %q: relay %d already used by %q", p.Name, p.Relay, other)
		} else {
			relays[p.Relay] = p.Name
		}
		seen := map[string]bool{}
		for _, r := range p.Roles {
			switch r {
			case RoleOSD, RoleMon, RoleVM:
			default:
				add("pc %q: unknown role %q (allowed: osd, mon, vm)", p.Name, r)
			}
			if seen[r] {
				add("pc %q: duplicate role %q", p.Name, r)
			}
			seen[r] = true
		}
		if p.Timeout.Duration != 0 && p.Timeout.Duration < MinTimeout {
			add("pc %q: timeout %s is below the minimum %s", p.Name, p.Timeout.Duration, MinTimeout)
		}
	}

	inOrder := map[string]int{}
	for _, n := range c.Shutdown.Order {
		inOrder[n]++
		if !names[n] {
			add("shutdown.order: %q is not a configured pc", n)
		}
	}
	for n, k := range inOrder {
		if k > 1 {
			add("shutdown.order: %q listed %d times", n, k)
		}
	}
	var missing []string
	for n := range names {
		if inOrder[n] == 0 {
			missing = append(missing, n)
		}
	}
	sort.Strings(missing)
	for _, n := range missing {
		add("shutdown.order: pc %q is missing", n)
	}
	return errors.Join(errs...)
}

// Ordered returns the PCs in shutdown order.
func (c *Config) Ordered() []PC {
	out := make([]PC, 0, len(c.Shutdown.Order))
	for _, n := range c.Shutdown.Order {
		p, _ := c.PC(n)
		out = append(out, p)
	}
	return out
}

// PC looks a PC up by name.
func (c *Config) PC(name string) (PC, bool) {
	for _, p := range c.PCs {
		if p.Name == name {
			return p, true
		}
	}
	return PC{}, false
}

// TimeoutFor returns the per-PC timeout or the global default.
func (c *Config) TimeoutFor(p PC) time.Duration {
	if p.Timeout.Duration > 0 {
		return p.Timeout.Duration
	}
	return c.Shutdown.Timeout.Duration
}
