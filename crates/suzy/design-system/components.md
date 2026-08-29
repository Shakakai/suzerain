# Components

Recurring UI patterns in Suzy today, the tokens/helpers behind them, and
what they replaced. All helpers live in
[`crates/suzy/src/theme.rs`](../src/theme.rs).

## Panel / card frame

The generic "boxed content" pattern: info boxes, copy-paste command blocks,
the secrets-store setup notice, the manifest viewer, the reveal-once secret
dialog.

**Before** (repeated ~6 times across `lib.rs`/`views.rs` with the exact
same literal):

```rust
egui::Frame::new()
    .fill(Color32::from_rgb(0x18, 0x1C, 0x22))
    .corner_radius(6.0)
    .inner_margin(egui::Margin::symmetric(10, 8))
    .show(ui, |ui| { ... });
```

**After:**

```rust
theme::panel_frame().show(ui, |ui| { ... });
```

`panel_frame()` uses `PANEL` fill, a `LINE2` hairline border, and zero
radius (`theme::RADIUS`) — no border was drawn before; adding one is a
deliberate part of adopting herdr's "hairline, not shadow" separation.

## Stat / dashboard tile

The small metric tiles on the Dashboard view ("daemons online", "agents",
per-state counts).

**Before:**

```rust
egui::Frame::new()
    .fill(Color32::from_rgb(0x20, 0x24, 0x2B))
    .corner_radius(6.0)
    .inner_margin(egui::Margin::symmetric(14, 10))
    .show(ui, |ui| { ... });
```

**After:**

```rust
theme::stat_frame().show(ui, |ui| { ... });
```

Uses `MASS` fill (a raised-surface tone, distinct from `PANEL`) so stat
tiles read as slightly elevated compared to a plain info panel.

## Warning / pending frame

Used for the pending-enrollment cards on the Castellans view.

**Before:** a bespoke amber-tinted fill (`0x2A2418`) with the same 6px
radius as every other frame — no visual link to "this needs attention"
beyond the tint.

**After:**

```rust
theme::warning_frame().show(ui, |ui| { ... });
```

Keeps the amber-tinted fill but borders it in `WAIT` (the same amber used
for the "waking" agent status) instead of a neutral line — ties the visual
language of "this needs a decision" together across the app.

## Chat bubbles

User/assistant turns in the agent chat tab.

**Before:**

```rust
const USER_BG: Color32 = Color32::from_rgb(0x2B, 0x3A, 0x55);
const ASSISTANT_BG: Color32 = Color32::from_rgb(0x24, 0x28, 0x30);
...
egui::Frame::new().fill(bg).corner_radius(8.0)...
```

**After:**

```rust
const USER_BG: Color32 = theme::USER_BUBBLE;
const ASSISTANT_BG: Color32 = theme::ASSISTANT_BUBBLE;
...
theme::bubble_frame(bg).show(ui, |ui| { ... });
```

Bubbles are distinguished by fill and layout (right-aligned for the user,
left for the assistant) rather than by a rounded-vs-square shape — both are
now zero-radius, consistent with every other frame in the app.

## Status badge / dot

The `●`/`○` status indicator next to an agent's name (sidebar, agent tab
header, dashboard session list).

**Before:** two independent `match status { ... }` statements — one in
`lib.rs` (`status_color`), a near-duplicate inline in the dashboard
sessions list in `views.rs` — with the agent's `running`/`idle` colors
swapped relative to herdr's naming convention (see `tokens.md`).

**After:** one function, used everywhere an agent status needs a color:

```rust
ui.label(RichText::new(format!("● {}", agent.name)).color(theme::status_color(&agent.status)));
```

`lib.rs::status_color` is kept as a thin `pub(crate)` re-export
(`theme::status_color`) so existing call sites didn't need touching beyond
the one indirection — new code should call `theme::status_color` directly.

Two other `match`-on-string-color functions exist and were **not** folded
into `status_color`, because they classify a different kind of string:
- `views::kind_color` — colors a *log event kind* (`message_end`,
  `crashed`, `spawned`, …), not an agent status.
- `views::action_color` — colors an *audit action* (`agent_create`,
  `daemon_remove`, …).

Where these functions' literals happened to reuse the same green as
`RUN`/`"running"`, they now reference `theme::RUN` directly (one green
value, three call sites) — but the functions themselves stay separate,
since merging them would produce wrong colors for kinds/actions that don't
map onto an agent status (e.g. `order_received`, `secret`-related audit
actions have no status equivalent and keep their own bespoke colors).

## Section headers

Not yet applied anywhere (added as part of this system, not retrofitted
onto existing `ui.heading(...)` calls — those are egui's own bold/large
heading style, which is a different visual role than herdr's small
uppercase tracked nav labels). Use `theme::section_label()` for new
small-caps structural labels:

```rust
ui.label(theme::section_label("castellans"));
// renders "C A S T E L L A N S" (thin-space tracked, uppercase, FAINT color)
```

egui has no native letter-spacing on `RichText` — `section_label` (see the
docstring in `theme.rs`) approximates herdr's `letter-spacing: .07em` by
inserting a thin space (U+2009) between characters. This is a best-effort
visual approximation, not exact CSS parity, and changes the text content —
don't use it for anything that needs to be read back verbatim (screen
readers, copy-paste of the literal string).

## Global chrome (buttons, scrollbars, separators)

Not a call-site pattern — `theme::apply(ctx)` is called once at startup
(`SuzyApp::with_config`) and again whenever the user toggles back to dark
mode, and sets `egui::Visuals` so buttons, checkboxes, combo boxes, and
separators all pick up zero corner radius and `LINE`/`LINE2` hairline
strokes without every widget call site needing an override.
