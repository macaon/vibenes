// SPDX-License-Identifier: GPL-3.0-or-later
//! Top window-chrome menu bar.
//!
//! Drawn as an `egui::TopBottomPanel::top` above the NES viewport
//! when [`MenuBarParams::visible`] is true and the app isn't in
//! fullscreen. The host's [`crate::ui::UiLayer::run`] decides
//! whether to call us each frame; we only render the panel itself
//! and emit [`UiCommand`]s for menu selections.
//!
//! Coexists with the F1 OSD - both reach the same actions through
//! the same command enum, so a "Reset" via the menu strip and a
//! "Reset" via the OSD do exactly the same thing. The strip is
//! the mouse-discoverable path; the OSD remains the gamepad-
//! driven path.
//!
//! ## Window-chrome rule
//!
//! The menu bar is window chrome - the NES viewport's pixel scale
//! is set independently. When the menu is visible, the window's
//! total height grows by [`MENU_BAR_HEIGHT_LOGICAL`] so the NES
//! image stays at exactly `scale * 240` (NTSC) regardless. When
//! hidden, the window shrinks back. See [`crate::main`] window-
//! sizing call sites for the math.

use std::path::{Path, PathBuf};

use egui::Panel;

use crate::shader_catalog::{Catalog, ShaderSource};
use crate::ui::commands::UiCommand;
use crate::ui::recent::RecentRoms;
use crate::ui::recent_shaders::RecentShaders;
use crate::video::{ParMode, PixelAspectRatio};

/// Logical pixel height reserved for the top menu bar. Matches
/// egui's default `interact_size.y` plus its top/bottom item
/// spacing so the strip lays out without clipping. The window
/// sizing in `crate::main` adds this to the NES viewport height
/// when the bar is visible.
pub const MENU_BAR_HEIGHT_LOGICAL: f32 = 24.0;

/// Per-frame inputs the menu strip needs to render dynamic state
/// (Recent submenu, scale checkmark on the active scale, FDS
/// item gray-state, etc.). Borrowed - the menu builder is
/// stateless beyond what's in here.
pub struct MenuBarParams<'a> {
    pub visible: bool,
    pub fullscreen: bool,
    pub nes_loaded: bool,
    pub current_scale: u8,
    pub current_par: ParMode,
    pub region_label: Option<&'static str>,
    pub mapper_label: Option<String>,
    pub fds_present: bool,
    pub recent: &'a RecentRoms,
    /// Discovered shader presets (bundled + user). The View > Shader
    /// submenu lists these grouped by source. Empty catalog renders
    /// only the "None" entry.
    pub shader_catalog: &'a Catalog,
    /// Path of the currently-active shader preset, if any. Used to
    /// draw the active-state checkmark on the matching menu item.
    pub current_shader: Option<&'a Path>,
    /// Recently-loaded shader presets (last via Browse... or
    /// catalog click), most-recent first. Drives the
    /// View > Shader > Recent submenu.
    pub recent_shaders: &'a RecentShaders,
}

/// Render the menu bar (a no-op when `params.visible` is false or
/// `params.fullscreen` is true). Pushes selected actions into
/// `cmds`; the host drains them in its `apply_ui_command` loop.
///
/// Takes a `&mut Ui` (the top-level Ui from the egui Context's
/// `run_ui` closure) and shows the panel inside it via
/// `Panel::top(...).show_inside`. The panel claims the top region
/// before the rest of the UI lays out.
pub fn run(ui: &mut egui::Ui, params: &MenuBarParams<'_>, cmds: &mut Vec<UiCommand>) {
    if !params.visible || params.fullscreen {
        return;
    }
    Panel::top("vibenes_menu_bar")
        .exact_size(MENU_BAR_HEIGHT_LOGICAL)
        .show_inside(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                file_menu(ui, params, cmds);
                emulation_menu(ui, params, cmds);
                view_menu(ui, params, cmds);
                tools_menu(ui, params, cmds);
                window_menu(ui, params, cmds);
                help_menu(ui, cmds);
            });
        });
}

