# Suzerain / Castellan — Architecture & Delivery Plan (v3)

**Status: DRAFT v3 for review — no code written yet.** *(Historical: this
plan predates the binary/protocol unification below — kept as-is for the
networking/isolation/secrets/manifest design, which is still current.
Where this doc and reality now disagree, reality wins — see
[docs/UNIFIED-AGENT-API-DESIGN.md](UNIFIED-AGENT-API-DESIGN.md) for what
actually shipped:*

- ***One binary, not two.*** *There is no separate `castellan` binary —
  `suzerain` is the only binary, selecting its role via `suzerain run
  --mode standalone|control|agent` (`standalone`, one box, is the
  default). `castellan` is now a library crate `suzerain` depends on.
  Everywhere below that says "castellan daemon" or "two daemons," read
  "the agent-hosting role of `suzerain`."*
- ***One client protocol, not four.*** *`suz`, Suzy, and `suzerain-mcp`
  all sit on one shared client crate (`suzerain-client`) over two
  transports (direct HTTP, or the iroh operator channel) — not four
  independent wire protocols. `suz` talks REST now, not a Unix socket.*
- ***Declarative provisioning exists.*** *An agent manifest's optional
  `[provision]` section can fully replace the hardcoded Alpine/npm/mise/pi
  bootstrap described in §6 below with typed install resolvers
  (npm/git/mise) + a raw-script escape hatch.*)

v3 deltas from review: n0 public relays accepted; **Gondolin microVMs replace bubblewrap/seatbelt as the isolation layer**; SOPS-via-CLI + age keypair confirmed; graceful-shutdown semantics for workspaces; daemon-scoped git key; infinite central retention with daemon-side pruning after ack.

## 1. Scope (confirmed)

Multi-server AI agent lifecycle system, control-plane/data-plane split, all node communication over **iroh**:

- **castellan** — Rust daemon per server (macOS + Linux). Provisions and supervises long-lived named agents; each agent is a **Pi process running in RPC mode inside its own Gondolin microVM**, fully isolated (own pi-home, workspace, extensions, secrets, egress policy).
- **suzerain** — Rust control plane. Registry of daemons + named agents, scheduling, manifest distribution, secret slicing from the age store, session-attach relay, and the **centralized, indefinitely-retained event-log store** that powers restore-on-any-server.

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

## 4. Secrets (age + Gondolin placeholders — Q6, Q7, Q-E)

Two-layer design, stronger than either alone:

1. **Store:** `secrets.age` (armored age file, YAML payload) in the fleet
   home (`$SUZERAIN_HOME`), encrypted to the operator's age recipient from
   `$SUZERAIN_HOME/age-keys.txt` (auto-generated on first use;
   `SOPS_AGE_KEY_FILE` overrides). Pure-Rust age (rage crate) — no sops
   subprocess; plaintext lives only in memory (`secrecy` types). Legacy
   `secrets.sops.yaml` auto-migrates once at startup.
2. **Slicing:** manifest declares scopes — `providers: [openai]`, `git_key: daemon` etc. Suzerain slices exactly those entries. An OpenAI-configured agent's bundle contains zero Anthropic material.
3. **Delivery & injection:** bundle streams over the encrypted iroh channel to castellan → gondolin-driver → **Gondolin HTTP hooks**: the guest env gets *placeholder* tokens; the host-side hook injects real credentials only for the allowlisted provider hosts. The agent process **never holds raw keys**, so even a fully prompt-injected agent cannot exfiltrate them.
   - Env-injection fallback (no placeholder) only for credentials that can't ride HTTP hooks.
4. **Git SSH (Q-E):** one SSH key per daemon host — any key `ssh-keygen` produces (ed25519/ecdsa/RSA, OpenSSH format), validated by parsing at upload time — held in the secrets store and delivered to the daemon host-side only. Guest ssh/git traffic to manifest-declared git hosts rides Gondolin's SSH egress proxy, which performs the upstream authentication with the key — **the private key never enters the microVM**, yet `git clone`/`pull`/`push` work transparently inside it.

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

