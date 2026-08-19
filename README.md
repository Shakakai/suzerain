# suzerain

Multi-server AI agent lifecycle system: a centralized control plane
(**suzerain**) and a per-server daemon (**castellan**) that spawns, supervises,
and persists coding agents — Pi running in RPC mode inside Gondolin microVMs —
with all node communication over iroh (QUIC, public-key identity).

See [`docs/PLAN.md`](docs/PLAN.md) for the architecture and
[`docs/PHASE0-FINDINGS.md`](docs/PHASE0-FINDINGS.md) for validated spike
results.

## Install

One line installs everything from the latest GitHub release (binaries for
linux x86_64 + macOS arm64, checksums verified, services enabled):

```sh
curl -fsSL https://raw.githubusercontent.com/Shakakai/suzerain/main/ops/install.sh | bash
```

Install a single component, a pinned version, or skip service setup:

```sh
curl -fsSL .../ops/install.sh | bash -s -- castellan                       # just the daemon
curl -fsSL .../ops/install.sh | bash -s -- --version v0.1.3 suzerain suz   # pinned
curl -fsSL .../ops/install.sh | bash -s -- --no-service suzerain           # binaries only
```

Components: **suzerain** (control plane), **castellan** (agent daemon — also
installs the gondolin driver and checks node/qemu/KVM), **suz** (operator
CLI), **suzerain-mcp** (MCP server). Binaries go to `~/.local/bin`; on Linux
and macOS the daemons are enabled as systemd user services / launchd agents.
See [`docs/RELEASING.md`](docs/RELEASING.md) for how releases are cut.

## Quickstart

Get the full stack running on one machine (control plane + daemon + web UI)
in a few minutes.

**1. Prerequisites**

```sh
brew install qemu mise          # linux: apt install qemu-system-arm
mise install                    # rust, node, sops toolchains
```

**2. Build + install binaries**

```sh
mise run package                # release build → ~/.local/bin
# (or dev: cargo run -p suzerain -- run)
```

**3. Secrets store** (agents need LLM keys; one-time setup)

```sh
age-keygen -o ~/.config/sops/age/keys.txt
export SOPS_AGE_KEY_FILE=~/.config/sops/age/keys.txt
cat > /tmp/secrets.plain.yaml <<'EOF'
providers:
  kimi-coding: "sk-your-key-here"
  anthropic: "sk-ant-your-key-here"
EOF
sops --encrypt --age $(age-keygen -y $SOPS_AGE_KEY_FILE) \
  --input-type yaml --output-type yaml /tmp/secrets.plain.yaml \
  > ~/.local/share/suzerain/secrets.sops.yaml
rm /tmp/secrets.plain.yaml
```

**4. Start the control plane**

```sh
suzerain run
# → prints its EndpointId, opens the operator socket,
#   and serves the web UI at http://127.0.0.1:8484
```

**5. Enroll a castellan** (same machine or any other — identical flow)

```sh
castellan init --suzerain <SUZERAIN_ENDPOINT_ID>   # prints the daemon's EndpointId
suz daemon approve <CASTELLAN_ENDPOINT_ID>
castellan run                                     # registers and takes orders
```

**6. Create your first agent**

```sh
suz agent create --manifest examples/researcher.toml
suz agent ask researcher-1 "hello"
# …or use the web UI at http://127.0.0.1:8484
# …or the desktop app: cargo run -p suzy (see docs/SUZY.md — connect by
#   EndpointId over iroh; allowlist Suzy's operator key in [operator])
```

Install as always-on services instead: `mise run install:services`
(systemd user units on Linux, launchd agents on macOS).

## Layout

```
crates/
  protocol/         suzerain-protocol: shared wire types (manifests, orders, events, framing)
  castellan/        per-server agent daemon (data plane)
  suzerain/         control plane
  suzerain-cli/     operator CLI (`suz`)
  suzerain-client/  async Rust client for the control plane's /api/v1 (REST + SSE)
  suzy/             desktop operator console (egui) — docs/SUZY.md
tools/
  gondolin-driver/  Node sidecar bridging castellan to Gondolin VMs
.pi/extensions/   pi extension pack (deep-research) — provisioned onto agents
```

## Prerequisites

