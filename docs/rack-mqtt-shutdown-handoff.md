# Handoff: MQTT graceful shutdown + relay cutoff for the rack

Context for Claude Code. Build a no-SSH, MQTT-based system to gracefully power
down 6 mini-PCs and then cut their relays, orchestrated from a GL.iNet Slate.

## Topology (all on one isolated subnet behind the Slate)

- **GL.iNet Slate (OpenWrt)** — hosts the **Mosquitto broker** AND the
  orchestrator script. It's the NAT boundary for this segment, but every client
  below connects *outbound* to the local broker, so NAT is irrelevant to this
  system. Nothing needs a port-forward; nothing crosses the Slate boundary.
- **6 mini-PCs** (Dell OptiPlex 7040 Micro + others, Linux/systemd) — each runs
  a local MQTT agent that powers *itself* off on command. No inbound access to
  the PCs, no SSH, no remote sudo.
- **Phidget SBC4** (`phidgetsbc`, on the PC subnet) — runs the existing
  `relay_controller` daemon that owns the relay channels (2x REL1000 on VINT hub
  ports 3 and 4, 8 channels, 6 of them powering the PCs). It must gain an MQTT
  subscriber so it opens a relay channel on command. It already runs a
  per-channel hardware failsafe; that must stay intact for channels not being
  commanded.

Everything talks to the broker on the Slate's LAN IP. Broker must NOT run on one
of the PCs being shut down.

## Message flow (the whole point)

1. Operator runs `rack-shutdown <pc...>` (or bare = all) on the Slate.
2. Script publishes `rack/pc/<name>/cmd` = `shutdown` for each target.
3. That PC's local agent receives it and runs `systemctl poweroff` locally.
4. On its way down, the agent publishes `rack/pc/<name>/status` = `down`
   (retained=false) just before poweroff; script also falls back to ping-down.
5. Script waits until each PC is confirmed down (status msg OR ping stops).
6. Script publishes `rack/relay/<channel>/set` = `open`.
7. `relay_controller` on the SBC4 receives it, opens that channel (relay opens,
   PC power cut). Failsafe still protects every other channel.

There is a matching `rack-up` path: publish `rack/relay/<channel>/set` = `close`
to re-energize a relay (PC powers on). PCs that boot with "AC recovery = power
on" then start automatically.

## MAPPING — operator must fill this in (ask if not provided)

Per PC, need: short name, static IP/hostname, relay channel index.
Relay channel indices in `relay_controller`: hub port 3 ch0-3 = channels 0-3,
hub port 4 ch0-3 = channels 4-7.

| name         | static IP      | relay channel |
|--------------|----------------|---------------|
| intel-91c45d | 192.168.8.196  | 0             |
| intel-d1babf | 192.168.8.194  | 1             |
| intel-177e78 | 192.168.8.162  | 2             |
| intel-a163f8 | 192.168.8.123  | 4             |
| intel-1ecbc8 | 192.168.8.234  | 6             |

5 PCs (down from 6). Relay channels are NON-CONTIGUOUS (0,1,2,4,6) — do not
assume 0..N; use the table. Channels 3,5,7 are not PC-powering. relay_controller
still owns all 8 channels; only these 5 are MQTT-commandable for shutdown.

