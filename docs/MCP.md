# suzerain-mcp — MCP server for the control plane

Exposes the suzerain control plane to LLM operator assistants (Claude
Code/Desktop, Cursor, …) over the Model Context Protocol: manage castellan
daemons and the full agent lifecycle via MCP tools. Design: `docs/MCP-PLAN.md`.

Out of scope by design: **secrets management** — provider keys, extra
secrets, and the git deploy key are added/removed only via the service
directly (`suz secrets set …`, web UI, SOPS store), never through the LLM.

## Run

The server speaks MCP over **stdio** and talks to the control plane REST
API (default `http://127.0.0.1:8484`):

```sh
suzerain-mcp                                   # SUZERAIN_API_URL to override
suzerain-mcp --api-url http://127.0.0.1:8484
```

`mise run package` installs the binary to `~/.local/bin` alongside
`suzerain`/`castellan`/`suz`.

## Client configuration

**Claude Code**

```sh
claude mcp add suzerain -- suzerain-mcp
# non-default control plane address:
claude mcp add suzerain -e SUZERAIN_API_URL=http://127.0.0.1:8484 -- suzerain-mcp
```

**Claude Desktop** (`claude_desktop_config.json`)

```json
{
  "mcpServers": {
    "suzerain": {
      "command": "suzerain-mcp",
      "env": { "SUZERAIN_API_URL": "http://127.0.0.1:8484" }
    }
  }
}
```

## Tools (22)

**Discovery** (consult before `agent_create` — the server also
pre-validates against these so creation succeeds on the first call):

| Tool | Returns |
|------|---------|
| `llm_providers` | Providers + models, annotated `key_injectable` / `key_configured` |
| `harness_catalog` | Harness kinds + provisionable versions |
| `pi_packages_search` | pi.dev extension catalog (install sources for `extensions[].source`) |
| `fleet_overview` | Daemon/agent counts, per-daemon capacity/free |
| `audit_tail` | Recent audit entries |

**Castellans:** `castellan_add` (no arg → human enrollment instructions;
with id → approve), `castellan_get`, `castellan_list` (incl. pending),
`castellan_remove`, `castellan_labels_set`.

**Agents:** `agent_list`, `agent_create` (manifest TOML *or* structured
fields), `agent_get`, `agent_delete`,
`agent_logs` (operational journal), `agent_session_events` (chat
transcript), `agent_session_send`, `agent_session_abort`.

There are deliberately **no start/stop/suspend/restore tools**: agents
suspend automatically after a period of inactivity (global default 30m;
per-agent override via the manifest `[lifecycle]` block) and wake
transparently when a message arrives — `agent_session_send` to a sleeping
agent queues the message durably and triggers the wake.

## Safety model

- **Destructive tools require `confirm`** to exactly equal the resource:
  `agent_delete` needs the agent name, `castellan_remove` needs the
  daemon's full EndpointId. A wrong value returns a descriptive error, no
  side effects.
- **Secrets:** never touched. If `agent_create` reports a missing secret,
  the error contains the exact `suz secrets set …` command — relay it to
  a human; don't try to work around it.
- **Async creates:** `agent_create` returns after validation; provisioning
  runs in the background. Poll `agent_get` / `agent_logs`.
- **Messaging:** `agent_session_send` returns once accepted; poll
  `agent_session_events` (`roles=["assistant"]`) for the reply.
- Stdio transport only, and the REST API is localhost-only: the server
  adds no network attack surface.

## Typical flow

```
llm_providers            → pick provider/model with key_configured=true
harness_catalog          → pick version
pi_packages_search       → optional: extensions
agent_create             → structured fields
agent_get / agent_logs   → until state=active
agent_session_send       → "hello"
agent_session_events     → read the reply
```
