// SPDX-License-Identifier: GPL-3.0-or-later
//! In-frame debug ruler. Paints scanline / dot coordinate labels
//! directly into the 256×240 RGBA8 NES framebuffer before it's
//! uploaded to the renderer. Toggleable from the host (F2 by default).
//!
//! Why draw into the framebuffer instead of compositing in egui? The
//! ruler needs to align pixel-perfect with NES coordinates so it can
//! be used to identify rendering artifacts at exact scanline / dot
//! positions. Doing the work in the framebuffer guarantees that -
//! scaling and PAR are applied uniformly to the labels and the image
//! together.
//!
//! Layout:
//!  - Left margin: 3-digit scanline numbers every 32 lines
//!    (`0 / 32 / 64 / ... / 224`), with single-pixel tick marks every
//!    8 lines for finer reading.
//!  - Top margin: 3-digit dot numbers every 32 dots
//!    (`0 / 32 / ... / 224`), tick marks every 8 dots.
//!
//! The labels are drawn in white on a black 1-pixel-padded
//! background so they stay readable regardless of game-side
//! background color.

const W: usize = 256;
const H: usize = 240;

/// 3×5 pixel font for digits 0-9. Each digit is five rows of three
/// bits, MSB on the left. Stored as five `u8`s with the digit's
/// pixels in bits 2-0.
const FONT_3X5: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111], // 0
    [0b010, 0b110, 0b010, 0b010, 0b111], // 1
    [0b111, 0b001, 0b111, 0b100, 0b111], // 2
    [0b111, 0b001, 0b111, 0b001, 0b111], // 3
    [0b101, 0b101, 0b111, 0b001, 0b001], // 4
    [0b111, 0b100, 0b111, 0b001, 0b111], // 5
    [0b111, 0b100, 0b111, 0b101, 0b111], // 6
    [0b111, 0b001, 0b010, 0b100, 0b100], // 7
    [0b111, 0b101, 0b111, 0b101, 0b111], // 8
    [0b111, 0b101, 0b111, 0b001, 0b111], // 9
];

const WHITE: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];
const BLACK: [u8; 4] = [0x00, 0x00, 0x00, 0xFF];

#[inline]
fn put(fb: &mut [u8], x: usize, y: usize, rgba: [u8; 4]) {
    if x >= W || y >= H {
        return;
    }
    let i = (y * W + x) * 4;
    fb[i..i + 4].copy_from_slice(&rgba);
}

/// Draw a 3×5 digit at `(x, y)` with a one-pixel black halo so it's
/// readable on any background.
fn draw_digit(fb: &mut [u8], x: usize, y: usize, digit: u8) {
    let glyph = &FONT_3X5[(digit % 10) as usize];
    // Halo: 5×7 black box.
    for dy in 0..7 {
        for dx in 0..5 {
            put(fb, x + dx, y + dy, BLACK);
        }
    }
    // Glyph: shifted +1 in both axes to sit inside the halo.
    for (row, bits) in glyph.iter().enumerate() {
        for col in 0..3 {
            if (bits >> (2 - col)) & 1 == 1 {
                put(fb, x + 1 + col, y + 1 + row, WHITE);
            }
        }
    }
}

/// Print up to a 3-digit number at `(x, y)`. Suppresses leading
/// zeros except when `value == 0`. Spacing is 4 px per digit.
fn draw_number(fb: &mut [u8], x: usize, y: usize, value: u16) {
    let v = value.min(999);
    let hundreds = (v / 100) as u8;
    let tens = ((v / 10) % 10) as u8;
    let ones = (v % 10) as u8;
    let mut col = x;
    if hundreds > 0 {
        draw_digit(fb, col, y, hundreds);
        col += 4;
    }
    if hundreds > 0 || tens > 0 {
        draw_digit(fb, col, y, tens);
        col += 4;
    }
    draw_digit(fb, col, y, ones);
}

