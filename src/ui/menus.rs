// SPDX-License-Identifier: GPL-3.0-or-later
//! NES Mini-style overlay menu. When [`OverlayState::open`] is true the
//! emulator pauses and a centered modal is drawn on top of a darkened
//! freeze-frame of the last rendered NES output. Items are picked with
//! ↑/↓/Enter on the keyboard or with the mouse; Esc / Backspace backs
//! out of submenus or closes the overlay from root.
//!
//! All UI state lives in [`OverlayState`]; widgets here are pure
//! renderers driven by the current screen + cursor, plus a scratch
//! `Vec<UiCommand>` they push host actions into.

use egui::{Align2, Color32, Context, FontId, Key, Painter, Pos2, Rect, Shape, Stroke, Vec2};

use crate::clock::Region;
use crate::nes::FdsInfo;
use crate::ui::{RecentRoms, UiCommand};
use crate::video::{ParMode, PixelAspectRatio, VideoSettings};

/// Which screen of the menu the user is currently on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Root,
    Scale,
    Aspect,
    Recent,
    /// FDS-only: disk-side chooser. Enabled from Root when the loaded
    /// cart is a Famicom Disk System image; greyed out otherwise.
    /// Also reachable via the F4 hotkey.
    Disk,
    /// Developer-facing diagnostic toggles (scanline ruler, OAM
    /// dump). Reachable from Root or via the F12 hotkey.
    Debug,
}

#[derive(Debug, Clone, Copy)]
pub struct OverlayState {
    pub open: bool,
    pub screen: Screen,
    pub cursor: usize,
    /// Last observed pointer position. The mouse-hover handler only
    /// steals the cursor from the keyboard / gamepad when the pointer
    /// *moves*; a stationary mouse that happens to sit over an item
    /// would otherwise overwrite every ↑/↓ input on the next frame.
    last_pointer: Option<Pos2>,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self {
            open: false,
            screen: Screen::Root,
            cursor: 0,
            last_pointer: None,
        }
    }
}

impl OverlayState {
    pub fn open_root(&mut self) {
        self.open = true;
        self.screen = Screen::Root;
        self.cursor = 0;
    }

    /// Jump the overlay straight into the disk-swap submenu. Used by
    /// the F4 hotkey — "please insert side B" prompts are common
    /// enough during multi-disk play that making the user tab through
    /// Root every time would be annoying.
    pub fn open_disk(&mut self) {
        self.open = true;
        self.screen = Screen::Disk;
        self.cursor = 0;
    }

    /// Jump straight to the Debug submenu. Wired to F12 in the host.
    pub fn open_debug(&mut self) {
        self.open = true;
        self.screen = Screen::Debug;
        self.cursor = 0;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn toggle(&mut self) {
        if self.open {
            self.close();
        } else {
            self.open_root();
        }
    }

    pub fn back_or_close(&mut self) {
        match self.screen {
            Screen::Root => self.close(),
            _ => {
                self.screen = Screen::Root;
                self.cursor = 0;
            }
        }
    }

    fn enter(&mut self, screen: Screen) {
        self.screen = screen;
        self.cursor = 0;
    }
}

/// Result of selecting (Enter / click) the active item.
enum Selected {
    Cmd(UiCommand),
    Goto(Screen),
    Close,
    Back,
    None,
}

/// One renderable line in the overlay. Building the same list for both
/// render and select keeps the two paths perfectly in sync — keyboard
/// Enter and mouse click resolve to the exact same `Selected`.
struct Item {
    label: String,
    badge: Option<String>,
    action: Selected,
    /// Disabled items are rendered dimmed and skip cursor stops.
    enabled: bool,
}

impl Item {
    fn new(label: impl Into<String>, action: Selected) -> Self {
        Self {
            label: label.into(),
            badge: None,
            action,
            enabled: true,
        }
    }

    fn with_badge(mut self, badge: impl Into<String>) -> Self {
        self.badge = Some(badge.into());
        self
    }

    fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
}

/// Snapshot of host-side debug toggles so the Debug submenu can show
/// current ON/OFF state next to each item. Filled by the host before
/// the egui pass and consumed by [`items_for`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DebugStatus {
    pub scanline_ruler_on: bool,
}

