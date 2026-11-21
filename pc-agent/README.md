# pc-agent (`rack-agent`)

One static x86_64 musl binary, deployed the same way to all five Proxmox hosts.
Its whole vocabulary is:

| topic | payload (byte-exact) | action |
|---|---|---|
| `rack/pc/<name>/cmd` | `shutdown` | publish retained `rack/pc/<name>/status=down`, wait up to 5 s for the PUBACK, then call logind `org.freedesktop.login1.Manager.PowerOff(false)` |
| `rack/ceph/maintenance/set` (only with `ceph_maintenance = true`, i.e. intel-1ecbc8) | `on` / `off` | run `/usr/bin/ceph --id <ceph_id> osd set\|unset noout` and then `norebalance`, and publish retained `rack/ceph/maintenance/state` = `on` / `off` / `error` |

Anything else, including `Shutdown`, `shutdown\n` and `shutdown now`, is logged
(escaped and truncated) and ignored. **Retained messages are always ignored**,
because a stale retained `shutdown` would otherwise power the host off at every
boot. A duplicate `shutdown` is ignored once a power-off has been requested.
Message content is never executed or used as an argument.

MQTT uses the Eclipse Paho client (`paho-mqtt` 0.14.0, which wraps the Paho
C library 1.3.16). It connects with TLS 1.2 or 1.3 (OpenSSL 3 default security level; Paho ignores its own version option) to `ssl://broker_host:broker_port`.
Only the configured CA is trusted, the server certificate is always verified,
and the host is checked too: an IP `broker_host` must match an IP SAN. It
authenticates with a client certificate plus a password. Username and client id
are both `name`. The session is clean, the LWT is retained `offline` (QoS 1),
and keepalive is 30 s. Paho handles reconnects itself, starting at 1 s and
doubling up to 30 s, and it also retries when the very first connect fails.
After every (re)connect the agent subscribes again and publishes retained
`online`, or `down` if a power-off is already under way. A status publish is
QoS 1, and the agent waits up to 5 s on Paho's delivery token, which completes
on PUBACK. Publishes made while disconnected are buffered in memory (max 16)
and sent after the reconnect. There is no on-disk persistence. On `systemctl
stop` the agent publishes `offline`. During a shutdown it disconnects cleanly,
so the retained `down` stays in place.

The guests are **not** orchestrated here. During host shutdown, Proxmox's
`pve-guests.service` stops the VMs and CTs. The orchestrator gates the relay
cut on sustained ping loss, not on `down`.

## Build

```
make            # cargo-zigbuild if installed, else Docker (rust-musl-cross) -> target/x86_64-unknown-linux-musl/release/rack-agent
make test       # cargo fmt --check, clippy -D warnings, unit tests (macOS ok, no bus/broker)
make e2e        # Docker e2e (see below)
```

The result is one statically linked x86_64 musl ELF with no runtime `.so`
dependencies. paho-mqtt's `bundled` feature builds the Paho C library with
**cmake**, and `vendored-ssl` builds **OpenSSL 3** from source (via
`openssl-src`, which needs **perl** and **make**). Both are linked in
statically. With zig as the C cross-compiler (`cargo zigbuild`), the build host
needs cmake, perl, make, zig and the `x86_64-unknown-linux-musl` rustup target.
No system OpenSSL is used. A cold build takes a few minutes because OpenSSL is
compiled. The same applies to `cargo test` on the dev host, which builds Paho C
and OpenSSL natively.

**Licences:** Paho (C library and Rust wrapper) is dual-licensed EPL-2.0 /
EDL-1.0 (Eclipse Distribution License, a BSD-3-Clause licence). We ship it
unmodified under EDL-1.0. OpenSSL 3 is Apache-2.0. Both are statically linked
into the binary, so keep their notices with any redistribution.

## Deploy (per host, as root)

1. Copy the binary to the host and run `./rack-agent install --dry-run` to see
   the plan. Then run `./rack-agent install --config ./<name>.toml`, or omit
   `--config` to install the placeholder example. The install step:
   - copies itself to `/usr/local/bin/rack-agent`
   - writes `/etc/systemd/system/rack-agent.service` and
     `/etc/polkit-1/rules.d/10-rack-poweroff.rules` (both embedded in the binary)
   - creates the system user with
     `useradd --system --user-group --no-create-home --shell /usr/sbin/nologin rack-agent` if
     it doesn't exist yet
   - installs `/etc/rack-agent/config.toml` (0640 root:rack-agent) **only if
     absent**. `--config` with an existing config is refused rather than
     silently ignored.
   - runs `systemctl daemon-reload`, `try-restart` (so an upgrade picks up the
     new binary), and `enable --now rack-agent`. If the config doesn't validate
     yet (for example the placeholder `name = "CHANGE-ME"`), it only runs
     `enable` and tells you to edit the config and start the service.
