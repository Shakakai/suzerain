# suzerain

Multi-server AI agent lifecycle system: a centralized control plane
(**suzerain**) and a per-server daemon (**castellan**) that spawns, supervises,
and persists coding agents — Pi running in RPC mode inside Gondolin microVMs —
with all node communication over iroh (QUIC, public-key identity).

See [`docs/PLAN.md`](docs/PLAN.md) for the architecture and
[`docs/PHASE0-FINDINGS.md`](docs/PHASE0-FINDINGS.md) for validated spike
results.

## Layout

```
crates/
  protocol/       suzerain-protocol: shared wire types (manifests, orders, events, framing)
  castellan/      per-server agent daemon (data plane)
  suzerain/       control plane
  suzerain-cli/   operator CLI (`suz`)
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

## Phase 2: control plane

```sh
cargo run -p suzerain -- run                          # control plane (iroh + operator socket)
cargo run -p suzerain-cli -- id                       # its EndpointId
cargo run -p castellan -- init --suzerain <SUZ_ID>    # prints this daemon's EndpointId
cargo run -p suzerain-cli -- daemon approve <CASTELLAN_ID>
cargo run -p castellan -- run                         # daemon registers + takes orders

suz daemon list
suz agent create --manifest examples/researcher.toml
suz agent ask researcher-1 "hello"
suz agent logs researcher-1        # centrally stored event log
suz agent stop researcher-1        # local journal pruned once acked; central keeps all
suz agent start researcher-1       # resumes the prior session
suz agent suspend researcher-1     # + VM checkpoint + bundle upload to control plane
suz agent restore researcher-1 --daemon <ID>   # restore on any approved daemon
suz agent attach researcher-1      # history + live interactive session
suz agent destroy researcher-1
```

`SUZERAIN_HOME` / `CASTELLAN_HOME` override the data dirs
(defaults `~/.local/share/{suzerain,castellan}`).

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
injects them only into requests to that provider's API host. `suz secrets`
lists configured entries (names only); `suz audit` shows the audit log.

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
