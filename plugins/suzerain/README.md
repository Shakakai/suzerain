# suzerain (agent plugin)

An [Agent Skills](https://agentskills.io) / Claude Code–compatible
plugin that teaches an AI coding assistant to **set up and operate a
Suzerain/Castellan fleet**: control plane, per-server daemons running pi
agents in Gondolin microVMs, and the Suzy desktop console.

## What's inside

| Piece | Path | Purpose |
|---|---|---|
| Skill | `skills/suzerain-admin/SKILL.md` | Setup, team deployment, and day-2 fleet operations, with progressive-disclosure references (`references/commands.md`, `references/troubleshooting.md`) |
| Commands | `commands/setup.md`, `commands/fleet-status.md` | `/suzerain:setup` guided install; `/suzerain:fleet-status` health report |
| MCP | `.mcp.json` | Wires `suzerain-mcp` so the assistant gets typed fleet tools (secrets are never exposed through MCP, by design) |

## Install

Claude Code (this repo is the marketplace):

```
/plugin marketplace add Shakakai/suzerain
/plugin install suzerain@suzerain
```

pi (the skill format is portable):

```sh
pi --skill plugins/suzerain/skills/suzerain-admin
# or copy it: cp -R plugins/suzerain/skills/suzerain-admin ~/.agents/skills/
```

Any harness implementing the Agent Skills standard can load
`skills/suzerain-admin/SKILL.md` directly.

## Prerequisites

The MCP wiring expects the `suzerain-mcp` binary on `PATH` (installed by
`ops/install.sh` or `mise run package`) and a running control plane. The
skill itself works with or without MCP — it drives the `suz` CLI.
