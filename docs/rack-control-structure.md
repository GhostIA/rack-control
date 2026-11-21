# Rack control: repo structure

One repo, organized by target machine. Each top-level dir deploys to one place.
Claude Code runs in the repo root and writes into these; you deploy each dir to
its target by hand (nothing here reaches the machines on its own). Build output
goes in each component's `bin/` (Go) or `target/` (Rust), both gitignored.

```
rack-control/
├── README.md                      # what this repo is; deploy map (dir -> machine)
├── CLAUDE.md                      # standing context for Claude Code (short; points at docs/)
├── docs/
│   ├── rack-mqtt-shutdown-handoff.md  # whole-system spec (topics, ACLs, decisions)
│   ├── rack-controller-merge.md       # phase 2: merge relay_controller+fan_control
│   ├── rack-control-structure.md      # this file
│   └── DEPLOY.md                      # deploy order, SBC4 swap, first live test, runbooks
│
├── sbc4/                          # -> the Phidget SBC4 (armv7l)
│   ├── rack_controller/           # PHASE 2 (deferred): merged C++, ONE process owns HUB0004
│   │   ├── rack_controller.cpp    # relays ports 3,4 (all 8 ch, blocking startup+stagger+failsafe)
│   │   │                          #   fans port 2 (OUT1100), SSR port 5 (REL1100), attach-driven
│   │   │                          #   + local Unix socket /run/rack_controller/ctl.sock
│   │   │                          #   NO MQTT / no network lib in the C++
│   │   ├── *.h                    # thin HAL header (Phidget calls behind it)
│   │   ├── Makefile               # rack_controller, test, check-mac
│   │   ├── rack_controller.service  # Restart=always, Conflicts=relay_controller.service
│   │   ├── tests/                 # macOS tests against a fake HAL
│   │   └── _src/                  # the two ORIGINALS, rollback baseline (never edited):
│   │       ├── relay_controller.cpp
│   │       └── fan_control.cpp
│   ├── relay-mqtt/                # Rust sidecar: MQTT <-> rack_controller socket
│   │   ├── Cargo.toml / Cargo.lock
│   │   ├── src/                   # rumqttc (rustls); rack/relay/+/set -> socket
│   │   ├── Makefile               # cargo zigbuild armv7-unknown-linux-musleabihf
│   │   ├── relay-mqtt.service     # user relay-mqtt, group rackctl, After= (no Requires=)
│   │   ├── config.example.toml
│   │   └── e2e/                   # Docker broker + fake controller (local only)
│   └── README.md                  # one hub owner, why merged, phase-2 status, units
│
├── slate/                         # -> the GL.iNet Slate 7 (OpenWrt, arm64)
│   ├── mosquitto/
│   │   ├── mosquitto.conf         # ONLY listener 8883 192.168.8.1; mTLS + password
│   │   ├── acl                    # per-user rules from the topic table (no secrets)
│   │   └── passwd.example         # user list; real passwd made by rackpki
│   ├── rackpki/                   # Go PKI tool (dev box): init-ca, server, client,
│   │   ├── go.mod / go.sum        #   passwd ($7$), bundle, expiry. Replaces gen-*.sh
│   │   ├── *.go
│   │   ├── Makefile               # builds for the host
│   │   └── e2e/                   # Docker mosquitto with generated certs (local only)
│   ├── rackctl/                   # Go orchestrator; argv[0] symlinks rack-shutdown, rack-up
│   │   ├── go.mod / go.sum
│   │   ├── *.go                   # shutdown, up, status, map-verify, ceph-maintenance
│   │   ├── Makefile               # CGO_ENABLED=0 GOOS=linux GOARCH=arm64
│   │   ├── rack.example.toml      # mapping name -> IP -> relay ch, roles, order, timeouts
│   │   └── e2e/                   # Docker broker + fake agents (local only)
│   ├── pki/                       # rackpki OUTPUT, gitignored entirely (CA key lives here)
│   └── README.md                  # opkg mosquitto-ssl, sysupgrade.conf, init script, renewal
│
├── pc-agent/                      # -> each mini-PC (x86_64 Proxmox VE)
│   ├── Cargo.toml / Cargo.lock
│   ├── src/                       # payload == shutdown -> logind PowerOff; Ceph maintenance
│   ├── Makefile                   # cargo zigbuild x86_64-unknown-linux-musl -> ONE binary for all 5
│   ├── rack-agent.service         # user rack-agent (name/creds from config, not build)
│   ├── config.example.toml        # per-PC: name, broker, cert paths, dry_run, ceph_maintenance
│   ├── polkit/
│   │   └── 10-rack-poweroff.rules # grants only login1.power-off to rack-agent
│   ├── e2e/                       # Docker broker + agent in dry_run (local only)
│   └── README.md
│
└── .gitignore                     # secrets, slate/pki/, bin/, target/, e2e/certs/, e2e/tmp/
```

