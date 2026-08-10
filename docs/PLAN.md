# Suzerain / Castellan — Architecture & Delivery Plan (v3)

**Status: DRAFT v3 for review — no code written yet.**

v3 deltas from review: n0 public relays accepted; **Gondolin microVMs replace bubblewrap/seatbelt as the isolation layer**; SOPS-via-CLI + age keypair confirmed; graceful-shutdown semantics for workspaces; daemon-scoped git key; infinite central retention with daemon-side pruning after ack.

## 1. Scope (confirmed)

Multi-server AI agent lifecycle system, control-plane/data-plane split, all node communication over **iroh**:

- **castellan** — Rust daemon per server (macOS + Linux). Provisions and supervises long-lived named agents; each agent is a **Pi process running in RPC mode inside its own Gondolin microVM**, fully isolated (own pi-home, workspace, extensions, secrets, egress policy).
- **suzerain** — Rust control plane. Registry of daemons + named agents, scheduling, manifest distribution, SOPS-sliced secret delivery, session-attach relay, and the **centralized, indefinitely-retained event-log store** that powers restore-on-any-server.

## 2. Networking & identity (iroh 1.0)

- Identity = iroh `SecretKey`/`EndpointId` (ed25519). No CA, no mTLS, no pre-shared certs. Enrollment: `castellan init` prints its EndpointId → operator approves with the single-operator token: `suzerain daemon approve <EndpointId>`. Non-allowlisted EndpointIds are rejected at the accept layer.
- Discovery: **n0 public relays + DNS (`presets::N0`) to start** (relays forward ciphertext only); mDNS for zero-config LAN setup; self-hosted relay remains a config option.
- One iroh `Router` per node, ALPN-multiplexed:

| ALPN | Direction | Purpose |
|---|---|---|
| *(iroh-gossip)* | all ↔ all | Fleet topic: presence, announcements, liveness deltas (**best-effort only**) |
| `suz/control/0` | suzerain→castellan | Targeted orders (Create/Start/Stop/Suspend/Restore/Destroy/UpdateManifest) + acks |
| `suz/logs/0` | castellan→suzerain | Reliable event-log shipping: `(agent_id, seq)` numbered, acked, resumable |
| `suz/attach/0` | cli↔suzerain↔castellan | Session attach: history, then live stream; prompt/steer/follow_up/abort downstream |
| `suz/restore/0` | suzerain→castellan | Agent bundle streaming (manifest + pi-home + session JSONLs), chunked + resumable |

**Gossip vs. reliable streams (Q11):** iroh-gossip (HyParView/PlumTree) has no delivery guarantees or persistence, so it carries only self-correcting ephemeral signals. Event logs use direct QUIC streams with acks + resume; the castellan journal is source-of-truth until suzerain acks (at-least-once, deduped by seq).

## 3. Isolation: Gondolin microVM per agent (Q12)

- One **Gondolin VM per agent** (QEMU backend by default; krun is experimental/ARM64-most-tested — start with QEMU). Host prerequisites: `qemu` + `node` (brew/apt — documented in setup; mise verifies).
- Because the guest is always Alpine Linux, **the agent runtime is identical on macOS and Linux hosts** — a genuine cross-platform win. All in-guest provisioning (mise tools, node, pi) targets Linux only.
- **Egress policy per agent** via Gondolin HTTP hooks: allowlists derived from the manifest (LLM provider endpoints, `github.com`/git host, npm registry, OTEL endpoint, nothing else by default).
- **Snapshots:** Gondolin disk checkpoints map to suspend→boot on the same host (fast path); cross-server restore uses the bundle (§7).

### 3.1 Integration: gondolin-driver sidecar

Gondolin's control plane is TypeScript; castellan is Rust. Design:

