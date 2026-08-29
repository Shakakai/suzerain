//! Suzy design-system kitchen sink.
//!
//! A standalone reference app that renders every styled token and chrome
//! helper from `suzy::theme` so the design system stays visible and honest —
//! this never hand-derives a color or radius, it only calls into `theme::`,
//! so it can't drift from the real values used by the app. See
//! `crates/suzy/design-system/` for the written documentation this mirrors.
//!
//! Run with `cargo run -p suzy --bin kitchen-sink`.

use eframe::egui;
use egui::{Color32, RichText, Vec2};
use suzy::theme;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 900.0])
            .with_title("Suzy — Kitchen Sink"),
        ..Default::default()
    };
    eframe::run_native(
        "Suzy — Kitchen Sink",
        options,
        Box::new(|cc| {
            theme::install_fonts(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(KitchenSinkApp))
        }),
    )
}

struct KitchenSinkApp;

impl eframe::App for KitchenSinkApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(8.0);
                ui.label(
                    RichText::new("Suzy Design System — Kitchen Sink")
                        .size(22.0)
                        .strong()
                        .color(theme::INK),
                );
                ui.label(
                    RichText::new("Every token and chrome helper in theme.rs, rendered live.")
                        .color(theme::FAINT),
                );
                ui.add_space(16.0);

                section(ui, "Typography", typography);
                section(ui, "Color tokens", color_tokens);
                section(ui, "Status colors", status_colors);
                section(ui, "Panel frames", panel_frames);
                section(ui, "Chat bubbles", chat_bubbles);
                section(ui, "Buttons & widgets", widgets);
                section(ui, "Section labels", section_labels);

                ui.add_space(24.0);
            });
        });
    }
}

fn section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.label(theme::section_label(title));
    ui.add_space(6.0);
    theme::panel_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        body(ui);
    });
    ui.add_space(20.0);
}

fn swatch(ui: &mut egui::Ui, name: &str, color: Color32) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(48.0, 24.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 0.0, color);
        ui.painter().rect_stroke(
            rect,
            0.0,
            egui::Stroke::new(1.0, theme::LINE2),
            egui::StrokeKind::Outside,
        );
        ui.add_space(8.0);
        ui.label(RichText::new(name).color(theme::DIM).monospace());
        ui.label(
            RichText::new(format!(
                "#{:02X}{:02X}{:02X}",
                color.r(),
                color.g(),
                color.b()
            ))
            .color(theme::FAINT2)
            .monospace()
            .size(11.0),
        );
    });
}

fn typography(ui: &mut egui::Ui) {
    ui.label(
        RichText::new("Archivo — display")
            .size(22.0)
            .color(theme::INK)
            .family(egui::FontFamily::Name(theme::DISPLAY_FONT.into())),
    );
    ui.label(
        RichText::new("Inter — body text, the default proportional face")
            .size(15.0)
            .color(theme::INK),
    );
    ui.label(
        RichText::new("JetBrains Mono — code, terminal, numeric")
            .monospace()
            .color(theme::INK),
    );
}

fn color_tokens(ui: &mut egui::Ui) {
    ui.columns(2, |cols| {
        cols[0].label(RichText::new("Dark (ink)").strong().color(theme::INK));
        for (name, c) in [
            ("BG", theme::BG),
            ("PANEL", theme::PANEL),
            ("MASS", theme::MASS),
            ("INK", theme::INK),
            ("DIM", theme::DIM),
            ("FAINT", theme::FAINT),
            ("FAINT2", theme::FAINT2),
            ("LINE", theme::LINE),
            ("LINE2", theme::LINE2),
            ("SPOT", theme::SPOT),
            ("SPOT_INK", theme::SPOT_INK),
        ] {
            swatch(&mut cols[0], name, c);
        }

        cols[1].label(RichText::new("Light (paper)").strong().color(theme::INK));
        for (name, c) in [
            ("BG_PAPER", theme::BG_PAPER),
            ("PANEL_PAPER", theme::PANEL_PAPER),
            ("MASS_PAPER", theme::MASS_PAPER),
            ("INK_PAPER", theme::INK_PAPER),
            ("DIM_PAPER", theme::DIM_PAPER),
            ("FAINT_PAPER", theme::FAINT_PAPER),
            ("FAINT2_PAPER", theme::FAINT2_PAPER),
            ("LINE_PAPER", theme::LINE_PAPER),
            ("LINE2_PAPER", theme::LINE2_PAPER),
            ("SPOT_PAPER", theme::SPOT_PAPER),
            ("SPOT_INK_PAPER", theme::SPOT_INK_PAPER),
        ] {
            swatch(&mut cols[1], name, c);
        }
    });
}

fn status_colors(ui: &mut egui::Ui) {
    for status in ["running", "idle", "waking", "sleeping", "failed", "unknown"] {
        ui.horizontal(|ui| {
            let color = theme::status_color(status);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(12.0, 12.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 6.0, color);
            ui.label(RichText::new(status).color(theme::DIM).monospace());
        });
    }
}

fn panel_frames(ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        theme::panel_frame().show(ui, |ui| {
            ui.set_min_size(Vec2::new(180.0, 60.0));
            ui.label(RichText::new("panel_frame()").color(theme::INK));
            ui.label(RichText::new("PANEL fill, LINE2 border").color(theme::FAINT2).size(11.0));
        });
        theme::stat_frame().show(ui, |ui| {
            ui.set_min_size(Vec2::new(180.0, 60.0));
            ui.label(RichText::new("stat_frame()").color(theme::INK));
            ui.label(RichText::new("MASS fill, dashboard tiles").color(theme::FAINT2).size(11.0));
        });
        theme::warning_frame().show(ui, |ui| {
            ui.set_min_size(Vec2::new(180.0, 60.0));
            ui.label(RichText::new("warning_frame()").color(theme::INK));
            ui.label(RichText::new("amber border, pending state").color(theme::FAINT2).size(11.0));
        });
    });
}

fn chat_bubbles(ui: &mut egui::Ui) {
    theme::bubble_frame(theme::USER_BUBBLE).show(ui, |ui| {
        ui.label(RichText::new("User bubble — USER_BUBBLE fill").color(theme::INK));
    });
    ui.add_space(6.0);
    theme::bubble_frame(theme::ASSISTANT_BUBBLE).show(ui, |ui| {
        ui.label(RichText::new("Assistant bubble — ASSISTANT_BUBBLE (= MASS) fill").color(theme::INK));
    });
    ui.add_space(6.0);
    theme::bubble_frame(theme::PANEL).show(ui, |ui| {
        ui.label(RichText::new("System text — SYSTEM_TEXT color").color(theme::SYSTEM_TEXT));
    });
    ui.add_space(6.0);
    theme::bubble_frame(theme::PANEL).show(ui, |ui| {
        ui.label(RichText::new("Error — ERROR color").color(theme::ERROR));
    });
}

fn widgets(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        let _ = ui.button("Default button");
        let _ = ui.button("Hover me");
        ui.checkbox(&mut true.clone(), "Checkbox");
        let _ = ui.radio(true, "Radio");
    });
    ui.add_space(8.0);
    ui.label("Zero corner radius, hairline borders applied globally via theme::apply(ctx).");
    ui.separator();
    let mut text = String::from("Text input");
    ui.text_edit_singleline(&mut text);
}

fn section_labels(ui: &mut egui::Ui) {
    ui.label(theme::section_label("section header"));
    ui.add_space(4.0);
    ui.label(theme::section_label("Workspaces"));
    ui.add_space(4.0);
    ui.label(theme::section_label("Agents & Sessions"));
}