/// Build the item list for the active screen.
fn items_for(
    screen: Screen,
    video: &VideoSettings,
    region: Option<Region>,
    recent: &RecentRoms,
    nes_loaded: bool,
    fds: Option<FdsInfo>,
    debug: DebugStatus,
) -> Vec<Item> {
    match screen {
        Screen::Root => {
            let mut items = vec![Item::new("Resume", Selected::Close)];
            items.push(Item::new("Open ROM…", Selected::Cmd(UiCommand::OpenRomDialog)));
            let recent_item = Item::new("Recent ROMs", Selected::Goto(Screen::Recent));
            items.push(if recent.is_empty() {
                recent_item.disabled()
            } else {
                recent_item.with_badge(format!("{}", recent.len()))
            });
            items.push(
                Item::new("Scale", Selected::Goto(Screen::Scale))
                    .with_badge(format!("{}×", video.scale)),
            );
            items.push(
                Item::new("Aspect ratio", Selected::Goto(Screen::Aspect))
                    .with_badge(par_badge(video.par_mode, region)),
            );
            // Disk submenu — enabled only for FDS carts.
            let mut disk_item = Item::new("Disk", Selected::Goto(Screen::Disk));
            if let Some(info) = fds {
                disk_item = disk_item.with_badge(fds_root_badge(info));
            } else {
                disk_item = disk_item.disabled();
            }
            items.push(disk_item);
            let reset_item = Item::new("Reset", Selected::Cmd(UiCommand::Reset));
            items.push(if nes_loaded { reset_item } else { reset_item.disabled() });
            items.push(Item::new("Debug", Selected::Goto(Screen::Debug)));
            items.push(Item::new("Quit", Selected::Cmd(UiCommand::Quit)));
            items
        }
        Screen::Scale => (VideoSettings::MIN_SCALE..=VideoSettings::MAX_SCALE)
            .map(|n| {
                let mut item = Item::new(
                    format!("{n}×"),
                    Selected::Cmd(UiCommand::SetScale(n)),
                );
                if video.scale == n {
                    item = item.with_badge("●");
                }
                item
            })
            .collect(),
        Screen::Aspect => {
            let mut items = Vec::new();
            let auto_resolved = ParMode::Auto.effective(region);
            let auto_label = format!("Auto ({})", auto_resolved.label());
            let mut auto = Item::new(auto_label, Selected::Cmd(UiCommand::SetAspectRatio(ParMode::Auto)));
            if matches!(video.par_mode, ParMode::Auto) {
                auto = auto.with_badge("●");
            }
            items.push(auto);
            for par in PixelAspectRatio::ALL {
                let mut item = Item::new(
                    par.label(),
                    Selected::Cmd(UiCommand::SetAspectRatio(ParMode::Fixed(par))),
                );
                if matches!(video.par_mode, ParMode::Fixed(p) if p == par) {
                    item = item.with_badge("●");
                }
                items.push(item);
            }
            items
        }
        Screen::Recent => {
            if recent.is_empty() {
                vec![Item::new("(no recent ROMs)", Selected::Back).disabled()]
            } else {
                recent
                    .iter()
                    .map(|path| {
                        let label = path
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.display().to_string());
                        Item::new(label, Selected::Cmd(UiCommand::OpenRom(path.clone())))
                    })
                    .collect()
            }
        }
        Screen::Disk => {
            let Some(info) = fds else {
                return vec![Item::new("(not an FDS cart)", Selected::Back).disabled()];
            };
            let mut items = Vec::with_capacity(info.side_count as usize + 1);
            for side in 0..info.side_count {
                let label = fds_side_label(side, info.side_count);
                let mut item = Item::new(label, Selected::Cmd(UiCommand::FdsInsert(side)));
                if info.current_side == Some(side) {
                    item = item.with_badge("●");
                }
                items.push(item);
            }
            let mut eject = Item::new("Eject", Selected::Cmd(UiCommand::FdsEject));
            if info.current_side.is_none() {
                // Already ejected — nothing to do.
                eject = eject.disabled();
            }
            items.push(eject);
            items
        }
        Screen::Debug => {
            // Toggleable diagnostics. The badge shows current state
            // so the user can confirm the click took without having
            // to dismiss the overlay.
            let mut ruler = Item::new(
                "Scanline ruler",
                Selected::Cmd(UiCommand::ToggleScanlineRuler),
            );
            ruler = ruler.with_badge(if debug.scanline_ruler_on { "ON" } else { "off" });
            // OAM dump fires once and prints to stderr; nothing to
            // badge — short cooldown handled host-side.
            let oam = Item::new(
                "OAM dump (8 frames)",
                Selected::Cmd(UiCommand::DumpOamBurst(8)),
            );
            vec![ruler, oam, Item::new("Back", Selected::Back)]
        }
    }
}

