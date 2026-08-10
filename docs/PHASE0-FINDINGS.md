# Phase 0 findings — spikes validated

Date: 2026-08-10. All spikes run locally on macOS (arm64). CI covers
build/fmt/clippy/test on macOS + Linux; the spikes themselves run manually
(they need LLM credentials / qemu).

## Spike (a): Rust → pi RPC — PASS

`crates/castellan/examples/spike_pi_rpc.rs` spawns `pi --mode rpc`, issues
`get_state`, sends a `prompt`, streams events to `agent_end`, and reads the
final answer via `get_last_assistant_text`.

- Framing per rpc.md: LF-only delimiter, `id`-correlated responses; response
  payloads nest under `data` (e.g. `data.sessionFile`, `data.text`).
- **Resume/restore validated**: a second process with `--session <file>`
  recalled the prior conversation ("PINEAPPLE" test). This is the primitive
  restore-on-any-server builds on: ship the session JSONL, spawn with
  `--session`.
- pi writes a final chunk during shutdown; if our side stops reading early pi
  hits EPIPE. Harmless, but castellan should drain stdout until EOF before
  reaping the child.

## Spike (b): iroh fabric — PASS (with two ordering lessons)

`crates/suzerain/examples/spike_iroh.rs` runs a control node and a daemon node:
order Ping/ack over `suz/control/0` and gossip announce on the fleet topic,
discovered via mDNS (+ n0 pkarr/relay via `presets::N0`).

- **Connect ordering matters (iroh 1.0.3/noq)**: with the gossip link already
  established, dialing a second connection to the same peer on a different ALPN
  hung in QUIC session resumption (`Resuming session` →
  `MultipathNotNegotiated`). Control-first, gossip-after works — including with
  the control connection held open. Design rule: castellan establishes its
  long-lived control connection at registration, gossip joins afterwards.
- **Accept handlers must `connection.closed().await`** after `send.finish()`;
  returning immediately after finishing a stream can discard the unflushed
  ack (observed as "closed by peer: 0" on the dialer).
- mDNS address lookup moved out of the main crate in iroh 1.0:
  `iroh-mdns-address-lookup` (`MdnsAddressLookup::builder().build(id)` +
  `endpoint.address_lookup()?.add(mdns)`).
- Gossip does not deliver your own broadcast back to you — don't wait on it.

## Spike (c)/(d): Gondolin VM + stdio streaming — PASS

`tools/gondolin-driver/src/index.mjs --spike` boots a Gondolin VM (QEMU,
Alpine 3.23, Linux 6.18 aarch64), runs buffered execs, and round-trips JSONL
through a long-running process via `vm.exec(argv, { stdin: true, stdout: "pipe" })`
— the exact pattern for bridging `pi --mode rpc` out of the guest.

- `ExecProcess.write()` / `.lines()` / `.result` give full bidirectional
  streaming; no SSH needed for the dataplane.
- Close stdin (`proc.end()`) and await exit before `vm.close()`; closing a VM
  with an in-flight exec rejects it with `server_shutdown`.
- **Guest base image has `node` but NOT `npm`, `git`, or `mise`.** In-guest
  provisioning must `apk add git npm` (or build a custom Gondolin image with
  them baked in — likely cleaner for boot time; decide in Phase 1).
- Host prerequisite: `qemu` (installed via brew/apt; mise cannot provide it).
  First boot downloads ~200MB of guest assets (cached afterwards).
