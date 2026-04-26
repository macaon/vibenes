// SPDX-License-Identifier: GPL-3.0-or-later
//! User-facing video settings: integer scale and pixel aspect ratio.
//! The window is sized to exactly one integer multiple of the NES
//! framebuffer scaled by PAR - drag-resize is disabled so the renderer
//! never has to deal with fractional scales.

use crate::clock::Region;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelAspectRatio {
    /// 1:1 - development / pixel-perfect output (256×240 at 1×).
    Square,
    /// 5:4 - the "standard" 4:3 TV aspect applied to 240-line content
    /// (320×240 at 1×).
    Standard,
    /// 8:7 - true NTSC pixel ratio (≈293×240 at 1×).
    NtscTv,
    /// 11:8 - PAL pixel ratio (352×240 at 1×).
    PalTv,
}

impl PixelAspectRatio {
    /// Base width at scale 1 in physical pixels. Derived from the NES
    /// 256-pixel framebuffer times `par_w / par_h`, rounded to the
    /// nearest integer. Height is always 240 at scale 1.
    pub const fn base_width(self) -> u32 {
        match self {
            PixelAspectRatio::Square => 256,
            PixelAspectRatio::Standard => 320,
            PixelAspectRatio::NtscTv => 293,
            PixelAspectRatio::PalTv => 352,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PixelAspectRatio::Square => "1:1 (square)",
            PixelAspectRatio::Standard => "5:4",
            PixelAspectRatio::NtscTv => "8:7 (NTSC)",
            PixelAspectRatio::PalTv => "11:8 (PAL)",
        }
    }

    pub const ALL: [PixelAspectRatio; 4] = [
        PixelAspectRatio::Square,
        PixelAspectRatio::Standard,
        PixelAspectRatio::NtscTv,
        PixelAspectRatio::PalTv,
    ];
}

/// Whether PAR follows the loaded ROM's region (Auto) or is pinned to
/// a specific ratio. Auto falls back to NTSC 8:7 when no ROM is
/// loaded so the empty window matches what an NTSC ROM would show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParMode {
    Auto,
    Fixed(PixelAspectRatio),
}

impl ParMode {
    pub fn effective(self, region: Option<Region>) -> PixelAspectRatio {
        match self {
            ParMode::Fixed(par) => par,
            ParMode::Auto => match region.unwrap_or(Region::Ntsc) {
                Region::Ntsc => PixelAspectRatio::NtscTv,
                Region::Pal => PixelAspectRatio::PalTv,
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct VideoSettings {
    pub scale: u8,
    pub par_mode: ParMode,
}

impl VideoSettings {
    pub const MIN_SCALE: u8 = 1;
    pub const MAX_SCALE: u8 = 6;

    /// Physical-pixel size of the NES content area at the current
    /// scale and effective PAR. Pass the loaded ROM's region so Auto
    /// mode can resolve; pass `None` before any ROM is loaded.
    pub fn content_size(&self, region: Option<Region>) -> (u32, u32) {
        let scale = self.scale.clamp(Self::MIN_SCALE, Self::MAX_SCALE) as u32;
        let par = self.par_mode.effective(region);
        (par.base_width() * scale, 240 * scale)
    }

    pub fn with_scale(mut self, scale: u8) -> Self {
        self.scale = scale.clamp(Self::MIN_SCALE, Self::MAX_SCALE);
        self
    }

    pub fn with_par_mode(mut self, par_mode: ParMode) -> Self {
        self.par_mode = par_mode;
        self
    }
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            scale: 2,
            par_mode: ParMode::Auto,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_size_scales_with_scale() {
        let v = VideoSettings::default().with_scale(3).with_par_mode(ParMode::Fixed(PixelAspectRatio::Square));
        assert_eq!(v.content_size(None), (256 * 3, 240 * 3));
    }

    #[test]
    fn auto_par_follows_region() {
        let v = VideoSettings::default();
        assert_eq!(v.content_size(Some(Region::Ntsc)), (293 * 2, 240 * 2));
        assert_eq!(v.content_size(Some(Region::Pal)), (352 * 2, 240 * 2));
    }

    #[test]
    fn fixed_par_ignores_region() {
        let v = VideoSettings::default().with_par_mode(ParMode::Fixed(PixelAspectRatio::Standard));
        assert_eq!(v.content_size(Some(Region::Ntsc)), (320 * 2, 240 * 2));
        assert_eq!(v.content_size(Some(Region::Pal)), (320 * 2, 240 * 2));
    }

    #[test]
    fn scale_clamped() {
        assert_eq!(VideoSettings::default().with_scale(0).scale, 1);
        assert_eq!(VideoSettings::default().with_scale(99).scale, 6);
    }
}