[[extensions]]                   # pi package install source (pi.dev catalog), or:
source = "npm:@scope/pi-package" #   pi install npm:… / git:… at provision time
[[extensions]]                   # …or each its own git repo, pinned (Q14)
url = "git@github.com:me/deep-research-ext.git"
ref = "v1.2.0"

[prompt]                         # rendered into the agent's isolated pi-home
append_system = """               # → APPEND_SYSTEM.md (appended to pi's system prompt)
You are a meticulous researcher.
"""

[secrets]
providers = ["openai"]           # sliced from the secrets store; delivered as Gondolin hooks

[egress]                         # extra allowlisted hosts beyond provider/git/npm/otel
allow = ["crates.io"]

[observability.otel]             # set on control plane, fanned out per agent (Q15)
endpoint = "https://otel.example.com"
headers = { authorization = "…" }
```

Per-agent isolation (Q8) is total: own VM, own `PI_CODING_AGENT_DIR` (in-guest), own workspace, own extensions, own secrets. Nothing global is shared between agents.

## 6. castellan — daemon internals

*(This module layout now lives in the `castellan` library crate, run
either as `suzerain --mode agent` or as standalone mode's co-located
child process — not a separate binary. See the note at the top of this
doc.)*

```
control/    iroh control client: enroll, heartbeat, order dispatch, reconnect
supervisor/ agent state machine, backoff restart, crash-loop detection, graceful stop
driver/     gondolin-driver sidecar client (unix socket JSON-RPC)
harness/    HarnessAdapter trait → PiRpcAdapter (spawn-in-VM, prompt/steer/abort,
            get_state/get_messages, session resume) — Codex/Claude adapters later
rpc/        pi JSONL framing (LF-only per rpc.md), id correlation, event fan-out
provision/  VM boot, in-guest mise install, repo clones, extension clones, pi-home
secrets/    secrets-store slices → gondolin HTTP hooks / env; redaction filters
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

*(This aspiration is now real: `Registry` (formalizing exactly this
trait), `SnapshotStore`, and `ChatStore` all exist —
`ChatStore` moved log payloads into a SQLite `chat_events` table rather
than files. See docs/UNIFIED-AGENT-API-DESIGN.md §4.2/§4.6–4.7.)*

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

- **Shared home, disjoint names:** both daemons default to one fleet home,
  `~/.local/share/suzerain/` (`SUZERAIN_HOME`/`CASTELLAN_HOME` override),
  with no overlapping file names: `suzerain.toml` vs `castellan.toml`,
  `suzerain.key` vs `castellan.key`, `suzerain.sock` vs `castellan.sock`,
  `suzerain.db`, `secrets.age` + `age-keys.txt`, `logs/`/`bundles/` vs
  `agents/`/`driver/`.
- **Disjoint identity:** each process has its own iroh keypair/EndpointId and its own entry in the registry; a co-located castellan is enrolled/approved exactly like a remote one.
- **Disjoint runtime resources:** separate gondolin-driver socket, separate service units (`suzerain.service` + `castellan.service` / two launchd agents), no fixed ports anywhere (iroh endpoints bind ephemeral/QUIC).
- **Scheduler neutrality:** the co-located daemon advertises labels/capacity like any other; nothing prefers or avoids it unless labeled.
- Non-v1 nicety (noted, not built): a single-process `suzerain --with-castellan` combined mode for absolute-minimal setups.

Day-one setup on a fresh machine: install qemu (brew/apt) + mise → `mise run setup` → `suzerain` (zero-config, SQLite, prints EndpointId) → `castellan init` (keypair, prints EndpointId; **same machine or another — identical flow**) → `suzerain daemon approve <id>` → `suzerain agent create --name foo --manifest foo.toml`.

## 11. Phased delivery

