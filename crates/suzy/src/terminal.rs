//! Terminal widget (M4): an `alacritty_terminal` grid rendered with egui —
//! full VT emulation for the agent VM shell. The widget owns the terminal
//! state machine; transport lives in net.rs (WebSocket → ShellMessage).

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use egui::{Color32, FontId, Key, Pos2, Rect, Ui, Vec2};

/// Input produced by the widget, sent to the shell transport.
#[derive(Debug, Clone)]
pub enum TermInput {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

#[derive(Clone, Default)]
struct VoidListener;
impl EventListener for VoidListener {
    fn send_event(&self, _event: Event) {}
}

/// Extra scrollback rows retained beyond the visible screen, so content that
/// scrolls off-screen is kept in `term.grid().history_size()` instead of
/// being discarded immediately.
const SCROLLBACK_ROWS: usize = 2000;

#[derive(Clone, Copy)]
struct TermSize {
    cols: usize,
    lines: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.lines + SCROLLBACK_ROWS
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub struct Terminal {
    term: Term<VoidListener>,
    processor: Processor,
    size: TermSize,
    font: FontId,
}

impl Default for Terminal {
    fn default() -> Self {
        let size = TermSize {
            cols: 80,
            lines: 24,
        };
        Self {
            term: Term::new(TermConfig::default(), &size, VoidListener),
            processor: Processor::new(),
            size,
            font: FontId::monospace(13.0),
        }
    }
}

impl Terminal {
    /// Feed raw pty output bytes into the terminal state machine.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    /// Write a line of system text (connection notices) into the grid so it
    /// scrolls with the transcript.
    pub fn write_system(&mut self, text: &str) {
        for line in text.split('\n') {
            let mut bytes = format!("\x1b[2m{line}\x1b[0m").into_bytes();
            bytes.extend_from_slice(b"\r\n");
            self.feed(&bytes);
        }
    }

