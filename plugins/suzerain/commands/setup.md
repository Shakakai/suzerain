---
description: Guided setup of the Suzerain stack (control plane + castellan daemon + secrets + first agent + optional Suzy) on this machine
argument-hint: "[--daemon-only | --control-plane-only]"
allowed-tools: Bash, Read, Write, AskUserQuestion
---

Set up the Suzerain agent fleet stack, following the
`suzerain-admin` skill's "fresh setup on one machine" workflow. Load
that skill first and follow it step by step.

Scope from $ARGUMENTS: default is the full stack (control plane +
daemon + secrets + first agent). `--daemon-only` installs/enrolls just
castellan (ask the user for the suzerain EndpointId and remind them to
`suz daemon approve` on the control-plane host). `--control-plane-only`
installs/configures just suzerain + secrets + the web UI.

Rules:
- Work step by step, verifying each step before moving on (the skill
  lists the verification for each).
- Never print or commit secret values; use `suz secrets set …` with
  stdin, or the sops encrypt flow from the skill.
- Before any reboot-level or system-level change (usermod, lingering,
  package installs), tell the user exactly what you're about to run.
- Finish with a summary: what was installed, the suzerain EndpointId,
  daemon EndpointId, web UI URL, and the exact next commands for adding
  Suzy or more daemons.