- **P0 — Scaffold & spikes. (DONE — see docs/PHASE0-FINDINGS.md.)** Workspace, protocol crate, CI (macOS+Linux). Validated: (a) Rust pi-RPC client incl. **session resume** (restore primitive); (b) iroh order/ack + gossip over mDNS+relays — *design rules: establish the control connection before gossip joins; accept handlers must `connection.closed().await` after finishing a stream*; (c) gondolin-driver boots a VM and streams a long-running process's stdio bidirectionally via `vm.exec` (no SSH needed for the dataplane); (d) guest recon: base image has `node` but not `npm`/`git`/`mise` — provisioning must `apk add` or ship a custom image.
- **P1 — castellan standalone. (DONE — validated end-to-end 2026-08-10.)** Provisioning pipeline in-VM (base apk packages; npm/pi/mise installed onto the host-mounted `/agent` volume because the guest rootfs is ~260MB; repo clones; extension repos; isolated pi-home with generated trust), supervisor with lifecycle states, seq-numbered JSONL journal, unix-socket control API + CLI (`create/start/stop/destroy/list/logs/attach/ask/exec`). Validated: create (58s cold) → ask → stop → start (5s warm) → **memory survives restart via session resume** → destroy. Findings folded into code comments: array-form `vm.exec` does not search $PATH (use absolute paths); apk's npm is incompatible with the guest's baked-in node (fetch the npm tarball instead); all driver/pi commands need timeouts + pending-drain on process death.
- **P2 — suzerain core + fabric. (DONE — validated end-to-end 2026-08-10.)** iroh control plane: persistent node identities (EndpointId allowlist enrollment: `castellan init` → `suz daemon approve`), one long-lived connection per daemon (castellan dials `suz/control/0`, registers; orders flow down the register stream; logs/attach are labeled bi-streams on the same connection — avoiding the multi-connection resumption hang from P0), fleet gossip for presence, heartbeat Ping orders. SQLite store (daemons/agents/log-index), least-loaded scheduler, operator unix socket + `suz` CLI. Central log shipping with `(agent_id, seq)` acks; castellan prunes only suspended-and-acked journals (pruning live journals detaches open append handles — found and fixed). Validated: enroll → create via control plane (56s) → ask through the attach relay → stop → local journal pruned, central retains all → start → cross-restart memory intact → destroy.
- **P3 — Attach & restore-anywhere. (DONE — validated end-to-end 2026-08-10.)** `suz agent attach` streams history (message_end events from the central log) then live events with interactive prompts, relayed suz → suzerain → castellan → pi. Suspend = graceful stop + Gondolin disk checkpoint + bundle upload (pi session files + pi-home, base64-chunked `BundleMessage`s on a labeled stream); start resumes the checkpoint in ~5s. Restore streams the bundle to any approved daemon (`suz agent restore --daemon <id>`), which writes files, re-provisions fresh (toolchain/repos from the manifest), and resumes the pi session. Validated with two daemons on one host: created on B → codeword → suspend → **restored on A → agent remembered the codeword** → suspend → checkpoint resume. Findings: gondolin guest assets need ≥0.12 for checkpoint support (0.5 lacks the manifest buildId — graceful fallback to plain stop); driver must catch the agent exec result promise or checkpoint/close kills it with an unhandled rejection.
- **P4 — Secrets & hardening. (DONE — validated end-to-end 2026-08-10.)** SOPS store (`secrets.sops.yaml`, age; decrypted via the sops CLI honoring `SOPS_AGE_KEY_FILE`) loaded into memory on suzerain; per-agent slicing — a manifest declaring only `kimi-coding` yields exactly that key. Delivery over the encrypted control channel; injection via **Gondolin HTTP-hook placeholders**: validated that pi's process env contains `KIMI_API_KEY=GONDOLIN_SECRET_…` (placeholder) and no `ANTHROPIC_API_KEY`, while LLM calls succeed (host-side injection, api.kimi.com only). Per-agent **egress allowlist** enforced (npm registry reachable / api.anthropic.com → 403). Secrets re-sliced at restore (never persisted in bundles). Journals redact all registered secret values. Audit log (`suz audit`) covers approvals + all lifecycle actions. Also fixed: journal seq watermark survives pruning; daemon startup reconciles stale `active` records to suspended; destroy is idempotent. Known gaps: SSH-clone egress path is wired (host-side key via gondolin ssh credentials) but not yet e2e-tested with a real SSH key; daemon state reporting on reconnect is naive (registry convergence).
- **P5 — Ops. (DONE — validated 2026-08-10.)** OTEL for the daemons themselves: `suzerain_protocol::telemetry` exports tracing spans via OTLP/HTTP-proto when `OTEL_EXPORTER_OTLP_ENDPOINT` is set. Pluggable store: `SUZERAIN_DATABASE_URL` selects sqlite (zero-config default) or postgres — validated with a full lifecycle against local postgres (portable SQL: TEXT/BIGINT columns, `?`→`$n` rewriting, `ON CONFLICT…excluded` upserts; pg INT4-vs-INT8 decode bug found and fixed via BIGINT DDL). Retention: keep-forever default; `[retention] days = N` prunes central log events older than N days (hourly sweep, validated). Ops packaging: `ops/systemd/*.service` + `ops/launchd/*.plist` user-service templates, `ops/install-services.sh` (detects OS, builds + installs binaries, enables services), `mise run package` / `mise run install:services` tasks.

