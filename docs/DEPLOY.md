# Deploy and operate rack-control

Manual deployment, in order. Nothing in this repo reaches the machines by
itself. Specs: [rack-mqtt-shutdown-handoff.md](rack-mqtt-shutdown-handoff.md)
(protocol, ACLs, decisions) and [rack-controller-merge.md](rack-controller-merge.md)
(phase 2). Exact build commands and flags live in each component's `Makefile`
and README; this file is the order and the safety rules.

Hosts: Slate `192.168.8.1`, SBC4 `192.168.8.165`, PCs per the
[host table](rack-mqtt-shutdown-handoff.md#hosts-and-roles).

## Safety rules (read first)

- **Never touch relays until `rackctl map-verify` has confirmed the mapping.**
  Until then, every shutdown uses `--no-relay`.
- Stopping or restarting `relay_controller` / `rack_controller` opens every
  relay: it's a full rack power cut. Only do it with every PC already shut
  down.
- Only one process may own the SBC4 hub.
- No secret values on the command line, in shell history, in the repo or in
  docs. Passwords come from 1Password via `op run`; never `cat` a `.env` (they
  are 1Password named pipes: `slate/.env` = Environment `rack-mqtt`, `./.env` =
  Environment `intel-rack-creds`, root passwords for console use only).

## 0. Build (dev box)

```
make -C slate/rackpki          # host binary
make -C slate/rackctl          # linux/arm64 (Slate)
make -C pc-agent               # x86_64-unknown-linux-musl
make -C sbc4/relay-mqtt        # armv7-unknown-linux-musleabihf
```

Run each component's unit tests first (`go vet ./... && go test ./...`, or
`cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`).

## 1. Broker + certs (Slate)

1. Generate PKI on the dev box (output in gitignored `slate/pki/`):
   ```
   rackpki init-ca
   rackpki server --ip 192.168.8.1
   rackpki client --all
   op run --environment <rack-mqtt environment ID> -- rackpki passwd
   rackpki expiry
   ```
   The Environment ID is shown in the 1Password app for `rack-mqtt`.
2. Store `ca.key`, `server.key` and every client key as **1Password Document
   items**. The CA key never leaves the dev box/1Password.
3. On the Slate: `opkg install mosquitto-ssl`, copy `slate/mosquitto/`
   (`mosquitto.conf`, `acl`, the generated `passwd`) to `/etc/mosquitto/`, and
   `ca.crt`, `server.crt`, `server.key` to `/etc/mosquitto/certs/` (key 0600,
   owned by the mosquitto user). Enable and start the init script.
4. Add `/etc/mosquitto/` and `/etc/rack/` (and later `/usr/bin/rackctl` and its
   symlinks) to `/etc/sysupgrade.conf`.
5. Check: only `192.168.8.1:8883` is listening (`netstat -ltn`), nothing on
   1883, nothing on WAN. Details: `slate/README.md`.

## 2. Agent on each PC

