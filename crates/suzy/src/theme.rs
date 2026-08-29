//! Suzy's design system: color tokens, typography choices, and chrome
//! helpers, ported from herdr's site chassis (herdrdev/herdr,
//! website/css/site.css) into egui/eframe. See
//! `crates/suzy/design-system/` for the full documentation — this module is
//! the single source of truth for the values it documents.
//!
//! Visual language: sharp/brutalist. Zero corner radius everywhere, hairline
//! 1px borders instead of shadows, restrained use of the accent color, and
//! uppercase tracked labels for section headers.

use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Frame, Margin, RichText, Shadow,
    Stroke, Style, Visuals,
};
use std::sync::Arc;

// ── color tokens (dark / "ink" ground — Suzy's default) ────────────────────

pub const BG: Color32 = Color32::from_rgb(0x17, 0x17, 0x1a);
pub const PANEL: Color32 = Color32::from_rgb(0x1e, 0x1e, 0x22);
pub const MASS: Color32 = Color32::from_rgb(0x26, 0x26, 0x2b);
pub const INK: Color32 = Color32::from_rgb(0xea, 0xe8, 0xee);
pub const DIM: Color32 = Color32::from_rgb(0xcd, 0xcc, 0xd2);
pub const FAINT: Color32 = Color32::from_rgb(0xb0, 0xaf, 0xb6);
pub const FAINT2: Color32 = Color32::from_rgb(0x90, 0x8f, 0x96);
pub const LINE: Color32 = Color32::from_rgb(0x26, 0x26, 0x2b);
pub const LINE2: Color32 = Color32::from_rgb(0x35, 0x35, 0x3d);
pub const SPOT: Color32 = Color32::from_rgb(0xcb, 0xa6, 0xf7);
pub const SPOT_INK: Color32 = Color32::from_rgb(0x17, 0x17, 0x1a);

// ── color tokens (light / "paper" ground) ───────────────────────────────────
// Not wired up to the theme toggle yet (Suzy's toggle just flips
// egui::Visuals::light()/dark()) — kept here so a real paper theme is a
// drop-in later instead of a re-derivation. See design-system/tokens.md.

pub const BG_PAPER: Color32 = Color32::from_rgb(0xef, 0xec, 0xe5);
pub const PANEL_PAPER: Color32 = Color32::from_rgb(0xe7, 0xe3, 0xda);
pub const MASS_PAPER: Color32 = Color32::from_rgb(0xdd, 0xd8, 0xcc);
pub const INK_PAPER: Color32 = Color32::from_rgb(0x15, 0x14, 0x0f);
pub const DIM_PAPER: Color32 = Color32::from_rgb(0x55, 0x53, 0x4a);
pub const FAINT_PAPER: Color32 = Color32::from_rgb(0x86, 0x82, 0x6f);
pub const FAINT2_PAPER: Color32 = Color32::from_rgb(0x92, 0x8e, 0x79);
pub const LINE_PAPER: Color32 = Color32::from_rgb(0xe2, 0xde, 0xd4);
pub const LINE2_PAPER: Color32 = Color32::from_rgb(0xcb, 0xc5, 0xb6);
pub const SPOT_PAPER: Color32 = Color32::from_rgb(0x88, 0x39, 0xef);
pub const SPOT_INK_PAPER: Color32 = Color32::WHITE;

// ── status colors (herdr's run/wait/idle/done, shared across grounds) ──────
//
// Suzy's agent statuses are running/idle/sleeping/waking/failed, not
// herdr's run/wait/idle/done — mapped by semantics, not by name:
//   running -> RUN    (actively doing work)
//   idle    -> IDLE   (settled, nothing to do)
//   waking  -> WAIT   (transient, waiting on something to complete)
//   sleeping-> DONE   (dormant; closest to herdr's muted "done" hue)
//   failed  -> ERROR  (herdr's site has no red token; ERROR is a new value
//                      chosen to sit in the same desaturated family as the
//                      others rather than reusing egui's default red)
pub const RUN: Color32 = Color32::from_rgb(0x5f, 0xae, 0x74);
pub const WAIT: Color32 = Color32::from_rgb(0xd3, 0xa0, 0x27);
pub const IDLE: Color32 = Color32::from_rgb(0x5a, 0x61, 0x5c);
pub const DONE: Color32 = Color32::from_rgb(0x6f, 0x6a, 0x86);
pub const ERROR: Color32 = Color32::from_rgb(0xc4, 0x6a, 0x6a);

