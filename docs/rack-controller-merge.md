# Merge brief: relay_controller + fan_control -> one rack_controller

**Status: MERGED (code), not yet deployed.** `sbc4/rack_controller/` now
holds `rack_controller.cpp`, `core.h`/`core.cpp`, `hal.h`/`hal_phidget.h`,
`Makefile`, `rack_controller.service` and `tests/`. See
[../sbc4/rack_controller/README.md](../sbc4/rack_controller/README.md). It was
verified with `make test` (fake HAL, real AF_UNIX socket), `make check-mac`
(Phidget22 headers) and `make xcheck` (relay-mqtt's real socket client and
parser against this server). It has not run on hardware. The SBC4 keeps
running the original `relay_controller` until the
[swap](DEPLOY.md#sbc4-systemd-swap-phase-2). Whole-system spec:
[rack-mqtt-shutdown-handoff.md](rack-mqtt-shutdown-handoff.md).

### Result vs. this brief

- One `Channel{Kind}` table (relays 0-7, then fans, then SSR), one
  channel-state mutex (never held across a Phidget call), one heartbeat
  thread that feeds every armed channel, and one set of event handlers. A
  second mutex only serializes output changes, so a slow device can't stall
  the heartbeat.
- Fans and SSR are opened before the blocking relay wait, so cooling comes up
  first. A relay attach failure is still fatal, as the brief requires.
- Relays use the blocking startup verbatim. The stagger is index-based: relay
  *i* gets slot *i*. It skips relays commanded `open`, and relays already on
  from a `close`. A socket `close` arms the failsafe before `setState(1)`.
- Fans and SSR use dynamic attach and are re-driven to their **commanded**
  value on re-attach.
- On a failsafe trip, the channel rejects commands (`err ... failsafe
  tripped`) until rack_controller reopens it, which it does automatically.
  After a VINT blip or a trip, a relay is reported `cmd=open` and stays open
  and unarmed, as in relay_controller, until `close <ch>`.
- fan_control's `--static` was dropped, because fans and SSR are always
  dynamic. Ports 3 and 4 are reserved for the relays.

## Why (the hard constraint)

The Phidget SBC4 does NOT allow multiple programs to open channels on the same
VINT hub. relay_controller (hub ports 3,4) and fan_control (hub ports 2,5) are
all on the SAME hub (HUB0004, S/N 592109). Running them as two processes causes
device-in-use contention / instability. They MUST be one process that owns the
whole hub.

## Inputs (in `sbc4/rack_controller/_src/`, never modified)

- `relay_controller.cpp`: 2x REL1000, hub ports 3 & 4, 8 channels, all driven.
  **Blocking startup**: `Phidget_openWaitForAttachment` (5 s timeout, exit on
  failure) for every channel, all start FALSE, then a one-time staggered
  power-on (2.5 s between channels, each channel's failsafe armed only as it
  turns on), then a heartbeat that resets the 1000 ms failsafe every 1/3 of the
  failsafe time. Serial is `-1` (TODO) in the original.
- `fan_control.cpp`: OUT1100 (hub port 2, 4ch, PWM duty) + REL1100 (hub port 5,
  4ch, SSR on/off). CLI flags (`--fan/--ssr/--duty/--static/--failsafe-ms`).
  Default dynamic model: non-blocking `Phidget_open`, each channel driven on
  ATTACH and re-driven on re-attach, mutex-guarded attached state, failsafe.

Both share a `Channel`-style struct, attach/detach/error handlers and a failsafe
reset heartbeat loop. The merge should UNIFY these, not duplicate them. Note the
two startup models differ: relays block, fans/SSR are attach-driven. The merge
keeps that difference.

## Output: `rack_controller`

One binary, one process, owns ALL of HUB0004:
- ports 3,4 -> relays (all 8 channels): relay_controller's blocking startup,
  stagger and failsafe
- port 2    -> fans (OUT1100 PWM duty)
- port 5    -> SSR (REL1100 on/off)

Files: `rack_controller.cpp`, a thin HAL header separating the Phidget calls,
`Makefile`, `rack_controller.service`, `tests/` (run on macOS against a fake
HAL).

### Requirements

1. **One unified channel model.** Merge both channel structs into one that
   carries a `Kind` (Relay | FanPwm | Ssr). One create loop, one set of
   attach/detach/error handlers, one mutex, one heartbeat loop that feeds the
   failsafe for every armed channel regardless of kind. Do NOT keep two
   parallel copies of the machinery.

2. **Preserve each kind's behavior exactly:**
   - Relays (3,4): the **blocking startup verbatim**: `openWaitForAttachment`,
     start FALSE, one-time 2.5 s stagger, failsafe armed as each turns on.
     Added: per-channel commanded state. The stagger skips channels commanded
     open; `open` = `setState(0)` with the failsafe still fed.
   - Fans (2): dynamic attach -> set duty cycle (default 0.5), failsafe armed.
   - SSR (5): dynamic attach -> set ON, failsafe armed.

3. **Commanded state is NOT persisted.** On restart all 8 relays come back on
   via the stagger, the same as relay_controller today.

4. **Local Unix-socket control surface** (shared contract with the relay-mqtt
   sidecar):
   - Path `/run/rack_controller/ctl.sock`; systemd
     `RuntimeDirectory=rack_controller`, dir 0750, socket 0660, group `rackctl`.
   - One ASCII request per line, at most 128 bytes; one connection may send many
     requests.
   - `open <ch>` / `close <ch>`: relay ch 0-7 (0-3 = port 3 ch0-3, 4-7 = port 4
     ch0-3). Every channel is commandable; which ones power PCs is a `rack.toml`
     fact. Reply `ok open <ch>` / `ok close <ch>` or `err <reason>`.
   - `fan <ch> <duty 0.0-1.0>` (ch 0-3), `ssr <ch> on|off` (ch 0-3). Reply
     `ok ...` or `err ...`.
   - `state`: lines `relay <ch> cmd=<open|closed> attached=<0|1>`,
     `fan <ch> duty=<x.xx> attached=<0|1>`, `ssr <ch> cmd=<on|off> attached=<0|1>`,
     then `end`.
   - Unknown or malformed input -> `err <reason>`. Socket errors never crash or
     `exit` the process.
   This lets cooling and relays be retuned at runtime WITHOUT restarting
   (critical: a restart is a full rack power cycle).

5. **Config: CLI flags** (operator choice). `--serial` (default 592109),
   `--fan`, `--ssr`, `--duty`, `--failsafe-ms` (default 1000), `--socket`.

6. **Failsafe semantics preserved for all kinds:** on loss of comms, outputs go
   to their safe state (relays open = PCs off; fans duty 0; SSR off). All wired
   NORMALLY OPEN / fail-to-off. Keep the 1/3-failsafe-time reset cadence.

7. **systemd unit** `rack_controller.service`: `Restart=always`,
   `Conflicts=relay_controller.service`, `RuntimeDirectory=rack_controller`.

### Do NOT

- Do not run relays and fans as two processes (the hub forbids it).
- Do not duplicate the Phidget/attach/failsafe machinery: unify it.
- Do not add MQTT or any network lib to this C++. It speaks ONLY the local
  Unix socket. MQTT lives in the separate Rust `relay-mqtt` sidecar.
- Do not change the proven blocking-open/stagger/failsafe logic for relays
  beyond adding the commanded-state override and the socket control.
- Do not modify `_src/`; it is the rollback baseline.

### Build

- One Makefile target `rack_controller`, C++17, `-lphidget22 -lpthread`. The
  real build happens on the SBC4 only.
- `make test`: protocol parser, commanded-state logic and HAL fake, built with
  clang++ on the dev box.
- `make check-mac`: `clang++ -std=c++17 -fsyntax-only -F/Library/Frameworks
  -framework Phidget22` on `rack_controller.cpp`.

### Deploy note

- ONE binary, ONE systemd unit, owning the whole hub. The swap from
  `relay_controller.service` to `rack_controller.service` cuts power to every
  PC, so it's done with the rack already shut down. Procedure:
  [DEPLOY.md](DEPLOY.md#sbc4-systemd-swap-phase-2).
- `fan_control` must not run anywhere (unit or Startup Settings entry) once
  rack_controller owns the hub.
