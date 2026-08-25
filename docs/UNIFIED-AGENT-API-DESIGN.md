# Unified Agent API — design document (DRAFT)

Status: **draft for discussion** — not implemented, not scheduled.

## 0. Why this doc exists

Today, "create/chat/suspend/delete" is already the *user-facing philosophy*
(see `docs/AUTO-SUSPEND.md` — start/stop/suspend/restore were deliberately
removed as user verbs in favor of create/chat/destroy). What is **not**
unified is everything underneath that philosophy:

- Two separate binaries (`suzerain`, `castellan`) with two separate data
  stores, two separate identities, two separate config files.
- **Four** distinct wire protocols carrying overlapping "create an agent /
  send it a message / destroy it" semantics, none sharing a client:
  1. `suz` CLI ↔ suzerain, plain JSONL over a Unix socket (`api.rs`)
  2. Suzy/`suzerain-client` ↔ suzerain, iroh operator channel wrapping REST
     (`OperatorHello::Rest`)
  3. `suzerain-mcp` ↔ suzerain, plain HTTP REST (`reqwest`)
  4. a local CLI ↔ castellan, plain JSONL over a *different* Unix socket
     (`daemon.rs`), used only in "standalone, no control plane" mode
- A fifth protocol, suzerain ↔ castellan itself, over iroh (`Order`/`OrderAck`
  + several `StreamHello` sub-streams) — this one *should* stay a network
  protocol (it crosses hosts), but its verb set doesn't line up 1:1 with any
  of the four above (e.g. castellan's local `attach`/`ask`/`exec` have no
  `Order` equivalent; `RestoreAgent`/`UpdateManifest` orders exist in the
  schema but are dead code in `dispatch_order`).
- Storage is hardcoded in three different shapes: suzerain's registry is a
  SQL `Store` (sqlite/postgres via an internal `Backend` enum, not a trait);
  bundle/snapshot storage is raw filesystem calls with one config knob (a
  directory path); castellan's per-agent state is flat JSON files
  (`state.json`), explicitly diverging from `docs/PLAN.md`'s own sketch.
- Placement is a single hardcoded algorithm (`scheduler.rs`'s two-phase
  filter+spread), not a swappable strategy.
- VM bootstrap (`provision.rs`) is a single hardcoded, Alpine/npm/mise/pi-
  specific imperative function — no declarative spec, no cloud-init
  equivalent, nothing pluggable.

This doc proposes closing those gaps: one binary, one registry, one
lifecycle API, and a small number of well-chosen trait boundaries so the
storage/placement/provisioning choices become configuration, not forks in
the code.

Everything below is graded by confidence: **[current]** = verified fact about
today's code, **[proposed]** = new design, **[open]** = a question this doc
does not resolve (see §7).

---

## 1. Current-state map (verified)

```
suz (CLI) ──JSONL/unix socket──► suzerain::api.rs ──┐
Suzy ──iroh operator channel──► suzerain (in-proc router)  │
suzerain-mcp ──HTTP REST──────► suzerain::web (axum) ──────┤──► actions.rs (create_agent/destroy_agent)
                                                            │       │
                                                            │       ├─► scheduler::place_or_preempt (hardcoded)
                                                            │       ├─► store::Store (SQL, sqlite|postgres via enum)
                                                            │       ├─► secrets::slice_for (age-encrypted)
                                                            │       └─► Order::CreateAgent ──iroh control──►┐
                                                            │                                                │
local CLI ──JSONL/unix socket──► castellan::daemon.rs ──┐  │                                    castellan::control.rs
  (standalone mode, no suzerain)                        ├──┴─► supervisor::Supervisor ◄──────────┘  (registers, serves
                                                         │         │                                 Orders, ships state/
                                                         │         ├─► state::AgentRecord (flat JSON, per agent)
                                                         │         ├─► provision::provision() (hardcoded Alpine/npm/mise)
                                                         │         ├─► rpc::PiAgent (pi process inside guest VM)
                                                         │         └─► driver::DriverClient (gondolin-driver, Node sidecar)
```

Key facts worth carrying into the design (all **[current]**):

- **Placement is already optional-hint-shaped.** `Schedule { require:
  BTreeMap<String,String>, daemon: Option<String> }` in the manifest is
  exactly the "optional field to hint placement, default = automatic" shape
  this project wants generalized — `scheduler::place()` already treats
  `daemon` as a hard pin that bypasses everything else, and falls back to a
  label/resource/spread-score search otherwise. This part needs *generalizing
  and exposing*, not inventing.
