# Suzerain Web UI — Product Spec (v1, for review)

**Status: DRAFT for review — no code yet.**

## 1. Vision & scope

A local-only web interface to the suzerain control plane, served by the
suzerain process itself on `127.0.0.1`. It is the operator's primary way to
see the fleet (castellans + agents), run agents end-to-end
(create → watch → chat → destroy; suspension and wake are automatic),
enroll new daemons,
and manage the secrets store — everything the `suz` CLI does today, plus the
two things a CLI does badly: watching live agent sessions and editing
structured configuration.

**Non-goals for v1**: multi-user/RBAC, remote access (localhost only), a web
terminal into guest VMs, mobile layout, dark mode (nice-to-have), editing
agent manifests after creation.

## 2. Users & security model

Single operator on the suzerain machine (the same person who runs `suz`
today). Threat model: any local process could previously hit the operator
socket (fixed by G6 peer-uid checks); the browser itself is trusted content
the operator opens deliberately.

- **Bind**: `127.0.0.1` only (no LAN exposure). Default port **8484**,
  configurable in `$SUZERAIN_HOME/suzerain.toml` (`[web] port`, `enabled`).
- **AuthN**: none required for localhost in v1 (matches the CLI's security
  posture post-G6). Optional `[web] token` — when set, the UI login screen
  requires it and API calls carry it as a bearer token.
- **Secrets discipline**: the web UI is *write-only* for secret values —
  masked everywhere, never returned by the API after write. Decryption
  happens only inside suzerain's process (as today), using the operator's
  age key; values are never persisted by the browser.

## 3. Architecture

```
┌──────────────────────────── suzerain process ───────────────────────────┐
│  axum web server (127.0.0.1:8484)                                       │
│  ├── /              embedded SPA (no build step)                        │
│  ├── /api/*         REST/JSON over Store + ControlPlane directly        │
│  └── /api/agents/*/session (SSE)   attach-stream relay                  │
│  Store (sqlite/pg)  ControlPlane (iroh)  secrets (age)                  │
└─────────────────────────────────────────────────────────────────────────┘
```

Key decisions:

1. **Embedded, not separate.** The web server lives inside `suzerain` and
   calls `Store`/`ControlPlane`/secrets functions directly — no socket hop,
   no duplicate auth story, single binary, same lifecycle as the control
   plane. The existing unix-socket API stays (CLI uses it); the HTTP API
   shares most of its logic via thin handlers.
2. **No-build-step SPA.** Vanilla JS (ES modules) + a small hand-rolled
   CSS. One `index.html`, a handful of `.js`/`.css` files embedded in the
   binary. No npm/vite/toolchain in the repo for the frontend. Rationale:
   this repo has zero frontend infra; a build step would dominate the
   project's tooling for marginal benefit at this UI's size. (If the UI
   grows past ~15 views, revisit with a real framework.)
3. **SSE for live data.** Session streaming uses Server-Sent Events
   (history events, then live events). List views poll every 5s (simple,
   cache-friendly); a global SSE channel for fleet events is a v1.1 option.
4. **REST/JSON API** (see §6). Versioned prefix `/api/v1` so the CLI can
   later migrate to it.

### Backend deltas required

| # | Change | Size |
|---|---|---|
| B1 | `axum` server module in `suzerain` (routes, static embedding, SSE) | medium |
| B2 | **Pending enrollments**: record rejected (unapproved) daemon registrations in a `pending_daemons` table (endpoint_id, hostname, first/last seen) so the UI can offer one-click approve | small |
| B3 | **Secrets write path**: decrypt→modify→re-encrypt the SOPS store via the `sops` CLI (or age-native), reload in-memory, audit every change. Masked list API | medium |
| B4 | Session SSE relay: history from central log → live attach stream → browser, with prompt endpoint | medium |
| B5 | Agent action endpoints (start/stop/suspend/restore/destroy) as thin wrappers over the existing order flow | small |
| B6 | Global activity feed endpoint (audit + fleet events) | small |

## 4. Views (the requested list + two freebies)

Global shell: left nav (Fleet, Castellans, Agents, Secrets, Activity), top
bar (suzerain EndpointId short, daemon/agent counts, "local" badge). Toasts
for actions, confirm dialogs for destructive ones.

### 4.1 Fleet dashboard (freebie)
Counts: daemons online/approved, agents by state, total allocated
vcpu/mem/VRAM per daemon (bars). Recent activity strip (last 10 audit
entries). Purpose: 5-second health read.