(Minimal secret delivery exists from P2 — agents need provider keys day one; the polished SOPS UX lands in P4.)

## 12. Resolved decisions log

| Q | Decision |
|---|---|
| Transport | iroh (QUIC, pubkey identity); gossip for presence only; reliable streams for control/logs/attach/restore |
| Co-location | suzerain + castellan may run on the same host: one shared fleet home with disjoint file names/identities/sockets, same iroh code path, scheduler-neutral |
| Identity | iroh EndpointId allowlist + single-operator token; no CA/mTLS |
| Discovery | n0 public relays + DNS to start; mDNS on LAN; self-host relay optional later |
| DB | pluggable: SQLite zero-config default, Postgres via config |
| Secrets | **age (rage crate, pure Rust)**: store is `secrets.age` (armored age file, YAML payload) in the fleet home, encrypted to the operator's age recipient from `$SUZERAIN_HOME/age-keys.txt` (auto-generated; `SOPS_AGE_KEY_FILE` overrides); legacy `secrets.sops.yaml` auto-migrates once via a final sops-CLI call. Per-agent slicing; Gondolin placeholder injection so guests never hold raw provider keys; one git SSH key per daemon (host-side only — guest git rides the ssh egress proxy). (2026-08-11: replaced the flaky sops-CLI write path with native age — no subprocesses anywhere in the secrets path.) |
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

## 14. Known gaps & debt (as of 2026-08-10)

Things that are built but incomplete, broken at the edges, or validated less
than the rest. Ordered roughly by severity.

### G1. ~~No crash respawn (supervision is log-only)~~ FIXED (2026-08-10)
The supervisor now runs a per-agent monitor that respawns on crash: pi exit →
pi-only respawn inside the live VM (session resumed); driver/VM death →
escalating retry that re-boots the VM and resumes. Exponential backoff
(2s→60s), crash-loop cap of 5 restarts per 10-minute window, then the agent
is journaled + persisted `Failed` and leaves the running map. Deliberate
stops set a `stopping` flag before shutdown so exits are never respawned.
Validated live: pi kill → respawn in 5s with memory; driver kill → pi-only
attempt fails → full re-boot in 13s with memory; rapid kill loop → marked
Failed; graceful stop → no respawn.

### G2. ~~Registries diverge on failure paths~~ FIXED (2026-08-10)
- **State reporting**: daemons open a `StateReport` stream after registration
  — full snapshot immediately, then every lifecycle transition (create/fail,
  active, suspended, failed-in-crash-loop, respawned, decommissioned).
  Suzerain applies entries only for agents whose registry row belongs to the
  reporting daemon (anti-spoofing); a `decommissioned` report deletes the row.
  Validated: local stop (no order) → DB suspended within seconds; crash-loop
  → DB failed with zero orders involved.
