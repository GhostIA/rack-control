// Package config loads and validates /etc/rack/rack.toml (TOML, read with
// viper and decoded strictly with mapstructure).
package config

import (
	"errors"
	"fmt"
	"net"
	"os"
	"reflect"
	"sort"
	"strings"
	"time"

	"github.com/go-viper/mapstructure/v2"
	"github.com/pelletier/go-toml/v2"
	"github.com/spf13/viper"
)

// Roles a PC may carry.
const (
	RoleOSD = "osd"
	RoleMon = "mon"
	RoleVM  = "vm"
)

// Broker is the [broker] table.
type Broker struct {
	Host         string `mapstructure:"host"`
	Port         int    `mapstructure:"port"`
	CA           string `mapstructure:"ca"`
	Cert         string `mapstructure:"cert"`
	Key          string `mapstructure:"key"`
	Username     string `mapstructure:"username"`
	PasswordFile string `mapstructure:"password_file"`
}

// Shutdown is the [shutdown] table.
type Shutdown struct {
	Timeout   time.Duration `mapstructure:"timeout"`
	CutDelay  time.Duration `mapstructure:"cut_delay"`
	Order     []string      `mapstructure:"order"`
	PVEQuorum int           `mapstructure:"pve_quorum"`
}

// PC is one [[pc]] entry.
type PC struct {
	Name    string        `mapstructure:"name"`
	IP      string        `mapstructure:"ip"`
	Relay   int           `mapstructure:"relay"`
	Roles   []string      `mapstructure:"roles"`
	Timeout time.Duration `mapstructure:"timeout"`
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
	Broker   Broker   `mapstructure:"broker"`
	Shutdown Shutdown `mapstructure:"shutdown"`
	PCs      []PC     `mapstructure:"pc"`
}

// Load reads, defaults and validates a config file.
// The file is read once and always parsed as TOML, whatever it is called.
func Load(path string) (*Config, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	return Parse(string(b))
}

// checkKeys rejects keys viper would otherwise reinterpret before the strict
// decode sees them: viper lowercases every key (so "host" and "HOST" would
// silently merge) and splits keys on "." (so a quoted "shutdown.cut_delay"
// would land in [shutdown]). Every rack.toml key is lowercase without dots.
func checkKeys(path string, node any) error {
	switch n := node.(type) {
	case map[string]any:
		keys := make([]string, 0, len(n))
		for k := range n {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		for _, k := range keys {
			if k != strings.ToLower(k) || strings.Contains(k, ".") {
				return fmt.Errorf("invalid config key %q (keys are lowercase, without dots)", path+k)
			}
			if err := checkKeys(path+k+".", n[k]); err != nil {
				return err
			}
		}
	case []any:
		for i, x := range n {
			if err := checkKeys(fmt.Sprintf("%s[%d].", strings.TrimSuffix(path, "."), i), x); err != nil {
				return err
			}
		}
	}
	return nil
}

// MinTimeout is the shortest accepted shutdown timeout: sustained ping loss
// needs 5 consecutive 1 s misses, so anything shorter could never succeed.
const MinTimeout = 10 * time.Second

// rawPC mirrors PC with pointers so missing required keys are detectable
// (a missing relay must not silently become relay 0).
type rawPC struct {
	Name    *string       `mapstructure:"name"`
	IP      *string       `mapstructure:"ip"`
	Relay   *int          `mapstructure:"relay"`
	Roles   *[]string     `mapstructure:"roles"`
	Timeout time.Duration `mapstructure:"timeout"`
}

type rawConfig struct {
	Broker   Broker   `mapstructure:"broker"`
	Shutdown Shutdown `mapstructure:"shutdown"`
	PCs      []rawPC  `mapstructure:"pc"`
}

// Parse decodes, defaults and validates TOML text.
func Parse(text string) (*Config, error) {
	var tree map[string]any
	if err := toml.Unmarshal([]byte(text), &tree); err != nil {
		return nil, fmt.Errorf("parse config: %w", err)
	}
	if err := checkKeys("", tree); err != nil {
		return nil, fmt.Errorf("parse config: %w", err)
	}
	v := viper.New()
	v.SetConfigType("toml")
	if err := v.ReadConfig(strings.NewReader(text)); err != nil {
		return nil, fmt.Errorf("parse config: %w", err)
	}
	return decode(v)
}

var durationType = reflect.TypeOf(time.Duration(0))

// strictHook replaces viper's lenient default decode hooks: durations must be
// strings like "120s" (a bare number would silently mean nanoseconds), and a
// float never truncates into an integer field such as relay.
func strictHook(from, to reflect.Type, data any) (any, error) {
	if to == durationType {
		s, ok := data.(string)
		if !ok {
			return nil, fmt.Errorf("want a duration string like \"120s\", got %v", data)
		}
		return time.ParseDuration(s)
	}
	if to.Kind() >= reflect.Int && to.Kind() <= reflect.Uint64 && (from.Kind() == reflect.Float32 || from.Kind() == reflect.Float64) {
		return nil, fmt.Errorf("want an integer, got %v", data)
	}
	return data, nil
}

// strict turns off viper's weak typing ("1" -> 1, "osd" -> ["osd"]).
func strict(dc *mapstructure.DecoderConfig) {
	dc.WeaklyTypedInput = false
	dc.DecodeHook = mapstructure.DecodeHookFuncType(strictHook)
}

// decode unmarshals, defaults and validates. UnmarshalExact rejects unknown
// keys so a typo (e.g. "cutdelay") never silently falls back to a default,
// and name/ip/relay/roles are required in every [[pc]].
func decode(v *viper.Viper) (*Config, error) {
	var raw rawConfig
	if err := v.UnmarshalExact(&raw, strict); err != nil {
		return nil, fmt.Errorf("parse config: %w", err)
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
	c.applyDefaults(v)
	if err := c.Validate(); err != nil {
		return nil, err
	}
	return &c, nil
}

// applyDefaults fills only keys that are absent, so explicit values such
// as cut_delay = "0s" are honoured.
func (c *Config) applyDefaults(v *viper.Viper) {
	if !v.IsSet("broker.port") {
		c.Broker.Port = 8883
	}
	if !v.IsSet("broker.username") {
		c.Broker.Username = "orchestrator"
	}
	if !v.IsSet("shutdown.timeout") {
		c.Shutdown.Timeout = 120 * time.Second
	}
	if !v.IsSet("shutdown.cut_delay") {
		c.Shutdown.CutDelay = 30 * time.Second
	}
	if !v.IsSet("shutdown.pve_quorum") {
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
	if c.Shutdown.Timeout < MinTimeout {
		add("shutdown.timeout %s is below the minimum %s", c.Shutdown.Timeout, MinTimeout)
	}
	if c.Shutdown.CutDelay < 0 {
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
		if p.Timeout != 0 && p.Timeout < MinTimeout {
			add("pc %q: timeout %s is below the minimum %s", p.Name, p.Timeout, MinTimeout)
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
	if p.Timeout > 0 {
		return p.Timeout
	}
	return c.Shutdown.Timeout
}