```
castellan (Rust)  <—JSON-RPC over unix socket—>  gondolin-driver (Node, per daemon)
                                                     │  @earendil-works/gondolin SDK
                                                     ├─ VM.create({ httpHooks, env, mounts })
                                                     ├─ vm.enableSsh() → host→guest SSH
                                                     ├─ spawn pi --mode rpc via SSH exec channel
                                                     │    (stdio streamed = our JSONL dataplane)
                                                     └─ vm.snapshot() / VM resume
```

One driver process per daemon manages all agent VMs. Castellan owns lifecycle/policy; the driver is a thin adapter. (Alternative: gondolin CLI + `attach`; the SDK sidecar is cleaner for streaming stdio and per-agent egress hooks. Validated in the P0 spike — specifically: SSH exec channel stdio fidelity for LF-delimited JSONL.)

## 4. Secrets (SOPS + Gondolin placeholders — Q6, Q7, Q-E)

Two-layer design, stronger than either alone:

1. **Store:** `secrets.sops.yaml` (age) on suzerain; decrypted via the **`sops` CLI** (mise-installed) using the age keypair at `~/.config/suzerain/keys.txt`; plaintext lives only in memory (`secrecy` types).
2. **Slicing:** manifest declares scopes — `providers: [openai]`, `git_key: daemon` etc. Suzerain slices exactly those entries. An OpenAI-configured agent's bundle contains zero Anthropic material.
3. **Delivery & injection:** bundle streams over the encrypted iroh channel to castellan → gondolin-driver → **Gondolin HTTP hooks**: the guest env gets *placeholder* tokens; the host-side hook injects real credentials only for the allowlisted provider hosts. The agent process **never holds raw keys**, so even a fully prompt-injected agent cannot exfiltrate them.
   - Env-injection fallback (no placeholder) only for credentials that can't ride HTTP hooks.
4. **Git SSH (Q-E):** one deploy key per daemon host, held by castellan (from the SOPS store, daemon scope) and used for guest clones via Gondolin's allowlisted SSH egress / `GIT_SSH_COMMAND` with the key mounted 0600 inside that agent's VM only.

## 5. Agent manifest (TOML; Q8, Q13, Q14, Q15)

```toml
name = "researcher-1"
harness = { type = "pi", version = "0.84.1" }
model = { provider = "openai", id = "gpt-5", thinking = "high" }

[toolchain]                      # applied in-guest via mise at provision time
tools = { node = "22", python = "3.12" }

[[repos]]                        # 1..n fresh SSH clones into /workspace (Q10)
url = "git@github.com:org/repo.git"
ref = "main"

[[extensions]]                   # each its own git repo, pinned (Q14)
url = "git@github.com:me/deep-research-ext.git"
ref = "v1.2.0"

[secrets]
providers = ["openai"]           # sliced from SOPS store; delivered as Gondolin hooks

[egress]                         # extra allowlisted hosts beyond provider/git/npm/otel
allow = ["crates.io"]

[observability.otel]             # set on control plane, fanned out per agent (Q15)
endpoint = "https://otel.example.com"
headers = { authorization = "…" }
```

Per-agent isolation (Q8) is total: own VM, own `PI_CODING_AGENT_DIR` (in-guest), own workspace, own extensions, own secrets. Nothing global is shared between agents.

## 6. castellan — daemon internals

```
control/    iroh control client: enroll, heartbeat, order dispatch, reconnect
supervisor/ agent state machine, backoff restart, crash-loop detection, graceful stop
driver/     gondolin-driver sidecar client (unix socket JSON-RPC)
harness/    HarnessAdapter trait → PiRpcAdapter (spawn-in-VM, prompt/steer/abort,
            get_state/get_messages, session resume) — Codex/Claude adapters later
rpc/        pi JSONL framing (LF-only per rpc.md), id correlation, event fan-out
provision/  VM boot, in-guest mise install, repo clones, extension clones, pi-home
secrets/    SOPS-sliced bundles → gondolin HTTP hooks / env; redaction filters
journal/    append-only seq-numbered JSONL + sqlite index; ack-based pruning (§8)
shipper/    suz/logs/0 reliable streaming with resumable offsets
```