/// Status-color lookup for an agent's status string. Single source of truth
/// — previously duplicated as independent `match` arms in lib.rs and
/// views.rs (kind_color/action_color are a different, log/audit-kind
/// concept and stay separate).
pub fn status_color(status: &str) -> Color32 {
    match status {
        "running" => RUN,
        "idle" => IDLE,
        "waking" => WAIT,
        "sleeping" => DONE,
        "failed" => ERROR,
        _ => FAINT,
    }
}

/// Color for a daemon's online/offline dot — the same `●` status-dot
/// pattern used for agent status (`status_color`), applied to the
/// online/offline vocabulary instead (there's no "waking"/"failed" for a
/// daemon connection, just up or down). `DONE`'s "dormant" role fits
/// offline better than `IDLE`'s "settled, nothing to do" — an offline
/// daemon isn't idly available, it's not connected at all.
pub fn online_color(online: bool) -> Color32 {
    if online {
        RUN
    } else {
        DONE
    }
}

// ── chat bubble colors ──────────────────────────────────────────────────────
// herdr's site has no chat-bubble concept; these are Suzy-specific, chosen
// to sit inside the same neutral/spot palette rather than reusing the old
// ad hoc blues. See design-system/components.md.

/// User bubble: PANEL tinted toward SPOT, so the accent shows through
/// without the bubble itself becoming a dominant fill (herdr uses SPOT
/// sparingly — hover/active states, not large surfaces, never a big flat
/// fill).
pub const USER_BUBBLE: Color32 = Color32::from_rgb(0x28, 0x22, 0x30);
/// Assistant bubble: MASS, the same raised-surface tone used for panels.
pub const ASSISTANT_BUBBLE: Color32 = MASS;
pub const SYSTEM_TEXT: Color32 = FAINT2;

// ── typography ───────────────────────────────────────────────────────────
//
// herdr's site: --disp: "Archivo" (headings), --body: "Inter" (body text),
// mono: "JetBrains Mono" (code/terminal/numeric). All three are vendored as
// OFL-licensed variable fonts under `crates/suzy/assets/fonts/` (sourced
// from the canonical google/fonts repo — see the OFL-*.txt license files
// alongside them) and installed by `install_fonts` below.
//
// Variable-font caveat: egui's text rasterizer renders a variable font at
// its default named instance (whatever the font's `fvar` table marks as
// default — Regular weight for all three here). It does not support
// selecting a different weight/width axis at runtime, so bold/italic
// requests still fall back to egui's built-in faces for those styles.

/// Name of the display font family (headings), installed by `install_fonts`.
/// Not one of egui's two built-in families (`Proportional`/`Monospace`), so
/// it's addressed by name: `FontFamily::Name(DISPLAY_FONT.into())`.
pub const DISPLAY_FONT: &str = "Archivo";

/// Install Archivo (display), Inter (body/proportional), and JetBrains Mono
/// (monospace) into `ctx`'s font atlas. Call once at startup, before
/// `apply` — unlike `apply` (visuals only, cheap, called again on every
/// light/dark toggle), reinstalling fonts rebuilds the glyph atlas and
/// should not be repeated per-frame or per-toggle.
///
/// Inter and JetBrains Mono are inserted ahead of egui's built-in faces for
/// the `Proportional`/`Monospace` families (so existing `FontId::proportional`
/// / `FontId::monospace` call sites everywhere in Suzy pick them up for
/// free), with the built-ins kept as a fallback for glyphs Inter/JetBrains
/// Mono don't cover (egui's own icon glyphs, wide unicode ranges, etc).
/// Archivo is installed as a new named family for headings/`section_label`.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    fonts.font_data.insert(
        "Inter".to_owned(),
        Arc::new(FontData::from_static(include_bytes!(
            "../assets/fonts/Inter-Variable.ttf"
        ))),
    );
    fonts.font_data.insert(
        "JetBrainsMono".to_owned(),
        Arc::new(FontData::from_static(include_bytes!(
            "../assets/fonts/JetBrainsMono-Variable.ttf"
        ))),
    );
    fonts.font_data.insert(
        DISPLAY_FONT.to_owned(),
        Arc::new(FontData::from_static(include_bytes!(
            "../assets/fonts/Archivo-Variable.ttf"
        ))),
    );

    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "Inter".to_owned());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, "JetBrainsMono".to_owned());
    // Archivo is a brand-new named family (unlike Proportional/Monospace
    // above, there's no pre-existing entry to prepend to), so its fallback
    // chain has to be spelled out explicitly — otherwise a heading with an
    // emoji/symbol (e.g. "👑 Suzy") would render tofu for any glyph Archivo
    // doesn't cover. Same fallback fonts egui's default Proportional family
    // ships with.
    fonts.families.insert(
        FontFamily::Name(DISPLAY_FONT.into()),
        vec![
            DISPLAY_FONT.to_owned(),
            "Ubuntu-Light".to_owned(),
            "NotoEmoji-Regular".to_owned(),
            "emoji-icon-font".to_owned(),
        ],
    );

    ctx.set_fonts(fonts);
}