#[inline]
fn blend(fb: &mut [u8], x: usize, y: usize, rgba: [u8; 4], alpha: u8) {
    if x >= W || y >= H {
        return;
    }
    let i = (y * W + x) * 4;
    let a = alpha as u32;
    let inv = 255 - a;
    for k in 0..3 {
        let dst = fb[i + k] as u32;
        let src = rgba[k] as u32;
        fb[i + k] = ((src * a + dst * inv) / 255) as u8;
    }
    fb[i + 3] = 0xFF;
}

/// Draw the scanline + dot coordinate ruler directly onto `fb`. The
/// caller passes a borrow of the 256×240 RGBA8 framebuffer; this
/// function mutates it in place.
///
/// Layout:
///  - Faint vertical + horizontal lines every 8 pixels for fine
///    counting; brighter lines every 32 pixels.
///  - 3-digit labels every 32 lines/dots in opaque white-on-black so
///    the count is readable on any background.
pub fn draw_scanline_ruler(fb: &mut [u8]) {
    if fb.len() < W * H * 4 {
        return; // Mismatched buffer; bail rather than panic.
    }

    // Vertical gridlines: each `step_by(8)` column gets a faint
    // white blend; every 32 dots gets a stronger one.
    for x in (0..W).step_by(8) {
        let alpha = if x % 32 == 0 { 0x90 } else { 0x40 };
        for y in 0..H {
            blend(fb, x, y, WHITE, alpha);
        }
    }
    // Horizontal gridlines.
    for y in (0..H).step_by(8) {
        let alpha = if y % 32 == 0 { 0x90 } else { 0x40 };
        for x in 0..W {
            blend(fb, x, y, WHITE, alpha);
        }
    }

    // Tick marks at the very edges so 8-line / 8-dot increments are
    // unambiguous even where gridlines coincide with bright pixels.
    for y in (0..H).step_by(8) {
        let len = if y % 32 == 0 { 4 } else { 2 };
        for dx in 0..len {
            put(fb, dx, y, WHITE);
        }
    }
    for x in (0..W).step_by(8) {
        let len = if x % 32 == 0 { 4 } else { 2 };
        for dy in 0..len {
            put(fb, x, dy, WHITE);
        }
    }

    // Opaque labels every 32 lines / dots. Top-left labels live in
    // the corner already covered by gridlines; later rows / cols
    // step away from the edge so the digits sit just inside the
    // gridline crossing.
    for y in (32..H).step_by(32) {
        draw_number(fb, 5, y, y as u16);
    }
    for x in (32..W).step_by(32) {
        draw_number(fb, x, 0, x as u16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank_fb() -> Vec<u8> {
        vec![0u8; W * H * 4]
    }

    #[test]
    fn ruler_draws_some_pixels() {
        let mut fb = blank_fb();
        draw_scanline_ruler(&mut fb);
        // At least the y=32 long tick should have lit cols 0..3.
        let i = 32 * W * 4;
        assert_eq!(fb[i], 0xFF);
        assert_eq!(fb[i + 4], 0xFF);
    }

    #[test]
    fn digit_zero_pixels_match_glyph() {
        let mut fb = blank_fb();
        draw_digit(&mut fb, 10, 10, 0);
        // Top row of '0' is 111 → white at (11,11),(12,11),(13,11).
        for dx in 0..3 {
            let i = (11 * W + 11 + dx) * 4;
            assert_eq!(&fb[i..i + 3], &[0xFF, 0xFF, 0xFF], "col offset {dx}");
        }
        // Center pixel of '0' should be background (halo black, glyph row1 = 101).
        let i = (12 * W + 12) * 4;
        assert_eq!(&fb[i..i + 3], &[0, 0, 0]);
    }

    #[test]
    fn ignores_mismatched_buffer_length() {
        let mut tiny = vec![0u8; 16];
        draw_scanline_ruler(&mut tiny); // must not panic
    }
}