    /// Grid contents as plain text (test support): one line per screen row,
    /// trailing whitespace trimmed.
    pub fn screen_text(&self) -> String {
        let mut lines: std::collections::BTreeMap<i32, String> = Default::default();
        for indexed in self.term.grid().display_iter() {
            let line = lines.entry(indexed.point.line.0).or_default();
            let col = indexed.point.column.0;
            while line.chars().count() < col {
                line.push(' ');
            }
            line.push(indexed.cell.c);
        }
        lines
            .values()
            .map(|l| l.trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Render into `ui`; returns inputs to send to the shell (keyboard +
    /// resize). The widget grabs focus on click.
    pub fn render(&mut self, ui: &mut Ui) -> Vec<TermInput> {
        let mut out = Vec::new();

        // Cell metrics from the monospace font.
        let (cell_w, cell_h) = ui.fonts(|f| {
            (
                f.glyph_width(&self.font, 'M').max(1.0),
                f.row_height(&self.font).max(1.0),
            )
        });
        let avail = ui.available_size();
        let cols = ((avail.x / cell_w) as usize).clamp(20, 500);
        let lines = ((avail.y / cell_h) as usize).clamp(4, 200);
        if cols != self.size.cols || lines != self.size.lines {
            self.size = TermSize { cols, lines };
            self.term.resize(self.size);
            out.push(TermInput::Resize {
                cols: cols as u16,
                rows: lines as u16,
            });
        }

        let size_px = Vec2::new(cols as f32 * cell_w, lines as f32 * cell_h);
        let (rect, response) = ui.allocate_exact_size(size_px, egui::Sense::click());
        if response.clicked() {
            response.request_focus();
        }
        let focused = response.has_focus();

        // Mouse-wheel scrollback: scroll the terminal's display offset into
        // history when hovering over the widget, instead of discarding
        // scrolled-off rows.
        if response.hovered() {
            let scroll_rows = ui.input(|i| i.smooth_scroll_delta.y) / cell_h;
            if scroll_rows.abs() >= 1.0 {
                self.term
                    .scroll_display(Scroll::Delta(scroll_rows.round() as i32));
            }
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 2.0, Color32::from_rgb(0x0D, 0x10, 0x14));

        // Grid → styled text runs (one painter.text call per run).
        let grid = self.term.grid();
        let mut cur_line: i32 = i32::MIN;
        let mut run = String::new();
        let mut run_fg = Color32::WHITE;
        let mut run_bg = Color32::TRANSPARENT;
        let mut run_col = 0usize;
        let flush = |painter: &egui::Painter,
                     line: i32,
                     col: usize,
                     run: &mut String,
                     fg: Color32,
                     bg: Color32,
                     font: &FontId| {
            if run.is_empty() {
                return;
            }
            let pos = Pos2::new(
                rect.min.x + col as f32 * cell_w,
                rect.min.y + line as f32 * cell_h,
            );
            if bg != Color32::TRANSPARENT {
                painter.rect_filled(
                    Rect::from_min_size(
                        pos,
                        Vec2::new(run.chars().count() as f32 * cell_w, cell_h),
                    ),
                    0.0,
                    bg,
                );
            }
            painter.text(pos, egui::Align2::LEFT_TOP, run.as_str(), font.clone(), fg);
            run.clear();
        };

        for indexed in grid.display_iter() {
            let line = indexed.point.line.0;
            let col = indexed.point.column.0;
            let cell = indexed.cell;
            if line != cur_line {
                flush(
                    &painter, cur_line, run_col, &mut run, run_fg, run_bg, &self.font,
                );
                cur_line = line;
                run_col = 0;
            }
            let (mut fg, mut bg) = (color_to_egui(cell.fg, true), color_to_egui(cell.bg, false));
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if cell.flags.contains(Flags::DIM) {
                fg = fg.gamma_multiply(0.6);
            }
            if col != run_col || fg != run_fg || bg != run_bg {
                flush(
                    &painter, cur_line, run_col, &mut run, run_fg, run_bg, &self.font,
                );
                run_fg = fg;
                run_bg = bg;
                run_col = col;
            }
            run.push(cell.c);
            for &zw in cell.zerowidth().unwrap_or(&[]) {
                run.push(zw);
            }
        }
        flush(
            &painter, cur_line, run_col, &mut run, run_fg, run_bg, &self.font,
        );

        // Cursor.
        if self.term.mode().contains(TermMode::SHOW_CURSOR) && focused {
            let p = grid.cursor.point;
            if p.line.0 >= 0 && (p.line.0 as usize) < lines {
                let pos = Pos2::new(
                    rect.min.x + p.column.0 as f32 * cell_w,
                    rect.min.y + p.line.0 as f32 * cell_h,
                );
                painter.rect_filled(
                    Rect::from_min_size(pos, Vec2::new(cell_w, cell_h)),
                    1.0,
                    Color32::from_rgba_unmultiplied(0xCC, 0xCC, 0xCC, 0x66),
                );
            }
        }

        // Keyboard input (only while focused).
        if focused {
            let events = ui.input(|i| i.events.clone());
            for event in events {
                match event {
                    egui::Event::Text(text) => {
                        out.push(TermInput::Data(text.into_bytes()));
                    }
                    egui::Event::Paste(text) => {
                        out.push(TermInput::Data(text.replace('\n', "\r").into_bytes()));
                    }
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        let bytes = key_to_bytes(key, &modifiers);
                        if !bytes.is_empty() {
                            out.push(TermInput::Data(bytes));
                        }
                    }
                    _ => {}
                }
            }
        }

        out
    }
}

pub fn key_to_bytes(key: Key, modifiers: &egui::Modifiers) -> Vec<u8> {
    // Ctrl+letter → control byte.
    if modifiers.ctrl || modifiers.mac_cmd {
        if let Key::A = key { /* fall through to letter map */ }
        let idx = match key {
            Key::A => Some(0x01u8),
            Key::B => Some(0x02),
            Key::C => Some(0x03),
            Key::D => Some(0x04),
            Key::E => Some(0x05),
            Key::F => Some(0x06),
            Key::G => Some(0x07),
            Key::H => Some(0x08),
            Key::K => Some(0x0B),
            Key::L => Some(0x0C),
            Key::N => Some(0x0E),
            Key::P => Some(0x10),
            Key::Q => Some(0x11),
            Key::R => Some(0x12),
            Key::T => Some(0x14),
            Key::U => Some(0x15),
            Key::W => Some(0x17),
            Key::Z => Some(0x1A),
            _ => None,
        };
        if let Some(b) = idx {
            return vec![b];
        }
    }
    match key {
        Key::Enter => b"\r".to_vec(),
        Key::Backspace => vec![0x7F],
        Key::Tab => {
            if modifiers.shift {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        Key::Escape => vec![0x1B],
        Key::ArrowUp => b"\x1b[A".to_vec(),
        Key::ArrowDown => b"\x1b[B".to_vec(),
        Key::ArrowRight => b"\x1b[C".to_vec(),
        Key::ArrowLeft => b"\x1b[D".to_vec(),
        Key::Home => b"\x1b[H".to_vec(),
        Key::End => b"\x1b[F".to_vec(),
        Key::PageUp => b"\x1b[5~".to_vec(),
        Key::PageDown => b"\x1b[6~".to_vec(),
        Key::Delete => b"\x1b[3~".to_vec(),
        Key::Backslash if modifiers.ctrl => vec![0x1C],
        _ => Vec::new(),
    }
}

// ── colors ───────────────────────────────────────────────────────────────

const PALETTE: [Color32; 16] = [
    Color32::from_rgb(0x1D, 0x1F, 0x21), // black
    Color32::from_rgb(0xCC, 0x66, 0x66), // red
    Color32::from_rgb(0xB5, 0xBD, 0x68), // green
    Color32::from_rgb(0xF0, 0xC6, 0x74), // yellow
    Color32::from_rgb(0x81, 0xA2, 0xBE), // blue
    Color32::from_rgb(0xB2, 0x94, 0xBB), // magenta
    Color32::from_rgb(0x8A, 0xBE, 0xB7), // cyan
    Color32::from_rgb(0xC5, 0xC8, 0xC6), // white
    Color32::from_rgb(0x66, 0x68, 0x6A), // bright black
    Color32::from_rgb(0xDE, 0x93, 0x5F), // bright red
    Color32::from_rgb(0xB5, 0xBD, 0x68), // bright green
    Color32::from_rgb(0xF0, 0xC6, 0x74), // bright yellow
    Color32::from_rgb(0x81, 0xA2, 0xBE), // bright blue
    Color32::from_rgb(0xB2, 0x94, 0xBB), // bright magenta
    Color32::from_rgb(0x8A, 0xBE, 0xB7), // bright cyan
    Color32::from_rgb(0xFF, 0xFF, 0xFF), // bright white
];

const FG_DEFAULT: Color32 = Color32::from_rgb(0xC5, 0xC8, 0xC6);
const BG_DEFAULT: Color32 = Color32::TRANSPARENT;

fn color_to_egui(color: Color, is_fg: bool) -> Color32 {
    match color {
        Color::Spec(Rgb { r, g, b }) => Color32::from_rgb(r, g, b),
        Color::Indexed(i) => indexed_color(i),
        Color::Named(named) => match named {
            NamedColor::Foreground => FG_DEFAULT,
            NamedColor::Background => BG_DEFAULT,
            other => {
                let idx = other as usize;
                let c = PALETTE.get(idx).copied().unwrap_or(FG_DEFAULT);
                if !is_fg && c == BG_DEFAULT {
                    Color32::TRANSPARENT
                } else {
                    c
                }
            }
        },
    }
}

fn indexed_color(i: u8) -> Color32 {
    match i {
        0..=15 => PALETTE[i as usize],
        16..=231 => {
            let i = i - 16;
            let r = cube(i / 36);
            let g = cube((i % 36) / 6);
            let b = cube(i % 6);
            Color32::from_rgb(r, g, b)
        }
        _ => {
            let v = 8 + (i.saturating_sub(232)) * 10;
            Color32::from_rgb(v, v, v)
        }
    }
}

fn cube(v: u8) -> u8 {
    if v == 0 {
        0
    } else {
        55 + v * 40
    }
}

// Keep the compiler honest about unused imports in case of refactors.
#[allow(dead_code)]
fn _type_markers(_: Column, _: Line) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_size_reports_scrollback_capacity() {
        let size = TermSize {
            cols: 80,
            lines: 24,
        };
        assert!(size.total_lines() > size.screen_lines());
        assert_eq!(size.total_lines(), size.lines + SCROLLBACK_ROWS);
        assert_eq!(size.screen_lines(), 24);
    }
}