- [mise](https://mise.jdx.dev) (manages rust, node, sops): `mise install`
- qemu (for Gondolin VMs): `brew install qemu` / `apt install qemu-system-arm`
- `sops` + an `age` keypair for the secrets store (control plane host)

## Build & test

```sh
mise run setup
mise run build
mise run test
mise run lint
```

## Phase 0 spikes

```sh
mise run spike:pi-rpc -- "say hi"            # Rust → pi --mode rpc (prompt/resume)
mise run spike:iroh -- control               # terminal 1
mise run spike:iroh -- daemon <ENDPOINT_ID>  # terminal 2
mise run spike:gondolin                      # boot a microVM, stream stdio
```

## Phase 1: castellan standalone

```sh
cargo run -p castellan -- run                        # foreground daemon (unix socket)
cargo run -p castellan -- create --manifest examples/researcher.toml
cargo run -p castellan -- ask researcher-1 "hello"
cargo run -p castellan -- attach researcher-1        # interactive
cargo run -p castellan -- stop researcher-1
cargo run -p castellan -- start researcher-1         # resumes the prior session
cargo run -p castellan -- logs researcher-1
cargo run -p castellan -- destroy researcher-1
```

(Standalone-mode verbs are local-only; once a daemon is enrolled, the
control plane owns the lifecycle — see Auto-suspend below.)

## Phase 2: control plane

```sh
cargo run -p suzerain -- run                          # control plane (iroh + operator socket)
cargo run -p suzerain-cli -- id                       # its EndpointId
cargo run -p castellan -- init --suzerain <SUZ_ID>    # prints this daemon's EndpointId
cargo run -p suzerain-cli -- daemon approve <CASTELLAN_ID>
cargo run -p castellan -- run                         # daemon registers + takes orders

suz daemon list
suz agent create --manifest examples/researcher.toml
suz agent ask researcher-1 "hello"   # wakes the agent if it's sleeping
suz agent logs researcher-1          # centrally stored event log
suz agent attach researcher-1        # history + live interactive session
suz agent config researcher-1 --auto-suspend 10m   # per-agent policy (or "never"/"default")
suz agent destroy researcher-1
```

`SUZERAIN_HOME` / `CASTELLAN_HOME` override the data dirs
(defaults `~/.local/share/{suzerain,castellan}`).

## Auto-suspend & transparent wake

Agents are never started or stopped by hand. The control plane tracks
daemon-reported activity for every agent (any turn in flight counts as
busy — a 30-minute test run is never mistaken for idle) and **suspends
agents automatically** after an inactivity timeout: graceful stop, VM
checkpoint, and a restore-bundle upload. **Sessions rotate on every
suspend**: the pi session is uploaded to the control plane in full (where
it is retained for history/audit), then removed from the guest — each
wake starts a fresh pi session, so agents never accumulate unbounded
session state. Sending a message to a sleeping
agent **wakes it transparently**: the message is held in a durable queue
while the agent boots (same-host checkpoint resume when possible, bundle
restore onto any daemon otherwise, with retries across daemons), then
delivered. Chat (CLI `ask`/`attach`, web UI, MCP) works against sleeping
agents exactly as if they were running.

Public agent statuses: **running** (turn in flight), **idle** (awake,
waiting), **sleeping** (suspended), **waking**, **failed**.

Global defaults in `$SUZERAIN_HOME/config.toml`:

```toml
[auto_suspend]
enabled = true
idle_timeout = "30m"        # suspend after this much inactivity
sweep_interval = "30s"
wake_retry_attempts = 3     # failed daemons are excluded on retry

[bundles]
dir = "/mnt/big-disk/suzerain-bundles"   # snapshot storage (default <data>/bundles)
```

Per-agent overrides: `[lifecycle] auto_suspend = "10m" | "never"` in the
manifest, or at runtime via `suz agent config <name> --auto-suspend …`
("default" clears the override). `"never"` also exempts the agent from
resource-pressure preemption: when a daemon is full, the scheduler may
suspend its longest-idle agents to make room for a new one.

## Secrets (Phase 4)

The control plane reads an age-encrypted SOPS store at
`$SUZERAIN_HOME/secrets.sops.yaml`:

```sh
age-keygen -o ~/.config/sops/age/keys.txt   # once
export SOPS_AGE_KEY_FILE=~/.config/sops/age/keys.txt
sops --encrypt --age $(age-keygen -y $SOPS_AGE_KEY_FILE) \
     --input-type yaml --output-type yaml secrets.plain.yaml \
     > $SUZERAIN_HOME/secrets.sops.yaml
```

```yaml
providers:
  kimi-coding: "sk-…"     # keyed by pi provider id
  anthropic: "sk-ant-…"
git:
  deploy_key: |           # one deploy key per daemon (SSH clones)
    -----BEGIN OPENSSH PRIVATE KEY-----
extra: {}
```

Each agent receives only the slice its manifest declares, delivered as
Gondolin placeholder env vars — the guest never holds raw keys, and the host
injects them only into requests to that provider's API host. Manage entries:

```sh
suz secrets                                # list configured entries (names only)
suz secrets set provider anthropic --value sk-ant-…
suz secrets set deploy-key < ~/.ssh/id_ed25519   # stdin for multi-line keys
suz secrets remove extra OLD_TOKEN
```

`suz audit` shows the audit log.

## Ops (Phase 5)

- **Database**: sqlite by default (zero config); set
  `SUZERAIN_DATABASE_URL=postgres://user@host/db` for postgres.
- **OTEL**: set `OTEL_EXPORTER_OTLP_ENDPOINT` on a daemon to export its own
  traces (OTLP/HTTP); agents get OTEL via the manifest `[observability.otel]`
  block.
- **Retention**: defaults to keep-everything; set `[retention] days = N` in
  `$SUZERAIN_HOME/config.toml` to prune central log events older than N days.
- **Services**: `mise run package` (release binaries → `~/.local/bin`),
  `mise run install:services` (systemd user units on Linux, launchd agents on
  macOS — see `ops/`).
