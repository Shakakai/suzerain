# MCP server plan — `suzerain-mcp`

An MCP server that exposes the suzerain control plane to LLM operator
assistants (Claude Code/Desktop, Cursor, etc.): manage castellans and the
full agent lifecycle over MCP tools.

Out of scope by design: **secrets management**. Provider API keys, extra
secrets, and the git deploy key are added/removed only via the service
directly (web UI / `suz` CLI / SOPS store) — never through the LLM.

Status: **implemented** — `crates/suzerain-mcp` (stdio transport, 22
tools) plus the §3 backend additions. Usage docs: `docs/MCP.md`.
Deferred: MCP resources/prompts, `--http` transport (gated on REST auth).

## 1. Architecture

```
MCP client (Claude Code, …)
   │  MCP (stdio, JSON-RPC)
   ▼
suzerain-mcp            ← new crate: crates/suzerain-mcp (binary)
   │  HTTP/JSON (localhost)
   ▼
suzerain run            ← existing control plane REST API (:8484)
   │  iroh orders
   ▼
castellan daemons
```

**Decisions:**

- **New binary crate `crates/suzerain-mcp`** (not embedded in `suzerain`):
  the MCP server is a thin adapter; keeping it a client of the REST API
  means zero new code paths in the control plane, one source of truth for
  validation/audit (the web layer already centralizes both), and the MCP
  server can run anywhere that can reach the API.
- **SDK: `rmcp`** (official `modelcontextprotocol/rust-sdk`), with
  `#[tool_router]` / `#[tool]` / `#[tool_handler]` macros, tokio-based.
  Beware the similarly named but unofficial `rust-mcp-sdk`.
- **Transport: stdio by default** (the standard for local MCP clients).
  Streamable HTTP (stateless, per the 2026-07-28 spec) as an optional
  `--http <port>` mode later — deferred until the REST API has auth, since
  today it is unauthenticated localhost-only.
- **Config:** `SUZERAIN_API_URL` (default `http://127.0.0.1:8484`).
  No credentials needed for localhost; the MCP server inherits the REST
  API's trust boundary.

**Note on scope:** this MCP server serves the *operator*, not the agents.
Agents stay MCP-free per the project philosophy (CLI tools + skills); this
is control-plane tooling and does not change the agent runtime story.

## 2. Tool catalog

Naming: `snake_case`, grouped `fleet_*` / `castellan_*` / `agent_*`, plus
discovery tools. 22 tools total.

**Discovery — everything `agent_create` needs to succeed on the first
call** (the MCP server pre-validates create params against these catalogs
and returns "did you mean …" errors before ever POSTing):

| # | Tool | Params | Backend |
|---|------|--------|---------|
| 1 | `llm_providers` | `provider?` (drill into one) | **NEW** `GET /api/v1/providers` (§3.4): every pi provider id with its models (`[{id, name}]`), annotated per provider with `key_injectable` (has an API-key env mapping — else the agent can never authenticate) and `key_configured` (store holds a key — else preflight rejects). The LLM can therefore only pick providers that will survive `catalog::validate_model` AND `secrets::preflight` |
| 2 | `harness_catalog` | — | **NEW** `GET /api/v1/harnesses` (§3.4): harness kinds + the exact versions castellan can provision (`{"pi": {"versions": ["0.84.1"]}}`). Today this is hardcoded in `web/app.js` with no API |
| 3 | `pi_packages_search` | `q?`, `type?`, `page?` | `GET /api/v1/pi-packages` (pi.dev extension catalog; feeds `extensions[].source` in `agent_create`) |
| 4 | `fleet_overview` | — | `GET /api/v1/overview` (counts by state, per-daemon capacity/free — for sizing `resources` and checking placement headroom) |
| 5 | `audit_tail` | `tail?` | `GET /api/v1/audit?tail=` (who did what; lets the assistant answer "why did X happen?") |

`castellan_list`/`castellan_get` (below) double as create-time discovery:
they expose daemon endpoint ids and effective labels for `schedule.daemon`
pins and `schedule.require` label matches.

**Castellans:**

| # | Tool | Params | Backend |
|---|------|--------|---------|
| 6 | `castellan_add` | `endpoint_id?` | No id → return enrollment instructions (control-plane EndpointId + `castellan init` command, like the web "Add castellan" page). With id → `POST /api/v1/daemons/approve` |
| 7 | `castellan_get` | `endpoint_id` | `GET /api/v1/daemons/{id}` (capacity, usage, GPUs, labels, agents, activity) |
| 8 | `castellan_list` | `include_pending?` | `GET /api/v1/daemons` + `GET /api/v1/daemons/pending` (merged, pending flagged) |
| 9 | `castellan_remove` | `endpoint_id`, `confirm`, `force?` | **NEW** `DELETE /api/v1/daemons/{id}` (§3.1) |
| 10 | `castellan_labels_set` | `endpoint_id`, `set{}`, `remove[]` | `POST /api/v1/daemons/{id}/labels` (labels drive `schedule.require` placement) |