/// Badge for the "Disk" entry on the root menu. Summarizes current
/// side + total-count in a single line, e.g. `Side A / 2` or
/// `ejected / 2`.
fn fds_root_badge(info: FdsInfo) -> String {
    match info.current_side {
        Some(s) => format!("{} / {}", fds_side_label(s, info.side_count), info.side_count),
        None => format!("ejected / {}", info.side_count),
    }
}

/// Human-readable label for one FDS disk side. Two-side games are
/// almost universally labeled Side A / Side B; larger counts (3+
/// sides on rare multi-disk games) get `Disk 1 Side A`, `Disk 1 Side
/// B`, `Disk 2 Side A`, ...
fn fds_side_label(side: u8, total: u8) -> String {
    if total <= 2 {
        match side {
            0 => "Side A".to_string(),
            1 => "Side B".to_string(),
            _ => format!("Side {}", side),
        }
    } else {
        let disk = side / 2 + 1;
        let face = if side & 1 == 0 { 'A' } else { 'B' };
        format!("Disk {disk} Side {face}")
    }
}

fn par_badge(mode: ParMode, region: Option<Region>) -> String {
    match mode {
        ParMode::Auto => format!("Auto ({})", ParMode::Auto.effective(region).label()),
        ParMode::Fixed(par) => par.label().to_string(),
    }
}

fn screen_title(screen: Screen) -> &'static str {
    match screen {
        Screen::Root => "vibenes",
        Screen::Scale => "Scale",
        Screen::Aspect => "Aspect ratio",
        Screen::Recent => "Recent ROMs",
        Screen::Disk => "Disk",
        Screen::Debug => "Debug",
    }
}

/// Wrapper so we can stash a selection in egui's frame-temp storage.
/// `Selected` isn't `Clone + Send + Sync`, so the click handler stores
/// the index it selected; the post-paint dispatcher rebuilds items
/// and pulls out the action by index.
#[derive(Clone, Default)]
struct PendingAction(usize);

fn clamp_cursor(state: &mut OverlayState, items: &[Item]) {
    if items.is_empty() {
        state.cursor = 0;
        return;
    }
    if state.cursor >= items.len() {
        state.cursor = items.len() - 1;
    }
    // Skip disabled items if the cursor landed on one (e.g., after a
    // recent-ROMs list shrank). Walk forward, then back, then give up.
    if !items[state.cursor].enabled {
        if let Some(idx) = next_enabled(items, state.cursor, 1) {
            state.cursor = idx;
        } else if let Some(idx) = next_enabled(items, state.cursor, items.len() - 1) {
            state.cursor = idx;
        }
    }
}

fn next_enabled(items: &[Item], start: usize, step: usize) -> Option<usize> {
    let n = items.len();
    if n == 0 {
        return None;
    }
    let step = step % n;
    let mut i = start;
    for _ in 0..n {
        i = (i + step) % n;
        if items[i].enabled {
            return Some(i);
        }
    }
    None
}

fn handle_nav_keys(ctx: &Context, state: &mut OverlayState, items: &[Item]) {
    let (up, down, select, back) = ctx.input_mut(|i| {
        (
            i.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
            i.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
            i.consume_key(egui::Modifiers::NONE, Key::Enter),
            i.consume_key(egui::Modifiers::NONE, Key::Backspace),
        )
    });
    if up {
        if let Some(idx) = next_enabled(items, state.cursor, items.len() - 1) {
            state.cursor = idx;
        }
    }
    if down {
        if let Some(idx) = next_enabled(items, state.cursor, 1) {
            state.cursor = idx;
        }
    }
    if back {
        state.back_or_close();
    }
    if select {
        // Stash the selection for the post-paint dispatcher. Re-using
        // the same data path as mouse clicks keeps both sources of
        // selection going through `apply_action` once per frame.
        ctx.data_mut(|d| {
            d.insert_temp(egui::Id::new("vibenes.menu.pending"), PendingAction(state.cursor))
        });
    }
}