/// Base radius for all chrome: herdr's site is sharp/brutalist (`--radius-*:
/// 0` in site.css) — no rounded corners anywhere in Suzy's own chrome.
pub const RADIUS: u8 = 0;

/// Hairline border width, matching herdr's 1px dividers.
pub const BORDER_WIDTH: f32 = 1.0;

// ── global style ─────────────────────────────────────────────────────────

/// Apply the brutalist chrome defaults globally: zero corner radius on every
/// widget/window/panel, hairline LINE/LINE2 strokes, and the dark palette
/// above. Call once at startup (`theme::apply(&cc.egui_ctx)`); per-call-site
/// frames (`panel_frame`, `bubble`) layer on top of this for fills.
pub fn apply(ctx: &egui::Context) {
    let mut visuals = Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = BG;
    visuals.faint_bg_color = PANEL;
    visuals.override_text_color = None;
    visuals.widgets.noninteractive.bg_fill = PANEL;
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, DIM);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(BORDER_WIDTH, LINE);
    visuals.widgets.inactive.bg_fill = MASS;
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, DIM);
    visuals.widgets.inactive.bg_stroke = Stroke::new(BORDER_WIDTH, LINE);
    visuals.widgets.hovered.bg_fill = MASS;
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, INK);
    visuals.widgets.hovered.bg_stroke = Stroke::new(BORDER_WIDTH, SPOT);
    visuals.widgets.active.bg_fill = SPOT;
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, SPOT_INK);
    visuals.widgets.active.bg_stroke = Stroke::new(BORDER_WIDTH, SPOT);
    // "open" backs a `Window`'s title bar and an expanded ComboBox/menu —
    // left at egui's stock light-gray default otherwise, which looks like a
    // foreign OS window bolted onto Suzy's dark chrome (`egui::Window`'s
    // header fill is `visuals.widgets.open.weak_bg_fill` when the caller
    // doesn't pass a custom `Frame`, which every dialog in this app doesn't).
    visuals.widgets.open.weak_bg_fill = PANEL;
    visuals.widgets.open.bg_fill = PANEL;
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, INK);
    visuals.widgets.open.bg_stroke = Stroke::new(BORDER_WIDTH, LINE2);
    visuals.selection.bg_fill = SPOT;
    visuals.selection.stroke = Stroke::new(1.0, SPOT_INK);
    visuals.hyperlink_color = SPOT;
    // Hairline borders, not shadows (see module docs) — egui's defaults
    // draw a soft blurred shadow behind every `Window` and popup/menu.
    visuals.window_shadow = Shadow::NONE;
    visuals.popup_shadow = Shadow::NONE;

    // Zero corner radius everywhere (brutalist chrome).
    let flat = CornerRadius::from(RADIUS);
    visuals.window_corner_radius = flat;
    visuals.menu_corner_radius = flat;
    visuals.widgets.noninteractive.corner_radius = flat;
    visuals.widgets.inactive.corner_radius = flat;
    visuals.widgets.hovered.corner_radius = flat;
    visuals.widgets.active.corner_radius = flat;
    visuals.widgets.open.corner_radius = flat;

    ctx.set_visuals(visuals);

    let mut style = Style::default();
    style.visuals = ctx.style().visuals.clone();
    ctx.set_style(style);
}