- **Duplicate-instance fencing**: castellan holds an flock on
  `castellan.lock`; a second daemon on the same data dir exits with a clear
  error. Validated.
- **Connection flapping FIXED (2026-08-12)**: the periodic `timed out` /
  `connection lost` churn (visible in user logs after registration) traced
  to QUIC idle timeouts — connection-level `max_idle_timeout` raised to 60s
  on both endpoints and the app heartbeat tightened 30s → 10s. Validated:
  zero flaps over 2 minutes of activity + idle.
- **Session fencing**: registrations carry a monotonically increasing epoch;
  a superseded session's disconnect handler no longer marks the daemon
  offline (kills the reconnect flap observed in P2–P4 e2e).
- Failed-create residue: covered by state reports (daemon marks and reports
  Failed) plus the existing idempotent destroy.

### G3. ~~Restore bundle freshness~~ FIXED (2026-08-10, event-driven 2026-08-11)
Bundles refresh **event-driven with debounce**, not on a timer: after journal
activity, the upload fires once the agent has been quiet for
`[bundle] quiet_secs` (default 30s); a `refresh_secs` backstop (default 900s)
forces an upload even for a continuously busy agent. Idle agents upload
nothing (validated: one upload per work burst, silence afterward). Suzerain
wipes the agent's bundle files on each upload start, so the central bundle
always mirrors the latest upload. Two convergence fixes fell out of the e2e:
(a) full state snapshots reconcile — agents owned by a daemon but absent from
its post-registration snapshot are marked Failed (covers wipe/loss); (b) the
restore guard only blocks when the owning daemon is actually live (a crashed
daemon's "active" is stale). Validated: codeword → NO suspend → debounced
upload ~quiet-period later → daemon hard-killed and local state wiped →
restarted daemon marks the agent Failed via snapshot → restore from the
refreshed bundle → agent remembers the codeword.

### G4. ~~iroh multi-connection quirk worked around, not root-caused~~ RESOLVED (not reproducible, 2026-08-11)
Rebuilt the failing condition as a deterministic repro
(`crates/suzerain/examples/spike_multiconn.rs`): second connection to the
same peer on a different ALPN while the first is open — same process, cross
process, gossip-first with live traffic (the exact spike-B failure shape),
close-first, new endpoint, tickets disabled. **All variants pass repeatedly
on iroh 1.0.3 / noq 1.1.1** (the identical versions that failed 3/3 during
Phase 0). Conclusion: the Phase 0 hang was a transient condition (the logs
showed the second handshake stalling mid-resumption with a
`MultipathNotNegotiated` path error — consistent with a relay/path hiccup,
not deterministic logic). The single-connection design rule stays regardless
(it is also simply better design), and the repro is kept as a regression
check: `cargo run -p suzerain --example spike_multiconn -- <variant>`. If it
ever recurs, the repro is packaged for an upstream issue.

### G5. ~~SSH git clones untested end-to-end~~ FIXED (2026-08-11)
Validated with a real read-only GitHub deploy key (created, tested, deleted):
the guest cloned `git@github.com:Shakakai/suzerain.git` through gondolin's
SSH proxy with the key held host-side from the SOPS store. Two issues found
and fixed: (a) the guest's empty `known_hosts` failed first contact — clone
commands now set `GIT_SSH_COMMAND=ssh -o StrictHostKeyChecking=accept-new`
(upstream verification stays host-side via the proxy); (b) host-mounted repos
trip git's `safe.directory` ownership check (host uid ≠ guest root) —
provisioning now sets `safe.directory '*'` in the guest. Debugging note: an
earlier silent failure traced to the host's ssh-agent fallback masking an
unenrolled key — always verify which identity authenticated (`ssh -v`).
Example manifest: `examples/researcher-ssh.toml`.

### G6. ~~Operator sockets are unauthenticated~~ FIXED (2026-08-11)
Both operator unix sockets (suzerain + castellan) now verify peer credentials
on every connection: `SO_PEERCRED` (Linux) / `LOCAL_PEERCRED` (macOS) — only
processes running as the same effective uid as the daemon may issue commands;
other uids are rejected and logged. Zero-config and exactly matches the
single-operator model. Happy path validated on macOS; the platform-specific
struct layouts are covered by a socketpair unit test. (A different-uid client
can't be exercised without root; the check is a single uid equality verified
by review + the unit test.)

### G7. ~~Secrets persisted on daemon disk~~ FIXED (2026-08-11)
Bundles now live only in daemon RAM: an in-memory `SecretStore` replaces the
`secrets.json` file (deleted, no longer written). When a daemon needs a
bundle it doesn't have — e.g. starting an agent after a daemon restart — it
pulls a freshly-sliced one from suzerain over a `StreamHello::Secrets`
stream; suzerain re-slices from the SOPS store after verifying the agent
belongs to that daemon. Standalone local creates still use the daemon-env
fallback (memory-only; restarts without a control plane require starting via
the control plane). Validated: create + ask with zero `secrets.json` on disk;
daemon killed (-9), restarted, `start` → bundle re-pulled → ask works; disk
still clean.

### G8. Smaller items
- **~~Scheduler ignores labels/capacity labels~~ FIXED (2026-08-11, resource-aware scheduling)**
  Two-phase placement (Kubernetes model, deep-researched): **filter**
  (approved+online → hard pin → label subset match → resource fit
  `capacity − allocated − reserve ≥ request`) then **spread score**
  (normalized free fraction, cpu/mem/vram weighted 1/1/1, disk 0.5).
  Manifest `[resources]` (vcpu/memory_mib/disk_mib with defaults 2/2048/5120,
  `[resources.gpu]` count+vram_mib) is **enforced into the Gondolin VM**
  (memory/cpus from the manifest, validated: 4 vCPU / 4 GiB guest).
  Daemons probe + report capacity (cpu/mem/disk/GPUs incl. nvidia-smi VRAM
  and Apple unified-memory semantics) at registration and usage via
  heartbeat acks; suzerain computes allocated as Σ agent requests.
  Operator-side label overrides via `suz daemon label` (merged, overrides
  win); `castellan init --label`. Rejection errors name every candidate and
  reason. GPU scheduling only — passthrough deferred. Network field dropped
  per review.
- **~~No restore integrity checks~~ FIXED (2026-08-11)** — bundle files now
  carry SHA-256 checksums at three layers: per-file hashes recorded in the
  bundle meta at upload (tamper evidence for at-rest corruption, verified
  before any restore streams out — validated with a deliberately corrupted
  stored bundle), sender-side hashes recomputed at restore-send, and
  receiver-side verification after decode (transit integrity).
- **~~Retention covers central logs only~~ FIXED (2026-08-11)** — the sweep
  now also prunes `audit.jsonl` entries older than `[retention] audit_days`
  and removes stale bundles older than `[retention] bundle_days` (both
  default 0 = keep forever, consistent with Q-F). Validated: 10-day-old
  audit entry and stale bundle pruned; fresh kept.
- **~~No e2e in CI~~ FIXED (2026-08-11)** — `ops/e2e.sh` runs the full
  lifecycle against a real stack (sops store → enroll → create → ask →
  stop/start memory → suspend/restore memory → central logs → destroy),
  locally and in the `e2e.yml` workflow (macos-latest: qemu/sops/age via
  brew, cached Gondolin guest assets, workspace build). Skips gracefully
  when `KIMI_API_KEY` is unset; runs for real once the repo secret exists.
  Validated locally and in CI.
- **Attach is single-viewer, history is central-log-derived** — no
  multi-viewer watch, and history reconstructs only `message_end` events
  (tool outputs render raw).
- **OTEL context not propagated to agents** — agent-side OTEL config exists
  (manifest block → env), but there's no trace-context link between daemon
  spans and agent activity.
- **Castellan state is JSON files, not SQLite** — the daemon-local store
  (`state.json` per agent) diverges from the plan's SQLite sketch; fine at
  current scale.