### 4.2 Castellans list
Table: endpoint (short id + copy), hostname, status (online/offline,
approved), labels (effective, chips), capacity vs free (vcpu, mem, VRAM,
disk as small bar meters), agent count, last seen. Row click → 4.3.
"Add castellan" button → 4.6.

### 4.3 Castellan details
- Header: endpoint id (full, copyable), hostname, os/arch, online since,
  heartbeat freshness.
- Capacity/usage cards: vcpu total/free, mem total/free, disk total/free,
  GPU list (kind, name, VRAM total/free — incl. Apple unified semantics).
- **Labels editor**: chips with add/remove (operator overrides; reported
  labels shown read-only, overrides visually distinguished).
- **Its agents** (same table as 4.4, pre-filtered).
- **Activity**: audit entries mentioning this daemon (approvals, orders,
  state reports); log-level events for its agents (collapsible per-agent
  journal tails from the central store).

### 4.4 Agents list
Table: name, status badge (running/idle/sleeping/waking/failed),
daemon (short id + hostname), model (provider/id), resources (vcpu/mem/gpu
summary), last activity (from central log), actions (chat, destroy) inline.
Start/stop/suspend/restore are not user actions: idle agents suspend
automatically and wake when a message arrives.
Filter by state/daemon, search by name.
"Create agent" button → 4.5.