## 7. suzerain — control plane internals

- Daemon registry (EndpointId allowlist, heartbeats via gossip + timeouts, capacity labels, drain).
- Agent registry: named pets; `Provisioning → Active ⇄ Suspended → Restoring → Active`, `Failed`, `Decommissioned`.
- Scheduler: label/capacity/locality filter → least-loaded.
- Central log store: JSONL payloads on disk (append-only), DB index (offsets, seq ranges, checksums). **Retain everything indefinitely (Q-F)**; S3 offload additive later.
- Session attach relay: history from central store → live via owning castellan.
- Operator CLI (`crates/suzerain-cli`) over iroh streams; browser HTTP deferred.

### Lifecycle flows

- **Graceful shutdown/suspend (Q-D):** agent is notified and given a cleanup window (finish turn, commit work) → checkpoint → VM snapshot (same-host boot fast path) → journal flushed until suzerain-acked.
- **Hard crash:** accepted — journal is authoritative up to last shipped seq; the agent loop reconciles on resume. Uncommitted workspace work may be lost.
- **Restore on any server (Q9, v1):** bundle (manifest + pi-home + session JSONLs + journal) streams over `suz/restore/0` → target castellan boots a fresh VM, re-clones repos at pinned refs, re-runs mise, resumes the pi session. Uncommitted worktree changes are not preserved (accepted, Q-D).

## 8. Log retention & pruning (Q-F)

- Suzerain: keeps everything, indefinitely.
- Castellan: keeps local journal segments until suzerain acks their seq ranges, then prunes — **except segments belonging to agents currently Active on this daemon** (active logs are never pruned). Suspended agents' logs are pruned once fully acked and the suspend snapshot/bundle is confirmed stored centrally.

## 9. Storage (pluggable — Q3)

`Storage` trait (daemons, agents, manifests, log index, audit) with `sqlite://` (**default, zero-config**, WAL) and `postgres://` backends; `sqlx` runtime-checked queries behind the trait. Log payloads are files; the DB indexes them.

## 10. Monorepo layout & setup

```
Cargo.toml (workspace)
crates/
  protocol/      # manifests, orders, events, ALPN consts, framing, state enums
  castellan/     # daemon binary + lib
  suzerain/      # control plane binary + lib
  suzerain-cli/  # operator CLI
tools/
  gondolin-driver/   # Node sidecar (TS, official gondolin SDK)
mise.toml        # rust, node, sops, + tasks (setup, dev, test)
docs/PLAN.md
```

### Co-location: suzerain + castellan on the same host

The architecture explicitly supports running both on one machine (the "laptop does everything" case) with **no special-case code path** — co-located peers talk over the same iroh protocols (loopback/mDNS is just another connection). Requirements this imposes, honored throughout:

- **Disjoint state:** `~/.local/share/suzerain/` vs `~/.local/share/castellan/` (XDG-respecting), separate configs, logs, DBs, sockets.
- **Disjoint identity:** each process has its own iroh keypair/EndpointId and its own entry in the registry; a co-located castellan is enrolled/approved exactly like a remote one.
- **Disjoint runtime resources:** separate gondolin-driver socket, separate service units (`suzerain.service` + `castellan.service` / two launchd agents), no fixed ports anywhere (iroh endpoints bind ephemeral/QUIC).
- **Scheduler neutrality:** the co-located daemon advertises labels/capacity like any other; nothing prefers or avoids it unless labeled.
- Non-v1 nicety (noted, not built): a single-process `suzerain --with-castellan` combined mode for absolute-minimal setups.

Day-one setup on a fresh machine: install qemu (brew/apt) + mise → `mise run setup` → `suzerain` (zero-config, SQLite, prints EndpointId) → `castellan init` (keypair, prints EndpointId; **same machine or another — identical flow**) → `suzerain daemon approve <id>` → `suzerain agent create --name foo --manifest foo.toml`.