fn file_menu(ui: &mut egui::Ui, params: &MenuBarParams<'_>, cmds: &mut Vec<UiCommand>) {
    ui.menu_button("File", |ui| {
        if ui.button("Open ROM…").clicked() {
            cmds.push(UiCommand::OpenRomDialog);
            ui.close();
        }
        ui.menu_button("Recent", |ui| {
            let entries: Vec<PathBuf> = params.recent.iter().cloned().collect();
            if entries.is_empty() {
                ui.add_enabled(false, egui::Button::new("(no recent ROMs)"));
            } else {
                for path in entries {
                    let label = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    if ui.button(label).clicked() {
                        cmds.push(UiCommand::OpenRom(path));
                        ui.close();
                    }
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

fn emulation_menu(
    ui: &mut egui::Ui,
    params: &MenuBarParams<'_>,
    cmds: &mut Vec<UiCommand>,
) {
    ui.menu_button("Emulation", |ui| {
        let reset = ui.add_enabled(
            params.nes_loaded,
            egui::Button::new("Reset"),
        );
        if reset.clicked() {
            cmds.push(UiCommand::Reset);
            ui.close();
        }
        ui.separator();
        // Read-only info entries. Disabled (so they don't react to
        // clicks) but visible - acts as a one-glance status echo
        // until the future status bar lands.
        let region_text = params
            .region_label
            .map(|r| format!("Region: {r}"))
            .unwrap_or_else(|| "Region: -".into());
        ui.add_enabled(false, egui::Button::new(region_text));
        let mapper_text = params
            .mapper_label
            .clone()
            .map(|m| format!("Mapper: {m}"))
            .unwrap_or_else(|| "Mapper: -".into());
        ui.add_enabled(false, egui::Button::new(mapper_text));
    });
}

fn view_menu(ui: &mut egui::Ui, params: &MenuBarParams<'_>, cmds: &mut Vec<UiCommand>) {
    ui.menu_button("View", |ui| {
        ui.menu_button("Scale", |ui| {
            for n in 1u8..=6 {
                let label = format!("{n}x");
                let is_current = params.current_scale == n;
                if ui
                    .selectable_label(is_current, label)
                    .clicked()
                {
                    cmds.push(UiCommand::SetScale(n));
                    ui.close();
                }
            }
        });
        ui.menu_button("Aspect", |ui| {
            for (mode, label) in [
                (ParMode::Auto, "Auto (region)"),
                (ParMode::Fixed(PixelAspectRatio::Square), "1:1 (square)"),
                (ParMode::Fixed(PixelAspectRatio::Standard), "5:4 (standard)"),
                (ParMode::Fixed(PixelAspectRatio::NtscTv), "8:7 NTSC TV"),
                (ParMode::Fixed(PixelAspectRatio::PalTv), "11:8 PAL TV"),
            ] {
                if ui
                    .selectable_label(params.current_par == mode, label)
                    .clicked()
                {
                    cmds.push(UiCommand::SetAspectRatio(mode));
                    ui.close();
                }
            }
        });
        ui.menu_button("Shader", |ui| {
            shader_submenu(ui, params, cmds);
        });
        ui.separator();
        if ui
            .selectable_label(params.visible, "Show Menu Bar")
            .clicked()
        {
            cmds.push(UiCommand::ToggleMenuBar);
            ui.close();
        }
    });
}

/// Render the View > Shader contents. Lists "None / Off" first,
/// then groups by source (Bundled, then User), separated by
/// dividers. Selectable-label state drives the active-shader
/// checkmark. A "Rescan" item at the bottom re-walks the source
/// directories so users can drop a preset into the user dir
/// without restarting.
fn shader_submenu(
    ui: &mut egui::Ui,
    params: &MenuBarParams<'_>,
    cmds: &mut Vec<UiCommand>,
) {
    let none_active = params.current_shader.is_none();
    if ui.selectable_label(none_active, "None / Off").clicked() {
        cmds.push(UiCommand::ClearShader);
        ui.close();
    }
    if !params.shader_catalog.is_empty() {
        ui.separator();
        // Flat alphabetized list under each source heading. The
        // catalog is already sorted (source -> category -> display
        // name) so we just walk it. The bundle stays small enough
        // that nested category submenus would be more friction
        // than help.
        let mut current_source: Option<ShaderSource> = None;
        for entry in params.shader_catalog.entries() {
            if Some(entry.source) != current_source {
                if current_source.is_some() {
                    ui.separator();
                }
                let header = match entry.source {
                    ShaderSource::Bundled => "Bundled",
                    ShaderSource::User => "User",
                };
                ui.label(egui::RichText::new(header).weak().small());
                current_source = Some(entry.source);
            }
            let active = params
                .current_shader
                .map(|p| p == entry.path)
                .unwrap_or(false);
            if ui
                .selectable_label(active, &entry.display_name)
                .clicked()
            {
                cmds.push(UiCommand::LoadShader(entry.path.clone()));
                ui.close();
            }
        }
    }
    ui.separator();
    if ui.button("Browse…").clicked() {
        cmds.push(UiCommand::BrowseShaderDialog);
        ui.close();
    }
    ui.menu_button("Recent", |ui| {
        let mut any = false;
        for path in params.recent_shaders.iter_existing() {
            any = true;
            let label = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.replace(['-', '_'], " "))
                .unwrap_or_else(|| path.display().to_string());
            let active = params
                .current_shader
                .map(|p| p == path.as_path())
                .unwrap_or(false);
            if ui.selectable_label(active, label).clicked() {
                cmds.push(UiCommand::LoadShader(path.clone()));
                ui.close();
            }
        }
        if !any {
            ui.add_enabled(false, egui::Button::new("(no recent shaders)"));
        }
    });
    ui.separator();
    if ui.button("Rescan").clicked() {
        cmds.push(UiCommand::RescanShaders);
        ui.close();
    }
}


fn tools_menu(ui: &mut egui::Ui, params: &MenuBarParams<'_>, cmds: &mut Vec<UiCommand>) {
    ui.menu_button("Tools", |ui| {
        if ui.button("Preferences…").clicked() {
            cmds.push(UiCommand::OpenPreferences);
            ui.close();
        }
        ui.separator();
        ui.add_enabled_ui(params.fds_present, |ui| {
            ui.menu_button("FDS Disk", |ui| {
                if ui.button("Eject").clicked() {
                    cmds.push(UiCommand::FdsEject);
                    ui.close();
                }
                for side in 0u8..4 {
                    if ui.button(format!("Insert side {}", side + 1)).clicked() {
                        cmds.push(UiCommand::FdsInsert(side));
                        ui.close();
                    }
                }
            });
        });
    });
}

fn window_menu(
    ui: &mut egui::Ui,
    params: &MenuBarParams<'_>,
    cmds: &mut Vec<UiCommand>,
) {
    ui.menu_button("Window", |ui| {
        if ui
            .selectable_label(params.fullscreen, "Fullscreen")
            .clicked()
        {
            cmds.push(UiCommand::ToggleFullscreen);
            ui.close();
        }
    });
}

fn help_menu(ui: &mut egui::Ui, cmds: &mut Vec<UiCommand>) {
    ui.menu_button("Help", |ui| {
        if ui.button("About vibenes").clicked() {
            cmds.push(UiCommand::ShowAbout);
            ui.close();
        }
        if ui.button("GitHub").clicked() {
            cmds.push(UiCommand::OpenGithub);
            ui.close();
        }
    });
}
