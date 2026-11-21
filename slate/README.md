# slate/: broker and orchestrator on the GL.iNet Slate 7 (OpenWrt, arm64)

| dir | what | lands at |
|---|---|---|
| `mosquitto/` | `mosquitto.conf`, `acl`, `passwd.example` | `/etc/mosquitto/` |
| `rackpki/` | Go tool that runs on the **dev Mac** only: CA, certs, `passwd`, deploy bundles | stays on the dev Mac |
| `pki/` (gitignored) | rackpki output: keys, certs, `passwd`, `deploy/<host>/` | never committed |
| `rackctl/` | Go orchestrator (`rackctl`, plus `rack-shutdown`/`rack-up` symlinks) | `/usr/bin/`, config in `/etc/rack/` |

The broker listens **only** on `192.168.8.1:8883` and requires TLS ≥1.2. Every
client needs all three of these: a client cert signed by the rack-control CA,
its username and password, and an ACL entry. There is no plaintext port.
**Never port-forward 8883 (or anything MQTT) on the WAN.**

## 1. Generate the PKI and passwords (dev Mac)

```sh
cd slate/rackpki
make                        # -> bin/rackpki (host build)
./bin/rackpki init-ca       # pki/ca.crt + ca.key (ECDSA P-256, 10 years)
./bin/rackpki server        # pki/broker.crt/.key, IP SAN 192.168.8.1 only (5 years)
./bin/rackpki client --all  # pki/<user>.crt/.key, CN = MQTT username (5 years)
op run --environment=cxdbzvay5orxm3fmqu4cydzn3y -- ./bin/rackpki passwd
op run --environment=cxdbzvay5orxm3fmqu4cydzn3y -- ./bin/rackpki bundle --all
./bin/rackpki expiry
```

- Output goes to `<repo>/slate/pki/` (or `--out DIR`). Keys, `passwd` and every
  bundle file are 0600. rackpki won't overwrite a file unless you pass `--force`.
- `passwd` reads `MQTT_ORCHESTRATOR_PASSWORD`, `MQTT_SBC4_PASSWORD`,
  `MQTT_INTEL_91C45D_PASSWORD`, `MQTT_INTEL_D1BABF_PASSWORD`,
  `MQTT_INTEL_177E78_PASSWORD`, `MQTT_INTEL_A163F8_PASSWORD` and
  `MQTT_INTEL_1ECBC8_PASSWORD` from the 1Password environment `rack-mqtt`, which
  `op run` injects. It writes Mosquitto 2.x `$7$` lines (PBKDF2-HMAC-SHA512,
  101 iterations, 12-byte salt). It fails if any variable is missing or empty,
  or has leading or trailing whitespace. It never prints a password. Don't read
  or `cat` `slate/.env`: it's a 1Password named pipe.
- `bundle <user>` builds `pki/deploy/<user>/` with `ca.crt`, `<user>.crt`,
  `<user>.key` and `<user>.password`. `bundle broker` builds
  `pki/deploy/broker/` with `ca.crt`, `broker.crt`, `broker.key`, `passwd`, plus
  the committed `acl` and `mosquitto.conf`. `bundle --all` builds both kinds.
  `bundle` first checks that each cert chains to `ca.crt` and matches its key.
- **CA key:** once every cert is issued, store `pki/ca.key` in 1Password as a
  **Document** item (you do this; the tools never touch 1Password items).
  Then delete the local copy (`rm slate/pki/ca.key`). Restore it from 1Password
  only when you need to sign again. Anyone who has it can mint a client cert.
- Copy each `deploy/<host>/` to its host (layout: `/etc/rack/tls/ca.crt`,
  `/etc/rack/tls/<user>.crt`, `/etc/rack/tls/<user>.key`,
  `/etc/rack/<user>.password`). Then delete `pki/deploy/` from the Mac.

## 2. Install the broker on the Slate

```sh
opkg update
opkg install mosquitto-ssl     # TLS build; no mosquitto-client needed (rackctl is Go)
```

Copy the broker bundle (from the Mac, e.g. with `scp -O` or a USB stick):

