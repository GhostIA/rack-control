# relay-mqtt (SBC4 sidecar)

Bridges the Slate broker to `rack_controller`'s local control socket:

- uses the Eclipse Paho MQTT client (`paho-mqtt` 0.14, the Rust wrapper over Paho C)
  and subscribes `rack/relay/+/set` (QoS 1) as MQTT 3.1.1 user/client id `sbc4`, mTLS +
  password, clean session, TLS verification on (chain against our CA only; the broker IP
  is checked against the cert by Paho C, which tries `X509_check_host` (CN fallback when
  the cert has no DNS SAN) and then the IP SAN; TLS >= 1.2 via OpenSSL 3's default
  security level, TLS 1.3 allowed). The broker cert from `rackpki` carries the IP SAN;
  since only our CA is trusted, the CN fallback can't admit a foreign cert;
- accepts only channels `0`..`7` (one ASCII digit) and payloads that are byte-exact
  `open` or `close`; retained messages and anything else are logged and ignored;
- sends `open <ch>` / `close <ch>` to `/run/rack_controller/ctl.sock` and publishes the
  retained `rack/relay/<ch>/state` = `open` | `closed` **only** after the controller
  replies `ok open <ch>` / `ok close <ch>` (or a bare `ok`). On `err ...`, a timeout
  (3 s), or a missing socket, nothing is published and the command is dropped. A state
  publish counts only once its PUBACK arrives (the Paho delivery token, bounded at 5 s);
  if it fails, every relay is re-read from the controller and republished. A request
  is re-sent once on a fresh connection only when the kept-open connection turned out
  dead before any reply (controller restarted); a timeout is never retried. Commands that
  waited more than 10 s behind a slow controller are dropped instead of applied late;
- on every broker (re)connect: publishes retained `rack/sbc4/status` = `online` (the
  LWT is retained `offline`), re-subscribes, sends `state`, and publishes all 8 retained
  relay states. It re-syncs the same way when the socket connection is re-established
  (a controller restart resets every relay to closed), and audits `state` every 60 s,
  republishing any relay that differs from what it last published (this catches an idle
  controller restart);
- reconnects forever to the broker (Paho's built-in automatic reconnect, 1 s to 30 s, also
  after a failed first connect; a warning is logged every 60 s while it is down) and to the
  socket (lazily per request, with 1 s to 30 s
  state-sync retries using `backon`). It never exits because either is missing. On SIGTERM
  it exits without an MQTT DISCONNECT, so the broker publishes the `offline` LWT.

Socket framing uses `tokio_util::codec::LinesCodec` (512-byte reply lines, 128-byte
requests in the fake) with two stricter rules, to stay byte-exact: a line ending in `\r\n`
is an error (LinesCodec would strip the `\r`), and a last line cut off by EOF is an
error, never a reply. Extra or out-of-step reply lines drop the connection. If the
SUBACK for `rack/relay/+/set` does not arrive (5 s), `online` is not published and the
subscribe is retried with backoff.

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

`make` needs `zig`, `cargo-zigbuild`, `cmake` and `rustup target add
armv7-unknown-linux-musleabihf` (plus `perl` and `make`, present on macOS). `paho-mqtt` is
built with `features = ["bundled", "ssl", "vendored-ssl"]`: its build script compiles the
Paho C library (`paho.mqtt.c`, via cmake) and OpenSSL 3 (`openssl-src`) from source with
zig's musl C toolchain and links both statically. The first build takes a few minutes. The
result is one static ARM EABI5 binary with no runtime libraries (no system OpenSSL needed
on the SBC4). For the docker build, the cross image must provide cmake.

### Licences

`paho-mqtt` and the bundled Paho C library are dual-licensed **EPL-2.0 / EDL-1.0**
(Eclipse Distribution License, BSD-style), at the recipient's choice.
The statically linked OpenSSL 3 is Apache-2.0. Everything else is MIT/Apache-2.0. Keep
these notices with any copy of the binary you hand out.

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
`username`, `password_file`, `socket`, `keep_alive_secs`, `log_level`. Unknown keys are
rejected.