fn paint_dim_layer(ctx: &Context) {
    // Raw layer painter in absolute screen coords — no Area state to
    // memoize. `content_rect` matches the surface-size override set
    // by `UiLayer::run`, so this always covers exactly what the GPU
    // will render.
    let screen = ctx.content_rect();
    let layer_id = egui::LayerId::new(
        egui::Order::Background,
        egui::Id::new("vibenes.menu.dim"),
    );
    let painter = ctx.layer_painter(layer_id);
    painter.rect_filled(screen, 0.0, Color32::from_black_alpha(160));
}

// Virtual-pixel canvas dimensions. Everything inside the overlay is
// laid out in *virtual* pixels and then mapped to screen pixels via a
// single integer scale factor. This eliminates the "panel shifts on
// window resize" jitter that the old egui-Frame-based layout had — no
// auto-sizing, no two layout passes fighting. Width picked to comfortably
// hold the longest label we currently ship ("Aspect ratio  8:7 (NES)").
const VW: f32 = 192.0;
// Vertical metrics (virtual px). `row_h` is the cell height; text is
// centered inside it. `title_h` is a slightly taller top strip that
// also holds the separator rule below the title. Sized for VT323 —
// its em-box is tall, so rows stay legible even at the tight pitch.
const TITLE_H: f32 = 16.0;
const ROW_H: f32 = 11.0;
const MARGIN: f32 = 5.0;
// Virtual-pixel font sizes. The painter multiplies by the integer
// scale `s`, so text lands on whole-pixel baselines at any window
// size. Keep title ≥ item so the hierarchy is obvious at a glance.
const TITLE_PX: f32 = 10.0;
const ITEM_PX: f32 = 8.0;

/// Integer scale + origin for the virtual canvas this frame.
///
/// Picks the largest `s` such that `VW * s` and `vh * s` both fit in
/// `rect`, floored to an integer so text stays on pixel boundaries.
/// Returns at least scale 1 even when the window is smaller than the
/// virtual canvas — clipping is preferable to non-integer scaling.
fn virtual_transform(rect: Rect, vh: f32) -> (f32, Pos2) {
    let sx = (rect.width() / VW).floor().max(1.0);
    let sy = (rect.height() / vh).floor().max(1.0);
    let s = sx.min(sy);
    let origin = egui::pos2(
        (rect.center().x - VW * s / 2.0).round(),
        (rect.center().y - vh * s / 2.0).round(),
    );
    (s, origin)
}

/// Map a virtual-pixel point to screen-space. All drawing goes through
/// this — change the transform and the whole overlay scales with it.
#[inline]
fn vpos(origin: Pos2, s: f32, vx: f32, vy: f32) -> Pos2 {
    egui::pos2(origin.x + vx * s, origin.y + vy * s)
}

#[inline]
fn vrect(origin: Pos2, s: f32, vx: f32, vy: f32, vw: f32, vh: f32) -> Rect {
    Rect::from_min_size(vpos(origin, s, vx, vy), Vec2::new(vw * s, vh * s))
}

fn paint_menu(ctx: &Context, state: &mut OverlayState, items: &[Item]) {
    // Use `screen_rect` (pure window extent) and a raw foreground layer
    // painter. No Area → no memoized position that could linger across
    // item-count or window-size changes. Every frame recomputes the
    // transform from scratch.
    // `content_rect` (not `viewport_rect`) is what reflects the
    // `raw_input.screen_rect` override set by `UiLayer::run`. Pointer
    // coordinates resolve in the same space, so keeping layout on
    // `content_rect` guarantees hit-tests align with visuals.
    let screen = ctx.content_rect();
    let vh = TITLE_H + MARGIN + items.len() as f32 * ROW_H + MARGIN;
    let (s, origin) = virtual_transform(screen, vh);

    let layer_id = egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("vibenes.menu.panel"),
    );
    let painter = ctx.layer_painter(layer_id);
    draw_card(&painter, state, items, origin, s);

    hit_test_rows(ctx, state, items, origin, s);
}

