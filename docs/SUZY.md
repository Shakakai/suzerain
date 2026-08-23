# Suzy — Suzerain Desktop UI — Analysis & Build Plan

**Status: v1 decisions RESOLVED; M0–M4 built (see §7). Suzy v1 is feature-complete per this plan.**

## Resolved decisions (2026-08-12)

1. **No approval/blocked state** — herdr's "blocked" state is dropped;
   agents run autonomously. (G2 closed)
2. **A workspace = a connection to one suzerain control plane over iroh.**
   The workspace stores the suzerain's EndpointId, which is simultaneously
   its address (reachable anywhere iroh reaches — N0 relays + NAT
   holepunching) and its verified identity. Multiple workspaces = multiple
   control planes, local or planetary. (G1 closed)
3. **Remote access: YES, via the iroh operator channel** (decision
   REVERSED 2026-08-12). Suzy never speaks HTTP to the control plane; it
   dials `suz/operator/0` by EndpointId. Authorization is Suzy's own iroh
   public key against the control plane's `[operator] allow` list in
   `$SUZERAIN_HOME/suzerain.toml`. (G5 → built; see §6.4)
4. **Global event stream approved** — `GET /api/v1/events` SSE built
   (G6 done; see §6.1).
5. **Terminal into the microVM approved** — every agent gets a shell tab
   alongside the chat tab (G4; design in §6.2, build is a later milestone).
6. **No "done" state** — use `idle`. (G3 closed)
7. **Agent rename** is allowed via restart: shut the pi client down, update
   the system prompt, restart the pi agent with a new session. A
   control-plane `rename` endpoint implementing that flow is a future
   backend delta (not built). (G8)
8. **No non-agent panes.** (G9 closed)
9. **Framework: egui/eframe.** (§4.1)