Subnet is 192.168.8.0/24 (the Slate's LAN). Broker address:
`<SLATE_LAN_IP>:1883` — confirm the Slate's own LAN IP (likely 192.168.8.1).

## Components to build

### 1. Mosquitto broker config (on the Slate / OpenWrt)

- Install `mosquitto` + `mosquitto-client` via opkg (OpenWrt). Note OpenWrt's
  package may be `mosquitto-ssl` or nossl variant — pick per TLS decision below.
- Listener on 1883, bound to the LAN interface only (not WAN).
- **Auth required** — no anonymous. Password file with distinct users:
  `orchestrator` (the CLI), `sbc4` (the relay-mqtt sidecar), and one per PC named
  after the PC: `intel-91c45d`, `intel-d1babf`, `intel-177e78`, `intel-a163f8`,
  `intel-1ecbc8`.
- **ACLs** (this is the security boundary, enforce strictly):
  - `orchestrator`: publish to `rack/pc/+/cmd` and `rack/relay/+/set`;
    subscribe to `rack/pc/+/status` and `rack/relay/+/state`.
  - each PC user (e.g. `intel-91c45d`): subscribe ONLY to its own
    `rack/pc/intel-91c45d/cmd`; publish ONLY to its own
    `rack/pc/intel-91c45d/status`. (A compromised PC cannot shut down neighbors
    or touch relays.)
  - `sbc4` (sidecar): subscribe ONLY to `rack/relay/+/set`; publish ONLY to
    `rack/relay/+/state`.
- **TLS decision:** DEFAULT to auth + ACLs on plaintext 1883 (trusted isolated
  segment; ACLs are what actually prevent cross-PC attacks). Make TLS (8883 +
  certs) an optional follow-up, clearly flagged, not v1. Ask operator if they
  want TLS from the start; if yes, generate a local CA + broker cert + per-client
  certs and wire them in.
- Persist config so it survives Slate firmware updates where possible (OpenWrt
  config persistence caveat — put it in /etc/mosquitto and back it up).

### 2. Per-PC agent (on each mini-PC, Linux/systemd) — Go binary

- A **Go** daemon (single static binary, `paho.mqtt.golang`), run as a systemd
  service. Subscribes to its own `rack/pc/<name>/cmd`; on payload EXACTLY
  `shutdown`, publishes `rack/pc/<name>/status=down` then invokes poweroff.
- **Security-critical:** accept ONLY the fixed payload `shutdown` on its own
  topic and perform exactly one hard-coded action. NEVER exec message content,
  never `sh -c`, no payload-derived commands. The binary's entire vocabulary is
  "poweroff on the word shutdown." Caps a broker compromise at power-off DoS,
  never code exec. (Compiled Go helps here: there's no shell-injection surface
  the way a shell+mosquitto_sub pipeline has.)
- **One binary, 5 machines:** all 5 PCs are x86-64 Linux, so
  `GOOS=linux GOARCH=amd64 go build` produces ONE static binary deployed
  identically to all of them. Per-PC identity (name, creds) comes from a config
  file / env / flags, NOT from separate builds. The binary is the same; only the
  config differs.
- Config per PC: its `name` (e.g. `intel-91c45d`), broker address, and its own
  `pcN` broker user/password. Creds file 0600, root-only. install.sh drops the
  binary, unit, and config, enables the service.
- Invoking poweroff: prefer calling systemd-logind over shelling out — Go can
  issue the logind `PowerOff` D-Bus call, or exec `/sbin/poweroff` directly as a
  fixed argv (no shell). Privilege: run the service as root (simplest for
  poweroff), OR run unprivileged + a narrow polkit rule granting only
  `org.freedesktop.login1.power-off`. Prefer polkit; document the tradeoff.

### 3. Relay side on the SBC4 — C++ controller (unchanged core) + Go MQTT sidecar

Architecture: Option 3 (sidecar). The safety-critical Phidget code stays frozen
as the working C++; a separate Go process handles MQTT and forwards relay
commands to the C++ over a LOCAL Unix domain socket. The C++ keeps sole
ownership of the Phidget channels and the failsafe; the sidecar never touches
hardware. This keeps the proven failsafe/attach/stagger logic untouched and puts
the new networked code in Go.

#### 3a. relay_controller.cpp (C++) — minimal additions only

- Do NOT rewrite the core: non-blocking Phidget_open, per-channel attach-driven
  power-on with stagger, per-channel hardware failsafe reset loop — all preserved.
- Add a **Unix domain socket** control interface (e.g. `/run/relay_controller.sock`,
  0660, root/own group). Listen on it in a thread; accept simple line commands
  from the local sidecar ONLY (local socket, not network):
  - `open <ch>`  -> set channel FALSE (NO relay opens, PC off)
  - `close <ch>` -> set channel TRUE (relay closes, PC on)
  - `state`      -> reply with each channel's current commanded/attached state
  - channel bounds-checked against the valid set {0,1,2,4,6}; reject others.
- Add a per-channel **commanded state** that the attach handler and heartbeat
  respect: a channel commanded `open` must STAY open across a re-attach (today
  attach = power on; that must no longer override an operator open). This is the
  one real logic change in the C++.
- Keep the failsafe heartbeat running for all channels; a commanded-open channel
  can keep its failsafe armed (already in the safe/off state) — just stop driving
  it on.
- No MQTT, no network library in the C++ — it only speaks the local socket. This
  is why the Paho-in-C++ and static-link headaches go away entirely.

#### 3b. relay-mqtt sidecar (Go) — new binary on the SBC4

- Small Go daemon. Connects to the Slate broker as user `sbc4`
  (`paho.mqtt.golang`), auto-reconnect. Subscribes `rack/relay/+/set`.
  - On `rack/relay/<ch>/set` = `open`|`close`: write `open <ch>` / `close <ch>`
    to the C++ Unix socket. Parse channel from topic, validate against {0,1,2,4,6}.
  - Publish `rack/relay/<ch>/state` (retained) after the C++ confirms, so the
    orchestrator/dashboard see real state.
- **Go cross-compiles to a single static binary** — build with
  `GOOS=linux GOARCH=arm GOARM=7` (SBC4 is armhf) on any dev box, scp the one
  binary over. No libs on the SBC, nothing to break on the overlay reboot wipe.
  This sidesteps the Phidget-SBC overlay/apt problem completely (the reason the
  old Paho-C plan was fragile).
- Runs as its own SBC user-app or systemd-style service. If launched via the
  Phidget web UI (`S20turnon`/`phidgetsbc_ua`, start-once no-respawn, logs to
  `/tmp/<app>/stdout`), keep it resilient: reconnect forever, never exit on MQTT
  or socket absence (the C++ may still be starting). The C++ controller and this
  sidecar are two separate user-app entries.

#### Ordering / resilience

- Sidecar must tolerate the C++ socket not being up yet (retry connect to the
  socket). C++ must tolerate the sidecar being absent (socket just has no client).
- Neither process restarting should disturb the other: the C++ owns hardware +
  failsafe regardless of the sidecar; the sidecar is stateless, reconnects.

### 4. rack-shutdown / rack-up CLI (on the Slate, shell)

- `rack-shutdown [name...]` — no args = all PCs. For each target: publish
  `rack/pc/<name>/cmd=shutdown`; wait for `rack/pc/<name>/status=down` (subscribe
  with timeout) OR ping-stops as fallback (configurable timeout, e.g. 90s);
  then publish `rack/relay/<channel>/set=open`. Report per-PC progress.
- `rack-up [name...]` — publish `rack/relay/<channel>/set=close` to re-energize.
  (PCs power on via BIOS AC-recovery.)
- Use `mosquitto_pub`/`mosquitto_sub` with the `orchestrator` creds. Put both
  on the Slate's PATH (/usr/bin or /root/bin). Read the mapping + broker creds
  from a single config file (e.g. /etc/rack/rack.conf) so names/IPs/channels
  live in one place.
- Order within a shutdown: gracefully down THEN cut relay, per PC. Never cut a
  relay before `status=down`/ping-down confirmation. If confirmation times out,
  do NOT cut power (leave it up, report the failure) — cutting a PC that didn't
  confirm risks an unclean power-off.

## Security requirements (enforce, don't skip)

- No anonymous broker access; ACLs scoped per-client as above.
- Broker listener bound to LAN only, never WAN; no port-forward on the Slate.
- Agents and relay_controller act on fixed payloads only; message content is
  never executed. Worst-case broker compromise = power-off DoS, not RCE.
- Creds files 0600, per-client distinct users (no shared account).
- Document the (optional) TLS upgrade path but default v1 to auth+ACL on 1883.

## Operator decisions (RESOLVED — build to these)

1. Mapping: table above, 5 PCs on channels 0,1,2,4,6 (non-contiguous). Slate LAN
   IP on 192.168.8.0/24 — confirm exact (likely 192.168.8.1) for broker address.
2. Hosts: all 5 are **Proxmox VE** hosts, so systemd is present. ONE Go amd64
   agent binary serves all 5.
   - **PVE graceful shutdown: defer to PVE.** The agent just issues host
     `PowerOff` (logind). systemd's shutdown runs `pve-guests.service`, which
     stops VMs/CTs (ACPI/clean, per-guest timeouts set in PVE) before the host
     powers off. The agent does NOT orchestrate guests. Per-guest shutdown
     order/timeout is PVE config, not this system's concern.
   - Because PVE guest-stop takes time, an agent-published `status=down` (sent
     before poweroff) is too early. **Gate relay-cut on real host-unreachable
     (ping-down), not on status**, with a timeout sized to cover guest shutdown
     (default 120s; raise for hosts with heavy guests). status may still be
     published as an advisory breadcrumb, but ping-down is the authoritative
     "safe to cut" signal.
3. Security: **ACLs AND creds, both from v1** (defense in depth; containment is
   the goal). Per-client least-privilege so one compromised node can't pivot:
   - A compromised PC: only its own cmd/status. Cannot shut down other PCs or
     touch relays.
   - A compromised SBC4 sidecar: only subscribe `rack/relay/+/set` + publish
     `rack/relay/+/state`. RECEIVES relay commands; cannot issue PC shutdowns.
   - Orchestrator (Slate) is the only client that can publish both `rack/pc/+/cmd`
     and `rack/relay/+/set` — the intentionally-privileged node; protect it most.
   - Switch (if an MQTT participant) gets a read-only scoped user; no publish to
     control topics.
   - Net containment: no single compromised node EXCEPT the orchestrator can both
     shut down a PC and cut its power; and even the orchestrator's worst case is
     power-off DoS, not RCE (agents act on fixed payloads only).
   - **Creds source: 1Password env.** Claude Code pulls broker passwords from the
     1Password env Claude Code has access to, generates Mosquitto password hashes
     from them, and writes the ACL file (ACL file holds usernames->topic perms,
     NO secrets — it is not replaced by 1Password). Do NOT hand-write or commit
     real passwords; commit only placeholder examples.
   - TLS: OPEN — see below. ACLs are enforced regardless of TLS.
4. Per-PC agent privilege: **polkit** — unprivileged agent user + a narrow rule
   granting only `org.freedesktop.login1.power-off`. Not root.
5. BIOS AC-recovery = power on: **confirmed enabled** on all PCs, so `rack-up`
   (relay close) boots them.
6. Go toolchain: operator builds the two Go binaries (PC agent amd64; SBC4
   sidecar arm/GOARM=7) on a dev box and deploys. Claude Code writes Go source +
   build scripts; it cannot cross-compile to or reach the live machines.

## TLS (RESOLVED — TLS from v1)

- **TLS on 8883 from the start. Disable/do not open plaintext 1883.** Externally-
  facing posture: credentials must never cross a boundary in cleartext, and MQTT
  sends the password in the CONNECT packet, so auth-without-TLS leaks it.
- **Local CA + per-client certs.** Generate a private CA (kept offline/secured,
  NOT committed), a broker server cert (SAN must include the Slate's broker
  address/hostname clients use), and a client cert per user: `orchestrator`,
  `sbc4`, and one per PC (`intel-91c45d` ... `intel-1ecbc8`). Optionally a
  `switch` client cert if the switch participates.
- **Mutual TLS (require_certificate true, use_identity_as_username optional).**
  Clients present certs; the broker verifies against the CA. This means a client
  needs BOTH a valid cert AND (if password auth is also on) its creds — belt and
  suspenders. At minimum require server-cert verification on every client (no
  `insecure`/skip-verify anywhere) so clients authenticate the broker too.
- **Cert material via 1Password where it fits**: store CA key, client keys, and
  passwords in the 1Password env; Claude Code writes configs that REFERENCE cert
  file paths on each host (certs are deployed per-host, keys 0600, never
  committed). ACL file (usernames->topics) is committed as placeholders only.
- Per-client TLS config:
  - Broker (Slate/Mosquitto): `listener 8883`, `cafile`, `certfile`, `keyfile`,
    `require_certificate true`, `tls_version tlsv1.2` (or 1.3), bound to LAN iface.
  - sbc4 Go sidecar & Go PC agents (`paho.mqtt.golang`): TLS config with the CA
    pool + client cert/key, ServerName set to the broker's cert SAN, verify ON.
  - `rack-shutdown`/`rack-up` (mosquitto_pub/sub on Slate): `--cafile --cert
    --key` flags, or a client config file.
- Certs expire: note renewal. For a homelab, long-lived (e.g. 5-10yr) CA + client
  certs are acceptable; document the regen path. Don't let an expired broker cert
  silently brick the shutdown path.

## Still to confirm

- Exact Slate LAN IP / hostname clients use to reach the broker — needed for the
  broker cert SAN and client ServerName. (Likely 192.168.8.1; confirm, and decide
  IP-SAN vs a hostname like `broker.rack.lan` with local DNS/hosts entries.)

## Do NOT

- Do not use SSH or any inbound path to the PCs.
- Do not run the broker on a PC that gets shut down.
- Do not rewrite relay_controller's Phidget core (open model, stagger, failsafe).
  The C++ gets ONLY a local Unix-socket control interface + per-channel
  commanded-state override. No MQTT or network library in the C++.
- MQTT lives in the Go sidecar (SBC4) and the Go per-PC agents — never in the C++.
- Do not interpret MQTT payloads as shell commands anywhere; agents act on the
  fixed payload `shutdown` only, relay sidecar on `open`/`close` only.
- Do not expose the broker beyond the isolated segment (LAN listener only).
- Do not assume contiguous relay channels — the 5 PCs are on 0,1,2,4,6.