/// Colors — derived from the NES mini home menu (red selection bar,
/// near-black card fill, dimmed inactive text). Not palette-accurate
/// to NES hardware, just stylistically close.
mod palette {
    use egui::Color32;
    pub const CARD_FILL: Color32 = Color32::from_rgb(16, 16, 24);
    pub const CARD_STROKE: Color32 = Color32::from_rgb(64, 64, 80);
    pub const TITLE: Color32 = Color32::from_rgb(240, 240, 240);
    pub const SEPARATOR: Color32 = Color32::from_rgb(80, 80, 96);
    pub const SELECT_BG: Color32 = Color32::from_rgb(208, 16, 16);
    pub const SELECT_FG: Color32 = Color32::WHITE;
    pub const ROW_FG: Color32 = Color32::from_rgb(208, 208, 208);
    pub const ROW_FG_DIS: Color32 = Color32::from_rgb(96, 96, 104);
    pub const BADGE_FG: Color32 = Color32::from_rgb(176, 176, 184);
}

fn draw_card(
    painter: &Painter,
    state: &OverlayState,
    items: &[Item],
    origin: Pos2,
    s: f32,
) {
    let vh = TITLE_H + MARGIN + items.len() as f32 * ROW_H + MARGIN;

    // Card background + 1-px (virtual) border.
    let card = vrect(origin, s, 0.0, 0.0, VW, vh);
    painter.rect_filled(card, 0.0, palette::CARD_FILL);
    painter.rect_stroke(
        card,
        0.0,
        Stroke::new(s.max(1.0), palette::CARD_STROKE),
        egui::StrokeKind::Inside,
    );

    // Title, centered in the title strip.
    let title = screen_title(state.screen);
    painter.text(
        vpos(origin, s, VW / 2.0, TITLE_H / 2.0 + 1.0),
        Align2::CENTER_CENTER,
        title,
        FontId::monospace(TITLE_PX * s),
        palette::TITLE,
    );
    // Separator rule under the title.
    let sep_y = TITLE_H - 2.0;
    painter.line_segment(
        [
            vpos(origin, s, MARGIN, sep_y),
            vpos(origin, s, VW - MARGIN, sep_y),
        ],
        Stroke::new(s.max(1.0), palette::SEPARATOR),
    );

    // Rows.
    let first_row_y = TITLE_H + MARGIN;
    for (idx, item) in items.iter().enumerate() {
        let row_y = first_row_y + idx as f32 * ROW_H;
        let active = idx == state.cursor;
        draw_row(painter, item, active, origin, s, row_y);
    }
}

fn draw_row(
    painter: &Painter,
    item: &Item,
    active: bool,
    origin: Pos2,
    s: f32,
    row_y: f32,
) {
    // Selection bar spans almost the full card width (inset 2 virtual
    // pixels on each side so it doesn't touch the card border).
    if active {
        let bar = vrect(origin, s, 2.0, row_y, VW - 4.0, ROW_H);
        painter.rect_filled(bar, 0.0, palette::SELECT_BG);
        // Chevron cursor on the left edge of the selection bar.
        let cy = row_y + ROW_H / 2.0;
        let tri = [
            vpos(origin, s, 5.0, cy - 3.0),
            vpos(origin, s, 9.0, cy),
            vpos(origin, s, 5.0, cy + 3.0),
        ];
        painter.add(Shape::convex_polygon(
            tri.to_vec(),
            palette::SELECT_FG,
            Stroke::NONE,
        ));
    }

    let text_color = if !item.enabled {
        palette::ROW_FG_DIS
    } else if active {
        palette::SELECT_FG
    } else {
        palette::ROW_FG
    };
    let badge_color = if !item.enabled {
        palette::ROW_FG_DIS
    } else if active {
        palette::SELECT_FG
    } else {
        palette::BADGE_FG
    };

    painter.text(
        vpos(origin, s, 14.0, row_y + ROW_H / 2.0 + 1.0),
        Align2::LEFT_CENTER,
        &item.label,
        FontId::monospace(ITEM_PX * s),
        text_color,
    );
    if let Some(badge) = item.badge.as_ref() {
        painter.text(
            vpos(origin, s, VW - 6.0, row_y + ROW_H / 2.0 + 1.0),
            Align2::RIGHT_CENTER,
            badge,
            FontId::monospace(ITEM_PX * s),
            badge_color,
        );
    }
}