**Agents:**

| # | Tool | Params | Backend |
|---|------|--------|---------|
| 11 | `agent_list` | `state?` filter | `GET /api/v1/agents` |
| 12 | `agent_create` | `manifest_toml` **or** structured fields (see below) | `POST /api/v1/agents` |
| 13 | `agent_get` | `name` | `GET /api/v1/agents/{name}` (state, manifest TOML, daemon, session file) |
| 14 | `agent_start` | `name` | `POST /api/v1/agents/{name}/start` |
| 15 | `agent_stop` | `name`, `force?` | `POST /api/v1/agents/{name}/stop` (`{force:true}` for unreachable daemons) |
| 16 | `agent_suspend` | `name` | `POST /api/v1/agents/{name}/suspend` (VM checkpoint + bundle upload; prerequisite for migration) |
| 17 | `agent_delete` | `name`, `confirm` | `POST /api/v1/agents/{name}/destroy` |
| 18 | `agent_restore` | `name`, `daemon_endpoint_id` | **NEW** `POST /api/v1/agents/{name}/restore` (§3.3) — move a suspended agent to another castellan (drain/rebalance) |
| 19 | `agent_logs` | `name`, `tail?` | `GET /api/v1/agents/{name}/logs?tail=` (operational journal: crashes, respawns, orders — distinct from the session transcript) |
| 20 | `agent_session_events` | `name`, `tail?`, `roles?` | **NEW** `GET /api/v1/agents/{name}/session/history` (§3.2) |
| 21 | `agent_session_send` | `name`, `message` | `POST /api/v1/agents/{name}/prompt` `{message, mode:"prompt"}` |
| 22 | `agent_session_abort` | `name` | `POST /api/v1/agents/{name}/prompt` `{mode:"abort"}` (stop a runaway turn without stopping the agent) |

**`agent_create` structured mode** (LLM-friendly; the server renders the
manifest): `name`, `provider`, `model`, `thinking?`, `harness_version?`,
`resources?{vcpu,memory_mib,disk_mib,gpu?}`, `repos?[]{url,ref}`,
`extensions?[]{source|url+ref}`, `append_system_prompt?`,
`schedule?{daemon,require}`, `egress_allow?[]`, `otel_endpoint?`.
`manifest_toml` takes precedence when both are given. Response includes the
rendered TOML so the LLM/user can see exactly what was submitted.

**Client-side pre-validation** (so creation succeeds on the first call):
before POSTing, the MCP server checks the structured input against the
discovery catalogs — provider exists and is `key_injectable` +
`key_configured`, model id is valid for the provider (with "did you mean …"
hints), harness version is in `harness_catalog`, a `schedule.daemon` pin
matches a known daemon, and `schedule.require` label keys exist on at
least one online daemon. The control plane re-validates everything anyway
(`catalog::validate_model`, `secrets::preflight` with exact `suz secrets
set …` remediation, scheduler rejections), so a skipped client-side check
degrades to a clear server error, never a wedged agent.

**Confirmation convention:** destructive tools (`agent_delete`,
`castellan_remove`) require `confirm` to exactly equal the
resource name/id — mirroring the web UI's type-the-name destroy flow. This
makes accidental LLM tool calls much harder than a bare boolean.

## 3. Backend gaps (control plane, do first)

### 3.1 `DELETE /api/v1/daemons/{id}` — castellan remove
Nothing today can remove an *approved* daemon (`store.rs` has
`delete_pending_daemon` only). Add `Store::delete_daemon(endpoint_id)` +
route + audit entry (`daemon_remove`). Semantics:
- Refuse (409) while agents are assigned to the daemon unless `?force=true`
  (force deletes their registry rows too, after a best-effort destroy
  order — same tolerance rules as `actions::lifecycle`).
- Always succeeds for offline daemons.

### 3.2 `GET /api/v1/agents/{name}/session/history` — session events as JSON
Today the transcript only exists as SSE replay in `web_session::session_sse`
(`history` events until `history_end`). Factor the reconstruction loop into
a shared function and expose a plain JSON snapshot:
`{items: [...], streaming: bool}` with `tail` (count) and `roles`
(user/assistant/tool) filters. MCP clients are request/response — an SSE
endpoint is awkward for them, and a snapshot enables pagination.

