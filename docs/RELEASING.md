# Releasing

Two long-lived branches: `main` (development) and `release` (what ships).
Releases are fully automated by `.github/workflows/release.yml`.

## Cutting a release

1. Open a PR from `main` into `release`.
   The workflow runs a validation build (release binaries + packaged archives
   for linux-x86_64 and macOS-arm64) on the PR — nothing is published.
2. Merge the PR. The workflow then:
   - computes the next version `v{MAJOR}.{MINOR}.{build}` — MAJOR/MINOR come
     from [`ops/version.env`](../ops/version.env) (bump manually via a normal
     PR to `main`), `build` auto-increments from the latest git tag;
   - stamps that version into the binaries (so `--version` matches the tag);
   - creates the git tag on the `release` branch and a **prerelease** on
     GitHub with auto-generated, categorized release notes
     (see [`.github/release.yml`](release.yml) — label PRs with
     `feature`/`bug`/`docs`/`chore` to get sectioned notes);
   - attaches per-component archives
     (`{suzerain,castellan,suz,suzerain-mcp}-{version}-{target}.tar.gz`),
     `SHA256SUMS.txt`, and `install.sh`.

Only PRs **from `main`** may be merged into `release` — the workflow fails
loudly otherwise. Direct pushes to `release` never publish.

A release can be re-run manually via `workflow_dispatch` (Actions → Release →
Run workflow), e.g. after fixing a transient failure.

## Installing

See the [README](../README.md#install) — the one-liner runs
[`ops/install.sh`](../ops/install.sh), which resolves the latest release
(including prereleases), verifies checksums, installs binaries plus the
gondolin driver, checks host dependencies (node, qemu, KVM), and optionally
enables systemd/launchd services (`--no-service` to skip).

## Versioning

- `v0.1.7` → MAJOR=0, MINOR=1 (manual, `ops/version.env`), build=7 (auto).
- To start a new line (e.g. `0.2.0`): bump `ops/version.env` on `main`; the
  next merge into `release` tags `v0.2.0`.
- All four binaries share one version (workspace version).
