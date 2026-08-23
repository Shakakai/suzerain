---
name: suzerain-admin
description: Set up, enroll, and operate a Suzerain/Castellan fleet of microVM-isolated AI coding agents. Use when the user asks to install, configure, or troubleshoot suzerain (control plane), castellan (agent daemon), suzy (desktop GUI), suz (CLI), or suzerain-mcp; enroll or approve daemons/operators; create, chat with, inspect, or destroy fleet agents; manage the age-encrypted secrets store; check fleet status, logs, or audit; or deploy the stack for a team across multiple servers.
---

# Suzerain Fleet Administration

Suzerain is a multi-server AI agent lifecycle system:

- **suzerain** — the control plane (one per fleet): registry, scheduler,
  age-encrypted secrets store, central event log, web UI on `http://127.0.0.1:8484`.
- **castellan** — the agent daemon (one per server): runs each agent as
  **pi in RPC mode inside its own Gondolin microVM**.
- **suzy** — the desktop GUI; connects to the control plane over iroh
  (`suz/operator/0`), authorized by its EndpointId public key.
- **suz** — the operator CLI (unix socket to the control plane).
- **suzerain-mcp** — MCP server exposing fleet tools to LLM assistants
  (never exposes secrets, by design).

All node-to-node traffic is iroh/QUIC with public-key identity
(`EndpointId`). There is no CA and no certs: enrollment is "init prints
an id, operator approves the id".

## Golden rules

1. **Secrets are write-only.** Never print, exfiltrate, or commit secret
   values. Use `suz secrets set provider <id> --value <key>` or stdin.
   `suz secrets` lists names only. If this plugin's MCP server is active,
   secrets are simply unavailable through it — that is intentional.
2. **Confirm before destructive ops.** `suz agent destroy`, removing a
   daemon (web UI / Suzy), and `suz secrets remove` are irreversible.
   Always confirm with the user first and state exactly what will be
   destroyed.
3. **Approve, don't bypass.** Daemons and operator clients must be
   approved by EndpointId (`suz daemon approve`, `suz operator approve`).
   Never suggest disabling the allowlists.
4. **Don't hand-hold the lifecycle.** Agents auto-suspend when idle and
   wake transparently on message. Never suggest manual start/stop as a
   fix; "sleeping" is a healthy state, not a problem.
5. **Run, don't recite.** You have a shell — prefer actually running
   `suz daemon list`, `suz agent list`, etc. over telling the user what
   to run. Read [references/commands.md](references/commands.md) for the
   full command reference when you need exact flags.

## Workflow: fresh setup on one machine

Do these in order; each step has a verification. If a step fails, see
[references/troubleshooting.md](references/troubleshooting.md).

1. **Install.** Preferred:
   `curl -fsSL https://raw.githubusercontent.com/Shakakai/suzerain/main/ops/install.sh | bash`
   (add `suzy` as an explicit component for the desktop app). From a
   source checkout instead: `mise install && mise run setup && mise run package`.
   Verify: `suzerain --version`, `castellan --version`, `suz --version`.
2. **Host prerequisites for castellan** (only where daemons run):
   node >= 22, qemu, and on Linux writable `/dev/kvm`
   (`sudo usermod -aG kvm $USER`, re-login). The castellan installer
   checks these and installs the gondolin driver automatically.
3. **Start the control plane:** `suzerain run` (or the installed
   systemd/launchd service). Capture its EndpointId (`suz id`).
   Verify: web UI responds at `http://127.0.0.1:8484`.
4. **Secrets** (the store is `$SUZERAIN_HOME/secrets.age`; its age identity
   is `$SUZERAIN_HOME/age-keys.txt`, auto-generated on first use — back it
   up). `suz secrets set provider <id> --value <key>`
   per LLM provider, and optionally `suz secrets set ssh-key < ~/.ssh/id_ed25519`
   so agents can pull/push private repos over SSH. Verify: `suz secrets`
   lists the entries (names only — values are write-only).
5. **Enroll the daemon:** on the daemon host,
   `castellan init --suzerain <SUZERAIN_ENDPOINT_ID>` prints the daemon's
   EndpointId; on the control plane, `suz daemon approve <CASTELLAN_ID>`;
   then `castellan run`. Verify: `suz daemon list` shows it online.
6. **First agent:** pick or write a manifest (model provider must have a
   key in the secrets store and be listed under `[secrets] providers`),
   `suz agent create --manifest <file>`, then
   `suz agent ask <name> "hello"`. Verify: reply streams back;
   `suz agent list` shows `idle`/`running`.
7. **Suzy (optional GUI):** start `suzy`, copy the operator EndpointId
   from its add-workspace dialog, `suz operator approve <SUZY_ID>`, then
   add a workspace with the control plane's EndpointId from `suz id`.

## Workflow: deploy for a team / multiple servers

Same flow, split across machines, plus:

- Control plane: run as a service; set `SUZERAIN_DATABASE_URL` to
  postgres for team use; put `[bundles] dir` on a large disk; back up
  `$SUZERAIN_HOME` **and the age key** (`$SUZERAIN_HOME/age-keys.txt`).
- Each daemon host: install `castellan` only
  (`install.sh | bash -s -- castellan`), enroll, approve, done. Label
  hardware for placement: `suz daemon label <id> --set gpu=true`.
- Each teammate: installs `suzy` (or `suz`), sends you the EndpointId
  from Suzy's add-workspace dialog; you run
  `suz operator approve <THEIR_ID>` (persists to `[operator] allow`).
- Web UI stays localhost-only; remote teammates use Suzy over iroh or
  `ssh -L 8484:127.0.0.1:8484 <host>`.

## Workflow: day-2 operations

- **Fleet health:** `suz daemon list`, `suz agent list`,
  `suz audit --tail 50`. Agent statuses: running / idle / sleeping /
  waking / failed. `sleeping` is normal (auto-suspend).
- **Chat/inspect:** `suz agent ask <name> "…"` (wakes sleeping agents),
  `suz agent attach <name>`, `suz agent logs <name>`.
- **Policy:** `suz agent config <name> --auto-suspend 10m|never|default`.
- **Secrets rotation:** `suz secrets set provider <id> --value <new>`;
  agents pick up slices on next wake.
- **Failed agent:** `suz agent logs <name>` first, then
  `suz agent destroy <name>` + recreate (manifests are immutable
  post-create — recreate to change).

## Reference material

- [references/commands.md](references/commands.md) — full `suz` /
  `castellan` / `suzerain` command reference, suzerain.toml knobs, env vars,
  and the agent manifest schema summary. Load it whenever you need exact
  flags or are writing a manifest.
- [references/troubleshooting.md](references/troubleshooting.md) — symptom
  → cause → fix for setup and fleet problems. Load it when any step
  fails or the user reports an error.
- Upstream docs (if the repo is checked out locally): `docs/PLAN.md`
  (architecture), `docs/AUTO-SUSPEND.md`, `docs/SUZY.md`, `docs/MCP.md`.
