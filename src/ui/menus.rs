//! Top menubar layout. Widgets here push `UiCommand` variants into the
//! caller's scratch `Vec`; the host drains and applies them after the
//! paint pass. Sub-phase 2 wires File → Open / Recent / Quit. Other
//! menus are still placeholder labels.

use egui::{MenuBar, Panel, Ui};

use crate::ui::{RecentRoms, UiCommand};

/// Returns the menubar's rendered height in logical points so the
/// caller can reserve that strip of the swapchain and letterbox the
/// NES render below it.
pub fn build_top_menubar(
    ui: &mut Ui,
    recent: &RecentRoms,
    cmds: &mut Vec<UiCommand>,
) -> f32 {
    let response = Panel::top("vibenes.menubar").show_inside(ui, |ui| {
        MenuBar::new().ui(ui, |ui| {
            file_menu(ui, recent, cmds);
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
    response.response.rect.height()
}

fn file_menu(ui: &mut Ui, recent: &RecentRoms, cmds: &mut Vec<UiCommand>) {
    ui.menu_button("File", |ui| {
        if ui.button("Open ROM…").clicked() {
            cmds.push(UiCommand::OpenRomDialog);
            ui.close();
        }
        ui.menu_button("Recent ROMs", |ui| {
            if recent.is_empty() {
                ui.add_enabled(false, egui::Label::new("(none yet)"));
                return;
            }
            for path in recent.iter() {
                // File name is more legible than the full path; the
                // full path is available as a hover tooltip for
                // disambiguating two ROMs with the same filename.
                let label = path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                let resp = ui.button(label).on_hover_text(path.display().to_string());
                if resp.clicked() {
                    cmds.push(UiCommand::OpenRom(path.clone()));
                    ui.close();
                }
            }
        });
        ui.separator();
        if ui.button("Quit").clicked() {
            cmds.push(UiCommand::Quit);
            ui.close();
        }
    });
}
