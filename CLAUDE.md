# rack-control

Homelab rack power control across 3 machine types: the GL.iNet Slate (broker +
`rackctl`), the Phidget SBC4 (relays), and 5 Proxmox PCs (`pc-agent`).

- Specs: docs/rack-mqtt-shutdown-handoff.md (whole system; its "Resolved
  decisions" section is authoritative), docs/rack-controller-merge.md (phase 2,
  deferred), docs/DEPLOY.md (order, runbooks, first live test).
- Languages: Rust for the daemons (pc-agent, sbc4/relay-mqtt), Go for the CLIs
  (slate/rackctl, slate/rackpki), C++ only in sbc4/rack_controller. No shell
  logic beyond one-line Makefile recipes.
- sbc4/rack_controller/_src/ holds the running originals. Never edit it.
- Never read, cat or print any .env file: ./.env and slate/.env are 1Password
  named pipes (intel-rack-creds, rack-mqtt). No secret values in commits or docs.
- You write files here; deployment is manual. No SSH, and nothing (including
  Docker e2e) may reach 192.168.8.0/24.
- No live tests except the intel-d1babf procedure in docs/DEPLOY.md, and only
  when the operator approves it. Never command a relay before map-verify.
- MQTT is TLS on 192.168.8.1:8883 only, verification always on. Payloads are
  byte-exact enums; message content is never executed.
