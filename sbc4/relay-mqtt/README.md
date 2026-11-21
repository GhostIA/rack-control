# relay-mqtt (SBC4 sidecar)

Bridges the Slate broker to `rack_controller`'s local control socket:

- subscribes `rack/relay/+/set` (QoS 1) as MQTT user/client id `sbc4`, mTLS + password,
  clean session, TLS verification on;
- accepts only channels `0`..`7` (one ASCII digit) and payloads that are byte-exact
  `open` or `close`; retained messages and anything else are logged and ignored;
- sends `open <ch>` / `close <ch>` to `/run/rack_controller/ctl.sock` and publishes the
  retained `rack/relay/<ch>/state` = `open` | `closed` **only** after the controller
  replies `ok open <ch>` / `ok close <ch>` (or a bare `ok`). On `err ...`, a timeout
  (3 s), or a missing socket, nothing is published and the command is dropped. A request
  is re-sent once on a fresh connection only when the kept-open connection turned out
  dead before any reply (controller restarted); a timeout is never retried. Commands that
  waited more than 10 s behind a slow controller are dropped instead of applied late;
- on every broker (re)connect: publishes retained `rack/sbc4/status` = `online` (the
  LWT is retained `offline`), re-subscribes, sends `state`, and publishes all 8 retained
  relay states. It re-syncs the same way when the socket connection is re-established
  (a controller restart resets every relay to closed), and audits `state` every 60 s,
  republishing any relay that differs from what it last published (this catches an idle
  controller restart);
- reconnects forever to the broker (1 s to 30 s backoff) and to the socket (lazily per
  request, with 1 s to 30 s state-sync retries). It never exits because either is missing.

It does nothing to hardware itself. **It needs the merged `rack_controller` (pending,
`sbc4/rack_controller/`), which serves the socket.** Today's `relay_controller` has no
socket, so until the merge is deployed the sidecar just stays online, logs that the socket
is missing, and publishes no relay state.

## Build

```sh
make check    # fmt --check, clippy -D warnings, unit + real-socket tests (host)
make          # cargo zigbuild -> target/armv7-unknown-linux-musleabihf/release/relay-mqtt
make docker   # same build in ghcr.io/rust-cross/rust-musl-cross:armv7-musleabihf,
              # with deps vendored first so the container runs with --network none
```

`make` needs `zig`, `cargo-zigbuild` and `rustup target add armv7-unknown-linux-musleabihf`.
The result is one static binary with no runtime libraries.

## Deploy on the SBC4 (manual)

| file | path | owner / mode |
|---|---|---|
| `relay-mqtt` binary | `/usr/local/bin/relay-mqtt` | root:root 0755 |
| `relay-mqtt.service` | `/etc/systemd/system/relay-mqtt.service` | root:root 0644 |
| `config.example.toml` | `/etc/rack/relay-mqtt.toml` | root:root 0644 (no secrets) |
| CA cert | `/etc/rack/tls/ca.crt` | root:root 0644 |
| client cert (CN `sbc4`) | `/etc/rack/tls/sbc4.crt` | root:root 0644 |
| client key | `/etc/rack/tls/sbc4.key` | relay-mqtt:root 0600 |
| broker password | `/etc/rack/sbc4.password` | relay-mqtt:root 0600 |

The key and password belong to the `relay-mqtt` user, not to group `rackctl`: that group
is shared with rack_controller (for the socket), and the controller has no business
reading the broker credentials.

The certs and key come from `slate/rackpki` (`rackpki bundle sbc4`). The password is
`MQTT_SBC4_PASSWORD` from the 1Password environment; write it as a single line (one
trailing newline is stripped). Don't commit any of these files.

```sh
# group shared with rack_controller (it owns /run/rack_controller, socket 0660 group rackctl)
groupadd --system rackctl
useradd --system --no-create-home --shell /usr/sbin/nologin -g rackctl relay-mqtt
install -m 0755 relay-mqtt /usr/local/bin/relay-mqtt
install -d -m 0755 /etc/rack /etc/rack/tls
install -m 0644 config.example.toml /etc/rack/relay-mqtt.toml
install -m 0644 ca.crt sbc4.crt /etc/rack/tls/
install -m 0600 -o relay-mqtt sbc4.key /etc/rack/tls/sbc4.key
install -m 0600 -o relay-mqtt sbc4.password /etc/rack/sbc4.password
install -m 0644 relay-mqtt.service /etc/systemd/system/relay-mqtt.service
systemctl daemon-reload
systemctl enable --now relay-mqtt
journalctl -u relay-mqtt -f
```

The unit has `After=rack_controller.service` but no `Requires=`, so either service can
restart without stopping the other. It runs as `relay-mqtt:rackctl` with
`ProtectSystem=strict`, `ProtectHome`, `NoNewPrivileges` and `/etc/rack` read-only.
The sidecar never opens Phidget devices, so it coexists with the single hub owner.

## Configuration

`/etc/rack/relay-mqtt.toml` (see `config.example.toml`; every key is optional and the
defaults are the production values): `host`, `port`, `ca_file`, `cert_file`, `key_file`,
`username`, `password_file`, `socket`, `keep_alive_secs`. Unknown keys are rejected.
`RUST_LOG` controls log verbosity (default `info`, logs go to the journal).

## Tests

- `make check` runs the unit tests (topic/payload/state parsing, the bridge logic with fake
  MQTT and controller traits) and `tests/socket.rs` (the real `UnixController` against
  the in-memory fake controller on a Unix socket in a temp dir: round trip, `err`,
  missing socket, controller restart (with and without traffic), oversized and out-of-step
  replies, no retry on timeout, stale-command drop, and the worker retry loop).
- `make e2e` (needs Docker) runs `e2e/`. A Rust runner generates throwaway test PKI and
  a mosquitto `passwd` into `e2e/work/` (gitignored, deleted afterwards) and vendors the
  crate dependencies. It then drives `docker compose` with an `eclipse-mosquitto:2.0.20`
  broker (mTLS + passwd + ACL on 8883, IP SAN), the `fake-controller` test binary sharing
  the socket volume, the sidecar, and an `e2e-check` client acting as the orchestrator.
  The check covers: a seeded retained `set` being ignored, all 8 states plus `online` on
  connect, `rack/relay/9/set` and non-exact payloads doing nothing, and
  `rack/relay/3/set=open` leading to a retained `rack/relay/3/state=open`. The compose
  network is `internal: true` with no published ports, so no container can reach the
  rack LAN.

`fake-controller` and `e2e-check` are test-only binaries: never deploy them.
