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

Provider keys are read from the daemon's own environment for the providers
declared in the manifest (SOPS-sliced delivery replaces this in Phase 4).
`CASTELLAN_HOME` overrides the data dir (default `~/.local/share/castellan`).