/// Standard panel/card frame: PANEL fill, hairline LINE2 border, zero
/// radius. Replaces the repeated
/// `egui::Frame::new().fill(Color32::from_rgb(0x18,0x1C,0x22)).corner_radius(6.0)`
/// call sites across lib.rs/views.rs.
pub fn panel_frame() -> Frame {
    Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(BORDER_WIDTH, LINE2))
        .corner_radius(CornerRadius::from(RADIUS))
        .inner_margin(Margin::symmetric(10, 8))
}

/// A warning/pending variant of `panel_frame` (used for pending-enrollment
/// cards, previously a bespoke amber-tinted fill).
pub fn warning_frame() -> Frame {
    Frame::new()
        .fill(Color32::from_rgb(0x2a, 0x24, 0x18))
        .stroke(Stroke::new(BORDER_WIDTH, WAIT))
        .corner_radius(CornerRadius::from(RADIUS))
        .inner_margin(Margin::symmetric(10, 8))
}

/// A stat-card frame (dashboard tiles): MASS fill, otherwise identical to
/// `panel_frame`.
pub fn stat_frame() -> Frame {
    Frame::new()
        .fill(MASS)
        .stroke(Stroke::new(BORDER_WIDTH, LINE2))
        .corner_radius(CornerRadius::from(RADIUS))
        .inner_margin(Margin::symmetric(14, 10))
}

/// A chat bubble frame with the given fill (see `USER_BUBBLE`/
/// `ASSISTANT_BUBBLE`). Zero radius, no border — bubbles are distinguished
/// by fill and layout (left/right), not by outline.
pub fn bubble_frame(fill: Color32) -> Frame {
    Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::from(RADIUS))
        .inner_margin(Margin::symmetric(10, 8))
}

/// Section-header label: uppercase, small, tracked. egui has no native
/// letter-spacing on `RichText` — this approximates herdr's
/// `letter-spacing: .07em` uppercase nav labels by inserting a thin space
/// (U+2009) between characters, which reads as tracked at small sizes
/// without the broken look of padding with regular spaces. Only use this
/// for short labels (section headers, nav items) — it's not appropriate for
/// body text or anything a screen reader needs read verbatim, since the
/// inserted thin spaces change the text content.
pub fn section_label(text: &str) -> RichText {
    let tracked: String = text
        .to_uppercase()
        .chars()
        .enumerate()
        .flat_map(|(i, c)| if i == 0 { vec![c] } else { vec!['\u{2009}', c] })
        .collect();
    RichText::new(tracked)
        .size(11.5)
        .color(FAINT)
        .family(FontFamily::Name(DISPLAY_FONT.into()))
}

/// Page-level heading text: Archivo (the documented display face), `INK`,
/// and the same 18px size `egui::Ui::heading` uses for `TextStyle::Heading`
/// — use `ui.label(theme::heading("Fleet"))` in place of `ui.heading(...)`
/// so headings actually render in the display font instead of falling back
/// to the body proportional font (Inter).
pub fn heading(text: impl Into<String>) -> RichText {
    RichText::new(text)
        .size(18.0)
        .color(INK)
        .family(FontFamily::Name(DISPLAY_FONT.into()))
}

/// Preconfigures an `egui::Window` as a centered, non-draggable modal — the
/// treatment every confirm/create/add dialog in the app should use, instead
/// of a loose floating window that can be dragged off-screen with nothing
/// indicating the rest of the app is inert while it's open. Chain further
/// builder calls (`.open(&mut open)`, `.resizable(false)`, ...) after this.
/// Pair with `modal_scrim(ctx)`, called once right before `.show(ctx, ...)`.
pub fn modal_window(title: &str) -> egui::Window<'static> {
    egui::Window::new(title)
        .collapsible(false)
        .movable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
}

/// Dims the rest of the app behind a modal window and swallows clicks so
/// they don't fall through to whatever's underneath. Call once per open
/// modal, before that modal's `egui::Window::show`.
pub fn modal_scrim(ctx: &egui::Context) {
    egui::Area::new(egui::Id::new("suzy_modal_scrim"))
        .order(egui::Order::Middle)
        .fixed_pos(egui::Pos2::ZERO)
        .show(ctx, |ui| {
            let screen = ui.ctx().screen_rect();
            ui.painter()
                .rect_filled(screen, 0.0, Color32::from_black_alpha(140));
            ui.allocate_rect(screen, egui::Sense::click());
        });
}
