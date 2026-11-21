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

MQTT: TLS 1.2+ to `broker_host:broker_port` with server verification always on,
a client certificate plus a password, username and client id = `name`, a clean
session, LWT retained `offline`, and retained `online` published on every
connect. On `systemctl stop` the agent publishes `offline`. During a shutdown it
disconnects cleanly, so the retained `down` stays in place.

The guests are **not** orchestrated here. During host shutdown, Proxmox's
`pve-guests.service` stops the VMs and CTs. The orchestrator gates the relay
cut on sustained ping loss, not on `down`.

## Build

```
make            # cargo-zigbuild if installed, else Docker (rust-musl-cross) -> target/x86_64-unknown-linux-musl/release/rack-agent
make test       # cargo fmt --check, clippy -D warnings, unit tests (macOS ok, no bus/broker)
make e2e        # Docker e2e (see below)
```

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
  are rejected, the `install --dry-run` plan, and the real publisher's PUBACK
  matching and 5 s timeout (with paused tokio time).
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
