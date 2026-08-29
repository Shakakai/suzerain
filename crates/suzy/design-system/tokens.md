# Tokens

Source: [herdrdev/herdr](https://github.com/herdrdev/herdr),
`website/css/site.css` (the site's real chassis stylesheet — not
`style.css`, which is an unrelated multi-theme terminal-mock picker used
for a marketing demo). Rust identifiers are in
[`crates/suzy/src/theme.rs`](../src/theme.rs).

## Color — dark / "ink" ground (Suzy's default)

| Token (Rust) | Hex | herdr var | Usage |
|---|---|---|---|
| `BG` | `#17171a` | `--bg` | App background, terminal chrome background |
| `PANEL` | `#1e1e22` | `--panel` | Card/panel fills (info boxes, dialogs) |
| `MASS` | `#26262b` | `--mass` | Raised surfaces (stat cards, assistant chat bubble) |
| `INK` | `#eae8ee` | `--ink` | Primary text |
| `DIM` | `#cdccd2` | `--dim` | Secondary text |
| `FAINT` | `#b0afb6` | `--faint` | Muted text (labels, hints) |
| `FAINT2` | `#908f96` | `--faint2` | Most-muted text (system/thinking text) |
| `LINE` | `#26262b` | `--line` | Hairline borders (subtle) |
| `LINE2` | `#35353d` | `--line2` | Hairline borders (visible, panel edges) |
| `SPOT` | `#cba6f7` | `--spot` | Accent — hover, active/selected, links |
| `SPOT_INK` | `#17171a` | `--spot-ink` | Text/foreground drawn on top of `SPOT` |

## Color — light / "paper" ground (defined, not yet wired to the theme toggle)

| Token (Rust) | Hex | herdr var |
|---|---|---|
| `BG_PAPER` | `#efece5` | `--bg` (paper) |
| `PANEL_PAPER` | `#e7e3da` | `--panel` (paper) |
| `MASS_PAPER` | `#ddd8cc` | `--mass` (paper) |
| `INK_PAPER` | `#15140f` | `--ink` (paper) |
| `DIM_PAPER` | `#55534a` | `--dim` (paper) |
| `FAINT_PAPER` | `#86826f` | `--faint` (paper) |
| `FAINT2_PAPER` | `#928e79` | `--faint2` (paper) |
| `LINE_PAPER` | `#e2ded4` | `--line` (paper) |
| `LINE2_PAPER` | `#cbc5b6` | `--line2` (paper) |
| `SPOT_PAPER` | `#8839ef` | `--spot` (paper) |
| `SPOT_INK_PAPER` | `#ffffff` | `--spot-ink` (paper) |

Suzy's light-mode toggle currently falls back to `egui::Visuals::light()`
rather than these — see `README.md`'s "out of scope" section.

## Status colors

herdr's site defines four agent states — `run` (working), `wait`
(blocked/transient), `idle`, `done` — as its status vocabulary. Suzy's
agents report a different vocabulary (`running`, `idle`, `waking`,
`sleeping`, `failed`). `theme::status_color()` maps between them by
**semantics**, not by name:

| Suzy status | Token | Hex | Why |
|---|---|---|---|
| `running` | `RUN` | `#5fae74` | Actively doing work — herdr's "run" |
| `idle` | `IDLE` | `#5a615c` | Settled, nothing to do — herdr's "idle" |
| `waking` | `WAIT` | `#d3a027` | Transient, waiting on something to finish — herdr's "wait" |
| `sleeping` | `DONE` | `#6f6a86` | Dormant — closest to herdr's muted "done" hue |
| `failed` | `ERROR` | `#c46a6a` | herdr's site has **no** red/error token — this is a new value, chosen to sit in the same desaturated family as the others rather than importing egui's default red |
| *(unknown)* | — | `FAINT` (`#b0afb6`) | Fallback for an unrecognized status string |

Note the previous code had `running` → gold and `idle` → green, which is
backwards from herdr's naming (herdr's "run" — the working state — is
green). The migration fixes that mismatch as a side effect of adopting
herdr's palette.

