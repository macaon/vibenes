// SPDX-License-Identifier: GPL-3.0-or-later
//! In-app screenshot capture.
//!
//! Writes the live PPU framebuffer (256×240 RGBA, no PAR scaling
//! and no host-side filtering) to a PNG sidecar under
//! `~/.config/vibenes/screenshots/`. Resolution-of-record is the
//! native NES output - useful for technical screenshots where
//! pixel alignment matters more than how the image renders in a
//! viewer.
//!
//! Naming: `<rom-stem>-<UTC-ISO>.png`, e.g.
//! `Castlevania III - Dracula's Curse (USA)-2026-05-02T18-43-21Z.png`.
//! ISO timestamp keeps `ls -lt` chronological. With no ROM loaded
//! the stem falls back to `vibenes`.
//!
//! Bound to F5 in the host event loop. Cancellation, scaled output,
//! and overlay-baked-in variants are out of scope for v1.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

/// Logical NES output dimensions. Re-exported so the host doesn't
/// have to reach into `gfx` just to call us.
pub const NES_WIDTH: u32 = 256;
pub const NES_HEIGHT: u32 = 240;

/// Resolve the directory PNGs land in. Mirrors `save::saves_dir`'s
/// XDG handling with a different leaf (`screenshots/` instead of
/// `saves/`). `None` only when neither `XDG_CONFIG_HOME` nor
/// `HOME` is set.
pub fn screenshots_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("vibenes").join("screenshots"))
}

/// Build a filename for the screenshot under `dir`. `rom_path` is
/// the optional currently-loaded cart path; the stem (no extension)
/// becomes the prefix. Stamps a UTC timestamp computed by hand
/// (no `chrono` dep): `YYYY-MM-DDTHH-MM-SSZ`. Colons are replaced
/// with hyphens so the filename works on case-sensitive Linux and
/// case-insensitive macOS / Windows alike (and avoids shell-quoting
/// surprises).
pub fn screenshot_path(dir: &Path, rom_path: Option<&Path>) -> PathBuf {
    let stem = rom_path
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "vibenes".into());
    let ts = format_iso_utc(SystemTime::now());
    dir.join(format!("{stem}-{ts}.png"))
}

/// Encode `rgba` (256×240×4) as a PNG and write to `path`. The
/// directory is created if missing. Mirrors `save::write`'s
/// philosophy: surface I/O errors via `anyhow` rather than swallow.
pub fn write_png(path: &Path, rgba: &[u8]) -> Result<()> {
    let expected = (NES_WIDTH as usize) * (NES_HEIGHT as usize) * 4;
    if rgba.len() != expected {
        anyhow::bail!(
            "screenshot framebuffer is {} bytes; expected {expected}",
            rgba.len(),
        );
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("creating screenshot dir {}", parent.display())
            })?;
        }
    }
    let file = std::fs::File::create(path)
        .with_context(|| format!("creating {}", path.display()))?;
    let w = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(w, NES_WIDTH, NES_HEIGHT);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .with_context(|| format!("writing PNG header to {}", path.display()))?;
    writer
        .write_image_data(rgba)
        .with_context(|| format!("writing PNG body to {}", path.display()))?;
    Ok(())
}

/// Format a `SystemTime` as `YYYY-MM-DDTHH-MM-SSZ` (UTC). Hand-
/// rolled to avoid pulling in `chrono` for a single timestamp -
/// derived from days-since-Unix-epoch via the standard
/// March-as-month-zero algorithm (Howard Hinnant's
/// `civil_from_days`). Subsecond is dropped; we don't need
/// millisecond resolution to disambiguate manual screenshots.
fn format_iso_utc(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let sod = (secs % 86_400) as u32;
    let hh = sod / 3600;
    let mm = (sod / 60) % 60;
    let ss = sod % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hh:02}-{mm:02}-{ss:02}Z")
}

/// Howard Hinnant's `civil_from_days` (proleptic Gregorian; works
/// for any year ≥ 0001). Output: (year, month 1-12, day 1-31).
fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    // 1970-01-01 is days = 0. Shift to a March-anchored epoch
    // that simplifies the leap-year math.
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = (if m <= 2 { y + 1 } else { y }) as i32;
    (year, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_path_uses_rom_stem_when_present() {
        let p = screenshot_path(
            Path::new("/tmp/screens"),
            Some(Path::new("/roms/Kirby (USA).nes")),
        );
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("Kirby (USA)-"),
            "name {name:?} should start with the ROM stem",
        );
        assert!(name.ends_with(".png"));
    }

    #[test]
    fn screenshot_path_falls_back_to_vibenes_when_no_rom() {
        let p = screenshot_path(Path::new("/tmp/screens"), None);
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("vibenes-"), "got {name:?}");
    }

    #[test]
    fn iso_utc_format_matches_known_epoch_dates() {
        // Unix epoch.
        let t0 = UNIX_EPOCH;
        assert_eq!(format_iso_utc(t0), "1970-01-01T00-00-00Z");
        // One day, one hour, one minute, one second past epoch.
        let t = UNIX_EPOCH
            + std::time::Duration::from_secs(86_400 + 3_600 + 60 + 1);
        assert_eq!(format_iso_utc(t), "1970-01-02T01-01-01Z");
        // Y2K millennium rollover (UTC).
        let t = UNIX_EPOCH + std::time::Duration::from_secs(946_684_800);
        assert_eq!(format_iso_utc(t), "2000-01-01T00-00-00Z");
        // Leap day 2024.
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_709_164_800);
        assert_eq!(format_iso_utc(t), "2024-02-29T00-00-00Z");
    }

    #[test]
    fn write_png_round_trips_to_decodable_file() {
        let dir = std::env::temp_dir().join(format!(
            "vibenes-screenshot-test-{}",
            std::process::id(),
        ));
        let path = dir.join("test.png");
        let rgba: Vec<u8> = (0..(256 * 240 * 4u32))
            .map(|i| (i & 0xFF) as u8)
            .collect();
        write_png(&path, &rgba).unwrap();
        // Decode the written file back; png crate's Decoder does
        // header validation so a corrupt PNG would error here.
        let f = std::fs::File::open(&path).unwrap();
        let mut decoder = png::Decoder::new(std::io::BufReader::new(f));
        let info = decoder.read_header_info().unwrap();
        assert_eq!(info.width, NES_WIDTH);
        assert_eq!(info.height, NES_HEIGHT);
        assert_eq!(info.color_type, png::ColorType::Rgba);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn write_png_rejects_wrong_buffer_size() {
        let dir = std::env::temp_dir().join(format!(
            "vibenes-screenshot-bad-{}",
            std::process::id(),
        ));
        let path = dir.join("bad.png");
        let rgba = vec![0u8; 10]; // way too small
        let err = write_png(&path, &rgba).unwrap_err();
        assert!(err.to_string().contains("expected"));
    }
}
