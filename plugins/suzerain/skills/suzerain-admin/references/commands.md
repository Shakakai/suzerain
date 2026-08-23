# Suzerain command & config reference

## Binaries

| Binary | Role | Default data dir |
|---|---|---|
| `suzerain` | control plane | `~/.local/share/suzerain` (`SUZERAIN_HOME`) |
| `castellan` | per-server agent daemon | shares the fleet home: `~/.local/share/suzerain` (`CASTELLAN_HOME`, else `SUZERAIN_HOME`) |
| `suz` | operator CLI (unix socket to control plane) | — |
| `suzerain-mcp` | MCP server (stdio → REST on :8484, override with `SUZERAIN_API_URL`) | — |
| `suzy` | desktop GUI (iroh operator channel) | `~/.config/suzy/` |

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/Shakakai/suzerain/main/ops/install.sh | bash
# options: components [suzerain|castellan|suz|suzerain-mcp|suzy|all],
#          --version vX.Y.Z, --bin-dir DIR, --no-service
```

From source: `mise install && mise run setup && mise run package`
(optional `mise run install:services` for systemd/launchd).

## Control plane

```sh
suzerain run          # foreground; prints EndpointId, serves :8484
suz id                # print the control plane's EndpointId
```

`$SUZERAIN_HOME/suzerain.toml` (castellan keeps its own `castellan.toml`
in the same folder — the file names never overlap):

```toml
[auto_suspend]
enabled = true
idle_timeout = "30m"        # suspend after this much inactivity
sweep_interval = "30s"
wake_retry_attempts = 3     # failed daemons excluded on retry

[bundles]
dir = "/mnt/big-disk/suzerain-bundles"   # default <data>/bundles

[retention]
days = 90                   # default: keep everything

[web]
port = 8484                 # 127.0.0.1 only, by design
# token = "…"               # optional bearer token

[operator]
allow = ["<ENDPOINT_ID>", …]  # suzy/operator clients (managed via `suz operator approve`)
```

Env: `SUZERAIN_DATABASE_URL=postgres://user@host/db` (default sqlite),
`OTEL_EXPORTER_OTLP_ENDPOINT` on daemons for traces.

## Daemons

```sh
castellan init --suzerain <SUZERAIN_ENDPOINT_ID>   # prints this daemon's EndpointId
castellan run                                       # registers + takes orders
suz daemon list
suz daemon approve <ENDPOINT_ID>
suz daemon label <id> --set gpu=true --set region=eu    # scheduling labels (--remove k to unset)
```

(Daemon removal lives in the web UI / Suzy castellans view, not the CLI.)

Daemon host prerequisites: node >= 22, qemu, Linux: writable `/dev/kvm`
(`sudo usermod -aG kvm $USER`, re-login). Guest VM images (~600MB)
auto-download to `~/.cache/gondolin` on first boot.

Castellan's own config is `castellan.toml` in the same fleet home
(`$CASTELLAN_HOME`, else `$SUZERAIN_HOME`, else `~/.local/share/suzerain`) —
written by `castellan init`; knobs: `suzerain_endpoint_id`, `labels`,
`max_agents`. Its identity is `castellan.key` beside suzerain's
`suzerain.key` — the two daemons share the folder, never a file name.

## Operators (Suzy / remote clients)

```sh
suz operator approve <ENDPOINT_ID>   # live + persisted to [operator] allow
suz operator list
```

Suzy shows its EndpointId in the add-workspace dialog; the workspace
takes the control plane's EndpointId from `suz id`.

## Agents

```sh
suz agent list
suz agent create --manifest <file.toml>     # [--daemon <id-or-hostname>]; label matching via [schedule] in the manifest
suz agent ask <name> "message"              # wakes the agent if sleeping
suz agent attach <name>                     # history + live interactive
suz agent logs <name>                       # central event log
suz agent config <name> --auto-suspend 10m  # or "never" / "default"
suz agent destroy <name>                    # destructive; confirm first
```

Statuses: `running` (turn in flight), `idle` (awake), `sleeping`
(suspended — normal!), `waking`, `failed`.

## Secrets

Store: `$SUZERAIN_HOME/secrets.age` (armored age file, YAML payload; key at
`$SUZERAIN_HOME/age-keys.txt` — auto-generated on first use,
`SOPS_AGE_KEY_FILE` to point elsewhere). Legacy `secrets.sops.yaml` is
auto-migrated once at startup.

```sh
suz secrets                                        # names only, never values
suz secrets set provider anthropic --value sk-ant-…
suz secrets set provider anthropic                 # reads value from stdin (preferred)
suz secrets set ssh-key < ~/.ssh/id_ed25519        # one per daemon; agents git pull/push over SSH
suz secrets remove extra OLD_TOKEN                 # destructive; confirm first
```

The SSH key can be any key `ssh-keygen` produces (ed25519, ecdsa, RSA —
validated by parsing at upload; passphrase-protected keys are rejected,
`ssh-keygen -p` removes a passphrase). The key stays host-side: gondolin's
ssh proxy authenticates upstream for the agent's git/ssh traffic to
allowlisted git hosts, so the private key never enters the microVM while
git pull/push work transparently inside it. `deploy-key` / `git` remain
accepted aliases for the `ssh-key` kind.

Store shape:

```yaml
providers:
  anthropic: "sk-ant-…"      # keyed by pi provider id
git:
  ssh_key: |
    -----BEGIN OPENSSH PRIVATE KEY-----
extra: {}
```

Each agent receives only the provider slice its manifest declares, as
Gondolin placeholder env vars — the guest never holds raw credentials
(the git SSH key is proxied host-side as well).

## Agent manifest (TOML)

```toml
name = "researcher-1"
harness = { type = "pi", version = "0.84.1" }
model = { provider = "kimi-coding", id = "kimi-for-coding", thinking = "high" }

[toolchain]
tools = { node = "22", python = "3.12" }

[[repos]]
url = "git@github.com:org/repo.git"     # fresh clone into /workspace
ref = "main"

[[extensions]]
source = "npm:@scope/pi-package"        # or { url = "git@…", ref = "v1.2.0" }

[prompt]
append_system = "You are a meticulous researcher."

[secrets]
providers = ["kimi-coding"]             # must exist in the secrets store

[egress]
allow = ["crates.io"]                   # beyond provider/git/npm/otel defaults

[lifecycle]
auto_suspend = "10m"                    # or "never"

[schedule]
require = { gpu = "true" }              # label subset match; daemon = "<id-or-hostname>" pins hard

[observability.otel]
endpoint = "https://otel.example.com"
headers = { authorization = "…" }
```

Manifests are immutable post-create; recreate to change (only
`auto_suspend` is mutable at runtime).

## MCP

```sh
claude mcp add suzerain -- suzerain-mcp
claude mcp add suzerain -e SUZERAIN_API_URL=http://127.0.0.1:8484 -- suzerain-mcp
```

Fleet management tools only — secrets are never exposed through MCP.