The file is loaded with [figment](https://docs.rs/figment/0.10): built-in defaults, then
the TOML file, then environment overrides. A missing or unreadable file is an error, and
so is a bad value. The process then exits 2. Errors print the whole context chain and name
the file and the key, e.g.

```
in /etc/rack/relay-mqtt.toml: parse config: unknown field: found `hots`, expected ... for key "default.hots" in TOML source string
```

Every key can be overridden by `RELAY_MQTT_<KEY>` (upper case), e.g.
`RELAY_MQTT_KEEP_ALIVE_SECS=10` or `RELAY_MQTT_LOG_LEVEL=debug`. Only the keys above are
read this way, so other `RELAY_MQTT_*` variables are ignored. Overrides are validated like
the file.

### Logging

Logs go through [fast_log](https://docs.rs/fast_log/1.7) (a writer thread) to stderr, as
`[2026-11-21T12:00:00Z INFO  relay_mqtt::worker] ...`. That is `/tmp/<app_name>/stderr`
under the Phidget launcher, or the journal under the systemd unit. The filter uses
`RUST_LOG` syntax. It comes from `log_level` (or `RELAY_MQTT_LOG_LEVEL`) if set, otherwise
from `RUST_LOG`, otherwise `info`. The Phidget launcher sets no `RUST_LOG`, so the config
key is the way to change it there. To see why each broker reconnect fails, use
`log_level = "info,paho_mqtt::c_trace=debug"`, which enables Paho C's trace. Paho reads
the level once, when the client is created, so restart after changing it. The log is
flushed before every exit (error, SIGTERM) and in the panic hook. Each flush waits at
most 2 s.

### Panics

Panics use [human-panic](https://docs.rs/human-panic/2.0). The release build has
`panic = "abort"`. The hook runs before the abort and does three things: it flushes the
log, prints `relay-mqtt <version>: panicked at <file:line>: <message>` to stderr, and
then human-panic prints its notice with the path of a TOML crash report
(`report-<uuid>.toml`, with the message, location, OS and backtrace) in the temp dir
(`$TMPDIR`, else `/tmp`):

- **Phidget launcher** (root or the `relay-mqtt` user): `/tmp/report-<uuid>.toml`, next
  to `/tmp/<app_name>/stderr`, which holds the panic message. The launcher does not
  restart the app, so both stay until the SBC4 reboots.
- **systemd unit** (`PrivateTmp=yes`): the service's private `/tmp`, i.e.
  `/tmp/systemd-private-*-relay-mqtt.service-*/tmp/` on the host. systemd removes it
  when the unit stops, so use `journalctl -u relay-mqtt` for the message.

`RUST_BACKTRACE=1` switches back to Rust's default panic output (message and backtrace on
stderr, no report).

## Tests

- `make check` runs the unit tests (topic/payload/state parsing, line framing, Paho option
  building, the bridge logic with fake MQTT and controller traits), `tests/socket.rs` (the
  real `UnixController` against the in-memory fake controller on a Unix socket in a temp
  dir: round trip, `err`, missing socket, controller restart (with and without traffic),
  oversized and out-of-step replies, no retry on timeout, stale-command drop, and the
  worker retry loop) and `tests/race.rs` (8 concurrent controller clients with no
  cross-talk; a burst of commands during a `state` sync; the controller dying
  mid-command; a broker reconnect while a state publish is in flight). The race tests use
  a fake broker that checks, at the moment of every state publish, that the controller
  really holds that state. `tests/paho_broker.rs` runs the real Paho adapter (callbacks,
  `ClientMqtt`, all connect options except TLS) against an embedded `rumqttd` broker on
  127.0.0.1: online + 8 states, retained and invalid commands ignored, a command applied;
  and, through a loopback proxy that cuts the connection, the LWT `offline`, Paho's
  automatic reconnect, re-subscribe, `online` and a full re-sync. TLS itself is only
  exercised by the docker e2e. The config tests cover unknown keys, a missing file,
  defaults, the example, `RELAY_MQTT_*` overrides and the `{:#}` chain. The logging test
  uses a slow sink that must hold the last line once `flush` returns.
  `tests/panic_hook.rs` runs the debug binary. With `--panic-test` (debug builds only),
  it checks the flushed log line, the panic message on stderr and the human-panic
  report. It also checks that a broken config, a missing config and a bad argument exit 2
  with the chained error.
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
