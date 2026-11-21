# Rack control — repo structure

One repo, organized by target machine. Each top-level dir deploys to one place.
Claude Code runs in the repo root and writes into these; you deploy each dir to
its target (nothing here reaches the three machines on its own).

```
rack-control/
├── README.md                      # what this repo is; deploy map (dir -> machine)
├── CLAUDE.md                      # standing context for Claude Code (short; points at docs/)
├── docs/
│   ├── mqtt-shutdown-handoff.md   # whole-system build spec
│   └── rack-controller-merge.md   # merge relay_controller+fan_control -> one binary
│
├── sbc4/                          # -> deploys to the Phidget SBC4 (/usr/userapps/)
│   ├── rack_controller/           # MERGED C++ — ONE process owns all of HUB0004
│   │   ├── rack_controller.cpp    # relay_controller + fan_control merged:
│   │   │                          #   relays hub 3,4 (PCs ch 0,1,2,4,6, staggered+failsafe)
│   │   │                          #   fans hub 2 (OUT1100 duty), SSR hub 5 (REL1100)
│   │   │                          #   + local Unix-socket control (open/close/fan/ssr/state)
│   │   │                          #   NO MQTT / no network lib in the C++
│   │   ├── Makefile               # one target rack_controller, C++17 -lphidget22 -lpthread
│   │   └── _src/                  # the two ORIGINALS, kept for reference/rollback:
│   │       ├── relay_controller.cpp
│   │       └── fan_control.cpp
│   ├── relay-mqtt/                # NEW: Go sidecar, MQTT <-> C++ Unix socket
│   │   ├── main.go                # paho.mqtt.golang; subscribes rack/relay/+/set,
│   │   │                          #   forwards to rack_controller's Unix socket
│   │   ├── go.mod / go.sum
│   │   └── build.sh               # GOOS=linux GOARCH=arm GOARM=7 (SBC4 armhf)
│   └── README.md                  # ONE hub owner (why merged); build C++ on-SBC
│                                  #   (overlay/apt caveat); cross-build Go; ONE startup entry
│
├── slate/                         # -> deploys to the GL.iNet Slate (OpenWrt)
│   ├── mosquitto/
│   │   ├── mosquitto.conf          # listener 8883 LAN-only, TLS + mTLS, auth required
│   │   ├── acl                     # per-client ACLs (orchestrator/sbc4/pc certs)
│   │   └── passwd.example          # user list; real passwd generated from 1Password creds
│   ├── pki/                        # CA + cert GENERATION scripts only; keys are gitignored
│   │   ├── gen-ca.sh               # make the local CA (key stays out of git)
│   │   ├── gen-cert.sh             # per-client/server cert from the CA
│   │   └── README.md               # SAN notes, which host gets which cert, 0600 keys
│   ├── bin/
│   │   ├── rack-shutdown           # ad-hoc CLI (graceful down -> cut relay)
│   │   └── rack-up                 # ad-hoc CLI (close relay -> PC boots)
│   ├── etc/
│   │   └── rack.conf               # mapping: name -> IP -> relay channel; broker creds
│   └── README.md                   # opkg installs, enable mosquitto, PATH for bin/
│
├── pc-agent/                      # -> deploys to each mini-PC (x86-64 Linux/systemd)
│   ├── main.go                     # Go agent: payload==shutdown -> logind PowerOff
│   ├── go.mod / go.sum
│   ├── build.sh                    # GOOS=linux GOARCH=amd64 -> ONE binary for all 5
│   ├── rack-agent.service          # systemd unit (name+creds from config, not build)
│   ├── config.example              # per-PC: name, broker, pcN creds (real is 0600)
│   ├── polkit/
│   │   └── 10-rack-poweroff.rules  # narrow login1.power-off grant (if not root)
│   ├── install.sh                  # drops binary+unit+config, enables service
│   └── README.md                   # per-PC install, creds 0600, polkit vs root
│
└── .gitignore                     # ignore real secrets: passwd, *.creds, certs/
```

## Where the CURRENTLY RUNNING code goes

