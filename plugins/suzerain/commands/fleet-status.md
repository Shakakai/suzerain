---
description: Fleet status report — daemons, agents, recent audit activity, and anything that needs attention
allowed-tools: Bash
---

Give the user a concise health report of their Suzerain fleet. Run
(don't just describe) the following, tolerating individual failures:

1. `suz daemon list` — daemons, online/offline, capacity/usage.
2. `suz agent list` — agents and statuses (running / idle / sleeping /
   waking / failed).
3. `suz audit --tail 20` — recent control-plane actions.
4. If any agent is `failed` or `waking` for a long time, run
   `suz agent logs <name>` for it.

Then summarize as a short report:
- **Daemons:** N online / M total, any capacity pressure.
- **Agents:** counts by status. Remember: `sleeping` is healthy
  (auto-suspend) — never flag it as a problem.
- **Needs attention:** failed agents (with the last error line from
  their logs), pending daemon approvals, offline daemons, stuck wakes.
- **Recent activity:** 3–5 notable audit entries.

End with at most three suggested next actions, each as an exact command.
Keep the whole report under ~30 lines.