Suzy is a Rust desktop GUI for the suzerain control plane, loosely modeled
on [herdr](https://github.com/herdrdev/herdr). This document:

1. Summarizes what herdr is and does.
2. Maps herdr's UI concepts onto suzerain's existing features and API.
3. Lists the gaps **on both sides** that need a product decision.
4. Lays out the build plan (crate layout, framework, milestones).

---

## 1. What herdr actually is

herdr is a **terminal multiplexer rebuilt around coding agents** — a TUI
(ratatui, single ~10MB Rust binary), *not* a desktop GUI. Key facts:

- **Layout model**: workspaces → tabs → panes. Panes are real terminals
  running arbitrary processes; agents (Claude Code, Codex, etc.) are just
  processes herdr **auto-detects** via screen-scraping or lifecycle hooks.
- **Agent state sidebar**: per-pane state (blocked / working / done / idle,
  color-coded) rolled up to tabs and workspaces. This is the headline
  feature — the state is *inferred*, not declared.
- **Client-server**: sessions live on a background server; the TUI detaches
  and reattaches, including over SSH (`--remote`, `--handoff`).
- **Scriptable control plane**: CLI + local socket API + an agent skill, so
  agents can spawn/manage panes and other agents.
- **Agents are anonymous**: there is no agent *definition*. You open a pane,
  type `claude`, and herdr notices. Configuration is per-launch, in the
  pane's shell environment.

**The fundamental difference for Suzy** (the one called out for this
project): in suzerain, agents are **declared** (a TOML manifest: name,
harness+version, provider+model, resources, placement, repos, extensions,
secrets scopes, egress, lifecycle policy), **scheduled** onto a daemon by
the control plane, and run as **pi in RPC mode inside a Gondolin microVM** —
not detected in a terminal. The UI's "create" flow is a structured form,
not "open a pane and run a command."

---

## 2. Concept mapping: herdr UI → suzerain

| herdr concept | suzerain equivalent | Suzy treatment |
|---|---|---|
| Workspace | *none* (flat agent registry) | **Gap G1** — group by daemon, or add a grouping concept |
| Tab | **Agent** (the natural unit) | One tab per open agent |
| Pane (terminal) | **Agent session** (pi RPC event stream) | Chat transcript view (like `web/` 4.8), not a terminal. **Gap G4** (real shell) |
| Sidebar state: blocked/working/done/idle | **Public status**: running / idle / sleeping / waking / failed (authoritative, daemon-reported `busy` flag — ground truth, not screen inference) | Sidebar dots per agent, rolled up per daemon/group. Palette: running=accent, idle=green, sleeping=blue/gray, waking=yellow pulse, failed=red. herdr's "blocked" and "done" have no direct analog — **Gap G2 / G3** |
| Server + detach/reattach | The whole suzerain architecture (control plane + daemons are always-on; agents persist in sqlite/pg and even auto-suspend to disk) | Free. Suzy is a thin client; closing it touches nothing |
| `--remote` over SSH | Multi-server is native (iroh/QUIC) and so is the operator channel since §6.4: Suzy dials `suz/operator/0` by EndpointId from anywhere | Built (G5 reversed) |
| Socket API / CLI / agent skill | Unix-socket JSONL API (`suz`), HTTP REST+SSE (`/api/v1` on :8484), and `suzerain-mcp` (MCP server) | Suzy consumes the **HTTP API** (it's the richest and already drives both the web UI and MCP) |
| Agent auto-detection | Not needed — agents are declared and registered | Replaced by the create-agent wizard |
| Worktree-per-ticket pattern (emergent, on top of herdr) | Manifest `[[repos]]` cloned fresh into the agent's VM; isolation is a whole microVM (stronger than a worktree) | Surface repos in the create form; no worktree UI needed |
| System notifications on state change | Nothing pushed today (web UI polls; per-session SSE only) | **Gap G6** — needs a global event stream; then `notify-rust` |
| config.toml, themes, keybindings | `$SUZERAIN_HOME/suzerain.toml` is control-plane config, not UI | Suzy gets its own `~/.config/suzy/config.toml` |
| Single static binary | Repo already ships binaries via `ops/install.sh` + mise | Add `suzy` component to package/release tasks |

### API surface Suzy consumes (all exists today)

Everything below is live in `crates/suzerain/src/web.rs` + `web_session.rs`:

- **Fleet**: `GET /api/v1/overview`, `/api/v1/daemons` (+details, labels,
  pending enrollments, approve/dismiss/remove), `/api/v1/audit`.
- **Agents**: `GET/POST /api/v1/agents`, `GET/PATCH .../{name}`,
  `POST .../{name}/{action}` (destroy), `.../logs`, `.../session_state`.
- **Session (flagship)**: `GET .../session` (SSE: history → live),
  `GET .../session/history` (reconstructed transcript incl. errored
  turns), `POST .../prompt` (`prompt` / `steer` / `follow_up` / `abort`).
- **Catalogs for the create form**: `/providers.json`,
  `/api/v1/providers` (annotated: key_configured, key_injectable),
  `/api/v1/harnesses`, `/api/v1/pi-packages` (pi.dev search proxy).
- **Secrets**: masked inventory + write-only CRUD + audited reveal-once.

Design consequence: **Suzy needs zero new read APIs for v1.** The only
backend delta worth considering is a push channel (G6).

---

## 3. Gaps requiring a decision

### 3.A Missing on the *suzerain* side (herdr features with no backing)

| # | Feature | What herdr does | Suzerain today | Decision needed |
|---|---|---|---|---|
| **G1** | **Workspaces / agent grouping** | Workspaces group tabs; state rolls up | Flat agent registry; only grouping axis is the daemon an agent runs on | (a) group by daemon only (free, honest); (b) UI-local grouping in Suzy config (no server change, doesn't sync); (c) add `group`/`project` to manifest + registry column (server change, benefits web UI + MCP too) |
| **G2** | **"Blocked / waiting-for-input" state** | Detects when an agent waits for tool approval (herdr's signature red state) | No such concept: pi in the guest runs autonomously; state vocabulary is running/idle/sleeping/waking/failed | Does pi RPC mode emit permission/approval requests we could surface as a state + approve/deny over attach? If yes: new protocol event + status. If no: drop "blocked"; document the difference |
| **G3** | **"Done" state** | Distinguishes finished-turn from never-started | `busy` flag gives running vs idle only | Cheap client-side: derive "just finished" from session events / `idle_secs` transition and flash the dot. Or add `last_turn_outcome` to the agent row. Recommend client-side |
| **G4** | **Real terminal into the agent environment** | Every pane is a shell; you can type around the agent | No exec/shell into Gondolin guests; WEB-UI.md explicitly lists "web terminal into guest VMs" as a non-goal | In scope for Suzy? It's a castellan+driver feature (pty stream into the VM), not a UI feature. Recommend: out of scope for v1, revisit as its own phase |
| **G5** | **Remote operator access** | `herdr --remote host` over SSH | Operator surfaces are localhost-only by design (unix socket uid-checked; web on 127.0.0.1) | (a) SSH tunnel / `ssh -L 8484` — zero code, recommend documenting for v1; (b) `[web] token` + configurable bind (spec already reserves this); (c) **native iroh operator channel** — Suzy is Rust and could be an iroh peer with its own keypair, the most suzerain-idiomatic but a real control-plane feature |
| **G6** | **Global event push** | Socket API event subscriptions drive the live sidebar | Web UI polls every 5s; SSE exists per-session only. WEB-UI.md lists "global SSE channel for fleet events" as a v1.1 option | Add `GET /api/v1/events` (SSE: agent status transitions, daemon online/offline, pending enrollments, audit). Small backend delta, big UX win; also fixes notifications (G7) and removes poll lag |
| **G7** | **Desktop notifications** | Fires on agent state change | Nothing | Trivial once G6 exists (`notify-rust`). Decide which transitions notify (failed and wake-complete are the obvious two) |
| **G8** | **Rename / reconfigure agents** | Rename workspaces/tabs/panes freely | Manifests are immutable post-create (WEB-UI.md resolved decision #5); only `auto_suspend` is mutable | Keep immutability (recreate to change) or allow display-name override? Recommend keeping; Suzy mirrors the web UI |
| **G9** | **Non-agent panes** (run any command alongside agents) | Core to a multiplexer | No such thing in the model | Recommend explicitly out of scope — Suzy is an operator console, not a multiplexer |
| **G10** | **Multi-attach / watch same session from several clients** | Multiple clients attach fine | Already fine: castellan attach is `tokio::broadcast`-based; SSE relay per connection | No decision — verify and use |

### 3.B Missing on the *herdr* side (suzerain features with no herdr analog)

These aren't blockers — they're extra surface area Suzy must find a home
for that herdr's UI never needed. Decision = how much makes v1.

| # | Suzerain feature | UI implication for Suzy |
|---|---|---|
| **S1** | Declarative manifests (harness pin, provider/model, thinking, toolchain, repos, extensions, prompt.append_system, egress, OTEL, lifecycle) | The create-agent **wizard** is Suzy's biggest new UI vs herdr. Port the web UI's two-pane form+live-TOML design. Feeds: `/providers`, `/harnesses`, `/pi-packages` |
| **S2** | Scheduling & placement (labels, require k=v, daemon pin; capacity/usage per node) | Castellans view + placement section in the wizard; scheduler rejection reasons must render (per-candidate list) |
| **S3** | Daemon enrollment & approval (pending enrollments, approve/dismiss, labels editor) | "Add castellan" view with copy-ready `castellan init` instructions — herdr has no concept of adding a machine |
| **S4** | Auto-suspend / transparent wake (sleeping status, wake narration, per-agent policy) | First-class in the sidebar (sleeping is a *normal* state, not offline) and in chat ("waking…" system lines — SSE already narrates). Policy editor on agent details |
| **S5** | Secrets store (age-encrypted, write-only, masked, audited reveal-once, per-agent scoping) | Secrets view (port of web 4.9). Decide: include in v1 or defer to v1.1 (CLI/web exist) |
| **S6** | Central event log + audit trail | Logs tab per agent (kind filter, search, paging) + global Activity view |
| **S7** | Resource model (vcpu/mem/disk/GPU requests vs node capacity) | Capacity bars on dashboard/castellan views; resources summary per agent |
| **S8** | MicroVM isolation, bundles, cross-daemon restore | Mostly invisible (good); restore shows up as "waking" narration |
| **S9** | MCP control plane (`suzerain-mcp`) | None directly — but worth a "connect your local agent to the fleet" docs hook; analogous to herdr's agent skill |

---

## 4. Build plan

### 4.1 Framework decision (the big one)

herdr is a TUI; Suzy was asked for as a **desktop GUI**. Options:

| Option | Pros | Cons |
|---|---|---|
| **egui/eframe** ✅ recommended | Pure Rust, single binary, immediate-mode fits ops-console UIs, trivial async bridging (tokio + channels), mature ecosystem (`egui_dock` for herdr-style dockable panes/tabs, `egui_notify`), matches repo's zero-frontend-toolchain ethos | Not pretty by default; custom widgets for chat bubbles |
| iced | Elm architecture, cleaner state model | More boilerplate; SSE/streaming integration is more work |
| gpui (Zed's) | GPU-fast, beautiful, built for exactly this kind of app | Young as a standalone framework, Linux/Windows immature, steeper learning curve |
| Tauri + reuse `web/` SPA | Zero new frontend work; the SPA already implements every view | Not a "Rust GUI" — the UI stays JS; adds a webview dependency the repo deliberately avoided |
| ratatui TUI | Maximum herdr fidelity, could reuse crossterm patterns | That's a *different* app than requested; the web UI already covers browsers, a TUI would overlap `suz attach` |

**Recommendation: egui/eframe + `egui_dock`.** v1 target: macOS arm64 +
Linux x86_64 (matches existing release matrix).

### 4.2 Crate layout

```
crates/
  suzerain-client/   NEW — shared async Rust client for /api/v1
                     (extracted from crates/suzerain-mcp/src/client.rs,
                      grown with typed models + SSE stream helpers)
  suzy/              NEW — the desktop app (eframe)
```

- `suzerain-client`: `reqwest` (rustls, already a workspace dep) +
  `eventsource`-style SSE over `reqwest` byte streams; typed wrappers for
  every endpoint in §2; `SessionStream` yields `History | Live | Notice`.
  Benefits `suzerain-mcp` (dedup) and any future Rust tooling.
- `suzy`: eframe app, owns a tokio runtime on a background thread;
  UI↔async via `std::sync::mpsc` + `egui::Context::request_repaint` on
  push events. No shared mutable state across the boundary — messages only.

### 4.3 UI structure (herdr mapped onto suzerain)

```
┌──────────────────────────────────────────────────────────────┐
│ top bar: suzerain id • fleet counts • connection status      │
├────────────┬─────────────────────────────────────────────────┤
│ SIDEBAR    │  MAIN (egui_dock)                               │
│            │                                                 │
│ ▾ fleet    │  tabs: [researcher-1] [auditor-1] [dashboard]   │
│  ▾ daemon  │ ┌─────────────────────────────────────────────┐ │
│   ● agt-1  │ │ per-agent tab:                              │ │
│   ◌ agt-2  │ │  Session │ Logs │ Manifest │ Resources      │ │
│  ▾ daemon2 │ │  (chat: history + live stream, prompt box,  │ │
│   ● agt-3  │ │   steer toggle, abort, wake narration)      │ │
│ + create   │ └─────────────────────────────────────────────┘ │
│ ⚙ castellans│                                                │
│ 🔑 secrets │  dashboard / castellans / secrets / activity    │
│ ≣ activity │  open as dockable views too                     │
└────────────┴─────────────────────────────────────────────────┘
```

- **Sidebar** = herdr's signature element: agent rows with status dots
  (palette in §2), grouped by daemon (v1; G1 decides more), rolled-up
  worst-of status per group. Driven by G6's event stream (or 5s poll
  until then).
- **Session view** = the flagship (port of web 4.8): user/assistant
  bubbles, collapsible tool-call and thinking blocks, session-era
  boundaries, status line (running/idle/sleeping/waking + model),
  prompt box (Enter send / Shift+Enter newline / steer / abort),
  "agent is sleeping — waking…" narration from SSE notices.
- **Create wizard** = the defining difference from herdr: structured form
  synced with an editable TOML preview, provider/model dropdowns limited
  to configured+injectable keys, harness versions, pi.dev package search
  for extensions, placement (labels/pin), scheduler rejection rendering.
- **Castellans / Add castellan / Secrets / Activity**: ports of web views
  4.2/4.3/4.6/4.9/4.10 (S3, S5, S6).
- **Notifications** (G7): `notify-rust` on `failed` and on wake-complete
  when the window is unfocused.
- **Config**: `~/.config/suzy/config.toml` — workspaces (name +
  suzerain EndpointId), theme. Suzy's iroh identity persists at
  `~/.config/suzy/iroh.key`.

### 4.4 Milestones

| Phase | Content | Exit criteria |
|---|---|---|
| **M0** | `suzerain-client` crate (typed API + SSE) extracted & adopted by `suzerain-mcp`; `suzy` skeleton: connect, sidebar list, dashboard (polling) | Browse fleet read-only from a native window |
| **M1** | Agent tabs + **session chat**: history, SSE live stream, prompt/steer/follow_up/abort, wake narration, status line | Chat end-to-end with running *and* sleeping agents |
| **M2** | Create-agent wizard (form ⇄ TOML, catalogs, scheduler errors); destroy with confirm; castellans view + approve/dismiss pending + labels editor | Full `suz` parity except secrets |
| **M3** | Backend **G6** `/api/v1/events` SSE + Suzy switch to push; desktop notifications; logs view; activity view | Sidebar is live; no polling |
| **M4** | Secrets CRUD (write-only + reveal-once dialog); theming/keybindings; `egui_dock` layouts (split sessions side-by-side — herdr's pane feel); packaging: `ops/install.sh` component, release matrix, `mise run package` | v1 ship |

M0–M2 need **zero** suzerain backend changes. G6 lands in M3 and is the
only planned backend delta.

### 4.5 Decisions checklist — RESOLVED (see top of document)

## 6. Backend deltas

### 6.1 Global fleet event stream (built)

`GET /api/v1/events` (SSE) serves a process-wide broadcast of lightweight
change hints emitted at the store/audit choke points
(`crates/suzerain/src/events.rs`): `agent_state`, `agent_activity`,
`agent` (created/removed), `daemon`, `pending_daemon`, `audit`, and
`resync` (receiver lagged — refetch everything). Payloads are advisory;
clients refetch the affected lists on receipt. Suzy's workspace loop
subscribes and refetches on any event, with a 15s fallback poll and
automatic resubscribe.

### 6.2 Terminal into the microVM (built — see §7 M4)

Spike finding: Gondolin's `vm.exec` supports long-running streaming
processes **and native ptys** (`ExecOptions.pty`, `ExecProcess.resize`) —
so the terminal is a relay feature with real pty semantics (job control,
colors, Ctrl-C), no `script(1)` shim required. Pipeline: driver
`shell_*` commands → castellan `StreamHello::Shell` JSONL relay (base64
frames) → suzerain WebSocket (`/api/v1/agents/{name}/shell`, transparent
wake) → `suzerain-client` `ShellConn` → suzy's `alacritty_terminal`
widget.

### 6.3 Agent rename (approved, not built)

`POST /api/v1/agents/{name}/rename {new_name}`: graceful pi shutdown →
registry rename (+ optional system-prompt update) → fresh pi session.
Surfaced in Suzy as an edit affordance on the agent header.

### 6.4 iroh operator channel (built — G5 reversed)

`suz/operator/0` on the control plane's existing iroh endpoint
(`crates/suzerain/src/operator.rs`). One connection per workspace;
multiplexed bi-streams, one op per stream (`OperatorHello`):

- `rest {method, path, body}` — executed **in-process against the same
  axum router** the HTTP API serves (`tower::ServiceExt::oneshot`), so
  there is exactly one implementation of every endpoint; replies with a
  `Reply {status, body}` frame (non-2xx maps to the client's
  `Error::Http`, preserving scheduler-rejection UX).
- `stream {path}` — SSE responses relayed as base64 `Chunk` frames; the
  client reassembles them with the same SSE block parser.
- `shell {name}` — native pty relay (`ShellMessage` frames both ways),
  sharing `dial_agent_shell` (wake narration + reload + daemon stream)
  with the WebSocket relay.

**AuthZ**: the connecting EndpointId must be in `[operator] allow` in
`$SUZERAIN_HOME/suzerain.toml`; rejections are logged with the caller's id.
Empty list = reject everyone (with a startup hint). Suzy shows its own
operator id (persisted at `~/.config/suzy/iroh.key`) in the add-workspace
dialog with a copy-ready config snippet.

**Client**: `suzerain-client` is iroh-only (the reqwest/tungstenite HTTP
code was removed): lazy endpoint bind, cached connection with redial-once,
boxed `Stream`s for session/events, `ShellConn` for the pty.
`Client::new(endpoint_id, key)` (N0 discovery) for production;
`Client::with_addr(addr, key)` for tests/LAN.

## 7. What is built (M0–M4, 2026-08-12)

- `crates/suzerain-client` — typed async client for `/api/v1`: fleet,
  agents, sessions, catalogs, secrets; SSE parsing for both streams
  (unit-tested).
- `crates/suzerain` — `GET /api/v1/events` global fleet stream (§6.1).
- `crates/suzy` — egui/eframe desktop app:
  - **Workspaces**: add/connect dialog (suzerain EndpointId + your
    allowlisted operator key, §6.4); profiles persisted at
    `~/.config/suzy/config.toml`; transport security is the iroh
    handshake itself.
  - **Sidebar**: agents grouped by daemon with live status dots
    (running=gold, idle=green, sleeping=blue, waking=orange, failed=red),
    driven by the fleet event stream; needs-attention markers.
  - **Dashboard**: fleet stat cards + castellan table (host, status,
    capacity, usage, agent count).
  - **Chat tab per agent** (the flagship): full transcript reconstruction
    (user/assistant bubbles, collapsible thinking/tool-call/tool-result
    blocks, session-era boundaries, crash system lines), live SSE
    streaming, prompt box (Enter=send, Shift+Enter=newline), Steer and
    Abort, sleeping-agent wake narration, reconnect on stream close.
  - **Create agent**: TOML manifest editor with template (scheduler
    rejection reasons surface in the status bar); destroy with confirm.

Run it: `cargo run -p suzy`; add a workspace with the suzerain's
EndpointId (`suz id`) after allowlisting Suzy's operator key (§6.4).

### M2 (built 2026-08-12)

- **Structured create-agent form** (`crates/suzy/src/create.rs`): two-pane
  form ⇄ TOML — provider dropdown limited to key_configured +
  key_injectable providers, catalog-driven model/harness-version
  dropdowns, thinking level, resources, repos, pi-package extensions,
  secrets multi-select, system-prompt addition, placement require-labels
  + daemon pin, auto-suspend, egress, OTEL. TOML pane is hand-editable
  ("apply TOML → form" / "regenerate from form"); scheduler rejections
  surface in the status bar.
- **Castellans view** (`crates/suzy/src/views.rs`): pending enrollments
  with Approve/Dismiss, per-daemon details (capacity/usage, labels with
  overrides distinguished), labels editor (k=v set / `-k` remove /
  chip-click removal), remove daemon, and a copy-ready add-castellan
  command panel using the workspace's pinned EndpointId.
- **Agent tabs**: Chat | Logs | Details. Logs: kind filter, substring
  search, tail, auto-refresh while open (piggybacks fleet events).
  Details: session eras, read-only pretty manifest, auto-suspend policy
  editor, destroy with confirm.
- Client: `approve_pending`, `dismiss_pending`, `remove_daemon`,
  `agent_logs_query`, daemon `reported_labels`/`label_overrides`.

### M3 (built 2026-08-12)

- **Desktop notifications** (`notify-rust`): fired on the snapshot diff in
  the workspace loop — agent entering `failed`, wake completing
  (`waking` → `idle`/`running`), and `needs_attention` flipping on.
  Gated on window focus (G7): a focused operator already sees the sidebar.
  Note for M4 packaging: macOS delivers notifications reliably only from
  a signed .app bundle; from a bare binary `show()` may no-op (debug-logged).
- **Activity view**: global audit feed per workspace ("≣ Activity" in the
  sidebar) — action filter, substring search, color-coded actions
  (creates/approvals green, destroys/removes red, secrets purple),
  auto-refresh piggybacked on fleet events (every mutation emits an
  `audit` event, so the feed is effectively live).

### M4 (built 2026-08-12)

- **VM terminal** (§6.2, the full pipeline):
  - `tools/gondolin-driver`: `shell_spawn`/`shell_stdin`/`shell_resize`/
    `shell_close` commands using Gondolin's native **pty support**
    (`ExecOptions.pty`, `ExecProcess.resize`) — discovered during the
    spike, no `script(1)` shim needed. Multiple concurrent shells per VM.
  - protocol: `StreamHello::Shell` + `ShellMessage` (base64 Data frames,
    Resize, Exit, Notice) — the stream stays JSONL like every channel.
  - castellan: driver shell methods + `handle_shell` relay on inbound
    streams (per-connection shell ids, agent workspace cwd, touch on
    activity so a shell keeps the agent awake).
  - suzerain: `GET /api/v1/agents/{name}/shell` WebSocket relay
    (axum `ws`) with transparent wake narration, same pattern as prompt.
  - client: `shell_connect` (tokio-tungstenite) + `ShellConn`
    (send_input/resize/next) + base64 helpers (roundtrip-tested).
  - suzy: **⌨ Shell tab** per agent — a real terminal widget
    (`alacritty_terminal` VT grid rendered in egui, `crates/suzy/src/
    terminal.rs`): 16/256/RGB colors, bold/dim/inverse, cursor, resize
    propagation, keyboard incl. Ctrl combos/paste/arrows. Shells persist
    across tab switches (detach/reattach like herdr); reconnect button on
    exit/close.
- **Secrets view** (S5): masked inventory (providers with used-by counts,
  SSH key presence, extra named), write-only set/delete, catalog-driven
  provider dropdown, **audited reveal-once dialog** (value shown once,
  never stored), setup instructions when the store is missing.
- **Workspace removal**: top-bar ➖ with confirm; tears down loops/streams
  and reconnects the rest (WsIds are positional → full reindex).
- **Theming**: persisted light/dark toggle (🌙/☀ in the top bar,
  `theme` in `~/.config/suzy/config.toml`).
- **Packaging**: `suzy` added to the release matrix
  (`.github/workflows/release.yml` COMPONENTS) and `ops/install.sh` as an
  opt-in component (`install.sh suzy`; not part of default "all" — it's a
  desktop app). Note: macOS notifications (M3) render reliably only from a
  signed .app bundle — a future packaging refinement.

### M4 testing system (built 2026-08-12)

Four layers, so every UI feature is exercised before "done":

1. **Mock control plane** (`crates/suzy/tests/common/mod.rs`): a real
   iroh endpoint speaking the operator protocol from canned state — `rest`
   ops against the same axum router (oneshot), `stream` ops relaying SSE
   body chunks, and the `shell` op piped to a real local `sh` process.
   No QEMU and no network discovery needed (direct-address dialing); runs
   in milliseconds.
2. **Headless UI tests** (`crates/suzy/tests/ui.rs`, 15 tests) with
   `egui_kittest` driving the actual `SuzyApp`: welcome, fleet sidebar +
   dashboard, **chat history + prompt round trip** (typed into the input,
   mock replies live over SSE), **shell tab round trip** (command injected
   through the widget channel → WS → local `sh` → output asserted in the
   terminal grid via `Terminal::screen_text()`), create-form defaults +
   submit, castellan pending-approval, secrets + audited reveal, logs/
   details (auto-suspend PATCH), activity feed, destroy with confirm,
   theme persistence, workspace removal. Plus unit tests: terminal key
   mapping, ANSI feed/erase, chat `message_end` conversion.
3. **Real-microVM shell probe** (`crates/suzy/examples/shell-probe.rs`):
   connects to a live agent's shell WS, runs `echo <marker>`, asserts the
   marker round-trips the real pipeline (VM pty → driver → castellan →
   suzerain WS → client). Wired into `ops/e2e.sh` (runs in the e2e CI job
   with KVM; skipped locally without `KIMI_API_KEY`).
4. **CI**: `cargo test --workspace` (ci.yml) runs layers 1–2 headlessly;
   fmt + clippy gates unchanged.

Testability refactors this required: suzy became a lib+bin crate (app
code in `src/lib.rs`, public state for harness inspection), config path
injection, `with_config` constructor, and — a real bug the harness caught
instantly — all background spawns now take an explicit tokio `Handle`
(calling `tokio::spawn` from the UI thread panicked with no reactor).

### Remaining ideas (post-v1)

- Agent rename endpoint (§6.3).
- Operator enrollment UX: pending-operator approvals (mirror daemon
  enrollment) instead of editing the `[operator] allow` list by hand.
- `.app` bundle + signing for macOS notifications.
- `egui_dock` split layouts (multiple sessions side-by-side).
