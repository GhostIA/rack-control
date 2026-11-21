# rack-control

Graceful shutdown and relay power cutoff for a 5-node Proxmox VE rack (cluster
`media`, Ceph), over MQTT with mTLS. An operator runs `rackctl shutdown all`
on the GL.iNet Slate. Each PC's agent powers itself off, rackctl waits for
sustained ping-down plus a sync delay, and only then asks the Phidget SBC4 to
open that PC's relay. `rackctl up` reverses it. Ceph is put into maintenance
(`noout`, `norebalance`) around full shutdowns.

No SSH, no inbound access to the PCs, and MQTT payloads are never executed: the
worst a broker compromise can do is power things off.

## Components

| dir | language | what |
|-----|----------|------|
| `slate/mosquitto/` | config | Mosquitto broker: `192.168.8.1:8883` only, mTLS + password, per-user ACLs |
| `slate/rackpki/` | Go | CA, broker and client certs, Mosquitto `$7$` password file, per-host bundles |
| `slate/rackctl/` | Go | orchestrator: `shutdown`, `up`, `status`, `map-verify`, `ceph-maintenance`; `rack-shutdown`/`rack-up` symlinks |
| `pc-agent/` | Rust | per-PC daemon: `shutdown` -> logind PowerOff; Ceph maintenance on intel-1ecbc8 |
| `sbc4/relay-mqtt/` | Rust | SBC4 sidecar: `rack/relay/<ch>/set` -> rack_controller's Unix socket |
| `sbc4/rack_controller/` | C++ | phase 2 (deferred): one process owning the Phidget hub; `_src/` holds the running originals |

## Deploy map (dir -> machine -> path)

| repo dir | machine | lands at |
|----------|---------|----------|
| `slate/mosquitto/` | Slate (192.168.8.1) | `/etc/mosquitto/`, certs in `/etc/mosquitto/certs/` |
| `slate/rackctl/` | Slate | `/usr/bin/rackctl` + `rack-shutdown`, `rack-up` symlinks; `/etc/rack/rack.toml`, `/etc/rack/tls/` |
| `slate/rackpki/` | dev box | not deployed; output in gitignored `slate/pki/` |
| `pc-agent/` | each PC (5x Proxmox, amd64) | binary, `rack-agent.service`, polkit rule, `/etc/rack/` (via `pc-agent install`) |
| `sbc4/relay-mqtt/` | SBC4 (192.168.8.165, armv7) | binary, `relay-mqtt.service`, `/etc/rack/` (phase 2) |
| `sbc4/rack_controller/` | SBC4 | built on-device; Phidget user app (repointed `ua_RelayController.conf`), replaced relay_controller |

## Status

- Phase 1: broker config, rackpki, rackctl, pc-agent, relay-mqtt (tested against
  a fake controller).
- Phase 2: the C++ merge. Until it lands the SBC4 runs the original
  `relay_controller`, relays aren't MQTT-commandable, and every shutdown uses
  `--no-relay`. The relay channel mapping is unverified until
  `rackctl map-verify`.

## Docs

- [docs/rack-mqtt-shutdown-handoff.md](docs/rack-mqtt-shutdown-handoff.md):
  whole-system spec (hosts, topics, ACLs, resolved decisions)
- [docs/DEPLOY.md](docs/DEPLOY.md): deploy order, SBC4 swap, first live test,
  shutdown/power-up runbooks, cert renewal
- [docs/rack-controller-merge.md](docs/rack-controller-merge.md): phase-2 C++
  merge brief
- [docs/rack-control-structure.md](docs/rack-control-structure.md): repo layout
- [sbc4/README.md](sbc4/README.md): SBC4 hub ownership and units

Secrets are never committed: passwords come from 1Password Environments, TLS
keys are 1Password Document items, and `slate/pki/` is gitignored.