The planned shell pieces are gone: `slate/bin/rack-shutdown`, `slate/bin/rack-up`
and `slate/etc/rack.conf` became `rackctl` + `rack.toml`; `slate/pki/gen-ca.sh`
and `gen-cert.sh` became `rackpki`; `pc-agent/install.sh` became the
`pc-agent install` subcommand. The exact file names inside each component dir
are whatever that component ships; the tree shows intent.

`e2e/` harnesses run only on the dev Mac (Docker, e.g. `eclipse-mosquitto:2.0.20`)
and must never reach 192.168.8.0/24. Their generated certs and scratch files
(`e2e/certs/`, `e2e/tmp/`) are gitignored.

## Where the CURRENTLY RUNNING code goes

The two working source files are in `sbc4/rack_controller/_src/` as the merge
inputs and rollback reference (baseline commit `c18c6bf`):
- `relay_controller.cpp`: runs today on the SBC4 as `relay_controller.service`
- `fan_control.cpp`

**Why merged, not two subdirs:** the SBC4 forbids two processes opening the same
VINT hub, and both programs are on HUB0004 (relays 3,4; fans 2; SSR 5). So they
MUST become ONE binary that owns the whole hub. See
[rack-controller-merge.md](rack-controller-merge.md). The merge is deferred to
phase 2.

## Deploy map (dir -> machine -> path)

| repo dir               | machine              | lands at |
|------------------------|----------------------|----------|
| `slate/mosquitto/`     | Slate (192.168.8.1)  | `/etc/mosquitto/` (+ `/etc/mosquitto/certs/`) |
| `slate/rackctl/`       | Slate                | `/usr/bin/rackctl`, symlinks `rack-shutdown`, `rack-up`; `/etc/rack/rack.toml`, `/etc/rack/tls/` |
| `slate/rackpki/`       | dev box              | not deployed; writes `slate/pki/` |
| `pc-agent/`            | each mini-PC         | binary, `rack-agent.service`, `/etc/polkit-1/rules.d/10-rack-poweroff.rules`, `/etc/rack/` (via `pc-agent install`) |
| `sbc4/relay-mqtt/`     | SBC4 (192.168.8.165) | binary + `relay-mqtt.service`; `/etc/rack/` (phase 2) |
| `sbc4/rack_controller/`| SBC4                 | built on-device; `rack_controller.service` replaces `relay_controller.service` (phase 2) |

NOTE: only ONE Phidget-opening process may run on the SBC4 (hub single-ownership).
Today that's `relay_controller`; after phase 2 it's `rack_controller`. The
units' `Conflicts=` enforces it for systemd, but also make sure no old binary is
registered in the Phidget web UI's Startup Settings. The sidecar does NOT open
Phidgets (it talks to the socket), so it coexists fine.

## .gitignore: real secrets never committed

See the repo `.gitignore`. It covers `.env` anywhere (1Password mounts),
`slate/pki/` entirely, `**/certs/`, `*.key` / `*.pem` / `*.crt` / `*.csr` /
`*.p12`, `*.password`, the real `slate/mosquitto/passwd`, real component
configs (only `config.example.toml` is tracked), and build output. Real
passwords come from the 1Password Environment `rack-mqtt` at deploy time; the
CA key, server key and client keys are generated by `rackpki` into `slate/pki/`
and kept as 1Password Document items. An exposed CA key = anyone can mint a
client cert and command the rack: treat it like the crown jewel.

## CLAUDE.md (keep it short)

Standing context only; point at the specs, don't inline them. The repo
[CLAUDE.md](../CLAUDE.md) is the live version. It covers: the three machines,
the spec files, Rust daemons + Go CLIs, `_src/` is never edited, deployment is
manual, never read `.env` files (1Password named pipes), and no live tests
except the intel-d1babf procedure in [DEPLOY.md](DEPLOY.md#first-live-test-intel-d1babf-only).

## First steps (done)

1. Baseline commit `c18c6bf`: the specs, the two originals in `_src/`, and
   `.gitignore`.
2. Phase 1 (parallel workers): slate/mosquitto + rackpki, rackctl, pc-agent,
   relay-mqtt (against a fake controller), and these docs.
3. Phase 2: the rack_controller merge, then the SBC4 swap and `map-verify`.
