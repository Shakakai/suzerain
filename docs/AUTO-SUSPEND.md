# Auto-Suspend & Transparent Wake — Implementation Plan

Status: **implemented** (all phases below landed; live end-to-end
verification in §9 passed 2026-08-13).
*(The mechanism described here is unchanged; only the binary story
changed since — "castellan" below means the agent-hosting role of the
single `suzerain` binary, not a separate binary. See
[docs/UNIFIED-AGENT-API-DESIGN.md](UNIFIED-AGENT-API-DESIGN.md).)*

**Session rotation (added after the initial implementation):** sessions
rotate on **every** suspend. The order flow is graceful stop → bundle
upload (session preserved centrally in full) → session files removed
from the guest disk → checkpoint → close. Both wake paths start a fresh
pi session: checkpoint resume spawns pi with no `--session` (the record's
session pointer is cleared at suspend), and bundle restore skips
`sessions/*` files entirely (they stay in the control plane's bundle
store for history/audit). Crash-respawn within an awake period still
resumes the current session. Persistent memory across rotations (beyond
the agent's persona via `APPEND_SYSTEM.md`) is deliberately future work.

**Session eras are tracked in the DB** (`agent_sessions` table): the
daemon reports its current session file on the state-report stream; a
changed file closes the open era and opens a new one (idempotent against
ack/report arrival races). The daemon also journals `session_started` /
`session_rotated`, so the central log itself carries boundaries — the web
chat (SSE + history JSON), CLI attach, and the agent detail page all
segment the conversation by era.

One deliberate interpretation, flagged during implementation: an open
attach/chat session bumps the activity clock but does **not** latch
`busy` — otherwise an idle browser tab would pin agents awake forever.
`busy` = a turn in flight only.

## Goal

Agents automatically suspend after a configurable period of inactivity and
transparently wake when a message arrives. Start/stop/suspend/restore
disappear as user-facing concepts; the only lifecycle verbs are **create**,
**destroy**, and **chat**. The suzerain is the single authority on all
suspend/wake decisions; castellans only report ground truth and execute
orders.

## Public state model

Internal states stay as-is (`protocol::state::AgentState`). All public
surfaces (web UI, MCP, CLI) present a derived status:

| Public status | Internal condition |
|---|---|
| `running` | Active and **busy** (turn in flight or live attach session) |
| `idle` | Active and settled (no turn in flight, waiting for input) |
| `sleeping` | Suspended |
| `waking` | Provisioning / Restoring (including wake-in-progress) |
| `failed` | Failed (additionally flagged when human intervention is required) |

A single mapping function lives in `suzerain-protocol` so every surface
renders identically.

---

## 1. Activity tracking (castellan)

**Ground truth lives on the daemon.** The supervisor already sees every pi
RPC event and every inbound prompt — we instrument those choke points.

- `RunningAgent` gains `last_activity: Instant` (and an RFC3339-persisted
  twin) and a `busy: AtomicBool`.
- `last_activity` is bumped on: any pi RPC event from the event pump, any
  prompt/steer/follow_up/abort, any attach stream open/close.
- `busy` = a turn is in flight (set on prompt/turn-start events, cleared on
  `agent_end`/`agent_settled`) **or** at least one attach session is open.
  This is the critical guard: an agent grinding through a 30-minute test
  suite emits events continuously, so its clock never goes stale and `busy`
  stays true — it is never a suspend candidate.
- Both are persisted in the on-disk `AgentRecord` so the clock **survives a
  castellan restart** (control-plane-managed only; standalone castellan gets
  tracking but no auto-suspend).

### Reporting (protocol change)

`AgentStateEntry` gains optional fields (serde-defaulted → wire-compatible):

```rust
pub struct AgentStateEntry {
    pub agent_id: Uuid,
    pub name: String,
    pub state: AgentState,
    #[serde(default)] pub idle_secs: Option<u64>,  // daemon-computed
    #[serde(default)] pub busy: Option<bool>,
}
```

Carried on the existing `StateReport` stream (snapshot at registration +
every 60s + on transitions). We report **`idle_secs` computed daemon-side**
rather than a raw timestamp: suzerain extrapolates
`idle_now = idle_secs + (now − report_received_at)`, which is **immune to
clock skew** between machines. Suzerain stores `idle_secs`, `busy`, and
`report_received_at` on the agent row.

## 2. Configuration

Global, in `$SUZERAIN_HOME/suzerain.toml` (alongside `[retention]`/`[web]`):

```toml
[auto_suspend]
enabled = true
idle_timeout = "30m"        # global default
sweep_interval = "30s"      # how often suzerain evaluates
wake_retry_attempts = 3

[bundles]
dir = "/mnt/external/suzerain-bundles"   # snapshot/bundle storage path;
                                         # default: $SUZERAIN_HOME/bundles
```

Per-agent override in the manifest (explicit opt-out must be deliberate):

```toml
[lifecycle]
auto_suspend = "10m"     # duration, or "never"; omitted = inherit global
```

Policy resolution: manifest `lifecycle.auto_suspend` → global
`[auto_suspend].idle_timeout`. `"never"` exempts the agent from **both**
idle-timeout suspension and resource-pressure preemption.

## 3. Auto-suspend sweep (suzerain, single authority)

New module `suzerain/src/lifecycle.rs`: a background task ticking at
`sweep_interval`:

1. For each agent row: state Active, daemon online, policy not `"never"`,
   `busy == false`, extrapolated idle ≥ effective timeout → issue the
   existing `SuspendAgent` order (checkpoint + bundle upload — unchanged
   mechanics, per the confirmed decision that every suspend uploads a
   bundle so wake placement stays flexible).
2. **Daemon-side revalidation (the safety latch):** `SuspendAgent` gains
   `only_if_idle: bool` and `not_since: Option<RFC3339>`. The supervisor
   re-checks ground truth at execution time — if the agent is mid-turn, has
   an open attach, or saw activity after `not_since`, the order is
   **refused** (`ack.success = false, "busy"`) and suzerain backs off until
   the next sweep. Suzerain's view can be up to 60s stale; the daemon's
   never is. This is the mechanism that guarantees we never kill a
   long-running turn.
3. Failures are logged + audited; retried next sweep.

Suspended agents **free their slot and resources on the daemon** (the
daemon's `agents` list is running agents). The VM checkpoint stays on the
originating daemon's disk as the wake fast path; the bundle lives at
`[bundles].dir` for cross-daemon restore.

## 4. Message queue & transparent wake (the "Activator")

Patterned on Knative's Activator, but durable. New table in `store.rs`:

```
pending_messages(id, agent_id, body, status, attempts,
                 last_error, created_at, delivered_at)
-- status: queued | waking | delivered | failed
```

### Delivery path (all entry points funnel through one function)

`actions::deliver_prompt(cp, name, message)`:

1. Agent Active + daemon online → existing direct attach path (fast path,
   no queue involvement).
2. Agent sleeping / failed-with-bundle / daemon-offline →
   a. Persist the message as `queued`.
   b. **Coalesce** (confirmed): while a wake is in flight, additional
      messages are appended to the same pending batch; on delivery the
      batch is sent as one prompt (messages joined with a separator).
   c. Spawn (or join) the wake task.
3. Agent failed **without** a restorable bundle → immediately fail the
   message, flag the agent `needs_attention` (rendered in UI as
   failed/requires-intervention).

### Wake task

1. **Placement:** prefer the last daemon if it's online and still holds the
   local checkpoint → `StartAgent` (fast resume, seconds). Otherwise run
   `scheduler::place` and stream the bundle (`Restore` flow) — possibly a
   migration.
2. **Retry with spread:** on failure, retry up to `wake_retry_attempts`,
   **excluding daemons that already failed this wake** (confirmed Q10).
3. **Terminal failure:** message(s) → `failed`, agent → Failed +
   `needs_attention`, waiters receive the error.
4. **Success:** deliver the coalesced batch, mark rows `delivered`
   (retained with `delivered_at` for audit; pruned by a periodic sweep).

Suzerain restart resilience: on boot, any `queued`/`waking` rows resume
their wake tasks — nothing is lost (confirmed Q10 durability requirement).

A per-agent lifecycle mutex in suzerain serializes suspend-vs-wake races
(single authority makes this trivial).

### Entry points & UX

- **`suz agent ask`**: blocks up to 300s total (wake+answer inside that
  budget is fine, confirmed Q11); prints a `waking agent…` notice to
  stderr while waiting.
- **`suz agent attach` / web chat SSE**: connect **immediately** (confirmed
  Q12) and emit synthetic status events — `waking`, `restoring on <host>`,
  `ready` — then replay history and begin the live relay. Prompts typed
  while waking enter the queue and are coalesced.
- **MCP `agent_ask`**: same semantics; tool description updated to state
  that sleeping agents wake automatically and first responses may take a
  few minutes.

## 5. Resource-pressure preemption (confirmed Q7 revision)

When `scheduler::place` is choosing a daemon for a **create** or **wake**
and the best-fit daemon is at `max_agents` or lacks the requested
resources:

1. Look for **preemptible** agents on that daemon: Active, `busy == false`
   (authoritatively idle — cannot be doing work), policy not `"never"`.
   Selection: longest-idle first (LRU).
2. Suspend them (same path as §3, including daemon-side revalidation) until
   the reservation fits, then proceed with placement.
3. If prevalidation refuses (agent went busy), fall to the next candidate
   daemon / next idle agent.

v1 scope: preemption is only attempted on the daemon the scheduler would
otherwise have chosen; if no preemptible idle agents exist, placement fails
with the current per-candidate reasons (no cross-daemon shuffling yet).
Anti-thrash: an agent that woke within the last 5 minutes is not a
preemption candidate (wake grace period).

## 6. API surface changes (hard removal, confirmed Q14)

| Surface | Removed | Added/changed |
|---|---|---|
| `suz` CLI | `agent start/stop/suspend/restore` | `agent config <name> --auto-suspend 10m\|never\|default`; `agent list/get` show public status + idle time |
| Operator socket (`api.rs`) | `agent_start/stop/suspend/restore` cmds | `agent_config`; `agent_ask`/`agent_attach` route through `deliver_prompt` |
| Web REST | `POST /agents/{name}/{start,stop,suspend}`, `/restore` | `PATCH /agents/{name}` (auto-suspend policy); status field is the public model |
| Web UI | Start/Stop/Suspend/Restore buttons | Status badge (running/idle/sleeping/waking/failed); chat always available; wake progress shown in-session; auto-suspend policy editor |
| MCP | `agent_start/stop/suspend/restore` tools | Descriptions rewritten: agents wake on demand; lifecycle is create/chat/delete only |
| Castellan socket | (unchanged mechanics) | tracking only; no auto-suspend in standalone mode |

Internal orders `StartAgent`/`SuspendAgent`/`Restore` **remain** — they are
now suzerain-internal machinery, just no longer user-invokable.

## 7. Store / migration notes

- `agents` table: add `idle_secs`, `busy`, `activity_reported_at`,
  `needs_attention`, `auto_suspend_override` (runtime-set policy, wins over
  manifest). Migration via `ALTER TABLE … ADD COLUMN` with duplicate-column
  tolerance (current migrations are `CREATE TABLE IF NOT EXISTS` only).
- New `pending_messages` table (above).
- `crate::bundle` paths resolve through `[bundles].dir` config instead of a
  hardcoded `data_dir()` subpath.

## 8. Edge cases & safety (checklist)

- [x] Long turn (> timeout): `busy` + continuous events ⇒ never suspended
      (live-verified: 150s quiet task, one refused suspend, none landed).
- [x] Turn ends 1s before sweep: `last_activity` refreshed at turn end ⇒
      full timeout elapses from *settle*, not turn start (live-verified).
- [x] Suspend decision on stale data: daemon revalidates via
      `only_if_idle`/`not_since` and may refuse (live-verified twice).
- [x] Wake vs. suspend race: per-agent mutex in suzerain.
- [x] Suzerain restart mid-wake: `pending_messages` resumed on boot.
- [x] Castellan restart: activity clock persisted in `AgentRecord`;
      live-verified (398s → 463s across the restart).
- [x] Message to failed agent with no bundle: error + `needs_attention`
      (confirmed Q16; induced live, then cleared by a successful wake).
- [x] Sessions rotate on every suspend: uploaded centrally in full →
      removed from the guest before checkpoint → fresh pi session on wake
      (live-verified same-host and cross-daemon; agent had no memory of
      the pre-suspend codeword; old session retained in the bundle store).
- [x] Wake storm (N agents messaged simultaneously): wakes are per-agent
      tasks; scheduler placement naturally serializes on capacity, and
      preemption may suspend idle agents to make room.
- [x] Clock skew between hosts: `idle_secs` extrapolation, no timestamps
      compared across machines.
- [x] Preemption thrash: 5-minute wake grace period; LRU selection;
      `never` agents untouchable.
- [x] Wake retries actually spread: a failed restore target is excluded
      from subsequent attempts (fixed after live verification showed all
      attempts hitting the same dead daemon).
- [x] Phantom daemons after a suzerain restart: the control plane marks
      all daemons offline at boot (fixed after a stale `online=1` row
      attracted a restore to a non-existent daemon).
- [x] Journal pruning must never detach a live journal: the shipper's
      prune guard now includes in-flight provisions (a wake's provision
      window has no running entry yet but an open Journal appending into
      the very file being pruned — the rename sent all wake events to an
      unlinked inode; found via missing `session_started` events).
- [x] Session-era double-open at create: `start_agent_session` is
      idempotent when the open row already tracks the file (ack/report
      arrival race opened two eras 0.6ms apart).

## 9. Testing

Unit coverage (**landed**): policy resolution (global vs. manifest vs.
runtime override), public state mapping, duration parsing, the scheduler
`max_agents` slot limit, and preemption candidate rules (LRU, grace
period, `never` exemption via `is_preemptible`).

Integration / live verification (**done 2026-08-13 against the dev fleet**
— one suzerain + two castellans, `[auto_suspend] idle_timeout = "2m"`,
`sweep_interval = "10s"`; all items passed):

- [x] **Long-running task safety (acceptance test for the core invariant):**
      a 150s quiet `sleep` command ran past the 2m timeout with the public
      status reading **running** (`busy == true`) throughout. The sweep
      evaluated the agent once on a stale snapshot and the daemon's
      revalidation **refused** (`busy: turn in flight`); no suspend ever
      landed. After the turn settled the status flipped to **idle**, the
      clock restarted, and the agent auto-suspended 2m later (audit:
      `agent_auto_suspend`).
- [x] Wake-from-chat: suspended agent answered `suz agent ask` in **6.9s**
      via the same-host checkpoint fast path (audit: `agent_wake`).
- [x] Coalescing: 3 web-API prompts sent while waking were delivered as
      **one batch** (`\n\n---\n\n` separators, verified in the central
      log); all three `pending_messages` rows marked `delivered`.
- [x] Wake failure spread: owning daemon killed before the ask; the wake
      excluded it and **restored the bundle onto a second daemon** (~70s
      re-provision). The agent retained full session memory (recalled all
      three codewords from the coalescing test).
- [x] Castellan restart mid-idle: reported `idle_secs` continued across
      the restart (398s → 463s), derived from the persisted
      `last_activity_at`; the daemon reconciled its orphaned VM to
      suspended on boot as designed.
- [x] Preemption: with a daemon at `max_agents` (2/2), an unpinned create
      suspended the **longest-idle preemptible** agent and placed the new
      one (audit: `agent_preempt_suspend`). Negative case also verified:
      a daemon full of `never` agents rejects the create cleanly
      (`agent slots full (2/2)`). Note: hard pins bypass fit checks
      (pre-existing operator-override semantics) and therefore skip
      preemption.
- [x] `never` agent: sat 300+s past the global timeout untouched, and was
      **skipped** as a preemption candidate in favor of the preemptible
      agent.

## 10. Implementation phases

1. **Protocol + tracking**: `AgentStateEntry` fields; supervisor
   activity/busy instrumentation + persistence; store columns; state
   mapping function.
2. **Config**: `[auto_suspend]`, `[bundles].dir`, manifest `[lifecycle]`,
   policy resolution.
3. **Sweep**: `lifecycle.rs` auto-suspend loop + `SuspendAgent`
   revalidation fields (daemon side).
4. **Wake path**: `pending_messages`, coalescing, wake task with
   retry/spread, `deliver_prompt`, CLI ask/attach.
5. **Surfaces**: web REST/UI, MCP, operator socket — remove verbs, public
   states, synthetic wake events, policy editing.
6. **Preemption**: scheduler hook + LRU selection + grace period.
7. **Docs & examples**: README quickstart, `docs/MCP.md`, `docs/WEB-UI.md`,
   example manifests with `[lifecycle]`.

Each phase lands green (`mise run test`/`lint`) and is independently
revertible; phases 1–3 are behavior-inert until the sweep is enabled.

## 11. Defaults chosen (push back if any of these are wrong)

- Coalesced batch delivered as one prompt, messages joined with `\n\n---\n\n`.
- Wake grace period before preemption eligibility: 5 min.
- Sweep interval: 30s (configurable).
- Wake retry: 3 attempts, failed daemons excluded.
- `"never"` protects against both idle-timeout and preemption.
- Suspended agents free their daemon slot; checkpoint retained locally for
  the fast path.
- Delivered message rows retained for audit, pruned on the retention sweep.