### 4.5 Create agent
Two-pane form:
- **Left — structured form**: name, harness version (default pinned),
  provider + model (dropdowns fed from pi's catalog where possible),
  thinking level, repos (repeatable URL+ref), extensions (repeatable
  URL+ref), resources (vcpu, memory MiB, disk MiB, gpu count + VRAM MiB),
  schedule (require k=v rows, daemon pin), secrets.providers (multi-select
  from configured providers — never shows values), OTEL endpoint.
- **Right — live TOML preview**, fully editable; the two panes sync.
- Validation client-side (name pattern, URLs, integers) + server-side
  errors surfaced with scheduler rejection details (the per-candidate
  reasons from G8 work).
- On success → agent details (4.7) with live provisioning progress.

### 4.6 Add castellans
- **Instructions panel**: exact commands to run on the new machine
  (install mise/qemu, `castellan init --suzerain <this-id>`, copy-ready
  with this suzerain's EndpointId + relay/discovery notes).
- **Pending enrollments** (B2): daemons that registered but aren't
  approved — hostname, endpoint id, os/arch, capacity summary, first/last
  seen — each with Approve / Dismiss buttons.
- **Manual approve**: paste an EndpointId (for daemons that haven't
  connected yet).

### 4.7 Agent details
- Header: name, status badge, daemon link, created, session file.
- Lifecycle: destroy only (confirm + type-name). Auto-suspend policy
  editor (per-agent override: duration, "never", or "default" to inherit).
- Tabs:
  - **Session** (link out to 4.8 or embedded chat preview + "open session").
  - **Manifest** (pretty TOML, read-only in v1).
  - **Logs**: central event log with kind filter, search, auto-scroll,
    seq-range paging; redaction applied (already server-side).
  - **Resources**: manifest requests + daemon free-at-placement snapshot.

### 4.8 Agent session (the flagship view)
Chat interface over the SSE relay (B4):
- History reconstructed from the central log (user/assistant bubbles; tool
  calls as collapsible blocks: tool name, args summary, result summary).
- Live streaming: assistant text deltas, thinking blocks (collapsed),
  tool execution start/end indicators, turn separators.
- Prompt box: multiline, Enter=send, Shift+Enter=newline, "steer" toggle
  for mid-run messages, abort button while streaming.
- Status line: agent status (running/idle/sleeping/waking), current model,
  turn state (idle/streaming).
- Session continues across automatic suspend/wake cycles (history is
  central-log-driven; sending to a sleeping agent queues the message and
  wakes it, with progress narrated as system lines).

### 4.9 Secrets
- **Providers** (pi provider ids): table of configured keys (masked
  `sk-…•••`), add provider (dropdown + value field, write-only), edit
  (replace value), delete (confirm).
- **Git SSH key**: presence indicator; upload/paste new key (textarea,
  write-only), delete.
- **Extra named secrets**: name + optional `@host` scope, add/edit/delete.
- Every mutation: audit entry + toast; store reloaded atomically
  (decrypt→modify→encrypt via native age; failure rolls back).
- "Used by" hint per provider (count of agents whose manifest declares it).

### 4.10 Activity (freebie)
Global audit feed: filterable by action (approve/create/start/…), actor,
daemon/agent. The machine-readable narrative of everything the control
plane did — the same `audit.jsonl` the CLI shows, but browsable.

## 5. Error & empty states

- Daemon offline while its agents show Active → banner on agent details
  ("daemon unreachable; last seen X").
- Scheduler rejection on create → the per-candidate reason list rendered
  as an error panel (reuses G8 error UX).
- Empty fleet → the dashboard becomes the "Add castellan" guide.
- Secrets store missing → Secrets page shows the setup instructions
  (age keygen + first file) instead of the editor.

## 6. API surface (`/api/v1`, JSON)

```
GET  /api/v1/overview                     dashboard aggregates
GET  /api/v1/daemons                      list (row + capacity + usage + effective labels)
GET  /api/v1/daemons/{id}                 details (+ its agents + audit slice)
POST /api/v1/daemons/approve              {endpoint_id}
GET  /api/v1/daemons/pending              unapproved registration attempts (B2)
POST /api/v1/daemons/pending/{id}/dismiss
POST /api/v1/daemons/{id}/labels          {set: {k:v}, remove: [k]}
GET  /api/v1/agents                       list (join daemon hostname; public status + idle secs)
POST /api/v1/agents                       {manifest_toml} → create; scheduler errors 409
GET  /api/v1/agents/{name}                details
PATCH /api/v1/agents/{name}               {auto_suspend: "10m"|"never"|"default"}
POST /api/v1/agents/{name}/destroy        {force?} — the only lifecycle action
GET  /api/v1/agents/{name}/logs?kind=&q=&tail=
GET  /providers.json                    pi provider→model catalog (generated: tools/gen-providers.mjs)
GET  /api/v1/agents/{name}/session        SSE: history events, then live (B4)
POST /api/v1/agents/{name}/prompt         {message, mode: prompt|steer|follow_up}
GET  /api/v1/secrets                      masked inventory (names, kinds, used-by counts)
PUT  /api/v1/secrets/providers/{id}       {value} (write-only; 204)
DELETE /api/v1/secrets/providers/{id}
PUT  /api/v1/secrets/git-ssh-key          {value}
DELETE /api/v1/secrets/git-ssh-key
PUT  /api/v1/secrets/extra/{name}         {value, hosts: []}
DELETE /api/v1/secrets/extra/{name}
GET  /api/v1/audit?action=&tail=
```

Conventions: 409 for scheduler/state conflicts, 422 for validation,
`{error, details?}` body. All mutating endpoints audit.

Destroy works from any state: a daemon-side "no agent" rejection is
treated as success, and `{force: true}` removes the registry row even when
the daemon is unreachable (the VM may keep running orphaned; forcing is
audit-logged). Chat is always available: agents list rows and the detail
header link into the session view (4.8) regardless of status, and sending
to a sleeping agent wakes it transparently.

The secrets provider dropdown and the create-agent provider/model
dropdowns are fed from `web/providers.json`, a checked-in snapshot of the
installed pi package's provider catalog (regenerate after upgrading pi:
`node tools/gen-providers.mjs`). Create-agent only offers providers with
a configured key in the secrets store. Harness + harness version are
dropdowns driven by the `HARNESSES` map in `web/app.js` (add entries
there as castellan learns new harnesses/versions). Key injection covers
every pi API-key provider via `provider_env_and_host`
(crates/protocol/src/secrets.rs).

## 7. Milestones

| Phase | Content | Exit criteria |
|---|---|---|
| **M1** | B1 server + shell + read-only lists (castellans, agents, daemon details, agent details incl. logs, activity) | browse the whole fleet read-only |
| **M2** | Actions: lifecycle buttons, create agent (form+TOML), daemon label editor | full CLI parity except secrets/session |
| **M3** | Agent session (SSE chat, history, streaming, steer/abort) | chat with an agent end-to-end |
| **M4** | Secrets CRUD (B3) + pending enrollments (B2) + add-castellan view | full spec |

M1+M2 are ~70% of daily use; M3/M4 follow independently.

## 8. Resolved decisions (2026-08-11)

1. **Port/config**: 8484, `[web] enabled = true` by default. ✅
2. **Vanilla no-build-step SPA.** ✅
3. **No `[web] token`** in v1; add when remote access becomes real.
4. **Audited "reveal once" button**: a `POST /api/v1/secrets/reveal`
   endpoint returns a value in the response once, writes a dedicated
   `secret_reveal` audit entry (name + actor, never the value), and the
   UI shows it in a dismissible dialog (no storage, no clipboard assist).
5. **Manifests read-only** after create; recreate to change. ✅
6. **Full transcript reconstruction** in the session view: user/assistant
   bubbles, thinking blocks (collapsed), tool-call blocks with args +
   result summaries. ✅