2. Put the credentials in place (from `rackpki bundle <name>`). They must be
   readable by the service group and by no one else:
   ```
   install -d -m 0755 /etc/rack /etc/rack/tls
   install -m 0644 -o root -g root       ca.crt          /etc/rack/tls/ca.crt
   install -m 0644 -o root -g root       <name>.crt      /etc/rack/tls/<name>.crt
   install -m 0640 -o root -g rack-agent <name>.key      /etc/rack/tls/<name>.key
   install -m 0640 -o root -g rack-agent <name>.password /etc/rack/<name>.password
   ```
3. `rack-agent check` validates the config. Check the log with
   `journalctl -u rack-agent -f`, and look for `connected as <name>; publishing online`.

`config.example.toml` documents every key. Unknown keys are rejected. `name` and
`ceph_id` must match `[a-z0-9-]+` and must not start with `-`.

## Configuration, logging and crashes

The config is loaded with [figment](https://docs.rs/figment/0.10): built-in
defaults, then the TOML file, then environment overrides. A missing or
unreadable config file is an error. Errors print the whole context chain and
name the file and the bad key, for example:

```
in config /etc/rack-agent/config.toml: parsing config: unknown field: found `dryrun`, expected ... for key "default.dryrun" in TOML source string
```

Every key can be overridden by `RACK_AGENT_<KEY>` (key upper-cased), for
example `RACK_AGENT_DRY_RUN=true`, `RACK_AGENT_BROKER_PORT=18883` or
`RACK_AGENT_LOG_LEVEL=debug`. Only the keys in `config.example.toml` are read
this way, so other `RACK_AGENT_*` variables are ignored. Overrides are
validated like the file. `rack-agent check` applies them too.

Logs go through [fast_log](https://docs.rs/fast_log/1.7) (a writer thread) to
stderr, which journald collects, as
`[2026-11-21T12:00:00Z INFO  rack_agent::mqtt] ...`. The filter uses `RUST_LOG`
syntax (`info`, `debug`, `info,rack_agent::mqtt=debug`). It comes from the
`log_level` key (or `RACK_AGENT_LOG_LEVEL`) if set, otherwise from `RUST_LOG`
(the unit sets `info`), otherwise `info`. The log is flushed before every exit
(normal, error, SIGTERM, `dry_run`), before and after the logind PowerOff call,
and in the panic hook. Each flush waits at most 2 s.

Panics use [human-panic](https://docs.rs/human-panic/2.0). The release build
has `panic = "abort"`. The hook runs before the abort and does three things:
it flushes the log, prints `rack-agent <version>: panicked at <file:line>:
<message>` to stderr (so it is in `journalctl -u rack-agent`), and then
human-panic prints its notice with the path of a TOML crash report
(`report-<uuid>.toml`, with the message, location, OS and backtrace) in the
temp dir. Under the unit's `PrivateTmp=yes`, that is the service's private
`/tmp`, which is on the host under
`/tmp/systemd-private-*-rack-agent.service-*/tmp/`. systemd removes that
directory when the unit stops, so expect it to be gone after the automatic
restart (`Restart=always`, 5 s later). **The journal is the place to look**,
and the report is a bonus if you catch it. Setting `RUST_BACKTRACE=1` switches back to Rust's
default panic output (message plus backtrace on stderr, no report).

## polkit vs root

The agent runs as the unprivileged `rack-agent` user under a hardened unit
(`NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, an
empty capability set, and the address families limited to unix/inet). It can
power the host off only because of `polkit/10-rack-poweroff.rules`. That rule
returns YES for exactly `org.freedesktop.login1.power-off` and
`org.freedesktop.login1.power-off-multiple-sessions`, for `subject.user ==
"rack-agent"`. logind asks for the second action instead of the first whenever
other sessions are logged in (an admin's SSH session, for example). No reboot,
suspend or inhibitor-override rights are granted, so if something holds a
shutdown inhibitor lock, the PowerOff call fails. The agent then logs the error,
republishes `online`, and accepts a later `shutdown`.

**polkit must be installed** (`apt install polkitd`). Without it, logind lets
only root power off, and `install` warns about this. Running the agent as root
would avoid polkit but give a network-facing process full root, so we don't.

## Ceph maintenance (intel-1ecbc8 only)

Set `ceph_maintenance = true` in that host's config. Then create a capped key
once, as root on intel-1ecbc8:

```
ceph auth get-or-create client.rack-maint mon 'allow command "osd set" with key=noout, allow command "osd unset" with key=noout, allow command "osd set" with key=norebalance, allow command "osd unset" with key=norebalance' -o /etc/ceph/ceph.client.rack-maint.keyring
chgrp rack-agent /etc/ceph/ceph.client.rack-maint.keyring
chmod 0640 /etc/ceph/ceph.client.rack-maint.keyring
```

On Proxmox, `/etc/ceph/ceph.conf` is a symlink to `/etc/pve/ceph.conf`, and
pmxcfs allows only root and www-data to read it, so `rack-agent` can't. Give
the agent a readable minimal copy, and set `ceph_conf` in its config:

```
# On intel-1ecbc8, as root: copy only what a client needs.
ceph config generate-minimal-conf > /etc/rack-agent/ceph.conf
chgrp rack-agent /etc/rack-agent/ceph.conf && chmod 0640 /etc/rack-agent/ceph.conf
# then in /etc/rack-agent/config.toml:
#   ceph_conf = "/etc/rack-agent/ceph.conf"
```

Regenerate the copy if the monitors change. `ceph_conf` must be an absolute
path. It comes only from the config, never from a message.

This key can only set or unset `noout` and `norebalance`. The agent runs exactly
`/usr/bin/ceph [--conf <ceph_conf>] --id rack-maint osd set noout`, then
`osd set norebalance` (or `unset` for both on `off`), through
`std::process::Command` with a fixed argv and no shell. Each run is killed after 20 s, and any failure publishes `error`. With
`dry_run = true`, ceph is never run and no state is published; the agent only
logs the commands.

## Testing

- **Unit tests** (`make test`): the logic sits behind `Publisher`, `PowerOff`
  and `CephRunner` traits with fakes, so no D-Bus or broker is needed. The tests
  cover: a retained `shutdown` is ignored, non-exact payloads are ignored, `down`
  is published before PowerOff, PowerOff still happens on ack timeout or publish
  error, a duplicate `shutdown` causes one PowerOff, a failed PowerOff reverts
  to `online`, the ceph topic is ignored when disabled, the exact ceph argv,
  ceph errors publish `error`, ceph never runs in dry_run, bad `name`/`ceph_id`
  are rejected, the `install --dry-run` plan, and a concurrent message storm
  that must cause exactly one power-off.
- **Adapter tests** run the real Paho client over loopback TCP, without TLS.
  Most use an embedded `rumqttd` broker (a dev-dependency) and cover: password
  auth (a wrong password never connects), subscribe and `online`, a retained
  `shutdown` stored on the broker being ignored, Paho retrying a failed first
  connect, and **race (a)**. In race (a), Paho's callback thread delivers a
  300-message burst while the loop handles one message at a time. The result
  must be exactly one `down`, acknowledged before the single PowerOff. A small
  in-test stub (`src/stub_broker.rs`) is used only for faults a real broker
  won't produce on cue:
  - the raw CONNECT fields (will, clean session, credentials)
  - PUBACKs that never arrive (timeout), and an ACL-rejected SUBACK (0x80), which is logged and not fatal
  - **race (b)**: the link drops while `down` is unacknowledged. PowerOff must
    happen exactly once within 5 s, and `down` is republished after the reconnect.
  - **race (c)**: a reconnect replays a retained `shutdown`, which is ignored.

  TLS itself is not unit-tested; the e2e run and the live broker cover it.
- **Config, logging and panic tests**: unknown keys (in a file too), a missing
  file, defaults, the example config, `RACK_AGENT_*` overrides (unrelated ones
  ignored), the `{:#}` context chain, and a deliberately slow log sink that
  must hold the last line once `flush` returns. `tests/panic_hook.rs` runs the
  debug binary. With `--panic-test` (a debug-build-only switch, absent from
  release), it checks that the line logged just before the panic is flushed,
  that the panic message reaches stderr, and that human-panic writes its
  report. It also checks that a broken config and a missing config exit 1
  with the chained error on stderr.
- **E2E** (`make e2e`, needs Docker): `e2e/` is a Rust driver that generates a
  throwaway CA, certs, a passwd and an ACL into the gitignored `e2e/work/`. It
  starts `eclipse-mosquitto:2.0.20` (mTLS + password + ACL) and the agent in
  `dry_run`, then asserts that: the ACL stops `intel-test` from receiving another
  PC's cmd; a retained `shutdown` published before the agent connects is
  ignored; `online` appears; `shutdown` on another PC's topic and `Shutdown`,
  `shutdown\n`, `shutdown now` and `` are all ignored; and `shutdown` produces a
  retained `down` and the container exits 0. The agent container is attached only
  to an `internal: true` network, so it has no route to the LAN. The broker
  also joins a normal bridge, only so that `127.0.0.1:18883` can be published
  for the driver.