Per PC (`<name>` = its hostname, e.g. `intel-d1babf`). Start with intel-d1babf
only, then do the [first live test](#first-live-test-intel-d1babf-only) before
the others.

1. `rackpki bundle <name>` (run under `op run` as above so the bundle can carry
   the password file). Copy the bundle to the PC by hand (console or your normal
   admin path; the system itself uses no SSH).
2. Copy the `pc-agent` binary over and run `pc-agent install` as root. It
   creates the `rack-agent` user, installs the binary, `rack-agent.service`,
   the [polkit rule](#polkit-rule) and the config, and enables the service.
   Edit the config from `config.example.toml`: `name`, cert paths,
   `dry_run = false`, and `ceph_maintenance = true` **only on intel-1ecbc8**.
3. Install the bundle: `ca.crt`, `<name>.crt`, `<name>.key` to `/etc/rack/tls/`,
   `<name>.password` to `/etc/rack/`; key and password 0640 root:rack-agent
   (or 0600 owned by rack-agent). Restart `rack-agent` if it started first.
4. Check: `journalctl -u rack-agent` shows a TLS connect; `rackctl status`
   shows `rack/pc/<name>/status` = `online`.

### Polkit rule

`/etc/polkit-1/rules.d/10-rack-poweroff.rules` (shipped as
`pc-agent/polkit/10-rack-poweroff.rules`) grants exactly one action to one user:

```
polkit.addRule(function(action, subject) {
    if (action.id == "org.freedesktop.login1.power-off" &&
        subject.user == "rack-agent") {
        return polkit.Result.YES;
    }
});
```

Caveat: logind checks `org.freedesktop.login1.power-off-multiple-sessions`
instead when another user has a session open (an SSH or console login, or a
PVE web shell). The rule doesn't grant that, so PowerOff is refused while
someone is logged in. That fails safe: no ping-down, so rackctl times out and
never cuts. Log out before a shutdown, or extend the rule deliberately.

### Ceph rack-maint key

One time, as root on intel-1ecbc8 (the maintenance node; the keyring file is
written locally), after `pc-agent install` has created the `rack-agent` user:

```
ceph auth get-or-create client.rack-maint mon \
  'allow command "osd set" with key=noout, allow command "osd unset" with key=noout, allow command "osd set" with key=norebalance, allow command "osd unset" with key=norebalance' \
  -o /etc/ceph/ceph.client.rack-maint.keyring
chown root:rack-agent /etc/ceph/ceph.client.rack-maint.keyring
chmod 0640 /etc/ceph/ceph.client.rack-maint.keyring
```

The key can only set/unset `noout` and `norebalance`. The agent runs exactly
`/usr/bin/ceph --id rack-maint osd set|unset noout|norebalance` (fixed argv).
Check as the agent user that it works:

```
runuser -u rack-agent -- /usr/bin/ceph --id rack-maint osd set noout
```

Only run that when you want `noout` set anyway (for example as step 1 of the
first live test). On PVE `/etc/ceph/ceph.conf` is a symlink into `/etc/pve`,
which is normally readable only by root and `www-data`. If the check fails
reading the config, give the agent its own minimal config (`[global]` with
`mon_host` and `keyring = /etc/ceph/ceph.client.rack-maint.keyring`, readable by
`rack-agent`) and point at it with a systemd drop-in for `rack-agent.service`:
`Environment=CEPH_CONF=/etc/rack/ceph.conf`. The argv stays fixed.

## 3. SBC4 sidecar (after the phase-2 merge only)

Not before phase 2: the running `relay_controller` has no socket.

1. Do the [SBC4 controller swap](#sbc4-controller-swap-phidget-user-app).
2. `rackpki bundle sbc4`; install to `/etc/rack/tls/` and `/etc/rack/sbc4.password`
   (0640 root:relay-mqtt or 0600 owned by relay-mqtt).
3. Create user `relay-mqtt` in group `rackctl`; install the binary and the
   config from `config.example.toml`, and register it as its own Phidget user
   app (an `ua_<App>.conf` + `S2x<App>` pair like rack_controller's, enabled).
4. Check: `rackctl status` shows `rack/sbc4/status` = `online` and all 8
   `rack/relay/<ch>/state` = `closed`.

### SBC4 controller swap (Phidget user app)

The SBC4's controllers are **Phidget user apps**, not systemd units: the web
UI's launcher (`phidgetsbc_ua.service` → `/etc/webif/init.d/S20<App>`, config
`/etc/webif/ua_<App>.conf`) starts each enabled app once at boot, with stdout
and stderr in `/tmp/<app_name>/`. Before the swap, `RelayController`
(`relay_controller`) was enabled and `FanController` (`fan_control --static`)
was enabled too, but it couldn't open the hub that relay_controller owned, so
the fans and SSR weren't driven at all. rack_controller replaces both: exactly
one hub owner.

Done on 2026-11-21:

1. Shut the whole rack down gracefully first:
   `rack-shutdown --no-relay all` and confirm every PC is ping-down. Stopping
   relay_controller opens all 8 relays.
2. Build on the SBC4: copy `sbc4/rack_controller/` (without `_src/`, `tests/`)
   to `/usr/userapps/rack_controller/` and run `make` there.
3. `groupadd -f rackctl`. rack_controller creates `/run/rack_controller/`
   (0750) and `ctl.sock` (0660) itself, both `root:rackctl`; without the group
   it warns and keeps them root-only.
4. Back up and disable `FanController`: `app_enabled="false"` in
   `/etc/webif/ua_FanController.conf` (backup `*.pre-rack`).
5. Repoint the existing RelayController entry at rack_controller, in
   `/etc/webif/ua_RelayController.conf` (backup `*.pre-rack`):
   ```
   app_name="rack_controller"
   app_cmd="rack_controller"
   app_args="--start-open"
   app_enabled="true"
   ```
6. Stop the old one (`kill -TERM $(pidof relay_controller)`, which opens every
   relay), then `/etc/webif/init.d/S20RelayController start`.
7. Check: `pidof rack_controller`; `/tmp/rack_controller/stderr` shows the
   fans and SSR attaching, `Channel N ... skipped: commanded open` for 0–7, and
   `Control socket listening`; a `state` request over the socket lists 8 relays
   `cmd=open`, fans `duty=0.50`, SSR `cmd=on`.

`--start-open` keeps every relay open so the PCs stay off for `map-verify` and
an ordered `rack-up`. **After the first rack-up, set `app_args=""`** so a
later SBC4 boot brings the relays back on as before.

**No automatic restart:** the Phidget launcher starts the app once. If
rack_controller crashes, nothing restarts it, and the hardware failsafe opens
every relay about 1 s later, an **unclean power cut to every running PC**.
Restart it by hand with `/etc/webif/init.d/S20RelayController start` (this
power-cycles the relays).

Rollback (rack shut down the same way): restore both `*.pre-rack` files, then
`/etc/webif/init.d/S20RelayController stop` and `start`.

Give the SBC4 a DHCP reservation for `192.168.8.165` on the Slate.

## 4. rackctl on the Slate

1. Copy `slate/rackctl/bin/rackctl` to `/usr/bin/rackctl`; `ln -s rackctl
   /usr/bin/rack-shutdown`, `ln -s rackctl /usr/bin/rack-up`.
2. `rackpki bundle orchestrator`; install to `/etc/rack/tls/` and
   `/etc/rack/orchestrator.password` (0600 root).
3. Copy `rack.example.toml` to `/etc/rack/rack.toml`. Leave the relay channels
   as they are (unverified) until `map-verify`.
4. `rackctl status` and `rackctl shutdown --dry-run all` print the plan without
   sending anything.

rackctl can also run from a dev-box build with the orchestrator bundle, which is
how the first live test is done before the Slate install.

## First live test (intel-d1babf only)

The only live test. Relays are never touched. Prerequisites: broker up, agent
installed on intel-d1babf, no one logged in on intel-d1babf.

1. Set Ceph `noout` (and `norebalance`) from any node as root:
   `ceph osd set noout && ceph osd set norebalance`.
2. `rackctl shutdown --force --no-relay intel-d1babf`
   (`--force` skips the partial-shutdown guard so the test doesn't depend on
   the other hosts' state; `--no-relay` means no relay command is published).
3. Confirm: `rack/pc/intel-d1babf/status` = `down`, then sustained ping-down
   of 192.168.8.194, and rackctl reporting success without a relay step.
4. Power intel-d1babf back on **by hand** (power button). Do not use
   `rackctl up` or any relay.
5. When it's back and its OSDs are up (`ceph -s`, `ceph osd tree`):
   `ceph osd unset norebalance && ceph osd unset noout`.

If the agent never powers off (e.g. polkit refused), rackctl times out and
reports; nothing is cut. Read `journalctl -u rack-agent` on the PC.

## Map-verify (phase 2, before any relay use)

Prerequisites: phase 2 deployed, every PC sustained-down
(`rackctl shutdown --no-relay all`) and every relay state `open`.
`rackctl shutdown all` with relays is only safe while every PC is already down,
since the mapping is still unverified. Channels 3, 5 and 7 aren't in the
mapping, so a shutdown never opens them; they must be opened too before
map-verify will run (see `slate/rackctl/` for how rackctl handles that).

`rackctl map-verify` then, per channel 0-7 (or only `--channels 3,4,5,6,7`):
closes it, waits up to 5 min (`--boot-timeout`) for a configured IP to answer, records it, shuts that PC down gracefully, and opens
the relay again. Paste the printed TOML snippet into `/etc/rack/rack.toml` and
back into `rack.example.toml` if it differs. Only after this is the mapping
trusted.

## Full-rack shutdown runbook

With the mapping verified (otherwise add `--no-relay` and leave relays alone):

1. Log out of every PC (see the polkit caveat). Check `ceph -s` is healthy.
2. `rack-shutdown all` (= `rackctl shutdown all`). It:
   1. sets Ceph maintenance `on` (intel-1ecbc8 sets `noout` + `norebalance`;
      waits for `rack/ceph/maintenance/state` = `on`);
   2. then one host at a time, in order: **intel-a163f8** (VM 200 media-stack,
      stopped by `pve-guests` while PVE quorum is still 5/5), **intel-91c45d**,
      **intel-d1babf**, **intel-177e78** (mon), **intel-1ecbc8** (mon leader +
      maintenance node, last);
   3. per host: `shutdown` -> sustained ping-down -> `cut_delay` 30 s -> relay
      `open` -> `state=open`.
3. If any host times out, rackctl stops there and does not cut it. Investigate
   that host; the hosts after it are still up.

Partial shutdowns (`rackctl shutdown <name...>`) are refused without `--force`
if they'd leave 2+ OSD hosts down, 2+ mon hosts down, or fewer than 3 PVE nodes
up. Set maintenance yourself first if the host has OSDs:
`rackctl ceph-maintenance on`.

## Full-rack power-up runbook

1. `rack-up all` (= `rackctl up all`): reverse order (intel-1ecbc8,
   intel-177e78, intel-d1babf, intel-91c45d, intel-a163f8), relay `close` with
   a 5 s stagger, wait for ping-up.
2. Once every OSD host pings, rackctl sets Ceph maintenance `off`.
3. Ping-up isn't OSD-up: check `ceph -s` and `pvecm status` until HEALTH_OK and
   quorum 5/5. VM 200 starts per its PVE start-on-boot setting.

After a rack_controller restart or SBC4 reboot all 8 relays come back on by
themselves (commanded state isn't persisted), so the rack boots without
`rack-up`.

## Cert renewal

CA: 10 years. Broker and client certs: 5 years. An expired broker cert breaks
every client, including the shutdown path, so don't let it lapse.

- Run `rackpki expiry` once a year; set a reminder about 4.5 years after
  issuance.
- Renew: restore `ca.key` from 1Password to `slate/pki/`, `rackpki server --ip
  192.168.8.1` and `rackpki client --all`, store the new keys as 1Password
  Document items, re-bundle, redeploy to `/etc/mosquitto/certs/` and each
  host's `/etc/rack/tls/`, then restart mosquitto, `rack-agent`, and
  `relay-mqtt`. Clients reconnect on their own.
- CA renewal (10 years) means a new `rackpki init-ca` and redeploying
  `ca.crt` plus every cert at once.
- Password rotation: update the `rack-mqtt` Environment, rerun
  `rackpki passwd` under `op run`, redeploy `passwd` and the affected host's
  `.password` file.
