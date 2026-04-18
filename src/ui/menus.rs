//! Top menubar layout. Sub-phase 1 ships an empty shell — items render
//! but have no actions wired. Sub-phase 2+ will take a `&mut Vec<UiCommand>`
//! and push commands when items are clicked.

use egui::{MenuBar, Panel, Ui};

pub fn build_top_menubar(ui: &mut Ui) {
    Panel::top("vibenes.menubar").show_inside(ui, |ui| {
        MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                ui.label("Open ROM…");
                ui.label("Recent ROMs");
                ui.separator();
                ui.label("Quit");
            });
            ui.menu_button("Emulation", |ui| {
                ui.label("Pause");
                ui.label("Reset");
                ui.separator();
                ui.label("Region override");
            });
            ui.menu_button("Video", |ui| {
                ui.label("Scale");
                ui.label("VSync");
            });
            ui.menu_button("Audio", |ui| {
                ui.label("Mute");
                ui.label("Volume");
            });
            ui.menu_button("Debug", |ui| {
                ui.label("Debug panel");
            });
        });
    });
}
