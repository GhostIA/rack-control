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
  ran as the Phidget user app `RelayController`. `FanController`
  (`fan_control`) was also enabled but couldn't open the shared hub.
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

The merge is **done** and **deployed** (2026-11-21): `rack_controller/`
(`rack_controller.cpp`, `core.*`, the HAL headers, `Makefile`, tests). Its README has
the details: [rack_controller/README.md](rack_controller/README.md). It has
been tested only against a fake HAL, and against relay-mqtt's real socket
client (`make xcheck`). It has not run on hardware yet. Until the swap is done
on the SBC4, the old `relay_controller` still owns the hub, relays aren't
MQTT-commandable, and `rackctl` runs with `--no-relay`. `_src/` stays the
rollback baseline and is never edited.

## How they run (Phidget user apps, no systemd units)

| app | config | notes |
|-----|--------|-------|
| `rack_controller` | `/etc/webif/ua_RelayController.conf` (repointed), `S20RelayController` | root; creates `/run/rack_controller/` + `ctl.sock` as `root:rackctl` 0750/0660; started once, **not restarted** if it dies |
| `FanController` | `ua_FanController.conf` | disabled: rack_controller owns the fans |
| `relay-mqtt.service` | `relay-mqtt`, group `rackctl` | `After=rack_controller.service`, no `Requires=`; reconnects forever to broker and socket |

Stopping or restarting the hub owner opens every relay (graceful exit or
failsafe), and commanded state isn't persisted, so a restart is a full rack
power cycle: all 8 relays come back on via the stagger. Restarting `relay-mqtt`
doesn't touch the relays.

The swap procedure (rack shut down first, `disable --now relay_controller`,
then `enable --now rack_controller`) is in
[../docs/DEPLOY.md](../docs/DEPLOY.md#sbc4-controller-swap-phidget-user-app).

## Build

- `rack_controller`: built on the SBC4 only (`make`, C++17, `-lphidget22
  -lpthread`; `make install` puts it in `/usr/userapps/rack_controller/`).
  `make test`, `make check-mac` and `make xcheck` run on the dev box.
- `relay-mqtt`: cross-built on the dev box (`make` ->
  `cargo zigbuild --release --target armv7-unknown-linux-musleabihf`), one
  static binary copied over. Nothing installed via apt.
