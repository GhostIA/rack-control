# rack_controller

The single owner of the SBC4's VINT hub (HUB0004, serial 592109). It merges
`_src/relay_controller.cpp` and `_src/fan_control.cpp` into one process, because
the hub won't let two processes open its channels. It adds a local Unix socket
so the `relay-mqtt` sidecar can command the relays. It has no network code.

| hub port | device | kind | startup model |
|---|---|---|---|
| 3, 4 | REL1000 x2 | relay ch 0-7 (0-3 = port 3, 4-7 = port 4) | relay_controller, **blocking**, verbatim |
| 2 | OUT1100 | fan PWM, default duty 0.5 | fan_control, dynamic attach |
| 5 | REL1100 | SSR, default on | fan_control, dynamic attach |

`_src/` holds the two originals. They are the rollback baseline and are never
edited.

## Files

| file | what |
|---|---|
| `rack_controller.cpp` | `main`: wires the Phidget HAL to the core, startup order, signals |
| `hal.h` | the thin HAL interface (no Phidget includes) |
| `hal_phidget.h` | the real HAL: one-to-one Phidget22 calls |
| `core.h`, `core.cpp` | unified `Channel{Kind}` model, mutex, heartbeat, stagger, commands, CLI, socket server |
| `tests/` | unit tests on a fake HAL, `test_server`, and the relay-mqtt cross-check |

## Behavior

Startup, in order:

1. **One create loop** for all 16 channels: serial, hub port, channel and the
   attach/detach/error handlers. Any failure is fatal (`exit 1`). The Phidget launcher does not restart it.
2. **One heartbeat thread** resets the failsafe on every armed channel of any
   kind, every failsafe-ms / 3 (333 ms by default).
3. **Fans and SSR, as in fan_control:** non-blocking `open`, started first so
   cooling comes up before the relay wait. On every attach the channel is
   driven to its commanded value (duty 0.5 / SSR on at start) and its failsafe
   is armed. A re-attach drives it again.
4. **Relays, verbatim from relay_controller:** `openWaitForAttachment` (5 s,
   fatal), `setState(0)` (fatal). No failsafe is armed yet. A SIGTERM during
   this phase stops before the next relay.
5. **Control socket** comes up.
6. **One-time relay stagger:** relay *i* gets slot *i*, 2.5 s apart.
   `setState(1)` and then `enableFailsafe(1000)` are both fatal on failure. Each
   relay is armed only as it turns on. A relay commanded `open` before its slot
   is skipped, as is one that was lost (detached or tripped) before its slot.
   A relay already switched on by `close` is left alone. A skipped relay keeps
   its empty slot.

On SIGINT or SIGTERM it shuts down gracefully: relays open, fan duty 0, SSR off,
handles closed and the socket unlinked. On a crash or hang, the hardware
failsafe drives every armed output off about 1 s later. Commanded state is not
persisted, so a restart brings all 8 relays back on through the stagger.

Failsafe trip: the channel is marked tripped, the heartbeat stops feeding it,
and commands get `err <kind> <ch> failsafe tripped`. Per Phidget, it rejects
input until reopened, so the main thread reopens it (close + non-blocking
open, retried every 5 s) and the attach clears the trip. Fans and SSR come
back at their commanded value. A relay comes back open.

Relay detach or trip: the relay is physically open, so `state` reports
`cmd=open` from then on, and relay-mqtt publishes `open`. As in
relay_controller, a re-attached relay isn't re-driven. `close <ch>` (for
example from `rack-up`) arms it and closes it again.

Locking: `mu_` guards the channel table and is never held across a Phidget
call. A second mutex serializes output changes: commands, the stagger and the
attach-time drive. So a slow device round trip can't stall the heartbeat for
the other channels.

## Socket protocol

Default `/run/rack_controller/ctl.sock`, mode 0660, group `rackctl`. Send one
ASCII request per line (`\n`). A line over 128 bytes is rejected with
`err line too long` and the connection stays usable. One connection can send
many requests, and up to 16 clients can connect at once.

