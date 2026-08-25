# Troubleshooting Suzerain / Suzy

Symptom → likely cause → fix. Prefer verifying with real commands
(`suz daemon list`, `suz agent logs <name>`, service logs) over guessing.

## Install / host setup

- **`install.sh`: "unsupported platform"** — only linux x86_64 and macOS
  arm64 are released. Build from source on anything else.
- **agent hosting can't boot VMs: node missing/old** — the gondolin driver
  needs node >= 22 (`node --version`). macOS: `brew install node`;
  Debian/Ubuntu's default nodejs is often too old — use nodesource or mise.
  (Not applicable to a `--mode control` / `--control-only` install, which
  doesn't need node/qemu at all.)
- **agent hosting can't boot VMs: qemu missing** — `brew install qemu` /
  `apt install qemu-system-x86` (the installer attempts this).
- **Linux: VMs fail, `/dev/kvm` errors** — enable KVM; in a cloud VM
  enable nested virtualization. Permissions:
  `sudo usermod -aG kvm $USER` then re-login
  (quick-and-dirty: `sudo chmod o+rw /dev/kvm`).
- **First agent boot is very slow** — the ~600MB guest image downloads
  to `~/.cache/gondolin` on first boot. Subsequent boots are fast.
- **`~/.local/bin: command not found`** — the installer warns when
  `~/.local/bin` isn't on `PATH`; add it to the shell profile.

## Control plane

- **`suzerain run` fails on secrets** — the store is missing or
  undecryptable. Check `$SUZERAIN_HOME/secrets.age` exists and
  `SOPS_AGE_KEY_FILE` points at the right age key (the identity lives at
  `$SUZERAIN_HOME/age-keys.txt`, auto-generated on first use; if the
  machine was rebuilt, restore `age-keys.txt` from backup — a fresh
  identity cannot decrypt an existing store).
- **`suz` can't reach the control plane** — is `suzerain run` (or its
  service) actually up? `systemctl --user status suzerain` /
  `launchctl list | grep suzerain`. `suz` talks REST to the web port
  (default `127.0.0.1:8484`, `SUZERAIN_API_URL` to override) — check
  `[web].port` in `suzerain.toml` matches, and that `[web].enabled` isn't
  `false` (disabling it cuts off `suz`/`suzerain-mcp` entirely; only
  Suzy's iroh operator channel still works in that case).
- **Web UI not loading** — it binds `127.0.0.1:8484` only. Remote
  access: `ssh -L 8484:127.0.0.1:8484 <host>` or use Suzy. If `[web]
  token` is set, the login screen requires it.

## Enrollment / networking

- **daemon never appears in `suz daemon list`** — standalone mode's
  co-located agent host approves itself automatically; this only applies
  to a dedicated (non-standalone) agent-hosting node. Pending enrollments
  need approval: `suz daemon approve <ENDPOINT_ID>` with the id printed
  by `suzerain init`. A wrong or stale control-plane EndpointId in
  `suzerain init --suzerain …` also causes this — re-run init.
- **nodes can't reach each other across networks** — iroh uses public
  relays + NAT hole-punching; a hostile symmetric NAT or blocked UDP can
  still defeat it. Same LAN? mDNS should just work. Check firewalls
  allow outbound UDP (QUIC).
- **Suzy connection rejected** — its EndpointId isn't in `[operator]
  allow`. Run `suz operator approve <id>` (shown in Suzy's
  add-workspace dialog). `suz operator list` shows the current set.

## Agents

- **create rejected by the scheduler** — no daemon matches the
  manifest's `[schedule]` constraints or resource requests, or no
  daemon is online. `suz daemon list` for capacity/labels; relax
  `[schedule] require`.
- **agent fails immediately on first turn: provider auth error** — the
  provider key isn't in the secrets store or isn't declared in the
  manifest's `[secrets] providers`. `suz secrets` (names only) and add
  with `suz secrets set provider <id>`.
- **agent `failed`** — `suz agent logs <name>` first (crash-loop
  detection is the usual trigger). Fix the cause (bad manifest, missing
  secret, failing repo clone), then destroy + recreate — manifests are
  immutable.
- **agent stuck `waking`** — wake retries across daemons
  (`wake_retry_attempts`, default 3) then gives up. Usual cause: every
  daemon is down or out of capacity. Bring a daemon back and re-send the
  message.
- **"my agent went away"** — it didn't; it's `sleeping` (auto-suspend).
  Just message it. Disable per-agent with
  `suz agent config <name> --auto-suspend never`.
- **git clone/push fails in the agent** — private repos need the git SSH
  key: `suz secrets set ssh-key < ~/.ssh/id_ed25519` (any ssh-keygen key —
  ed25519/ecdsa/RSA; passphrase-protected keys are rejected, remove it with
  `ssh-keygen -p`). The key must be authorized on the git host (on GitHub:
  a write-enabled deploy key on the repo, or any account key). The key
  never enters the guest — the host-side ssh proxy authenticates for it —
  so `ssh -T git@github.com` inside the VM is a valid connectivity test.

## Services / ops

- **systemd user service won't start on a server** — no lingering user
  session: `loginctl enable-linger $USER`, then
  `systemctl --user enable --now suzerain`.
- **macOS: launchd agent not running** — check
  `launchctl list | grep -i suz` and logs under
  `~/.local/share/suzerain`.
- **disk filling up** — bundle store (`[bundles] dir`) and the central
  log grow forever by default. Move bundles to a big disk; set
  `[retention] days = N`.
