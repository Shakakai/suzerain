# Suzy design system

Suzy is explicitly modeled on [herdr](https://github.com/herdrdev/herdr)
(see `docs/SUZY.md`) — a terminal multiplexer for coding agents. herdr's
own site (`website/css/site.css`) defines a small, consistent visual
language: a neutral ink/paper palette, a restrained purple accent, and a
sharp, brutalist chrome (zero corner radius, hairline borders, uppercase
tracked labels). This design system ports that language into Suzy's
egui/eframe codebase so it stops being reinvented ad hoc at each call site.

The implementation lives in [`crates/suzy/src/theme.rs`](../src/theme.rs) —
that module is the single source of truth for every value documented here.
This folder is the *why* and *how*; `theme.rs` is the *what*.

## Documents

- [`tokens.md`](tokens.md) — every color, typography, and structural token,
  with its Rust identifier and a usage note.
- [`components.md`](components.md) — recurring UI patterns (panels, status
  badges, chat bubbles, section headers) with before/after code.
- **Kitchen sink** — `crates/suzy/src/bin/kitchen_sink.rs` is a standalone
  eframe app that renders every token and chrome helper in `theme.rs` live
  (color swatches, status dots, panel/stat/warning/bubble frames, buttons,
  section labels). It calls into `theme::` for every value, so it can never
  drift from what's actually shipping. Run it with
  `cargo run -p suzy --bin kitchen-sink` whenever you want to check what a
  token or helper actually looks like, or to sanity-check a new one you're
  about to add. It also has a "Typography" section rendering Archivo, Inter,
  and JetBrains Mono live.

## Fonts

Archivo, Inter, and JetBrains Mono are vendored under
`crates/suzy/assets/fonts/` (OFL-licensed, sourced from the canonical
[google/fonts](https://github.com/google/fonts) repo — see the `OFL-*.txt`
files alongside them) and installed once at startup via
`theme::install_fonts(ctx)`, called before `theme::apply`. See
`tokens.md`'s Typography section for exactly which egui font family each
one replaces and the variable-font caveat (no runtime weight/width
selection).

## Principles

1. **Sharp, not rounded.** Every corner in Suzy's own chrome is 0px
   (`theme::RADIUS`). herdr's site sets `--radius-sm/md/lg: 0` deliberately
   — rounded corners read as generic SaaS chrome; square ones read as an
   operator tool. This does **not** apply to the ANSI terminal grid in
   `terminal.rs` — that's emulating a real shell, which has its own
   conventions.
2. **Hairline borders, not shadows.** Panels are separated by a 1px
   `theme::LINE`/`theme::LINE2` stroke, never a drop shadow or elevation
   effect.
3. **Restrained accent.** `theme::SPOT` (a muted purple) is for hover
   states, active/selected indicators, and small highlights — never a large
   flat fill. If you find yourself filling a whole panel with SPOT, that's
   a sign the design has drifted from herdr's original intent.
4. **Uppercase, tracked labels for structure.** Section headers and nav-like
   labels use `theme::section_label()` — small, uppercase, letter-spaced —
   to separate "structure" text from body/content text.
5. **One status vocabulary.** Agent state (running/idle/waking/sleeping/
   failed) always resolves through `theme::status_color()`. Don't hand-roll
   a new `match` on status strings — see `tokens.md` for the mapping.

## How to use this when building UI

- Never write `Color32::from_rgb(...)` or a bare `.corner_radius(N)` for
  Suzy's own chrome. Reach for a `theme::` constant or helper
  (`theme::panel_frame()`, `theme::stat_frame()`, `theme::warning_frame()`,
  `theme::bubble_frame()`, `theme::status_color()`, `theme::section_label()`).
- If the pattern you need isn't covered by an existing helper, add one to
  `theme.rs` rather than inlining a one-off — that's how the duplicated
  status-color `match` statements and repeated panel-fill literals crept in
  originally.
- egui's built-in named colors (`Color32::GRAY`, `KHAKI`, `LIGHT_RED`, …)
  are still used in a few places for ad hoc semantic meaning (muted text,
  warnings, errors) that predate this system and weren't in scope for the
  first migration pass. Treat new uses of those as a smell — prefer
  `theme::FAINT`/`theme::WAIT`/`theme::ERROR` instead, and feel free to
  migrate an old one to a token when you're already touching that line.
- New screens should call `theme::apply(ctx)` implicitly by virtue of
  running inside `SuzyApp` (it's applied once at startup) — you shouldn't
  need to call it yourself.

## What's intentionally out of scope

- **The ANSI terminal palette** (`terminal.rs::PALETTE`, `FG_DEFAULT`,
  `BG_DEFAULT`, `indexed_color`) is a 256-color ANSI emulation table for
  whatever shell/program is running inside the agent's pty. It is not
  Suzy's own chrome and must never be reskinned to match the brand palette
  — doing so would make real terminal output (a program's own ANSI colors)
  look wrong.
- **A real light/"paper" theme.** herdr's site has a light ground
  (`data-mode="paper"`); the `_PAPER` constants in `theme.rs` mirror it, but
  Suzy's light-mode toggle still falls back to `egui::Visuals::light()`
  rather than using them. Wiring that up is a follow-up.