```sh
mkdir -p /etc/mosquitto/certs
cp deploy/broker/mosquitto.conf deploy/broker/acl deploy/broker/passwd /etc/mosquitto/
cp deploy/broker/ca.crt deploy/broker/broker.crt deploy/broker/broker.key /etc/mosquitto/certs/
chown -R mosquitto:mosquitto /etc/mosquitto
chmod 0700 /etc/mosquitto/certs
chmod 0600 /etc/mosquitto/passwd /etc/mosquitto/acl /etc/mosquitto/certs/broker.key
chmod 0644 /etc/mosquitto/mosquitto.conf /etc/mosquitto/certs/ca.crt /etc/mosquitto/certs/broker.crt
```

OpenWrt's `mosquitto-ssl` package ships a procd init script. It uses
`/etc/mosquitto/mosquitto.conf` unless UCI generation (`use_uci`) is switched
on. Make sure it's off:

```sh
uci -q get mosquitto.owrt.use_uci   # must print nothing or 0
/etc/init.d/mosquitto enable
/etc/init.d/mosquitto restart
logread -e mosquitto          # logs go to syslog
netstat -ltn | grep 8883      # must show 192.168.8.1:8883 and nothing on 1883
```

The listener binds to `192.168.8.1`, so br-lan must have that address before
mosquitto starts. If it fails at boot with "Cannot assign requested address",
restart it after the network comes up (`/etc/init.d/mosquitto restart`).

## 3. Orchestrator files

Put `rackctl` at `/usr/bin/rackctl`, with symlinks `/usr/bin/rack-shutdown` and
`/usr/bin/rack-up` pointing to it. Put its config and the orchestrator bundle in
`/etc/rack/` (`/etc/rack/tls/…`, `/etc/rack/orchestrator.password`, all 0600
root). See `rackctl/`.

## 4. Survive sysupgrade

OpenWrt keeps only the files listed for backup. Add them to `/etc/sysupgrade.conf`:

```
/etc/mosquitto/
/etc/rack/
/usr/bin/rackctl
/usr/bin/rack-shutdown
/usr/bin/rack-up
```

After a firmware upgrade, run `opkg install mosquitto-ssl` again (packages
don't survive a sysupgrade, but the config does) and `/etc/init.d/mosquitto enable`.
Keep the Mac's `pki/` (minus `ca.key`) and 1Password as the source of truth.

## 5. Certificate expiry and renewal

Run `./bin/rackpki expiry` on the Mac now and then (it exits 2 when anything is
within 180 days, `--warn-days N` to change). **An expired `broker.crt` or
`ca.crt` breaks the shutdown path:** every client refuses the broker, so
`rack-shutdown` can't reach the PCs or the relays. Renew well ahead of time:

- **Broker or client cert** (5 years): restore `ca.key` from 1Password, then run
  `rackpki server --force` or `rackpki client <user> --force`, then
  `rackpki bundle broker --force` or
  `op run --environment=cxdbzvay5orxm3fmqu4cydzn3y -- ./bin/rackpki bundle <user> --force`
  (user bundles include the password, so they need `op run`). Deploy the bundle, restart mosquitto or the
  client service, and delete `ca.key` again.
- **CA** (10 years; rackpki refuses leaves that would outlive it): run
  `init-ca --force`, reissue *every* cert, and redeploy everything at once.
  Old and new certs don't interoperate.
- **Password change**: update the 1Password item, then run
  `op run … -- rackpki passwd --force` and `bundle <user> --force`. Deploy
  `passwd` to the Slate and `<user>.password` to the host, then restart both.

## Security notes

- The ACL (`mosquitto/acl`) gives each user least privilege. A PC can read only
  its own `rack/pc/<name>/cmd` and write only its own `status`. `sbc4` only
  receives relay commands and reports relay and sbc4 state. Only `orchestrator`
  can command shutdowns and relays. `intel-1ecbc8` also handles the Ceph
  maintenance topics.
- `use_username_as_clientid true`: the client ID is always the username, so
  one client can't kick another user's session off by reusing its client ID.
- `use_identity_as_username false`: the broker checks the password against the
  CONNECT username, not against the cert CN. Each cert's CN is the same as its
  username by convention. One host's cert plus another host's password would
  still pass, so treat each host's key and password as a pair.
- Mosquitto 2.x accepts every SUBSCRIBE and enforces `read` on each message it
  delivers. A subscription to a forbidden topic receives nothing. A forbidden
  publish is dropped without notice.
- `rackpki/e2e/` runs the real `eclipse-mosquitto:2.0.20` against rackpki output
  in Docker, with fake passwords, on an internal-only network
  (`make -C slate/rackpki e2e`).