### 3.3 `POST /api/v1/agents/{name}/restore` — expose restore over REST
`agent_restore` (migrate a suspended agent's bundle to another daemon)
exists only on the operator unix socket (`api.rs`). Add a REST route
`{daemon_endpoint_id}` so `agent_restore` can be an MCP tool.

### 3.4 Create-time discovery endpoints — providers + harnesses
Two catalogs `agent_create` needs are currently unavailable or
unannotated:

- **`GET /api/v1/providers`** — new handler merging three things the
  control plane already knows: `web/providers.json` (provider ids, model
  id/name lists), `secrets::inventory()` (which providers have keys —
  names only, no values), and `provider_env_and_host()` (which providers
  can receive a key in-guest at all). Response per provider:
  `{models: [{id, name}], key_injectable: bool, key_configured: bool}`.
  Keeps `/providers.json` (raw static file) untouched for the web UI's
  existing uses.
- **`GET /api/v1/harnesses`** — today the provisionable harness/version
  list lives only as the hardcoded `HARNESSES` map in `web/app.js`.
  Move it to a checked-in `web/harnesses.json` (same pattern as
  `providers.json` + `tools/gen-providers.mjs`), serve it from the
  control plane, and refactor the web UI to fetch it. Adding a harness or
  version then becomes a one-file edit that every client sees.

## 4. Exclusions, resources, and prompts

**Explicitly excluded: all secrets operations.** No secret tools of any
kind — no add, remove, list, or reveal. Secrets (LLM provider keys, extra
secrets, the git deploy key) are managed only via the service directly
(web UI / `suz` CLI / SOPS store).

One deliberate refinement for create-time usability (§2, §3.4): the
`llm_providers` discovery tool exposes a per-provider **`key_configured`
boolean**. That is the minimum information an LLM needs to avoid picking
providers that preflight will reject — provider ids are already public
via the catalog, so the only new disclosure is "a key exists: yes/no".
Nothing else crosses the line: no extra-secret names, no usage counts, no
inventory endpoint, and never values.

`agent_create` structured mode therefore does **not** offer a
`secrets_providers` field; the rendered manifest copies the provider from
the model spec into `secrets.providers` (the existing control-plane
warning covers a mismatch), and any `extra` secret scopes must be added by
editing the TOML and using `manifest_toml` mode — still the operator's
choice to write, with no secret *values* involved.

**MCP resources** (read-only, cacheable context instead of tool round
trips): `suzerain://fleet/overview`, `suzerain://providers`,
`suzerain://agents/{name}/manifest`. Optional in v1.

**MCP prompts** (scaffolds): `create-research-agent`, `drain-castellan`
(suspend+migrate every agent off a host), `triage-failed-agent`
(logs → session events → suggested fix). These make the server genuinely
useful, not just a CRUD mirror.

## 5. Security model

- MCP server ↔ control plane: localhost REST, same trust as the web UI.
- Audit: every mutating call goes through the web layer's existing
  `audit::record`. Add `via: "mcp"` to the audit detail so operator and
  assistant actions are distinguishable in the log.
- Destructive tools require the `confirm`-matches-name convention (§2).
- No secrets surface at all: secret management happens outside the MCP
  server via the service directly (§4).
- Stdio transport only in v1 → no network attack surface beyond the
  existing localhost API. Do not add the HTTP transport until the REST API
  gains auth (bearer token at minimum).

## 6. Build order

1. **Backend gaps (§3)** — daemon delete, session-history JSON, restore
   route, and the providers/harnesses discovery endpoints, with tests.
   Independent of MCP; useful to the web UI too.
2. **Crate skeleton** — `crates/suzerain-mcp`: rmcp server, stdio
   transport, typed REST client (`SuzerainClient` over reqwest), tool
   router with the 4 read-only tools (`castellan_list`, `agent_list`,
   `agent_get`, `fleet_overview`) end to end. Validates the SDK wiring.
3. **Full tool surface (§2)** — mutations with confirm semantics,
   `via:"mcp"` audit tagging, structured `agent_create` + TOML render.
4. **MCP resources + prompts**, `--http` transport (behind REST auth).
5. **Docs** — `docs/MCP.md`: client config snippets (Claude Code
   `mcp add`, Claude Desktop JSON), tool reference, security notes;
   README section.

## 7. Open questions

- **Remote operation:** keep the MCP server localhost-only (ssh port
  forward for remote use), or invest in REST auth + streamable HTTP? v1
  assumes localhost.
- **`agent_create` structured vs TOML only:** structured mode is more work
  but far more reliable for LLMs; TOML-only is the fallback if we want to
  ship v1 faster.
- **Streaming:** MCP 2026-07-28 long-running tasks could stream
  `agent_session_send` progress; v1 returns after the prompt is accepted
  and the caller polls `agent_session_events`.