| request | reply |
|---|---|
| `open <ch 0-7>` | `ok open <ch>`: `setState(0)`, failsafe still fed |
| `close <ch 0-7>` | `ok close <ch>`: arms the failsafe first if it isn't armed, then `setState(1)` |
| `fan <ch 0-3> <duty 0.0-1.0>` | `ok fan <ch> <duty %.2f>` |
| `ssr <ch 0-3> on\|off` | `ok ssr <ch> on\|off` |
| `state` | `relay <ch> cmd=<open\|closed> attached=<0\|1>` x8, `fan <ch> duty=<x.xx> attached=<0\|1>`, `ssr <ch> cmd=<on\|off> attached=<0\|1>`, then `end` |
| anything else | `err <reason>` |

Parsing is strict. A channel is one digit, separated by single spaces, and a
duty is a plain decimal. A failed `setState` (or a detached or tripped channel)
returns `err ...` and is logged, and the commanded state stays unchanged. No
socket or request error ever exits the process. If the socket can't be bound,
it retries every 2 s. It never deletes a non-socket file at that path.

Try it by hand on the SBC4: `printf 'state\n' | socat - UNIX-CONNECT:/run/rack_controller/ctl.sock`.

## CLI

```
rack_controller [--serial N] [--fan PORT[:CHANS]] [--ssr PORT[:CHANS]]
                [--duty D] [--failsafe-ms N] [--socket PATH] [--help]
```

The defaults are `--serial 592109 --fan 2:0,1,2,3 --ssr 5:0,1,2,3 --duty 0.5
--failsafe-ms 1000 --socket /run/rack_controller/ctl.sock`. Ports 3 and 4 are
reserved for the relays, and fan and SSR can't share a port. fan_control's
`--static` is gone, because fans and SSR are always dynamic now.

## Build and test

On the dev Mac (no hardware):

```
make test        # clang++ -std=c++17 -Wall -Wextra -Werror; fake HAL + real AF_UNIX socket
make check-mac   # -fsyntax-only against /Library/Frameworks/Phidget22.framework
make xcheck      # relay-mqtt's real UnixController/Bridge/parser vs. this socket server
```

On the SBC4 (the only real build; needs `libphidget22`):

```
make             # g++ -std=c++17 -O2 ... -lphidget22 -lpthread -o rack_controller
sudo make install   # /usr/userapps/rack_controller/rack_controller
```

## Swap from relay_controller (rack shut down first)

Any restart of the hub owner power-cycles every relay. Stopping it opens all 8,
and the start staggers them all back on. So do this only with the rack already
down. The full procedure is in
[../../docs/DEPLOY.md](../../docs/DEPLOY.md#sbc4-controller-swap-phidget-user-app).

1. `rack-shutdown --no-relay all`, then confirm every PC is ping-down.
2. On the SBC4: `make && sudo make install`.
3. `groupadd -f rackctl` (rack_controller sets `/run/rack_controller/` and
   `ctl.sock` to `root:rackctl` itself).
4. Repoint `/etc/webif/ua_RelayController.conf` at rack_controller
   (`app_name="rack_controller"`, `app_cmd="rack_controller"`, `app_args` empty
   or `--start-open`, `app_enabled="true"`) and disable `ua_FanController.conf`.
5. `kill -TERM $(pidof relay_controller)`, then
   `/etc/webif/init.d/S20RelayController start`.
6. `/tmp/rack_controller/stderr` shows the fans and SSR attaching, the relay
   channels, and `Control socket listening`.

There is no systemd unit: the Phidget user-app launcher starts it once and does
not restart it. Rollback: restore the `*.pre-rack` copies of both ua configs and
restart via `S20RelayController stop` and `start`.

## Untested on hardware

Everything here ran only against the fake HAL. Check these on the SBC4 at the
swap:

- `Group=rackctl` with `User=root`, and that the socket comes out `root:rackctl 0660`.
- That a REL1000 re-attach really comes back unarmed.
- The exact Phidget error event for a failsafe trip (`EEPHIDGET_FAILSAFE`).