## 11. Phased delivery

- **P0 — Scaffold & spikes. (DONE — see docs/PHASE0-FINDINGS.md.)** Workspace, protocol crate, CI (macOS+Linux). Validated: (a) Rust pi-RPC client incl. **session resume** (restore primitive); (b) iroh order/ack + gossip over mDNS+relays — *design rules: establish the control connection before gossip joins; accept handlers must `connection.closed().await` after finishing a stream*; (c) gondolin-driver boots a VM and streams a long-running process's stdio bidirectionally via `vm.exec` (no SSH needed for the dataplane); (d) guest recon: base image has `node` but not `npm`/`git`/`mise` — provisioning must `apk add` or ship a custom image.
- **P1 — castellan standalone. (DONE — validated end-to-end 2026-08-10.)** Provisioning pipeline in-VM (base apk packages; npm/pi/mise installed onto the host-mounted `/agent` volume because the guest rootfs is ~260MB; repo clones; extension repos; isolated pi-home with generated trust), supervisor with lifecycle states, seq-numbered JSONL journal, unix-socket control API + CLI (`create/start/stop/destroy/list/logs/attach/ask/exec`). Validated: create (58s cold) → ask → stop → start (5s warm) → **memory survives restart via session resume** → destroy. Findings folded into code comments: array-form `vm.exec` does not search $PATH (use absolute paths); apk's npm is incompatible with the guest's baked-in node (fetch the npm tarball instead); all driver/pi commands need timeouts + pending-drain on process death.
- **P2 — suzerain core + fabric.** Enrollment, control protocol, gossip presence, SQLite store, CLI CRUD, **central log shipping (suz/logs/0)** + ack-based pruning.
- **P3 — Attach & restore-anywhere.** Attach relay w/ history; suspend (snapshot) / boot; cross-server bundle restore.
- **P4 — Secrets & hardening.** SOPS slicing → Gondolin placeholder hooks, journal redaction, audit log.
- **P5 — Ops.** systemd/launchd units, OTEL for the daemons themselves, Postgres backend, retention/offload policies.

(Minimal secret delivery exists from P2 — agents need provider keys day one; the polished SOPS UX lands in P4.)

## 12. Resolved decisions log

| Q | Decision |
|---|---|
| Transport | iroh (QUIC, pubkey identity); gossip for presence only; reliable streams for control/logs/attach/restore |
| Co-location | suzerain + castellan may run on the same host: disjoint state/identity/sockets, same iroh code path, scheduler-neutral |
| Identity | iroh EndpointId allowlist + single-operator token; no CA/mTLS |
| Discovery | n0 public relays + DNS to start; mDNS on LAN; self-host relay optional later |
| DB | pluggable: SQLite zero-config default, Postgres via config |
| Secrets | SOPS (age) via sops CLI; per-agent slicing; Gondolin placeholder injection so guests never hold raw keys; one git deploy key per daemon |
| Pi isolation | full per-agent: VM + PI_CODING_AGENT_DIR + workspace + extensions |
| Isolation | Gondolin microVM per agent (QEMU backend), per-agent egress allowlist |
| Restore | any-server from v1 via centralized logs + bundle streaming; uncommitted worktree loss accepted; graceful shutdowns give cleanup window |
| Toolchain | mise everywhere (host + in-guest agent tools); extensions = pinned git repos |
| Agent lifetime | long-lived named pets; suspend/boot on demand |
| Retention | control plane keeps all logs forever; daemon prunes acked segments not in active use |
| OTEL | configured on control plane, fanned out as a manifest block → env on agent spawn |
| Repo | cargo workspace at root; mise manages toolchains |

## 13. Out of scope for v1

Non-Pi harnesses (seam only) · web UI · multi-user RBAC · A2A · per-agent gossip watch topics · container isolation beyond Gondolin · cost dashboards · krun backend (QEMU first)
