# rackctl: rack orchestrator (Slate 7)

`rackctl` runs as root on the GL.iNet Slate 7 (OpenWrt, arm64). It shuts the
rack's Proxmox PCs down gracefully over MQTT, cuts each PC's relay **only after
the PC has stopped answering ping**, and brings the rack back up. It is one
static Go binary with busybox-style aliases:

| invoked as      | same as            |
|-----------------|--------------------|
| `rack-shutdown` | `rackctl shutdown` |
| `rack-up`       | `rackctl up`       |

It never runs a shell or any external command.

## Build and install

```sh
make            # bin/rackctl, linux/arm64, static (CGO_ENABLED=0, -trimpath -s -w)
make host       # bin/rackctl-host for this machine (dry runs)
make test       # go vet ./... && go test ./...
make e2e        # Docker end-to-end rig (see below)
```

On the Slate:

```sh
install -m 0755 rackctl /usr/bin/rackctl
ln -sf rackctl /usr/bin/rack-shutdown
ln -sf rackctl /usr/bin/rack-up
install -m 0600 rack.example.toml /etc/rack/rack.toml   # then edit
# TLS + password, from rackpki bundle orchestrator:
#   /etc/rack/tls/ca.crt  /etc/rack/tls/orchestrator.crt  /etc/rack/tls/orchestrator.key (0600)
#   /etc/rack/orchestrator.password (0600)
```

Add `/etc/rack/` and `/usr/bin/rackctl` (plus the two symlinks) to
`/etc/sysupgrade.conf` so they survive firmware upgrades.

## Commands

```
rackctl [--config FILE] <command> [flags] [args]   (default /etc/rack/rack.toml)

shutdown [--force] [--dry-run] [--no-relay] all|NAME...
up [--dry-run] all|NAME...
status
map-verify [--dry-run] [--channels 3,4,5,6,7] [--boot-timeout 5m]
ceph-maintenance [--dry-run] on|off
relay [--force] [--dry-run] open|close CH
completion bash|zsh|fish|powershell
help [command]
```

The command line is parsed by [cobra](https://github.com/spf13/cobra) (pflag
syntax): flags may come before, between or after the arguments, long flags
take two dashes (`--force`, `--config=FILE` or `--config FILE`), and `--`
ends flag parsing. The single-dash form the old parser also took (`-force`,
`-config FILE`) is now a usage error (exit 2), so update any script or hook
that still uses it. `--config` works with every command, in any position.
`rackctl <command> --help` (or `rackctl help <command>`) shows each command's
flags. `rack-shutdown --help` is the help for `rackctl shutdown`.

Exit codes are 0 on success (including `--help`), 1 on a failure or refusal,
and 2 for a usage or config error. A usage error prints the error and the
command's usage; a runtime failure prints only the error. `shutdown` and `up` require an explicit target (`all` or names).
Running bare `rack-shutdown` does nothing destructive. Only one rackctl
(other than `status`) can run at a time, enforced by a lock on
`/var/run/rackctl.lock`.

### shutdown

1. **Partial shutdown guard** (named PCs, skipped for `all` or with `--force`):
   rackctl pings every other PC to find out which are already down, then
   refuses if the resulting down set has **≥2 OSD hosts** (Ceph EC 2+1),
   **≥2 mon hosts** (mon quorum), or fewer than `pve_quorum` PCs up.
2. **Ceph maintenance** (`all`, or any target with role `osd`): rackctl
   publishes `rack/ceph/maintenance/set=on` and waits up to 30 s for state
   `on`, re-sending every 10 s. If that fails it aborts before touching any
   PC, unless `--force` is given.
3. **One host at a time, in `order`:**
   - publish `rack/pc/<name>/cmd=shutdown`
   - wait up to `timeout` (per-PC override) for **sustained ping loss**
     (5 consecutive misses at 1 s). Any reply resets the count, so a flapping
     host never counts as down.
   - **Timeout: the relay is NOT cut, the failure is reported and the
     sequence STOPS.** Later hosts are not touched.
   - sleep `cut_delay`, publish `rack/relay/<ch>/set=open`, and wait ≤10 s for
     the retained state `open`. A missing confirmation is reported but does not
     stop the sequence, because the host is already down.
   - `--no-relay` never publishes any relay command. Use it for the first live
     tests.

Ctrl-C or SIGTERM at any point stops without cutting anything else. A
cancelled ping never counts as a miss. A **local pinger failure** (no raw
socket permission, out of file descriptors, network down) also stops the
procedure without a cut. It is never treated as "host down". Only a timeout,
or a host-unreachable from neighbour resolution, counts as a miss.

### up

Relays `close` in **reverse** order with a 5 s stagger (each waits ≤10 s for
state `closed`). rackctl then waits ≤300 s per host for a ping reply. Once
**every OSD host in the rack** answers, it publishes ceph maintenance `off`,
re-sending for up to 180 s while the Ceph node's agent connects. Hosts that
never came up are reported, exit code 1.

### status

Shows each PC's ping state, retained `status` and relay state, then all 8
relays, the sbc4 status and the ceph maintenance state.

### map-verify

The relay numbers in `rack.example.toml` are **unverified**. `map-verify`
discovers the real wiring:

- It refuses unless every PC is sustained-down **and** every relay 0–7 has
  retained state `open`. Run `rackctl shutdown all` first, then
  `rackctl relay open <ch>` for each unmapped channel (3, 5, 7), because
  relay_controller closes all 8 at boot.
- For each channel 0..7 (or only the `--channels` given, e.g.
  `--channels 3,5,7`; values 0–7, duplicates dropped, tested in ascending
  order): `close` it, then wait up to `--boot-timeout` (default 5 min, at least
  30 s) for any configured IP to answer. If one does, wait ≤120 s for its agent's `online`, send `shutdown`,
  wait for sustained ping loss plus `cut_delay`, then `open` it again. If none
  answers, `open` it.
- Any surprise (a second PC answers, the agent never comes online, the PC
  won't go down) stops with that relay left **closed** and the PC running,
  so nothing gets an unclean power cut.
- Finally it prints the mapping and a ready-to-paste `[[pc]]` snippet. Exit
  code 1 means the mapping differs from the config.

### relay

`relay open|close CH` publishes `rack/relay/CH/set` and waits ≤10 s for the
matching retained state. **Opening** a channel whose configured PC still
answers ping is refused unless `--force`, so a live host is never cut by
hand. Closing is always allowed, because it only powers a host on.

### Config

`/etc/rack/rack.toml` (see `rack.example.toml`) is always read as TOML,
whatever the file is called, with [viper](https://github.com/spf13/viper), and
decoded strictly. It is validated before anything runs:
- unknown keys are rejected (viper `UnmarshalExact`), anywhere in the file
- keys must be lowercase and must not contain dots (viper would otherwise
  merge `host`/`HOST` and move a quoted `"shutdown.cut_delay"` into
  `[shutdown]`)
- no type coercion: `relay = "1"`, `relay = 1.5`, `roles = "osd"` and
  `port = "8883"` are errors
- durations are strings with a unit (`"120s"`, `"5m"`); a bare number is an
  error
- every `[[pc]]` needs `name`, `ip` (IPv4), `relay` (0–7) and `roles`
  (`[]` allowed)
- names, IPs and relays must be unique
- `order` must list every PC exactly once
- timeouts must be at least 10 s

Defaults apply only to keys that are absent: port 8883, username
`orchestrator`, timeout 120 s, cut_delay 30 s, pve_quorum 3. There are no
environment-variable overrides for config keys or the config path (only
`RACKCTL_LOG_LEVEL` = trace|debug|info|warn|error, which sets the level of
the diagnostic log on stderr; default info).

### --dry-run

Prints every publish, wait and sleep the command would do. It makes **zero
broker connections and zero pings**, and doesn't need certs or the password
file. It assumes all PCs are up for the guard.

## MQTT

`ssl://<host>:8883`, TLS ≥1.2, the rack CA as the only root, the client cert,
and ServerName = `broker.host` (IP SAN) with verification on. Client ID =
username = `orchestrator`, with a clean session. The read-only `status` uses
client ID `orchestrator-status-<pid>`, so running it during a shutdown doesn't
kick the orchestration off the broker. Same user and cert, so the ACL is
unchanged. Commands are QoS 1 and never
retained. rackctl subscribes only to `rack/pc/+/status`, `rack/relay/+/state`,
`rack/sbc4/status` and `rack/ceph/maintenance/state`. Payloads must match
byte-for-byte; anything else is logged and ignored. A state wait only accepts a
live message that arrived **after** the command was sent. Retained messages,
including redeliveries after a reconnect, are kept for display but never
count as a confirmation.

## Layout

```
main.go                 argv[0] dispatch, cobra commands and flags, lock, wiring
internal/config         rack.toml via viper + validation (unknown keys rejected)
internal/orch           all procedures, behind Bus / Pinger / Clock interfaces
internal/mqttbus        paho.mqtt.golang Bus (mTLS + password)
internal/ping           privileged ICMP (ip4:icmp) Pinger
e2e/                    Docker rig + Go test (build tag e2e)
```

Unit tests run the procedures against a simulated rack (fake bus, pinger and
virtual clock) and check the core invariant on every run: **no relay open
without ≥5 consecutive ping misses from the host wired to that channel, at
least `cut_delay` earlier.**

## E2E rig (`make e2e`)

`e2e/e2e_test.go` (build tag `e2e`, skipped if Docker isn't running) does the
following:

- generates a throwaway CA, broker and client certs (Go crypto/x509), random
  passwords, a Mosquitto `$7$` passwd file, an ACL and `mosquitto.conf` into
  `e2e/work/` (gitignored)
- cross-builds `rackctl` and `e2e/fakenode` for the Docker host's Linux arch
- starts `eclipse-mosquitto:2.0.20` and fake PCs and sbc4 on an
  **`internal: true`** network `172.30.0.0/24`, which has no route to
  192.168.8.0/24
- runs `rackctl shutdown all` in a container (NET_RAW) and asserts:
  - the shutdown order
  - ceph `on` comes first
  - each relay open arrives only after that fake PC exited and no longer
    answers ping
- runs it again with the second host ignoring `shutdown`, and asserts:
  - only the first relay is cut
  - the rest never receive a command
  - rackctl exits non-zero