- **Co-location is already a stated (deferred) goal.** `docs/PLAN.md` §10
  explicitly designs for suzerain+castellan on one host today (shared "fleet
  home" dir, disjoint filenames, disjoint sockets/identities) and explicitly
  flags, as non-v1: *"a single-process `suzerain --with-castellan` combined
  mode for absolute-minimal setups."* That is this project's binary-merge
  goal, already named and deferred once.
- **The auto-suspend design is the right model for a unified lifecycle.**
  `docs/AUTO-SUSPEND.md`'s five-way public status (`running/idle/sleeping/
  waking/failed`) is computed by one function
  (`suzerain_protocol::state::public_status`) shared by every surface today.
  That's the precedent to extend, not replace.
- **Two of the four pluggability seams this doc wants already half-exist**:
  the SQL backend is an internal enum (not exposed as config beyond a
  connection-string env var) and the bundle directory is one config knob.
  Neither is a trait. The other two (chat/transcript storage, placement
  strategy) don't exist as seams at all today — they're free functions over
  concrete types.
- **castellan already runs standalone** (`control.rs::run_control_client`
  returns immediately if `suzerain_endpoint_id` is unset, logging
  "standalone mode"), with its own local command surface in `daemon.rs`. This
  standalone mode is the existing precedent for "one box, no fleet" — but it
  is a *parallel, divergent* code path today (separate socket, separate
  command names, separate secrets mechanism `bundle_from_env`), not the same
  API running in single-node mode.

---

## 2. Goals

1. **One binary** (`suzerain`, castellan folded in) that can run as:
   - `agent` — a compute node only (today's castellan role)
   - `control` — a control-plane-only node (today's suzerain role, for a
     dedicated always-on registry host)
   - `standalone` (default with zero config) — both roles in one process, one
     data dir, no network hop for local-only fleets
   selected by config/flags, not by which binary you invoked.
2. **One registry, one lifecycle API, one wire protocol** for
   create/chat/config/destroy, spoken identically by `suz`, Suzy, the MCP
   server, and any future client — replacing today's four divergent
   surfaces. The wire type is always the typed manifest/argument structs
   (JSON), never a TOML string — see §4.3.0.
3. **Config-driven placement**: an agent manifest works unchanged whether the
   node is standalone or part of a 50-host fleet; placement hints
   (`schedule.require`, `schedule.daemon`) stay optional and additive to a
   default automatic scheduler, per the pattern already in place.
4. **Pluggable storage**: a `Registry`/`Storage` trait (agents, daemons,
   manifests, log index — formalizing what `docs/PLAN.md` §9 already
   *intended*), a `ChatStore` trait (today: JSONL files + a SQL index; net-new
   as an interface), and a `SnapshotStore` trait (today: raw local
   filesystem; net-new as an interface) — each with the current
   implementation kept as the default backend, so this is additive, not a
   rewrite.
5. **Pluggable placement strategy**: extract `scheduler.rs`'s logic behind a
   `PlacementStrategy` trait; ship the existing filter+spread algorithm as
   the default; leave room for pin-only, bin-pack, or cost-aware strategies
   later.
6. **Declarative, pluggable provisioning**: replace (or wrap) `provision.rs`'s
   hardcoded imperative sequence with a declarative spec (packages, mounts,
   pre-start scripts — a cloud-init analogue) so non-Alpine/non-npm/non-pi
   agent shapes are configuration, not new Rust code, while keeping the
   current sequence as the default/only implementation initially.
7. **A generic-enough Agent API** that the "agent" concept isn't hard-coded
   to pi/Gondolin at the API boundary, even if pi/Gondolin remains the only
   implementation for now.

## 3. Non-goals (for this doc / this phase)

- Multi-user RBAC, multi-tenant isolation of the control plane itself.
- Replacing Gondolin/pi as the execution substrate.
- Solving distributed consensus for a multi-control-plane HA setup (out of
  scope; assume one authoritative control-plane role at a time, consistent
  with today's single-suzerain design).
- Rewriting the iroh transport layer.

---

## 4. Proposed architecture

### 4.1 Single binary, config-selected role

```toml
# suzerain.toml
[role]
mode = "standalone"   # "standalone" | "control" | "agent"
```

- `mode = "agent"` ≈ today's `castellan` binary/behavior.
- `mode = "control"` ≈ today's `suzerain` binary/behavior, minus any local
  agent-hosting.
- `mode = "standalone"` (default): **one binary, but two OS processes**, not
  one process. This is a revision from an earlier draft of this section
  (which proposed one process with an in-process trait call between the two
  roles) after a security review flagged that arrangement as a real
  regression — see the box below.

**[decided, revised after security review]** Standalone mode is a single
binary that, on startup, **launches a second OS process of itself** in a
distinct internal role (re-exec with an internal `--agent-worker` flag —
not user-facing, not one of the three `mode` values above, purely an
implementation detail of how standalone mode is assembled). The **parent**
process holds the full registry, the full age-encrypted secrets store, the
placement/scheduling logic, and serves the client-facing lifecycle API. The
**child** process holds only supervisor/driver/provisioning logic and, at
any moment, only the per-agent `SecretBundle` slices actually in use by its
running agents — never the long-term store.

The two processes talk over the **same internal protocol** described in
§4.3's `Order`/`OrderAck`/`StreamHello` layer, just carried over a local
transport (an anonymous socket pair created at spawn time, rather than an
iroh/QUIC connection) instead of a network hop. This preserves the "one
internal protocol, two delivery mechanisms" property from §4.3 — distributed
mode uses iroh between two hosts, standalone mode uses a local socket
between a parent and its child, but the message shapes and the code that
handles them are identical either way.

Why this replaces the earlier one-process design:

- **Security isolation, not just failure isolation.** A security review of
  this doc found that a single shared process would put the full long-term
  secrets store (every provider key, every git SSH key, for every agent) and
  every live agent's execution surface in one address space — a
  memory-disclosure bug or a compromised agent's execution path pivoting
  into the host process could reach every secret on the box, not just the
  one agent's slice. Two OS processes restore the same boundary today's two
  separate binaries give for free: the agent-hosting process physically
  cannot read the long-term store's memory, because it never has it — only
  the secret slices explicitly pushed to it per agent, exactly as
  `Order::CreateAgent{secrets}` already works in distributed mode today.
- **Failure isolation falls out for free.** The original motivation for
  runtime separation (a wedged/crash-looping provisioning task must not
  starve the registry/API) is satisfied automatically by the OS process
  boundary — no need for a separate Tokio runtime/thread-pool scheme inside
  one process to approximate it. Castellan's existing bounds (`MAX_RESTARTS`,
  `RESTART_WINDOW`, `PROVISION_TIMEOUT`) remain in place in the child process
  as defense in depth underneath this, not instead of it.
- **Socket/identity trust boundaries stay narrow.** The client-facing API
  (§4.3) is served only by the parent process; anything requiring
  agent-execution access (attach relay, `exec`, provisioning) is mediated
  through the internal protocol into the child, the same way a remote
  `castellan` would be mediated today — same-uid access to the parent's
  socket does not, by itself, hand out the full secrets store or raw
  in-VM execution the way a single shared process would have.
- The two standalone-mode processes keep **two distinct iroh identities**,
  unchanged from today's suzerain/castellan separation — see the transport
  revision immediately below, which settles this by making the parent↔child
  link itself an ordinary iroh connection rather than a socket pair.

**[decided, researched, revised]** Spawn mechanism and local transport,
resolving the last previously-open item in §7. The transport half of this
was revised once during implementation planning — see below for why.

- **Spawn: re-exec via `std::process::Command::new(std::env::current_exe()?)`
  with an internal `--agent-worker` flag — fork+exec (or `posix_spawn` under
  the hood), never raw `fork()` without an immediate `exec()`.** This
  project already runs a multithreaded Tokio runtime by the time it would
  spawn the child, and Tokio's own maintainers document forking a
  multithreaded process as unsupported unless the child execs immediately —
  ordinary Rust code allocates, and only async-signal-safe calls are valid
  between a `fork()` and an `exec()` in a threaded process. Chromium hit
  this same hazard as its browser process became heavily threaded and moved
  off raw `fork()`-without-exec for the same reason. Re-exec behaves
  identically on macOS and Linux, so there's no platform-specific spawn
  path to maintain. **This part is unchanged and still what's implemented.**
- **Transport: loopback iroh over `127.0.0.1`, not a `socketpair`.** The
  original plan (a `UnixStream` pair modeled on OpenSSH's privsep channel)
  ran into a real gap during implementation: the `Order`/`OrderAck`/
  `StreamHello` protocol depends on iroh QUIC's ability to open several
  independent, concurrent bidirectional streams on one connection (Register,
  StateReport, Attach, Shell, Restore, and Logs are all separate streams
  today) — a bare `socketpair` is a single byte stream with no free
  multiplexing, so carrying the real protocol over one unchanged would have
  required designing and implementing a new stream multiplexer (framing,
  stream-id allocation, backpressure, teardown) and rewriting both sides'
  `control.rs` to use it instead of iroh's `open_bi`/`accept_bi`. That's a
  meaningfully sized, correctness-and-security-sensitive project on its own
  — bigger than any single Phase 1 step — for a purely-local link.
  Given that, the parent and child instead dial each other over ordinary
  iroh, on localhost, exactly like a real cross-host `control`↔`agent` pair
  — **zero new protocol code**, both sides' existing, already-tested
  `control.rs` logic runs completely unchanged, and the "same protocol,
  same code, different transport" property this section promises is
  satisfied more literally than the socketpair plan would have been (it's
  not just the same message *shapes*, it's the exact same connection code).
  The tradeoffs this accepts: a real (if tiny and local-only) network hop
  and iroh/QUIC handshake at standalone-mode startup, instead of an
  in-process pipe. A real socketpair-based multiplexed transport remains
  possible future work if that overhead ever matters, but isn't needed for
  a correct v1.
- Because the transport is now ordinary iroh, **each process keeps its own
  iroh identity**, same as today's separate suzerain/castellan keys — the
  "moot" framing in an earlier draft (no iroh identity needed for a socket
  pair) no longer applies, and there's no new identity-collapse question to
  resolve: standalone mode's parent and child are, from iroh's perspective,
  just an `agent` node and a `control` node on the same host, auto-approved
  for each other at spawn time.

This mirrors a well-established pattern for privilege-separated daemons
(one supervising binary, a re-exec'd child holding the more sensitive or
more crash-prone half of the work) rather than Nomad's/k3s's simpler "one
process, multiple logical roles" model cited in an earlier draft — the
extra process boundary here is a deliberate, security-motivated departure
from that precedent, not an oversight. The transport revision changes *how*
the two processes talk, not *why* there are two of them — the security and
failure-isolation rationale above is unaffected.

#### 4.1.1 Installation modes

Two install paths, not one, since agent-hosting and control-only nodes have
different host dependencies (qemu/KVM/node for Gondolin VMs vs. none of
that for a pure registry host):

- **`full` (default)** — installs everything needed for `mode = standalone`
  or `mode = agent`: qemu, KVM group setup, node (for `gondolin-driver`),
  the works. This is what `ops/install.sh` installs with no flags, matching
  today's default "just try it" path.
- **`control`-only** — installs only what `mode = control` needs (no
  qemu/KVM/node), for a dedicated always-on registry host that will never
  itself run agent VMs. Selected explicitly (e.g. `install.sh --control-only`
  or equivalent), since it's the narrower, opt-in case.

Both paths install the same single binary; the difference is purely which
host packages the installer pulls in, matching the `mode` the operator
intends to run.

### 4.2 One registry, replacing three data models

Today: SQL `agents`/`daemons` tables in suzerain (`store.rs`) + flat JSON
`state.json` per agent in castellan (`state.rs`) + raw-filesystem bundles
(`bundle.rs`). Proposed: a single `Registry` trait, agent/daemon rows as the
single source of truth regardless of mode:

```rust
#[async_trait]
trait Registry: Send + Sync {
    async fn create_agent(&self, row: &AgentRow) -> Result<()>;
    async fn update_agent_state(&self, id: &Uuid, state: AgentState) -> Result<()>;
    async fn get_agent(&self, id: &Uuid) -> Result<Option<AgentRow>>;
    async fn get_agent_by_name(&self, name: &str) -> Result<Option<AgentRow>>;
    async fn list_agents(&self) -> Result<Vec<AgentRow>>;
    async fn delete_agent(&self, id: &Uuid) -> Result<()>;
    // ... daemon/session/pending-message methods, mirroring store.rs today
}
```

- Default impl: today's `Store` (sqlite default, postgres opt-in) — behavior
  unchanged, just reachable through a trait object instead of a concrete
  struct.
- In standalone mode, this *is* castellan's authoritative state too — no
  separate `state.json` per agent. This directly resolves the gap
  `docs/PLAN.md` §14 already flags: "Castellan state is JSON files, not
  SQLite — diverges from the plan's sketch."
- In distributed mode (control + N agent nodes), each agent node still needs
  a *local* cache for offline resilience (what `state.rs` is today) — but as
  a cache of registry rows, reconciled on reconnect, not an independent
  source of truth with its own schema.

### 4.3 One lifecycle API, one verb set

Consolidate the four client-facing protocols into one typed API, exposed
over whichever transport a given deployment needs (local Unix socket for
same-host CLI, REST for MCP/web, iroh operator channel for remote GUI) —
**one verb set, multiple transports**, rather than today's arrangement of
divergent verb sets *per* transport.

**Revision note**: an earlier draft of this section (and the accompanying
chat discussion) surfaced several API-style problems in review, before any
code was written. §4.3.0–4.3.6 below capture the corrected design; the
original issues are kept as a record of *why*, since they're exactly the
kind of mistake worth not repeating in the next API this project designs.

#### 4.3.0 Wire type vs. authoring format — the manifest is never a string on the wire

**Problem identified in review**: an earlier draft had `agent.create` accept
the manifest as a raw TOML string, mirroring how manifests are authored as
files today. That conflates two different jobs — *manifest as a
hand-edited file* (TOML is a good fit: comments, human-editable) and
*manifest as an API payload* (a string blob is a bad fit: no client-side
type-checking, every language binding needs a TOML parser just to call
`create`, and it's inconsistent with `suzerain-mcp` having *already* grown
a second, structured `create_from_structured` path today specifically to
avoid round-tripping TOML text).

**Resolved**: the wire type for every verb that takes or returns a manifest
is the **typed `AgentManifest` struct**, serialized as JSON (or whatever the
transport's native encoding is) — never a TOML string. TOML remains exactly
what it's already good at: the *on-disk, human-authored file format*. The
`suz` CLI (and any future SDK) parses a `--manifest path.toml` file to the
typed struct **locally**, then calls the same typed API every other client
calls. One schema, two serializations (TOML for files, JSON on the wire),
never a string-in-a-string.

#### 4.3.1 `placement` is not a separate argument — it lives in the manifest

**Problem identified in review**: an earlier draft had `agent.create(manifest,
placement?)` as two arguments carrying overlapping data — `placement` was a
generalized duplicate of `manifest.schedule.{require,daemon}`. Two
arguments for one decision invites an unanswered "which one wins if both
are set" question baked into the API from day one.

**Resolved**: `agent.create(manifest)` — one argument. All placement hints
(`require`, `pin`, `strategy`) live exclusively in `manifest.schedule` (see
§4.4). There is no separate `placement` parameter anywhere in this API.

#### 4.3.2 Real enums instead of stringly-typed sentinels

**Problem identified in review**: two fields encoded a fixed, closed set of
choices as a plain string with magic sentinel values re-parsed server-side —
today's `agent_config`'s `auto_suspend` (where `"default"`/`"inherit"` are
sentinels that clear an override, anything else is duration-parsed — flagged
by `api.rs`'s own comment: *"default"/"inherit" clears the runtime
override"*), and the draft `placement.strategy: Option<String>` proposed
alongside it.

**Resolved**: both become real enums at the API boundary:

```rust
enum AutoSuspendOverride {
    Inherit,           // was the string "default"/"inherit"
    Never,             // was the string "never"
    After(Duration),   // was a duration string
}

enum PlacementStrategy {
    Spread,            // default: today's least-loaded scheduler
    BinPack,
    Pin,               // degenerates automatically when manifest.schedule.pin is set
    Custom(String),    // escape hatch for operator-registered strategies, not a free-for-all
}
```

Labels (`schedule.require: BTreeMap<String,String>`) correctly remain plain
strings — they're genuinely open-ended and operator-defined, unlike a
policy selector picking from a fixed, code-defined set of implementations.

#### 4.3.3 `force` and other booleans become named modes

**Problem identified in review**: `agent.destroy(name, force?)` (and
similarly `start`'s internal `force`) is boolean blindness — `destroy("x",
true)` tells a reader nothing at the call site without checking docs.

**Resolved**:

```rust
enum DestroyMode { Graceful, Force }
agent.destroy(name, mode: DestroyMode = Graceful)
```

Any future bare on/off toggle in this API should default to a small named
enum instead of `bool`, as a standing style rule, not a one-off fix.

#### 4.3.4 `config`'s patch argument is typed, not a generic blob

Following directly from 4.3.2: `agent.config(name, patch)`'s `patch` is a
struct with explicit optional fields, not a generic JSON blob:

```rust
struct AgentConfigPatch {
    auto_suspend: Option<AutoSuspendOverride>,  // None = leave unchanged
    // future patchable fields go here, each Option<T>
}
```

#### 4.3.5 `ask` — split the concerns instead of bundling four behaviors into one verb

**Problem identified in review**: today's `agent_ask` (and an earlier draft
of `agent.ask`) bundles (a) send a message, (b) block server-side awaiting
completion, (c) a **hardcoded 300s timeout** the caller has no say over, and
(d) a log-tail fallback if the live event stream missed the completion
marker. That's a lot of undocumented policy inside one verb for a generic
public API.

**Resolved**: split into a primitive and a convenience wrapper:

```rust
agent.send(name, message) -> MessageId          // enqueue, returns immediately
agent.attach(name)                              // stream events, including completions, caller-driven
agent.ask(name, message, timeout: Duration) -> Reply
    // sugar: send() + wait for the matching completion event via attach(),
    // bounded by a CALLER-SUPPLIED timeout — no server-side magic number
```

`ask` stays the ergonomic one-call option for the common case; it's just no
longer the *only* option, and it no longer hides a timeout decision made on
the caller's behalf.

#### 4.3.6 `list` reserves room for filtering and pagination

**Problem identified in review**: `agent.list()` takes no arguments, fine at
today's scale (tens of agents, one operator) but this API is explicitly
pitched at scaling to dozens of hosts, which implies more agents than a bare
unbounded list comfortably returns.

**Resolved**: `agent.list(filter: Option<AgentFilter>, cursor: Option<Cursor>)
-> Page<AgentSummary>`. The first implementation may ignore `filter`/`cursor`
(returning everything as one page) — the point is reserving the shape now so
adding real pagination later isn't a breaking change.

#### 4.3.7 A small, typed error taxonomy instead of free-text matching

**Problem identified in review**: errors today are free-text (`anyhow`-style),
to the point that `suzerain-mcp` already does fuzzy substring ("did you
mean") matching against server error *messages* for control flow — a sign
that error text is being used where an error *code* belongs.

**Resolved**: a small closed enum of client-facing error kinds, each
carrying whatever structured detail it needs, with a free-text message
alongside for humans but never required for programmatic handling:

```rust
enum ApiError {
    NotFound { resource: String, name: String },
    NameConflict { name: String },
    ValidationError { field: String, reason: String },
    NoCapacity,
    SecretMissing { name: String },
    Unreachable { detail: String },
    // ...
}
```

#### 4.3.8 Idempotency is a stated guarantee, not an accident

`create`'s uniqueness-by-`name` already makes repeat calls fail rather than
double-provision, but nothing states whether that's deliberate API behavior
or an implementation side effect. **Resolved**: documented explicitly —
`agent.create` is idempotent keyed on `name`; a second `create` call with a
name that already exists (regardless of whether the first call's
provisioning has completed) returns `NameConflict` rather than creating a
second agent, and callers may safely retry a `create` that failed due to a
transport error using the same name.

#### 4.3.9 Verb naming: RPC style, consistently

**Problem identified in review**: the draft verb set mixed
REST-resource-style names (`list`, `get`, `config`) with RPC-style names
(`ask`, `attach`) — not wrong on its own, but an inconsistency that reads as
assembled from two different instincts rather than designed.

**Resolved**: RPC style throughout (`agent.<verb>`), since several verbs
(`ask`, `attach`, `send`) don't map cleanly onto CRUD nouns anyway and
forcing them to would be worse than the inconsistency it fixes.

#### The verb set

Proposed verb set (mostly already true of `suzerain::api.rs` today; this
mainly folds in castellan's standalone-only verbs and removes the
duplication, and reflects 4.3.0–4.3.9 above):

| Verb | Maps to today | Notes |
|---|---|---|
| `agent.create(manifest: AgentManifest) -> AgentSummary` | `agent_create` (suzerain) / `create` (castellan daemon.rs) | manifest is the typed struct (JSON on the wire), never a TOML string (4.3.0); placement lives in `manifest.schedule`, no separate argument (4.3.1); idempotent by `name` (4.3.8) |
| `agent.send(name, message) -> MessageId` | (new — the primitive `agent_ask` was implicitly built on) | enqueue only, returns immediately (4.3.5) |
| `agent.ask(name, message, timeout: Duration) -> Reply` | `agent_ask` / `ask` | sugar over `send`+`attach`, caller-supplied timeout (4.3.5) |
| `agent.attach(name)` | `agent_attach` / `attach` | streaming relay + history replay |
| `agent.config(name, patch: AgentConfigPatch) -> AgentSummary` | `agent_config` | typed patch, not a string/blob (4.3.4) |
| `agent.destroy(name, mode: DestroyMode)` | `agent_destroy` / `destroy` | named mode, not a bare bool (4.3.3) |
| `agent.list(filter?, cursor?) -> Page<AgentSummary>` | `agent_list` / `list` | reserved pagination/filter shape (4.3.6) |
| `agent.get(name) -> AgentSummary` | (implicit in list today) | |
| `agent.logs(name, tail?)` | `agent_logs` / `logs` | |
| `daemon.*`, `secret.*`, `audit.*` | unchanged | operator/fleet-admin surface, out of scope for the "generic Agent API" but stays on the same transport |

**[current]** `castellan::daemon.rs`'s `exec` (raw argv exec inside the VM)
has no equivalent in suzerain's API today and is arguably not part of the
*generic* Agent API (it's a pi/Gondolin-specific debug tool) — proposed to
keep it as an implementation-specific extension, not part of the core verb
set in §7's generic-API framing.

`suzerain-client`, `suz`, and `suzerain-mcp` today each hand-roll their own
wire encoding/decoding against three different transports. Proposed: one
shared client crate implementing this verb set, generic over transport
(Unix socket / HTTP / iroh operator channel), so `suz`/Suzy/MCP become thin
adapters instead of three independent protocol implementations.

**[decided]** Today's `Order`/`OrderAck` (suzerain→castellan across hosts)
stays a *different*, lower-level protocol from the client-facing one above
— confirmed, not just an oversight. It carries placement decisions and
secret bundles, which a generic client must never see directly. Two layers:
the client-facing lifecycle API (create/ask/attach/config/destroy) on top,
and an internal placement/execution protocol underneath (`Order`/`OrderAck`
+ `StreamHello::*`) carried over iroh between two hosts in distributed mode,
or over the local parent↔child socket pair in standalone mode (§4.1) — same
message shapes, same handling code, different transport. This also directly
resolves the two dead `Order` variants noted in §1 (`RestoreAgent`,
`UpdateManifest` are unhandled in `dispatch_order` today) — as part of
unification work, both should either be implemented for real or removed
from the enum, since a unified internal protocol shouldn't carry dead
variants.

**[decided, new]** `Register`/`RegisterResponse` (the handshake that opens
this internal protocol) gains an explicit `protocol_version: u32` field on
both sides, plus a shared `const PROTOCOL_VERSION` in `suzerain-protocol`.
Today's handshake carries no version at all, and compatibility is handled
ad hoc (new fields default via `#[serde(default)]`) — fine for additive
changes, but with no negotiated floor a genuinely incompatible future change
has no way to fail cleanly. There is no existing fleet to migrate, so this
costs nothing to add now and is pure insurance: `RegisterResponse` can
reject (`accepted: false`, with a clear message) a peer whose
`protocol_version` it doesn't understand, rather than failing confusingly
deeper in the handshake or silently misbehaving.

### 4.4 Config-driven placement (generalizing what exists)

Per §4.3.1, placement is not a separate API argument — it's entirely a
property of the manifest passed to `agent.create`:

```toml
[schedule]
# all optional; omitted = fully automatic
require = { zone = "office" }   # label subset match (today, unchanged)
pin = "hostname-or-endpoint-id" # hard pin (today's schedule.daemon, renamed for clarity)
strategy = "spread"             # NEW: selects a PlacementStrategy enum variant (§4.3.2, §4.5);
                                 # "spread" | "bin-pack" | "pin" | any operator-registered custom name
```

`strategy` deserializes to the `PlacementStrategy` enum from §4.3.2, not a
free-form string re-parsed by convention — an unrecognized value is a
`ValidationError` (§4.3.7) at manifest-validation time, not a silent
fallback.

No behavior change for existing manifests (`require`/`daemon` already work
exactly this way) — this section is mostly about (a) making "no control
plane configured" and "one control plane, no other hosts" also go through
`place()` (today standalone castellan skips scheduling entirely, since
there's only ever one target) so the same code path is exercised at every
scale, and (b) exposing a `strategy` selector per §4.5.

### 4.5 Pluggable placement strategy

**Revision note**: an earlier draft's trait signature (`choose(candidates:
&[DaemonRow], constraints)`) didn't match what the real `scheduler::place()`
actually needs — it independently queries *both* the daemon table and the
agent table today (to compute each daemon's current load, which a bare
`DaemonRow` doesn't carry). Passing only `&[DaemonRow]` would either force
the strategy back into `Registry` itself (defeating the point of the trait)
or silently drop the load-fit logic. Corrected below.

```rust
struct Candidate {
    daemon: DaemonRow,
    allocated: Allocated,   // today's per-daemon summed resource usage — computed by the
}                           // caller (from live Registry data) BEFORE choose() is invoked,
                            // so the strategy stays pure and never touches Registry itself

trait PlacementStrategy: Send + Sync {
    fn choose(&self, candidates: &[Candidate], constraints: &Constraints) -> Result<Placement>;
}
```

- Default impl = today's `scheduler.rs` filter+spread algorithm, moved
  behind this trait with the caller (not the strategy) doing the
  `Registry`-querying and `Allocated` computation it does today (it already
  has a solid unit test suite to carry over as the conformance suite for
  this trait). In standalone mode, the "candidate list" is always exactly
  one entry (the local node) — `choose()` degenerates to a no-op, which is
  the mechanism that makes standalone mode not a special case.
- Precedent: Kubernetes' scheduler framework (pluggable filter/score
  extension points), Nomad's selectable scheduler algorithms — **[precedent,
  from prior research]**.
- **[decided]** Preemption (`place_or_preempt`) is a **separate,
  strategy-agnostic layer above** `PlacementStrategy`, not part of the trait
  itself — it's a capacity-recovery policy (suspend longest-idle-eligible
  agents until a plain `choose()` succeeds), orthogonal to *which* heuristic
  picks the winning candidate. Any `PlacementStrategy` impl gets preemption
  "for free" by being called from the same `place_or_preempt` wrapper
  `scheduler.rs` already has today, unchanged in shape.

### 4.6 Pluggable chat/transcript storage

**Revision note**: an earlier draft described this as an "extraction" around
an existing JSONL-file arrangement. A closer read of the code found that
framing was misleading: there is no existing `ChatStore`-shaped module to
extract from. Event-log file paths (`<data>/logs/<agent_id>.jsonl`) are
constructed inline and redundantly at three separate call sites (`api.rs`,
`control.rs`, `web_session.rs`), there is no `history_since`-equivalent
anywhere (`tail`/`agent_logs` reads the *entire* file into memory and slices
it, on every call), and the `log_index` table's `acked_through` bookkeeping
really belongs with `Registry`, not chat storage. This is closer to new
construction than extraction — worth calling out honestly rather than
carrying the original "mechanical wrap" framing forward.

Per the decision to store the event log itself in SQLite (not files):

```rust
trait ChatStore: Send + Sync {
    async fn append(&self, agent_id: &Uuid, event: &LogEvent) -> Result<()>;
    async fn tail(&self, agent_id: &Uuid, n: usize) -> Result<Vec<LogEvent>>;
    async fn history_since(&self, agent_id: &Uuid, seq: u64) -> Result<Vec<LogEvent>>;
}
```

Default impl: a `chat_events` table (`agent_id, seq, at, kind, payload`) —
one row per `LogEvent`, `payload` stored as a JSON column (sqlite's `JSON`
affinity / postgres' `jsonb`), `PRIMARY KEY (agent_id, seq)` giving natural
dedup and an efficient `WHERE agent_id = ? AND seq > ?` for
`history_since`. This directly fixes every problem above:

- `tail`/`history_since` become real, indexed SQL queries instead of a
  full-file read-and-slice on every call.
- No more triplicated path-construction — one table, one place events are
  written and read, referenced from `api.rs`/`wake.rs`/`control.rs` through
  this trait instead of each reconstructing a file path independently.
- The event *payloads themselves* are what `log_index`'s `acked_through`
  watermark was always indexing — with the events living in SQL directly,
  that watermark becomes a plain column/derived query against `chat_events`,
  and can move into `Registry` (or stay adjacent to `ChatStore` — an
  implementation detail, not a new public seam) instead of being a
  half-orphaned table pointing at files outside the database.
- Since the default `Registry` impl is already SQL (`Store`, sqlite/postgres
  via its internal `Backend` enum, §4.2), the default `ChatStore` impl can
  reasonably share the *same* connection pool/backend selection as
  `Registry` — one database, two tables (or table sets), rather than
  standing up a second storage system. They remain two separate traits
  (a Postgres-only chat store paired with a sqlite registry, or vice versa,
  stays possible), but the common case is one DB serving both.
- This is the interface a Postgres-native or otherwise remote-DB-backed
  transcript store would implement later without touching `api.rs`,
  `wake.rs`, or `control.rs`'s call sites — same benefit the earlier draft
  claimed, now actually backed by a real module boundary instead of an
  imagined one.

### 4.7 Pluggable snapshot/bundle storage

**Revision note**: an earlier draft's `write_file` signature (`data: &[u8],
sha256: &str`) didn't match the real `bundle.rs::write_file`, which takes
`data_base64: &str` and an *optional* `sha256: Option<&str>` (bundle chunks
arrive as base64 inside JSONL `BundleMessage`s, and the hash is only
sometimes supplied). Corrected below; also making the buried
`retention::load_config()` bundle-directory lookup an explicit constructor
argument instead of a hidden global read.

```rust
trait SnapshotStore: Send + Sync {
    async fn write_start(&self, agent_id: &Uuid, manifest: &AgentManifest, session_file: Option<&str>) -> Result<()>;
    async fn write_file(&self, agent_id: &Uuid, rel_path: &str, data_base64: &str, sha256: Option<&str>) -> Result<()>;
    async fn load(&self, agent_id: &Uuid) -> Result<StoredBundle>;
}
```

- Default impl = today's `bundle.rs` (local filesystem, hand-rolled
  base64/sha256 already in place), constructed with its root directory
  passed in explicitly at startup (from `[bundles].dir` config, resolved
  once, not re-read via a buried `retention::load_config()` call inside
  every function) rather than reaching for global config on every call.
- This is the seam an S3/rsync-to-peer-Castellan/NFS-backed bundle store
  would implement — directly serves the "configurable VM snapshot storage
  system" ask. Bundle *contents* (only `sessions/`+`pi-home/`, per
  `control.rs::bundle_files`) are unaffected; only *where the bytes live*
  changes.

### 4.8 Declarative, pluggable VM/agent bootstrap

Today, `provision.rs::provision()` is one linear async function: apk
install → SSH config → npm/pi install → repo clones → extensions → mise →
trust.json → append-system prompt. No spec object describes this — it's
read directly off `AgentManifest` fields and executed as Rust control flow.
`provision.rs`'s 13 steps (see research summary in this doc's source
material) are, in effect, an *undeclared* spec already; this section makes
it declarative and pluggable in one pass, not two.

#### 4.8.1 The `Provisioner` trait

**Revision note**: an earlier draft's proposed signature matched the real
`provision()` function's parameters exactly — but the real function's
*first line* (`AgentPaths::for_agent(&record.id)`) reaches into a free
function that reads `$CASTELLAN_HOME`/`$SUZERAIN_HOME` env vars directly.
That's hidden global state a trait boundary doesn't remove just by existing
around it — every `Provisioner` impl, including a future
`DeclarativeProvisioner`, would silently inherit the same implicit
dependency. Fixed by making the host paths an explicit argument:

```rust
trait Provisioner: Send + Sync {
    async fn provision(
        &self,
        driver: &DriverClient,
        record: &AgentRecord,
        paths: &AgentPaths,       // NEW — resolved by the caller, not looked up
        bundle: &SecretBundle,    // internally via env vars
    ) -> Result<BTreeMap<String, String>>; // env var -> placeholder value, as today
}
```

Default impl = today's `provision.rs` logic, moved behind this trait with
its `AgentPaths::for_agent(...)` call lifted to the caller — otherwise
unchanged, no behavior change for existing `harness.type = "pi"` manifests.
A `DeclarativeProvisioner` (below) becomes a second impl, selectable per
manifest, and gets the same explicit `paths` argument for free.

`provision.rs`'s standalone-only `bundle_from_env` fallback (which reads
provider keys straight from the daemon process's own environment,
bypassing `SecretBundle` entirely) is dropped as part of unification, not
carried forward: per §4.1, every deployment — including standalone mode —
now always has a control-role process holding the full secrets store and
slicing bundles the normal way (`secrets::slice_for`), delivered to the
agent-hosting process over the internal protocol exactly like the
distributed case. There is no longer a "no control plane at all" mode for
`Provisioner` to special-case.

#### 4.8.2 The declarative spec (`[provision]` in the manifest)

An additive, optional manifest section. When present, it fully replaces the
hardcoded step sequence for that agent (no partial-override merging with the
built-in pi path — merging imperative and declarative provisioning for the
same agent would reintroduce exactly the special-casing this doc is trying
to remove). When absent, `harness.type = "pi"` continues to use the default
`Provisioner` impl unchanged.

```toml
[provision]
base_image = "alpine:3.20"        # or a Gondolin-recognized image ref

# Step 1: OS packages, installed before anything else.
packages = ["git", "curl", "bash", "ca-certificates"]

# Step 2: host->guest bind mounts, beyond the standard /agent mount
# (workspace/pi-home/sessions/extensions are always mounted; this is
# for anything additional a provisioner needs).
[[provision.mounts]]
host = "extra-data"    # relative to the agent's host root dir
guest = "/agent/extra"
read_only = true

# Step 3: declarative package installs, run in the order listed.
# Each entry names a resolver the Provisioner knows about (npm/pip/git/mise/
# raw-script), not an arbitrary shell command, so idempotency rules (4.8.3)
# apply per resolver instead of being the author's problem.
[[provision.install]]
resolver = "npm"
package = "@earendil-works/pi-coding-agent"
version = "0.84.1"          # pins the same way harness.version does today
prefix = "/agent/toolchain/global"

[[provision.install]]
resolver = "git"
url = "https://github.com/octocat/Hello-World.git"
ref = "master"
dest = "/agent/workspace/Hello-World"

[[provision.install]]
resolver = "mise"
tools = { node = "20", python = "3.12" }

# Step 4: arbitrary scripts, for anything the built-in resolvers don't
# cover. Explicitly the escape hatch, not the primary mechanism -- a
# manifest that's 90% raw-script entries has told you it should be a
# custom Provisioner impl instead.
[[provision.run]]
when = "pre_start"           # "pre_start" | "post_start"
script = "echo hi > /agent/workspace/marker"
env = { FOO = "bar" }

[provision.trust]
paths = ["/agent/workspace"]  # -> pi-home/trust.json equivalent

[provision.prompt]
append_system = "You are ..."  # unchanged from today's [prompt] section
```

Design choices worth calling out explicitly:

- **Typed install resolvers, not raw shell, as the primary surface.** This
  is the one deliberate departure from a literal cloud-init clone (cloud-init
  leans heavily on raw `runcmd`). Reason: `provision.rs` today already
  encodes real idempotency logic per package type (marker-file version
  checks for pi, existence checks for the npm tarball, `mise install --yes`
  semantics) — collapsing that into "just run scripts" would either lose
  those guarantees or require every manifest author to reimplement them.
  Raw scripts (`provision.run`) remain available for the long tail, matching
  `docs/PLAN.md`'s general bias toward "hardcode the common path, leave an
  escape hatch."
- **`base_image` is new** — today's guest is implicitly always the same
  ~260MB Alpine rootfs. Making it an explicit (optional, defaulted) field is
  what actually unlocks "not just Alpine" later; this doc does not propose
  building that support now, only reserving the field.
- Everything under `[provision]` is `harness`-neutral by construction — a
  second harness (§4.9) would supply its own `install`/`run` entries rather
  than needing a new manifest section.

#### 4.8.3 Idempotency and ordering rules

Directly generalizing the guarantees `provision.rs` already provides ad hoc:

1. **Steps run in file order** within each array (`packages`, then `mounts`,
   then `install` in listed order, then `run` entries in listed order,
   split by `when`). No implicit parallelism, no dependency graph — matches
   today's linear execution and keeps failure diagnosis simple (this is a
   deliberate simplicity choice, not a limitation to lift later without a
   concrete need).
2. **Every resolver must be re-run-safe.** Re-provisioning (e.g. after a
   fresh-VM boot with the host mount already populated — today's "already
   provisioned, no checkpoint" branch in `supervisor.rs`) re-runs the full
   list; each resolver is responsible for a cheap "already satisfied" check
   before doing work (mirroring today's version-marker-file and
   existence-check patterns). This is a resolver-author contract, not
   something the framework can universally enforce for `provision.run`
   scripts — documented as such, same as cloud-init's own `runcmd` (which
   explicitly always re-runs and pushes idempotency onto the script author).
3. **Failure is fail-fast, whole-provision, no partial rollback.** If any
   step errors, provisioning fails the same way `provision.rs` does today
   (surfaces as `AgentState::Failed`, per `supervisor.rs`'s existing
   `provision_and_start` error handling) — no attempt to roll back completed
   steps. The VM is disposable and gets rebuilt from scratch on the next
   `start`/retry, which is the existing recovery story and needs no new
   mechanism.
4. **`PROVISION_TIMEOUT` (today: 15 minutes) applies to the whole declarative
   sequence**, not per-step — consistent with today's outer bound existing
   specifically because of a real wedge incident, not something a spec
   format should let an agent author opt out of.

#### 4.8.4 Migration story

`provision.rs`'s current logic is expressible entirely in this format (it
was, after all, the source for it) — a follow-on task (not part of this
doc's phase 1) could auto-generate the equivalent `[provision]` block for
`harness.type = "pi"` manifests as a way to validate the spec covers the
real case, while the hand-written `Provisioner` impl remains the shipped
default for that harness either way. No existing manifest needs to change.

### 4.9 Generic Agent API framing

`harness.type` is already a field (today only `"pi"` is valid, enforced in
`provision::validate_manifest`). The trait boundaries in §4.5–4.8 are what
make "any AI Agent user" plausible without a rewrite: a different harness
would need its own `Provisioner` impl and its own `rpc.rs`-equivalent (the
`PiAgent` adapter that speaks the harness's specific stdio protocol), but
the registry, chat storage, snapshot storage, placement, and client API all
stay as-is. This doc does **not** propose building a second harness now —
only confirming that the trait seams above are where that boundary would
live later.

---

## 5. What does NOT need to change

Deliberately calling out the things this doc leaves alone, since a
unification effort this broad risks scope creep:

- The iroh transport, ALPN protocols, and wire framing (`framing.rs`) — solid
  as-is.
- The secrets model (age-encrypted store, per-manifest slicing, Gondolin
  placeholder-injection so guests never see real values) — orthogonal to
  this doc, don't touch.
- The auto-suspend/wake state machine and its public status mapping
  (`AgentState`, `public_status`) — this is already the right shape; the
  unification should sit *around* it, not replace it.
- `docs/AUTO-SUSPEND.md`'s hard removal of start/stop/suspend/restore as
  user verbs — this doc's verb set (§4.3) is consistent with that decision,
  not a reversal of it.

---

## 6. Migration sketch (order of operations, not a committed plan)

**Status: all four steps are done and verified** (build + clippy + full
workspace test suite clean at every increment, plus live end-to-end smoke
tests for steps 2–4, including two real Gondolin VM boots with real
network installs — see the notes at the end of this section for exactly
what landed).

There is no existing fleet to migrate today — this project has no deployed
`suzerain`/`castellan` installs to keep compatible mid-rollout, so the
sketch below is purely about sequencing the *implementation* work, not
about staging a rollout across already-running hosts or providing a
compatibility/import shim. (This also means the earlier concern about
"does merging binaries force lockstep versioning across a live fleet" is
moot for now — worth revisiting if/when this project has real deployed
fleets, but not a constraint on this plan today.)

1. **Extract all five trait boundaries together, in one pass, around
   existing/corrected concrete implementations — change no user-visible
   behavior.** `Registry` around `Store`, `SnapshotStore` around `bundle.rs`
   (with the fixed signature, §4.7), `Provisioner` around `provision.rs`
   (with the explicit `paths` argument, §4.8.1), `PlacementStrategy` around
   `scheduler.rs` (with the corrected `Candidate`-based signature, §4.5),
   and `ChatStore` as the new SQLite-backed event-log module (§4.6) — this
   last one is net-new construction, not a wrap, and should be built with
   the same care as any new storage layer, but there's no reason to
   sequence it separately from the other four; do all five together.
   Add the `protocol_version` field to `Register`/`RegisterResponse` in the
   same pass, since it touches the same protocol crate.
2. **Done.** Merge the binaries with real OS-process separation for
   standalone mode (§4.1): the re-exec'd parent/child split (transport
   revised to loopback iroh, not a socket pair — see §4.1), `mode` config,
   and the two install paths (§4.1.1). Also done, ahead of schedule:
   castellan's separate `daemon.rs` local socket and `bundle_from_env`
   standalone-secrets path are removed outright (not just superseded),
   since a standalone deployment now always has a control-role process.
3. **Done.** Collapse the client protocols into one shared client crate +
   verb set. By the time this started, step 2 had already retired
   castellan's local socket, leaving three protocols (not four) to collapse
   — `suz`, Suzy, and `suzerain-mcp` — all now sit on `suzerain-client`'s
   typed methods over two transports (iroh operator channel / direct
   HTTP). The old Unix-socket operator API (`suzerain/src/api.rs`) is
   retired outright, since `suz` was its only consumer.
4. **Done.** The declarative provisioning spec (§4.8.2) and its
   `DeclarativeProvisioner` implementation of the `Provisioner` trait —
   harness-neutral install resolvers (npm/git/mise) + a raw-script escape
   hatch, selected per manifest via `provision.is_some()`. §4.9's
   second-harness framing remains exactly that — framing, not a built
   second harness — consistent with the original scope.

Each step should still be independently shippable and reversible in the
sense that it can be tested and reviewed on its own — "do all five traits
together" means one pass of work, not one untestable commit.

**What actually landed in step 1**, in order:

1. `protocol_version: u32` on `Register`/`RegisterResponse`
   (`crates/protocol/src/control.rs`), validated on both sides —
   `crates/suzerain/src/control.rs`'s `register()` now rejects a mismatched
   daemon cleanly instead of failing deeper in the handshake.
2. `Registry` trait (`crates/suzerain/src/registry.rs`, previously a stub) —
   `Store` implements it by delegating to its existing inherent methods,
   zero behavior change; `ControlPlane::registry()` added alongside the
   existing `store()`.
3. `SnapshotStore` trait (`crates/suzerain/src/bundle.rs`) — signature fixed
   to match the real code (`data_base64: &str`, `sha256: Option<&str>`);
   `LocalSnapshotStore` resolves its root once at construction instead of
   via a buried `retention::load_config()` call on every operation; the
   existing free functions (`write_start`/`write_file`/`load`) are
   untouched wrappers over new `_in`-suffixed variants that take the root
   explicitly.
4. `PlacementStrategy` trait (`crates/suzerain/src/scheduler.rs`) — uses the
   corrected `Candidate { daemon, allocated }` signature from §4.5 (the
   caller computes live load before invoking the strategy); `place()`'s
   public signature is unchanged, now implemented as
   `place_with(cp, constraints, &DefaultPlacementStrategy)`; preemption
   stays the separate, strategy-agnostic layer §4.5 decided on.
5. `Provisioner` trait (`crates/castellan/src/provision.rs`) — takes an
   explicit `paths: &AgentPaths` argument (§4.8.1's fix for the hidden
   `$CASTELLAN_HOME`/`$SUZERAIN_HOME` env-var lookup); `provision()` is
   unchanged for its one existing caller, now a thin wrapper over
   `provision_with_paths`. `bundle_from_env` is **not** removed yet — that's
   correctly scoped to step 2 (it needs the always-present control-role
   process from the binary merge as its replacement, which doesn't exist
   yet); removing it now would regress castellan's still-in-use standalone
   local `create` path with nothing to replace it.
6. `ChatStore` trait (`crates/suzerain/src/chat_store.rs`, new) — a
   `chat_events` SQL table (migration v6 in `store.rs`, sharing `Registry`'s
   connection pool) with `append`/`tail`/`history_since`, `Store`
   implementing the trait by the same delegation pattern as `Registry`.
   **Deliberately partial**: this is genuinely new construction (per §4.6's
   revision note), and the nine existing read call sites
   (`api.rs`/`web_session.rs`/`web.rs`'s chat/logs/attach-history endpoints)
   each have different replay/tailing semantics that deserve their own
   careful, individually-tested migration rather than one blind rewrite.
   What's landed: the table, the trait, and the **write path** —
   `control.rs`'s `handle_logs` now writes every shipped event into
   `chat_events` *in addition to* the existing JSONL file. The JSONL file
   remains the source of truth for all reads until the follow-up work
   migrates each reader and the dual-write can be dropped. This is called
   out explicitly in `chat_store.rs`'s module doc comment so it isn't
   mistaken for a completed cutover.

Every step above was verified with `cargo build --workspace`, `cargo clippy
--workspace --tests`, and `cargo test --workspace` (all clean) before
moving to the next.

**What actually landed in step 2**, in order — with the transport revision
from §4.1 (loopback iroh, not a socket pair) applied throughout:

1. `[role]` config (`crates/suzerain/src/retention.rs`'s `Config`) — a new
   `RoleMode::{Standalone (default), Control, Agent}` enum, overridable via
   `suzerain run --mode <standalone|control|agent>` (falls back to config
   when omitted). `crates/suzerain/main.rs` now dispatches on this: `agent`
   calls `castellan::run_foreground()` directly; `control` runs the
   control-plane startup and then waits for a shutdown signal (the old
   Unix-socket operator API this used to await was retired in step 3, see
   below); `standalone` does the same plus spawns the co-located
   agent-worker.
2. `crates/suzerain/src/standalone.rs` (new) — `spawn_agent_worker(cp)`:
   loads/creates the co-located agent's iroh identity *in the parent*
   (`castellan::control::identity()`, same call the child would make
   anyway), auto-approves it (`cp.store().approve_daemon(...)`, no manual
   `suz daemon approve` step), points its `castellan.toml` at the parent's
   own endpoint id, then re-execs `current_exe() run --mode agent` with
   `kill_on_drop(true)`. The child's exit is monitored in a background task
   purely for logging — it deliberately does **not** race the client-facing
   `api::serve` future via `tokio::select!`, since that would let the
   child's exit cancel and restart live API service; the control plane
   keeps serving if the local agent-worker goes down, same as when a real
   remote castellan disconnects.
3. **Verified live**, not just compiled: `suzerain run` in a scratch data
   dir produces a real parent+child process pair, real iroh registration
   (`daemon registered` / `registered with control plane` on both sides),
   and `suz daemon list` shows the co-located agent as `approved online` —
   with zero manual approval steps. Re-ran after the step-3-adjacent changes
   below to confirm they didn't regress it.
4. **castellan's standalone local CLI/socket removed** (per an explicit
   decision made mid-implementation, going beyond this doc's original step-2
   scope into what was step 3's territory) — `crates/castellan/src/daemon.rs`
   (the local Unix-socket JSONL server) is deleted, along with
   `provision::bundle_from_env` (its standalone-secrets fallback) and
   castellan's own `create/start/stop/destroy/list/logs/attach/ask/exec` CLI
   verbs. `castellan::run_foreground()` (used by both `castellan run` and
   the standalone-mode child) now just runs the instance-lock, crash-
   reconciliation, and control-client logic — no local socket to race
   against. `castellan run` remains valid and useful for a real distributed
   `agent`-only host (via `castellan init --suzerain <id>`); what's gone is
   the "castellan with no control plane, talk to it directly" scenario,
   which the standalone-mode merge makes obsolete (every deployment now has
   a control-role process, even a co-located one).
5. `ops/install.sh` gets a `--control-only` flag (default remains "full"):
   skips the Gondolin runtime (node/qemu/KVM/driver bundle) entirely, for a
   dedicated `mode = control` registry host. The runtime-dependency
   install now also runs for a plain `suzerain` install (not just
   `castellan`), fetching the castellan archive's driver bundle even if the
   `castellan` binary itself wasn't separately requested — `suzerain`'s own
   standalone/agent modes need that same runtime. `ops/systemd/
   suzerain.service` and `ops/launchd/com.suzerain.controlplane.plist` gain
   the same mise-shims `PATH` entry `castellan.service`/`com.suzerain.
   castellan.plist` already had, since standalone mode (the default) now
   needs qemu/node reachable from the parent process's own environment (the
   child inherits it).

Not done from the original step-2 scope: nothing — items 3–5 of the
original sketch (unifying castellan's local verbs, removing
`bundle_from_env`, the install split) all landed as part of this pass,
per an explicit decision to go further than the minimal §4.1 wiring in one
combined session rather than stopping at a bare parent/child spawn.

**A real bug found and fixed during step 2's live testing**: `kill_on_drop`
on the standalone-mode child (`tokio::process::Child`) only fires on a
*graceful* Rust-level exit (a scope ending, a spawned task dropped when the
tokio runtime shuts down) — an external `kill`/`systemctl stop`/`launchctl
unload` sends SIGTERM, which by default terminates the process immediately
without running any Rust destructors. Verified this concretely: sending
`kill -TERM` to the parent left the agent-worker child (and its qemu VM)
running, orphaned. Fixed by having `main()` catch SIGTERM (in addition to
SIGINT) and return normally, so the runtime-shutdown drop cascade actually
runs — re-verified: SIGTERM to the parent now cleanly tears down both
processes. This is the kind of gap live testing catches that unit tests and
a read-through of the code would not.

**What actually landed in step 3**, in order:

1. `suzerain-client`'s `Client` is now transport-generic
   (`crates/suzerain-client/src/transport.rs`, new): a `Transport` trait
   with two primitives (`rest`, `sse`) that every one of `Client`'s ~40
   typed methods already funneled through. `IrohTransport` is today's iroh
   operator-channel logic moved over unchanged (Suzy's call sites and
   behavior are 100% unaffected — verified by a clean workspace build with
   zero changes to `crates/suzy`). `HttpTransport` is new: direct HTTP
   against `/api/v1/...` via reqwest, for local-only callers. `Client::http
   (base_url)` is the new constructor; `Client::raw(method, path, body)` is
   an escape hatch for calls not covered by a typed method.
2. Two new REST endpoints on `suzerain::web` (`GET`/`POST
   /api/v1/operators`) replacing the old socket-only `operator_list`/
   `operator_approve` — same behavior (live approval via
   `cp.add_operator_allow`, persisted via `retention::add_operator_allow`).
3. `suzerain-mcp`'s `ApiClient` is now a thin wrapper over
   `suzerain_client::Client`'s HTTP transport (`crates/suzerain-mcp/src/
   client.rs`) — its own `get`/`get_query`/`post`/`delete` methods are
   unchanged, so `server.rs`'s ~18 tool implementations needed zero edits.
4. `suz` (`crates/suzerain-cli/src/main.rs`) is fully rewritten onto
   `suzerain_client::Client::http(...)` — the old hand-rolled Unix-socket
   JSONL protocol is gone. New `Client` methods added along the way:
   `create_agent_full` (manifest + daemon pin), `operators`/
   `operator_approve`, and `ask(name, message, timeout)` — a **real,
   working implementation of §4.3.5's send+poll composition**: `prompt()`
   to send, then poll `session_state()` until the turn settles (bounded by
   a caller-supplied timeout, not a hardcoded one), then read the last
   assistant message from `session_history()`. `attach` is composed from
   `session_stream()` (SSE) + `prompt()`.
5. The old Unix-socket operator API (`suzerain/src/api.rs`) is deleted —
   `suz` was its only consumer. `main()`'s foreground loop no longer awaits
   it; see the SIGTERM-handling note above for what replaced it.
   `[web].enabled = false` now means `suz`/`suzerain-mcp` can't reach the
   control plane at all (only Suzy's iroh operator channel still works,
   since it dials the same router in-process) — a real, intentional
   behavior change, logged as a warning rather than left silent.
6. **Verified live**: a full session — `suz id`, `daemon list`, `secrets`,
   `audit`, `operator approve`/`list` (live, via the new REST endpoints),
   `daemon label` — all working over real HTTP calls against a running
   standalone instance. `suz agent create` against a real manifest
   correctly surfaced the exact multi-line secrets-preflight error from the
   server; with a secret set, `agent create` **actually scheduled and
   booted a real Gondolin VM** (confirmed via `ps aux`: a real
   `qemu-system-aarch64` process and `gondolin-driver` running), reaching
   `state=waking` before being destroyed for cleanup. Error paths
   (`destroy`/`ask`/`config` against a nonexistent agent) all returned
   promptly with the server's real error text.

**What actually landed in step 4**, in order:

1. `ProvisionSpec`/`MountSpec`/`InstallEntry`/`RunEntry`/`RunWhen`/
   `TrustSpec` added to `crates/protocol/src/manifest.rs` — an optional
   `provision: Option<ProvisionSpec>` field on `AgentManifest`.
   `InstallEntry` is an internally-tagged enum (`resolver = "npm"|"git"|
   "mise"`, matching §4.8.2's TOML shape exactly) with three variants.
   `RunWhen::PostStart` exists in the schema (so it round-trips) but is
   rejected by `validate_manifest` with a clear message — no Provisioner
   hook exists yet for "after the harness process starts," so accepting it
   silently would drop user-specified behavior on the floor.
2. `ensure_npm_toolchain`/`write_npm_shim` extracted out of the hardcoded
   pi sequence into shared helpers (`crates/castellan/src/provision.rs`) —
   "bootstrap npm in a bare Alpine guest" isn't pi-specific, and both
   `PiProvisioner` and the new declarative npm resolver need it.
3. `DeclarativeProvisioner` (implements `Provisioner`): boots the VM itself
   (mounts must exist at boot time, so `[provision.mounts]` folds into the
   same `driver.boot()` call `PiProvisioner` makes) — apk packages → git
   ssh config → typed installs in listed order (npm/git/mise resolvers) →
   pre_start run scripts → trust.json → `APPEND_SYSTEM.md`. Nothing in it
   is pi-specific; a manifest can install `@earendil-works/pi-coding-agent`
   itself via a plain npm install entry, per §4.9's harness-neutral framing.
4. `crates/castellan/src/supervisor.rs`'s provisioning call site now
   selects `DeclarativeProvisioner` vs. `PiProvisioner` based on
   `manifest.provision.is_some()` — the only place this decision is made.
5. `examples/declarative.toml` (new) — a full worked example.
6. **Verified live**, twice, against a real standalone instance:
   - First run (`examples/declarative.toml`, includes a `mise` install
     entry) got through packages → npm install of the real
     `@earendil-works/pi-coding-agent` package (~77s, a real network
     install) → git clone of a real GitHub repo → and failed at the mise
     resolver with a clean, correctly-propagated error. Confirmed this is
     a **pre-existing environmental gap shared with the hardcoded path**,
     not a regression: the exact same `curl -fsSL https://mise.run | ...`
     command already existed unchanged in `PiProvisioner`; `mise.run` is
     reachable from the host but apparently not through the guest's egress
     proxy with today's allowlist. Not fixed (out of scope for this pass —
     it's a pre-existing gap, not something step 4 introduced), but worth
     flagging for whoever next touches the mise resolver or egress
     allowlist.
   - Second run (same manifest minus the `mise` entry) reached
     "declarative provisioning complete." Confirmed on disk: the
     `PROVISIONED` marker file from the `run` entry had the exact expected
     content, `trust.json` was exactly `{"/agent/workspace":true}`,
     `APPEND_SYSTEM.md` had the exact configured text, the GitHub repo was
     really cloned, and the agent reached `status=idle` — meaning `pi`
     itself (installed via the npm resolver, no hardcoded pi-install step
     at all) actually spawned successfully afterward.

---

## 7. Risks / open questions (see also the questions asked separately)

- **Resolved (§4.1), revised after security review**: standalone mode is
  two OS processes (a re-exec'd parent/child pair), not one process with an
  internal runtime split as originally drafted. This gives both the
  originally-intended failure isolation (a wedged provisioning task can't
  starve the registry/API — now guaranteed by the OS process boundary, not
  by a Tokio-runtime scheme) *and* closes a real security gap the earlier
  one-process design had: the full long-term secrets store and every live
  agent's execution surface would otherwise share one address space. Two
  processes restore the same secrets/execution boundary today's two
  separate binaries already provide.
- **Resolved (§4.3)**: `Order`/`OrderAck` stays a separate, lower-level
  protocol beneath the client-facing lifecycle API, confirmed as intentional
  rather than left ambiguous. Following from this, `RestoreAgent` and
  `UpdateManifest` (currently dead `Order` variants in `dispatch_order`)
  need to be either implemented or removed as part of this work. Also
  **resolved, new**: this handshake gains an explicit `protocol_version`
  field so future incompatible changes have a clean negotiation point —
  pure insurance, since there's no existing fleet to protect today.
- **Resolved (§4.5)**: preemption (`place_or_preempt`) is a separate,
  strategy-agnostic layer above `PlacementStrategy`, not part of the trait —
  every strategy impl gets it "for free." The trait's own signature is also
  corrected to pass pre-computed `Candidate{daemon, allocated}` data rather
  than a bare `&[DaemonRow]`, since the real scheduler needs live
  per-daemon load the latter can't express.
- **Resolved (§4.6)**: `ChatStore`'s honest framing is "new construction,"
  not "extraction" — there is no existing module to wrap, and per the
  decision to back it with SQLite, the design now specifies a concrete
  `chat_events` table shape rather than leaving the storage model
  unspecified.
- **Resolved (§4.7/§4.8.1)**: `SnapshotStore` and `Provisioner`'s trait
  signatures are corrected to match the real code (base64/optional-hash for
  bundle writes; an explicit `AgentPaths` argument for provisioning instead
  of a hidden env-var lookup).
- **Resolved (§6)**: all five trait extractions are one combined pass, not
  staged — per explicit decision, since the risk-profile differences
  between them (§ from the earlier review) don't change the value of
  landing them together.
- **Resolved (§6)**: no existing fleet exists to migrate, so the
  operational risks raised in review (mixed-version rollout, `state.json`
  import, install-dependency splits) are addressed by (a) the new
  `protocol_version` field for future-proofing and (b) the two install
  paths in §4.1.1 — not by a migration/rollback plan, which isn't needed
  yet.
- **Resolved (§4.1), researched against precedent, transport later
  revised**: the standalone-mode spawn mechanism is re-exec via
  `Command::new(current_exe())` with an internal flag (never raw `fork()`
  without exec, which is unsupported in a process already running a
  multithreaded Tokio runtime). The local transport was originally planned
  as a `socketpair`-created `UnixStream` pair (OpenSSH-privsep-style) but
  was changed during implementation planning to **loopback iroh over
  `127.0.0.1`**, once it became clear the real protocol's multiple
  concurrent sub-streams (Register/StateReport/Attach/Shell/Restore/Logs)
  would need a hand-built multiplexer over a bare socket pair — a bigger,
  riskier build than reusing iroh unchanged for a purely local link. No
  remaining open items in this doc; the one open PlacementStrategy question
  below is explicitly punted, not resolved.
- **Punted, not resolved**: how an operator would actually register a
  custom `PlacementStrategy::Custom(String)` implementation (§4.3.2) — a
  plugin/dylib mechanism, a compiled-in registry selected by name, or
  something else — is left unspecified. Since exactly one built-in strategy
  ships initially, this doesn't block any work in §6's migration sketch;
  revisit only once a second strategy implementation is actually needed.

---

## Appendix: file-level mapping (current → proposed)

| Current | Role today | Proposed |
|---|---|---|
| `crates/suzerain` (bin) | control plane process | The **only** binary — `suzerain --mode=control`, `--mode=agent`, or `--mode=standalone` (default, spawns its own agent-hosting child) |
| `crates/castellan` (bin) | daemon process | **Removed outright** (post-launch decision, beyond original scope) — `castellan` is now a lib-only crate `suzerain` depends on; `suzerain init` replaces `castellan init` for a real distributed agent host |
| `suzerain/src/store.rs` | concrete SQL `Store` | default `impl Registry` |
| `castellan/src/state.rs` | flat-JSON per-agent cache | local `Registry` cache (agent-mode only) or removed (standalone) |
| `suzerain/src/bundle.rs` | local-fs bundle store | default `impl SnapshotStore` (corrected signature, §4.7) |
| `suzerain/src/scheduler.rs` | hardcoded placement | default `impl PlacementStrategy` (corrected `Candidate`-based signature, §4.5); preemption stays a wrapper layer above it |
| `castellan/src/provision.rs` | hardcoded VM bootstrap | default `impl Provisioner` (explicit `AgentPaths` argument, §4.8.1) |
| `castellan/src/provision.rs::bundle_from_env` | standalone-only env-var secrets fallback | removed — standalone mode always has a control-role process slicing real secrets (§4.1, §4.8.1) |
| (no existing module) | inline JSONL log-path construction in 3 call sites, no `history_since` | new `ChatStore`, SQLite-backed `chat_events` table (§4.6) — net-new, not an extraction |
| `suzerain/src/api.rs`, `castellan/src/daemon.rs` | 2 divergent local verb sets | 1 verb set, shared client crate, served by the standalone parent process |
| `suzerain-client`, `suz`'s ad-hoc socket code, `suzerain-mcp`'s `ApiClient` | 3 divergent client implementations | thin adapters over 1 shared client |
| `crates/protocol/src/control.rs::Register`/`RegisterResponse` | no version field | `protocol_version: u32` added, `PROTOCOL_VERSION` const (§4.3) |

## Addendum: the `castellan` binary was removed entirely

A post-launch decision, beyond every step's original scope: having both
`suzerain run --mode agent` and a separate `castellan run` do the same
thing was judged confusing rather than useful — "two ways to do one
thing." `crates/castellan/src/main.rs` is deleted; the crate is now
lib-only (`castellan::run_foreground()`, `castellan::control::*`, etc.,
all still used internally by `suzerain`). Concretely:

- `suzerain init --suzerain <id> --label k=v` (new subcommand) replaces
  `castellan init` for a real, dedicated distributed agent-hosting host —
  identical behavior, same underlying identity/config code.
- `ops/dev-network.sh` simplified to just run `suzerain run` (standalone
  mode already does the two-process dance it used to orchestrate by hand).
- `ops/install-services.sh`, the systemd unit, and the launchd plist
  collapsed to one service — `castellan.service`/`com.suzerain.
  castellan.plist` deleted outright.
- `ops/install.sh` and `.github/workflows/release.yml`: no more separate
  `castellan` component/archive — the Gondolin driver bundle now ships
  inside `suzerain`'s own release archive, since `suzerain` is what needs
  it (in `standalone`/`agent` mode). This also *simplified* `install.sh`:
  the old "download the castellan archive purely for its driver bundle
  even when the castellan binary itself wasn't requested" workaround is
  gone entirely.
- `ops/e2e.sh`: rewritten to boot one standalone `suzerain run` instead of
  separately booting and enrolling a `castellan run` process.
- README, the `suzerain-admin` plugin skill (SKILL.md + both reference
  docs), and RELEASING.md updated throughout — these are operationally
  load-bearing (an LLM assistant follows them literally), not just prose,
  so stale `castellan run`/`castellan init` instructions would have
  actively broken automated setup.

**Verified live**, twice: (1) standalone mode boots correctly with zero
`castellan` binary present anywhere on the system (confirmed via `which -a
castellan` finding nothing, after cleaning up stale artifacts left over
from before this change); (2) `suzerain init` on a separate host/home dir,
approved from the control plane, then run as a genuinely separate
`suzerain run --mode agent` process — `suz daemon list` showed *both* the
standalone co-located agent and this independent distributed agent host as
`approved online` simultaneously.
