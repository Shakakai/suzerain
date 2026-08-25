---
description: Guided setup of the Suzerain stack (one binary — standalone mode by default, or split control/agent roles — plus secrets + first agent + optional Suzy) on this machine
argument-hint: "[--agent-only | --control-plane-only]"
allowed-tools: Bash, Read, Write, AskUserQuestion
---

Set up the Suzerain agent fleet stack, following the
`suzerain-admin` skill's "fresh setup on one machine" workflow. Load
that skill first and follow it step by step.

Scope from $ARGUMENTS: default is standalone mode — one `suzerain run`
does everything (control plane + secrets + first agent, no separate
enrollment step). `--agent-only` configures this host as a dedicated
agent-hosting node (`suzerain init --suzerain <id>` then `suzerain run
--mode agent`; ask the user for the control plane's EndpointId and remind
them to `suz daemon approve` on the control-plane host).
`--control-plane-only` installs/configures suzerain in `--mode control`
(no local agent hosting) + secrets + the web UI.

Rules:
- Work step by step, verifying each step before moving on (the skill
  lists the verification for each).
- Never print or commit secret values; use `suz secrets set …` with
  stdin (the store and its age identity are created automatically in the
  fleet home on first write).
- Before any reboot-level or system-level change (usermod, lingering,
  package installs), tell the user exactly what you're about to run.
- Finish with a summary: what was installed, the suzerain EndpointId,
  daemon EndpointId, web UI URL, and the exact next commands for adding
  Suzy or more daemons.