fn hit_test_rows(
    ctx: &Context,
    state: &mut OverlayState,
    items: &[Item],
    origin: Pos2,
    s: f32,
) {
    // Raw pointer + click check — no egui widget interaction registry.
    // Safe because the overlay is modal and nothing else is drawing
    // widgets while it's open. `hover_pos` is the current mouse
    // position; `pointer_interact_pos` lags or is `None` outside of
    // widget interactions and would cause hover-highlight mismatches.
    let (pointer_pos, clicked) = ctx.input(|i| {
        (i.pointer.hover_pos(), i.pointer.primary_clicked())
    });
    let Some(pos) = pointer_pos else {
        state.last_pointer = None;
        return;
    };

    // Only let the mouse drive the cursor when it has actually moved
    // since the last frame. Without this, a stationary pointer that
    // happens to sit over an item would overwrite keyboard / gamepad
    // navigation every frame, pinning the highlight to the mouse
    // position. A click is always honored regardless — the user
    // clearly wants the row they clicked on.
    let moved = state
        .last_pointer
        .map(|prev| (prev - pos).length_sq() > 0.0)
        .unwrap_or(true);
    state.last_pointer = Some(pos);

    let first_row_y = TITLE_H + MARGIN;
    for (idx, item) in items.iter().enumerate() {
        if !item.enabled {
            continue;
        }
        let row_y = first_row_y + idx as f32 * ROW_H;
        let rect = vrect(origin, s, 2.0, row_y, VW - 4.0, ROW_H);
        if rect.contains(pos) {
            if moved || clicked {
                state.cursor = idx;
            }
            if clicked {
                ctx.data_mut(|d| {
                    d.insert_temp(
                        egui::Id::new("vibenes.menu.pending"),
                        PendingAction(idx),
                    )
                });
            }
        }
    }
}

/// Public entry point used by [`crate::ui::UiLayer::run`]. Renders the
/// overlay (if open) and resolves any selection (keyboard or mouse)
/// into commands for the host. Returns whether the overlay is open
/// after this frame, so the caller can adjust its pause logic.
pub fn run_overlay(
    ctx: &Context,
    state: &mut OverlayState,
    video: &VideoSettings,
    region: Option<Region>,
    recent: &RecentRoms,
    nes_loaded: bool,
    fds: Option<FdsInfo>,
    debug: DebugStatus,
    cmds: &mut Vec<UiCommand>,
) -> bool {
    if !state.open {
        // Even when closed, swallow F1 here would conflict with the
        // host's own F1 handler. So we leave key dispatch to main.rs.
        return false;
    }

    // Build items, clamp cursor, capture pending selection, paint —
    // all in one pass. We resolve the pending selection by rebuilding
    // items just-in-time so the action carries the right `Selected`.
    let mut items = items_for(state.screen, video, region, recent, nes_loaded, fds, debug);
    if items.is_empty() {
        items.push(Item::new("(empty)", Selected::Back).disabled());
    }
    clamp_cursor(state, &items);

    handle_nav_keys(ctx, state, &items);
    clamp_cursor(state, &items);

    paint_dim_layer(ctx);
    paint_menu(ctx, state, &items);

    // Drain any selection that paint_menu / handle_nav_keys stashed.
    let pending: Option<PendingAction> = ctx
        .data_mut(|d| d.remove_temp(egui::Id::new("vibenes.menu.pending")));
    if let Some(PendingAction(idx)) = pending {
        if idx < items.len() && items[idx].enabled {
            // Move out of the action by replacing with a placeholder.
            let action = std::mem::replace(&mut items[idx].action, Selected::None);
            apply_resolved(state, action, cmds);
        }
    }

    state.open
}

fn apply_resolved(state: &mut OverlayState, action: Selected, cmds: &mut Vec<UiCommand>) {
    match action {
        Selected::Cmd(cmd) => {
            // Settings (Scale / Aspect) stay on the current submenu
            // so the user sees the ● move to the just-picked option
            // and can keep adjusting. Other commands dismiss the
            // overlay so the user sees the effect immediately.
            let stays_on_submenu = matches!(
                cmd,
                UiCommand::SetScale(_)
                    | UiCommand::SetAspectRatio(_)
                    | UiCommand::ToggleScanlineRuler
                    | UiCommand::DumpOamBurst(_)
            );
            cmds.push(cmd);
            if !stays_on_submenu {
                state.close();
            }
        }
        Selected::Goto(screen) => state.enter(screen),
        Selected::Close => state.close(),
        Selected::Back => state.back_or_close(),
        Selected::None => {}
    }
}
