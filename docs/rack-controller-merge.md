# Merge brief: relay_controller + fan_control -> one rack_controller

## Why (the hard constraint)

The Phidget SBC4 does NOT allow multiple programs to open channels on the same
VINT hub. relay_controller (hub ports 3,4) and fan_control (hub ports 2,5) are
all on the SAME hub (HUB0004, S/N 592109). Running them as two processes causes
device-in-use contention / instability. They MUST be one process that owns the
whole hub.

## Inputs (provided)

- `relay_controller.cpp` — 2x REL1000, hub ports 3 & 4, 8 channels. 5 PCs on
  channels 0,1,2,4,6 (non-contiguous). Non-blocking Phidget_open, per-channel
  attach-driven staggered power-on, per-channel hardware failsafe heartbeat.
- `fan_control.cpp` — OUT1100 (hub port 2, 4ch, PWM duty) + REL1100 (hub port 5,
  4ch, SSR on/off). CLI flags (--fan/--ssr/--duty/--static/--failsafe-ms),
  non-blocking open, attach-driven, failsafe.

Both already share the same architecture: a `Channel` struct, attach/detach/error
handlers, a mutex-guarded attached-state array, and a failsafe reset heartbeat
loop. The merge should UNIFY these, not duplicate them.

## Output: `rack_controller`

One binary, one process, owns ALL of HUB0004:
- ports 3,4 -> relays (PCs) — keep relay_controller's staggered power-on + failsafe
- port 2    -> fans (OUT1100 PWM duty)
- port 5    -> SSR (REL1100 on/off)

### Requirements

1. **One unified channel model.** Merge both `Channel` structs into one that
   carries a `Kind` (Relay | FanPwm | Ssr). One create/open loop, one set of
   attach/detach/error handlers, one mutex, one heartbeat loop that feeds the
   failsafe for every attached channel regardless of kind. Do NOT keep two
   parallel copies of the machinery.

2. **Preserve each kind's behavior exactly:**
   - Relays (3,4): attach -> staggered power-on (index-based stagger), failsafe
     armed, commanded-state override (a channel commanded open stays open across
     re-attach — see the MQTT handoff; this is the one relay logic addition).
   - Fans (2): attach -> set duty cycle (default 0.5), failsafe armed.
   - SSR (5): attach -> set ON, failsafe armed.

3. **Local Unix-socket control surface** (shared with the MQTT sidecar, per the
   main handoff). One socket, commands for all kinds:
   - `open <ch>` / `close <ch>`   -> relay channel (validate against {0,1,2,4,6})
   - `fan <ch> <duty 0.0-1.0>`     -> set a fan channel's duty at runtime
   - `ssr <ch> <on|off>`           -> set an SSR channel
   - `state`                       -> report all channels
   This is what lets cooling and relays be retuned at runtime WITHOUT restarting
   (critical: the process owns the hub continuously; restarts are to be avoided).

4. **Config:** relay channel set, fan port/channels + default duty, SSR
   port/channels, failsafe ms. Keep fan_control's CLI flags OR move to a small
   config — operator preference. Defaults: relays on 3,4 (PC channels 0,1,2,4,6),
   fans port 2 @ 50%, SSR port 5 on, failsafe 1000ms.

5. **Startup model unchanged:** non-blocking Phidget_open so it starts with no
   devices attached; each channel comes up on its ATTACH. Never exit on a missing
   device or absent socket client. One launcher entry (webif S## or systemd)
   runs this single binary.

6. **Failsafe semantics preserved for all kinds:** on loss of comms, outputs go
   to their safe state (relays open = PCs off; fans duty 0; SSR off). All wired
   NORMALLY OPEN / fail-to-off. Keep the ⅓-failsafe-time reset cadence.

### Do NOT

- Do not run relays and fans as two processes (the hub forbids it).
- Do not duplicate the Phidget/attach/failsafe machinery — unify it.
- Do not add MQTT or any network lib to this C++ — it speaks ONLY the local
  Unix socket. MQTT lives in the separate Go sidecar.
- Do not change the proven failsafe/stagger logic for relays beyond adding the
  commanded-state override and the socket control.

### Build

- One Makefile target `rack_controller`, C++17, `-lphidget22 -lpthread`.
- Replaces the separate relay_controller + fan_control build targets.

### Deploy note

- This is ONE binary now, ONE startup entry, owning the whole hub. The old two
  entries (if both were registered) must be removed — having either old binary
  also running would re-create the hub contention.
