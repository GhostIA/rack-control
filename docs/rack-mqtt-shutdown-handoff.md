# Handoff: MQTT graceful shutdown + relay cutoff for the rack

Context for Claude Code. Build a no-SSH, MQTT-based system to gracefully power
down the 5 Proxmox mini-PCs and then cut their relays, orchestrated from a
GL.iNet Slate. Every operator decision is resolved; see
[Resolved decisions](#resolved-decisions). Deploy steps live in
[DEPLOY.md](DEPLOY.md).

## Topology (all on one isolated subnet behind the Slate)

- **GL.iNet Slate 7 (GL-BE3600, arm64, OpenWrt)**: hosts the **Mosquitto
  broker** at `192.168.8.1:8883` (TLS only) AND the `rackctl` orchestrator. It's
  the NAT boundary for this segment, but every client below connects *outbound*
  to the local broker, so NAT is irrelevant to this system. Nothing needs a
  port-forward; nothing crosses the Slate boundary.
- **5 mini-PCs** (Proxmox VE 9.2, cluster `media`, systemd): each runs
  `pc-agent`, a local MQTT agent that powers *itself* off on command. No
  inbound access to the PCs, no SSH, no remote sudo.
- **Phidget SBC4** (`phidgetsbc.local`, 192.168.8.165, armv7l, PhidgetSBC4 OS
  4.0.0): HUB0004 serial **592109**. Runs the existing `relay_controller` as the
  systemd service `relay_controller`, which owns the relay channels (2x REL1000
  on VINT hub ports 3 and 4, 8 channels). Phase 2 replaces it with the merged
  `rack_controller` plus the `relay-mqtt` sidecar, so a relay opens on command.
  The per-channel hardware failsafe stays intact for every channel. The SBC4
  address comes from DHCP: give it a DHCP reservation on the Slate.
- **Switch**: not an MQTT client.

Everything talks to the broker on the Slate's LAN IP. The broker must NOT run on
one of the PCs being shut down.

## Hosts and roles

Ceph: erasure coding 2+1, `min_size 2`, failure domain host. Proxmox quorum is
3 of 5.

| name         | IP            | relay ch (unverified) | roles |
|--------------|---------------|-----------------------|-------|
| intel-91c45d | 192.168.8.196 | 0                     | osd |
| intel-d1babf | 192.168.8.194 | 1                     | osd |
| intel-177e78 | 192.168.8.162 | 2                     | osd, mon |
| intel-a163f8 | 192.168.8.123 | 4                     | mon, vm (VM 200 media-stack) |
| intel-1ecbc8 | 192.168.8.234 | 6                     | osd, mon (leader); Ceph maintenance node |

- The relay channel column is **unverified**. It lives only in `rack.toml`
  (`slate/rackctl/rack.example.toml` is the template) and stays unverified until
  `rackctl map-verify` confirms it on hardware. Don't copy it into code.
- Relay channel indices: hub port 3 ch0-3 = channels 0-3, hub port 4 ch0-3 =
  channels 4-7. The PC channels are NON-CONTIGUOUS (0,1,2,4,6); never assume
  0..N.
- **All 8 channels are commandable** (`rack/relay/0..7/set`). The sidecar
  validates 0-7. Which channel powers which PC is a `rack.toml` fact, not a
  protocol fact.

## Message flow (the whole point)

1. Operator runs `rackctl shutdown <name...>` (or `rack-shutdown`, the same
   binary via an argv[0] symlink) on the Slate. `all` = every PC in the
   configured order.
2. For `all`, rackctl first publishes `rack/ceph/maintenance/set` = `on`; the
   agent on intel-1ecbc8 sets `noout` and `norebalance` and publishes
   `rack/ceph/maintenance/state` = `on`.
3. One host at a time: rackctl publishes `rack/pc/<name>/cmd` = `shutdown`.
4. That PC's agent publishes `rack/pc/<name>/status` = `down` (retained), waits
   for the publish to complete (at most 5 s), then calls logind
   `PowerOff(false)`. systemd's shutdown runs `pve-guests.service`, which stops
   the VMs/CTs before the host powers off.
5. rackctl waits for **sustained** ping loss (5 consecutive misses). `status`
   is only an advisory breadcrumb; ping-down is the authoritative signal.
6. rackctl waits `cut_delay` (default 30 s) so disks finish syncing after the
   network drops, then publishes `rack/relay/<ch>/set` = `open`.
7. `relay-mqtt` on the SBC4 forwards `open <ch>` to `rack_controller` over its
   Unix socket, and after `ok` publishes `rack/relay/<ch>/state` = `open`
   (retained). rackctl waits for that state. The failsafe still protects every
   other channel.
8. On a timeout (default 120 s, per-PC override) rackctl does NOT cut power. It
   reports and stops the sequence.

The matching `rackctl up` (`rack-up`) path runs in reverse order: publish
`rack/relay/<ch>/set` = `close` with a 5 s stagger, and wait for ping-up. PCs
with "AC recovery = power on" start by themselves. Once every OSD host is up,
rackctl sets Ceph maintenance `off`.

## Topics

Commands are QoS 1 and never retained. Clients use CleanSession=true and
**ignore retained messages on every command topic**. Payloads are compared
byte-exact; anything else is logged and ignored.

| topic                         | payloads                                         | publisher -> subscriber      | retained |
|-------------------------------|--------------------------------------------------|------------------------------|----------|
| `rack/pc/<name>/cmd`          | `shutdown`                                       | orchestrator -> that PC      | no |
| `rack/pc/<name>/status`       | `online` (connect), `offline` (LWT), `down` (before poweroff) | PC -> orchestrator | yes |
| `rack/relay/<ch>/set`         | `open`, `close` (ch 0-7)                         | orchestrator -> sbc4         | no |
| `rack/relay/<ch>/state`       | `open`, `closed`                                 | sbc4 -> orchestrator         | yes |
| `rack/sbc4/status`            | `online`, `offline` (LWT)                        | sbc4 -> orchestrator         | yes |
| `rack/ceph/maintenance/set`   | `on`, `off`                                      | orchestrator -> intel-1ecbc8 | no |
| `rack/ceph/maintenance/state` | `on`, `off`, `error`                             | intel-1ecbc8 -> orchestrator | yes |

## Components to build

### 1. Mosquitto broker config (`slate/mosquitto/`, on the Slate)

- Install `mosquitto-ssl` via opkg (the TLS-capable OpenWrt package). rackctl
  is a native MQTT client, so `mosquitto-client` is optional (debugging only).
- **One listener only:** `listener 8883 192.168.8.1`. No plaintext 1883,
  nothing on WAN.
- `allow_anonymous false`, `password_file`, `acl_file`, `cafile`/`certfile`/
  `keyfile` under `/etc/mosquitto/certs/`, `require_certificate true`,
  `use_identity_as_username false`, `tls_version tlsv1.2`.
- **Users:** `orchestrator` (rackctl), `sbc4` (the relay-mqtt sidecar), and one
  per PC named after the PC: `intel-91c45d`, `intel-d1babf`, `intel-177e78`,
  `intel-a163f8`, `intel-1ecbc8`. Each user's cert CN equals its username.
- **ACLs** (the security boundary; derived exactly from the topic table):
  - `orchestrator`: publish `rack/pc/+/cmd`, `rack/relay/+/set`,
    `rack/ceph/maintenance/set`; subscribe `rack/pc/+/status`,
    `rack/relay/+/state`, `rack/sbc4/status`, `rack/ceph/maintenance/state`.
  - each PC user (e.g. `intel-91c45d`): subscribe ONLY to its own
    `rack/pc/intel-91c45d/cmd`; publish ONLY to its own
    `rack/pc/intel-91c45d/status`. intel-1ecbc8 also gets subscribe
    `rack/ceph/maintenance/set` and publish `rack/ceph/maintenance/state`.
  - `sbc4`: subscribe ONLY to `rack/relay/+/set`; publish ONLY to
    `rack/relay/+/state` and `rack/sbc4/status`.
- Persist config across Slate firmware updates: list `/etc/mosquitto/` and
  `/etc/rack/` (and the rackctl binary) in `/etc/sysupgrade.conf`. Details in
  `slate/README.md`.

### 2. PKI tool: `rackpki` (`slate/rackpki/`, Go, runs on the dev box)

Replaces the planned `gen-ca.sh` / `gen-cert.sh` scripts. Output goes to the
gitignored `slate/pki/`.

- `init-ca`: ECDSA P-256 CA, 10 years.
- `server --ip 192.168.8.1`: broker cert with an **IP SAN**, 5 years.
- `client <user>|--all`: CN = username, 5 years.
- `passwd`: reads the `MQTT_*_PASSWORD` environment variables and writes a
  Mosquitto 2.x `$7$` (PBKDF2-SHA512) password file, mode 0600. Run under
  `op run` against the 1Password Environment `rack-mqtt`.
- `bundle <user>`: assembles a per-host deploy dir.
- `expiry`: prints every cert's expiry.

The generated keys are stored by the operator as 1Password Document items; the
tool only generates them locally.

### 3. Per-PC agent: `pc-agent` (Rust, on each mini-PC)

- A **Rust** daemon (one static x86_64 musl binary, `rumqttc` with rustls),
  run as the systemd service `rack-agent.service` under the unprivileged user
  `rack-agent`. Subscribes to its own `rack/pc/<name>/cmd`. On payload EXACTLY
  `shutdown` (not retained), it publishes `rack/pc/<name>/status=down`, waits
  for the publish to complete (at most 5 s), then calls logind
  `org.freedesktop.login1.Manager.PowerOff(false)` over the system bus (zbus).
- **Security-critical:** accept ONLY the fixed payload `shutdown` on its own
  topic and perform exactly one hard-coded action. NEVER exec message content,
  never `sh -c`, no payload-derived commands. Payloads are parsed into enums by
  byte-exact match. A broker compromise is capped at power-off DoS, never code
  exec.
- **Ceph maintenance (intel-1ecbc8 only, `ceph_maintenance = true`):** handles
  `rack/ceph/maintenance/set` = `on`|`off` by running exactly
  `/usr/bin/ceph --id rack-maint osd set|unset noout` and the same for
  `norebalance` (fixed argv, no shell), then publishes the resulting state
  (`error` on failure). The `client.rack-maint` key is capped to those four
  commands; see [DEPLOY.md](DEPLOY.md#ceph-rack-maint-key).
- `dry_run = true` logs "would power off" and exits 0 instead of calling
  logind, and never runs ceph. It's for container tests only.
- **One binary, 5 machines:** per-PC identity (name, cert, password file) comes
  from a TOML config, not separate builds.
- **Privilege: polkit**, not root. A narrow rule
  (`pc-agent/polkit/10-rack-poweroff.rules`) grants only
  `org.freedesktop.login1.power-off` to `rack-agent`.
- `pc-agent install` (a subcommand of the binary; it replaces `install.sh`)
  copies the binary, unit, polkit rule and config, creates the user, and
  enables the service.

### 4. Relay side on the SBC4: C++ controller + Rust MQTT sidecar

Architecture: sidecar. The safety-critical Phidget code stays in C++; a
separate Rust process handles MQTT and forwards relay commands to the C++ over
a LOCAL Unix domain socket. The C++ keeps sole ownership of the Phidget hub and
the failsafe; the sidecar never touches hardware.

#### 4a. rack_controller (C++, phase 2: DEFERRED)

The merge of `relay_controller` + `fan_control` into one `rack_controller` is
**deferred to phase 2**; see [rack-controller-merge.md](rack-controller-merge.md).
Until it lands, the SBC4 runs the old `relay_controller`, which has no socket,
so relays are not MQTT-commandable and every `rackctl shutdown` must use
`--no-relay`. relay-mqtt needs the merge to be useful.

What the merge adds:

- Relays keep `relay_controller`'s **BLOCKING startup** unchanged:
  `Phidget_openWaitForAttachment` per channel, a one-time 2.5 s stagger, a
  1000 ms failsafe, reset every 1/3 of the failsafe time. (Earlier drafts of
  this doc said "non-blocking Phidget_open, attach-driven power-on" for relays;
  that was wrong. Only fans and SSR use `fan_control`'s dynamic attach model.)
- A **Unix domain socket** at `/run/rack_controller/ctl.sock` (systemd
  `RuntimeDirectory=rack_controller`, dir 0750, socket 0660, group `rackctl`).
  One ASCII request per line, at most 128 bytes, many requests per connection:
  - `open <ch>` -> set the relay FALSE (NO relay opens, PC off). Reply
    `ok open <ch>` or `err <reason>`.
  - `close <ch>` -> set the relay TRUE (relay closes, PC on).
  - `fan <ch> <duty 0.0-1.0>`, `ssr <ch> on|off` (ch 0-3).
  - `state` -> lines `relay <ch> cmd=<open|closed> attached=<0|1>`,
    `fan <ch> duty=<x.xx> attached=<0|1>`, `ssr <ch> cmd=<on|off> attached=<0|1>`,
    then `end`.
  - Relay ch 0-7 (0-3 = port 3 ch0-3, 4-7 = port 4 ch0-3). Unknown or malformed
    input -> `err <reason>`; never crash, never exit.
- A per-channel **commanded state**: the stagger skips channels commanded open;
  `open` = `setState(0)` with the failsafe still fed.
- **Commanded state is NOT persisted.** A `rack_controller` restart brings all
  8 relays back on (the original behavior). Stopping it opens every relay, so
  any restart of it is a full rack power cycle.
- No MQTT, no network library in the C++.

#### 4b. relay-mqtt sidecar (Rust, `sbc4/relay-mqtt/`)

- Small Rust daemon (static armv7 musl binary). Connects as user `sbc4`,
  reconnects forever to both the broker and the socket. Subscribes
  `rack/relay/+/set`.
  - On `rack/relay/<ch>/set` = `open`|`close`: validate ch 0-7 and the payload,
    write `open <ch>` / `close <ch>` to the socket, and publish retained
    `rack/relay/<ch>/state` only after `ok`.
  - On connect: query `state` and publish all 8 channels.
  - LWT `rack/sbc4/status` = `offline`; `online` on connect.
- Runs as the systemd service `relay-mqtt.service`, user `relay-mqtt` in group
  `rackctl`. `After=rack_controller.service` but no `Requires=`, so neither
  restart disturbs the other.

#### Ordering / resilience

- The sidecar tolerates the socket not being up yet (retry). The C++ tolerates
  the sidecar being absent (the socket just has no client).
- The C++ owns hardware + failsafe regardless of the sidecar; the sidecar is
  stateless and reconnects.

### 5. Orchestrator CLI: `rackctl` (Go, on the Slate)

Replaces the planned shell `rack-shutdown` / `rack-up` scripts (no
`mosquitto_pub`/`mosquitto_sub`). One arm64 binary with busybox-style argv[0]
symlinks `rack-shutdown` and `rack-up`. The name -> IP -> relay channel mapping,
roles and timeouts live in `rack.toml`.

- `shutdown [--force] [--dry-run] [--no-relay] [all|name...]`
  - `all` uses the configured order (default intel-a163f8, intel-91c45d,
    intel-d1babf, intel-177e78, intel-1ecbc8), one host at a time, and sets
    Ceph maintenance `on` first.
  - Per host: send `cmd`, wait for sustained ping loss, wait `cut_delay`,
    publish relay `open`, wait for `state=open`. `--no-relay` stops after the
    ping-down confirmation. On a timeout: no cut, report, stop the sequence.
  - **Partial-shutdown guard** (anything short of `all`, unless `--force`):
    refuse if the result would leave 2 or more OSD hosts down, 2 or more mon
    hosts down, or fewer than 3 PVE nodes up.
- `up [all|name...]`: reverse order, relay `close` with a 5 s stagger, wait for
  ping-up, then Ceph maintenance `off` once every OSD host is up.
- `status`: PC status, relay state, ping, sbc4 and Ceph maintenance state.
- `map-verify`: refuse unless every PC is sustained-down and every relay state
  is `open`. For each ch 0-7: close, wait up to 180 s for any configured IP to
  answer, record it, gracefully shut that PC down again, open the relay. Prints
  the discovered mapping and a TOML snippet for `rack.toml`.
- `ceph-maintenance on|off`.
- ICMP ping via `golang.org/x/net/icmp` behind a `Pinger` interface.

## Security requirements (enforce, don't skip)

- TLS 1.2+ on 8883 only, verification ON in every client (`ServerName
  "192.168.8.1"`, no InsecureSkipVerify / insecure anywhere).
- **mTLS AND password:** a client needs a CA-signed cert (CN = username) and its
  password. ACLs scoped per-client as above.
- Broker listener bound to the LAN IP only; no port-forward on the Slate.
- Agents and the sidecar act on fixed payloads only; message content is never
  executed. Worst-case broker compromise = power-off DoS, not RCE.
- Keys and password files on hosts: `/etc/rack/tls/ca.crt`,
  `/etc/rack/tls/<user>.crt`, `/etc/rack/tls/<user>.key`,
  `/etc/rack/<user>.password`, mode 0600 (or 0640 to the service group).
  Distinct users; no shared account.
- No secret values in the repo or docs. Commit only `*.example` files.

## Resolved decisions

Build to these. They replace the old "Still to confirm" and "OPEN" items.

1. **Mapping:** the host table above, 5 PCs (not 6). The relay channels in it
   are unverified until `rackctl map-verify`; the mapping lives only in
   `rack.toml`.
2. **Broker address:** `192.168.8.1:8883`, with an IP SAN (no hostname, no local
   DNS dependency). Clients set ServerName `192.168.8.1`.
3. **TLS from v1:** 8883 only, plaintext 1883 never opened. mTLS + password,
   `use_identity_as_username false`, cert CN = MQTT username.
4. **Hosts:** all 5 are Proxmox VE 9.2. ONE amd64 agent binary serves all 5.
   PVE guest shutdown is deferred to PVE (`pve-guests.service`); the agent does
   not orchestrate guests.
5. **Relay-cut gate:** sustained ping-down (5 consecutive misses) + `cut_delay`
   (30 s), never `status`. Timeout 120 s default with a per-PC override; on
   timeout, no cut.
6. **Ceph-aware ordering:** full shutdown sets maintenance (`noout`,
   `norebalance`) first via intel-1ecbc8, then goes intel-a163f8 (VM host, while
   PVE quorum is intact), intel-91c45d, intel-d1babf, intel-177e78, intel-1ecbc8
   (mon leader, last). Power-up is the reverse, with maintenance off once every
   OSD host is up. Partial shutdowns are guarded (see rackctl).
7. **All 8 relay channels commandable**; validation is 0-7. The PC mapping is a
   config fact.
8. **Commanded relay state is not persisted.** rack_controller restart = all on.
9. **Relays keep relay_controller's blocking startup**; only fans/SSR are
   attach-driven.
10. **SBC4:** `relay_controller` currently runs as a systemd service; phase 2
    ships `rack_controller.service` (`Conflicts=relay_controller.service`) and
    `relay-mqtt.service`. HUB0004 serial 592109. SBC4 at 192.168.8.165 via DHCP:
    add a DHCP reservation.
11. **C++ merge deferred to phase 2.** relay-mqtt needs it; until then use
    `--no-relay`.
12. **Languages:** Rust for the daemons (`pc-agent`, `relay-mqtt`), Go for the
    CLIs (`rackctl`, `rackpki`). The planned shell CLIs and `gen-*.sh` scripts
    are gone.
13. **Per-PC agent privilege:** polkit, unprivileged `rack-agent` user.
14. **BIOS AC-recovery = power on:** confirmed on all PCs, so relay close boots
    them.
15. **Secrets:** broker passwords in the 1Password Environment `rack-mqtt`
    (variables `MQTT_ORCHESTRATOR_PASSWORD`, `MQTT_SBC4_PASSWORD`,
    `MQTT_<NAME upper, - -> _>_PASSWORD`), mounted at `slate/.env`. Root
    passwords in `intel-rack-creds`, mounted at `./.env`. Both mounts are named
    pipes: never read or print them. TLS keys are 1Password Document items. The
    ACL file holds no secrets and is committed.
16. **Fans and SSR are not on MQTT in v1.** The switch is not an MQTT client.
17. **Cert lifetimes:** CA 10 years, server and clients 5 years. Renewal path in
    [DEPLOY.md](DEPLOY.md#cert-renewal).

## Do NOT

- Do not use SSH or any inbound path to the PCs.
- Do not run the broker on a PC that gets shut down.
- Do not open plaintext 1883 or disable cert verification anywhere.
- Do not rewrite relay_controller's relay core (blocking open, stagger,
  failsafe). The C++ gets ONLY a local Unix-socket control interface + the
  per-channel commanded-state override. No MQTT or network library in the C++.
- MQTT lives in the Rust sidecar (SBC4), the Rust per-PC agents and the Go
  rackctl, never in the C++.
- Do not interpret MQTT payloads as shell commands anywhere; agents act on
  `shutdown` only (plus `on`/`off` for Ceph maintenance on intel-1ecbc8), the
  sidecar on `open`/`close` only.
- Do not expose the broker beyond the isolated segment.
- Do not assume contiguous relay channels or trust the mapping before
  `map-verify`.
- Do not cut a relay before sustained ping-down.