Put the two working source files into `sbc4/rack_controller/_src/` as the merge
inputs and rollback reference:
- `relay_controller.cpp`  -> `sbc4/rack_controller/_src/relay_controller.cpp`
- `fan_control.cpp`       -> `sbc4/rack_controller/_src/fan_control.cpp`

**Why merged, not two subdirs:** the SBC4 forbids two processes opening the same
VINT hub, and both programs are on HUB0004 (relays 3,4; fans 2; SSR 5). So they
MUST become ONE binary that owns the whole hub. See `rack-controller-merge.md`.

Claude Code MERGES these two into `sbc4/rack_controller/rack_controller.cpp`
(unified channel model + the LOCAL Unix-socket control surface; NO MQTT in C++).
Commit the two originals in `_src/` FIRST — they're the proven safety-critical
code; `_src/` is your rollback baseline. MQTT lives in the Go `relay-mqtt/` sidecar.

## Deploy map (dir -> machine -> path)

| repo dir              | machine        | lands at                         |
|-----------------------|----------------|----------------------------------|
| `sbc4/rack_controller`| Phidget SBC4   | `/usr/userapps/rack_controller/` (build on-device) — ONE binary, ONE startup entry, owns the whole hub |
| `sbc4/relay-mqtt`     | Phidget SBC4   | its own user-app entry (Go sidecar) |
| `slate/`              | GL.iNet Slate  | `/etc/mosquitto/`, `/usr/bin/`, `/etc/rack/` |
| `pc-agent/`           | each mini-PC   | `/etc/systemd/system/`, `/usr/local/bin/`, `/etc/polkit-1/rules.d/` |

NOTE: only ONE Phidget-opening process may run on the SBC4 (hub single-ownership).
That's `rack_controller`. If either old binary (relay_controller / fan_control)
is still registered in Startup Settings, REMOVE it — a second hub opener
re-creates the contention. The Go sidecar does NOT open Phidgets (talks to the
socket), so it coexists fine.

## .gitignore must exclude real secrets (critical — TLS keys + passwords)

```
# secrets — never commit
slate/mosquitto/passwd
**/*.creds
**/certs/
**/pki/*.key
**/pki/*.crt
**/pki/ca.*
*.key
*.pem
*.crt
config            # real per-PC agent config (has creds); keep config.example
```
Commit only `*.example` / placeholder files. Real passwords come from the
1Password env at deploy time; CA key, client keys, and certs are generated by the
pki/ scripts and deployed per-host (keys 0600), never committed. An exposed CA
key = anyone can mint a client cert and command the rack — treat it like the
crown jewel.

## CLAUDE.md (keep it short)

Standing context only — point at the specs, don't inline them:
```
# rack-control
Homelab rack power/cooling control across 3 machines (SBC4, Slate, Proxmox PCs).
Specs: docs/mqtt-shutdown-handoff.md (whole system), docs/rack-controller-merge.md
(merge the two C++ programs into one — the SBC4 hub forbids two processes on it).
Originals to merge are in sbc4/rack_controller/_src/ — commit before merging.
You write files here; deployment to the 3 machines is manual (you cannot reach
them, cannot cross-compile to them). Ask the open questions before starting.
```

## First steps

1. `git init` in `rack-control/`.
2. Drop the two originals into `sbc4/rack_controller/_src/`
   (`relay_controller.cpp`, `fan_control.cpp`). The old fan `Makefile` is
   reference only — the merged build gets a new one.
3. Put the specs at `docs/mqtt-shutdown-handoff.md` and
   `docs/rack-controller-merge.md`.
4. Add `CLAUDE.md` and `.gitignore`.
5. Commit this baseline (clean rollback point — the proven C++ in _src/).
6. Start Claude Code: "Read docs/rack-controller-merge.md and
   docs/mqtt-shutdown-handoff.md. First merge the two _src/ programs into
   sbc4/rack_controller/rack_controller.cpp with the Unix-socket control surface.
   Then build slate/, pc-agent/, and the Go relay-mqtt sidecar. Ask me the open
   questions first. You can't reach the machines — just write files."