herdr additionally distinguishes a `--st-done` text color from the `--done`
fill (`#94e2d5` on dark, `#2aa198` on light) for legibility when "done" text
sits on its own fill color. Suzy doesn't currently render status text on a
`DONE`-filled background, so this distinction wasn't ported — revisit if a
future status badge needs it.

## Chat bubble colors (Suzy-specific — herdr's site has no chat UI)

| Token | Hex | Derivation |
|---|---|---|
| `USER_BUBBLE` | `#282230` | `PANEL` tinted toward `SPOT` — the accent shows through without being a dominant flat fill |
| `ASSISTANT_BUBBLE` | = `MASS` (`#26262b`) | Same raised-surface tone as panels/stat cards |
| `SYSTEM_TEXT` | = `FAINT2` (`#908f96`) | Muted system/notice text |
| `ERROR` | = status `ERROR` (`#c46a6a`) | Shared with the status vocabulary — one red across the app |

## Typography

herdr's site:

```
--disp: "Archivo", system-ui, -apple-system, "Segoe UI", sans-serif;   /* display/headings */
--body: "Inter", system-ui, -apple-system, "Segoe UI", Helvetica, sans-serif; /* body text */
mono:   "JetBrains Mono", ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; /* code/terminal/numeric */
```

All three are vendored under `crates/suzy/assets/fonts/` as OFL-licensed
variable fonts (sourced from the canonical
[google/fonts](https://github.com/google/fonts) repo — `ofl/archivo`,
`ofl/inter`, `ofl/jetbrainsmono` — each directory's `OFL.txt` is copied
alongside its font file for license provenance) and installed by
`theme::install_fonts(ctx)`, called once at startup before `theme::apply`:

| Family | File | Installed as |
|---|---|---|
| Archivo | `Archivo-Variable.ttf` | New named family `FontFamily::Name("Archivo")` (`theme::DISPLAY_FONT`) — headings, `section_label` |
| Inter | `Inter-Variable.ttf` | Inserted ahead of egui's built-in face for `FontFamily::Proportional` — every existing `FontId::proportional`/default-text call site picks it up automatically |
| JetBrains Mono | `JetBrainsMono-Variable.ttf` | Inserted ahead of egui's built-in face for `FontFamily::Monospace` — same free upgrade for `FontId::monospace` call sites (chat code blocks, `create.rs`'s manifest editor, the terminal's rendered glyphs are separate — see below) |

egui's built-in faces stay registered as a fallback after each vendored
font, for glyphs the vendored font doesn't cover (egui's own icon glyphs,
wide unicode ranges).

**Variable-font caveat**: egui's text rasterizer renders a variable font at
its default named instance only (whatever the font's `fvar` table marks
default — Regular weight for all three). It cannot select a different
weight/width axis at runtime, so bold/italic requests for these families
still fall back to egui's built-in faces for that style.

**Not affected**: `terminal.rs`'s pty grid renders its own `FontId::monospace`
sized glyphs for whatever the shell outputs — it inherits JetBrains Mono
like any other monospace call site, but its *content* (colors, glyphs) is
still the ANSI emulation, untouched by this system.

Base body text on herdr's site is ~14.5px at 1.65 line-height; egui's
defaults are close enough that no explicit override was introduced.

## Structural tokens

| Token | Value | Herdr source | Usage |
|---|---|---|---|
| `RADIUS` | `0` (u8) | `--radius-sm/md/lg: 0` | Every corner in Suzy's own chrome (not the ANSI terminal grid) |
| `BORDER_WIDTH` | `1.0` (f32) | 1px dividers throughout site.css | Hairline panel/border strokes |

herdr's site also has a subtle 72px background grid (`--grid`) as a low-key
flourish on the marketing page. Not ported — it reads well behind large
hero sections, less obviously so behind a dense operator console; consider
it only if a specific screen (e.g. the dashboard) wants the texture.
