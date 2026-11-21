# sbc4: Phidget SBC4 relay and cooling control

Target: `phidgetsbc.local`, 192.168.8.165 (DHCP; give it a reservation on the
Slate), armv7l, PhidgetSBC4 OS 4.0.0. Built-in VINT hub HUB0004, serial
**592109**:

| hub port | device  | use |
|----------|---------|-----|
| 2        | OUT1100 | fans (PWM duty) |
| 3        | REL1000 | relays ch 0-3 |
| 4        | REL1000 | relays ch 4-7 |
| 5        | REL1100 | SSR |

Which relay channel powers which PC is in `rack.toml` on the Slate and is
unverified until `rackctl map-verify`.

## One hub owner

The SBC4 doesn't allow two processes to open channels on the same VINT hub, and
relays, fans and SSR are all on HUB0004. So exactly one process may own the hub.

- **Today:** `relay_controller` (from `rack_controller/_src/relay_controller.cpp`)
  runs as the systemd service `relay_controller`. `fan_control` must not run
  alongside it.
- **After phase 2:** `rack_controller` (the merge of both originals) is the only
  owner. `relay-mqtt` never opens Phidgets; it only talks to rack_controller's
  Unix socket, so it coexists.

Also check the Phidget web UI's Startup Settings: no second hub opener may be
registered there.

## Why merged

Running relay_controller and fan_control side by side re-creates the hub
contention. One process with one channel model, one mutex and one failsafe
heartbeat owns ports 2-5. Relays keep relay_controller's blocking startup
(`openWaitForAttachment`, one-time 2.5 s stagger, 1000 ms failsafe reset every
1/3 of the window); fans and SSR keep fan_control's attach-driven model. Brief:
[../docs/rack-controller-merge.md](../docs/rack-controller-merge.md).

## Phase-2 status

The merge is **deferred**. `rack_controller/` currently contains only `_src/`,
the two originals, which are the rollback baseline and are never edited.
`relay-mqtt/` is built and tested against a fake controller that implements the
socket contract (`/run/rack_controller/ctl.sock`). It is not useful on hardware
until the merge ships, so until then relays are not MQTT-commandable and
`rackctl` runs with `--no-relay`.

## systemd units

| unit | user | notes |
|------|------|-------|
| `relay_controller.service` | (existing) | runs today; disabled at the phase-2 swap |
| `rack_controller.service` | as shipped (hub access) | `Restart=always`, `Conflicts=relay_controller.service`, `RuntimeDirectory=rack_controller` (socket 0660, group `rackctl`) |
| `relay-mqtt.service` | `relay-mqtt`, group `rackctl` | `After=rack_controller.service`, no `Requires=`; reconnects forever to broker and socket |

Stopping or restarting the hub owner opens every relay (graceful exit or
failsafe), and commanded state isn't persisted, so a restart is a full rack
power cycle: all 8 relays come back on via the stagger. Restarting `relay-mqtt`
doesn't touch the relays.

The swap procedure (rack shut down first, `disable --now relay_controller`,
then `enable --now rack_controller`) is in
[../docs/DEPLOY.md](../docs/DEPLOY.md#sbc4-systemd-swap-phase-2).

## Build

- `rack_controller`: built on the SBC4 only (`make`, C++17, `-lphidget22
  -lpthread`). `make test` and `make check-mac` run on the dev box.
- `relay-mqtt`: cross-built on the dev box (`make` ->
  `cargo zigbuild --release --target armv7-unknown-linux-musleabihf`), one
  static binary copied over. Nothing installed via apt.
