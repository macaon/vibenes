// SPDX-License-Identifier: GPL-3.0-or-later
//! User-tunable runtime settings persisted across launches.
//!
//! Today only `scale` is persisted. The on-disk format is a tiny
//! `key=value` file (one entry per line, `#` introduces a comment)
//! at `$XDG_CONFIG_HOME/vibenes/settings.kv` (falling back to
//! `$HOME/.config/vibenes/settings.kv`). TOML + serde is the
//! eventual destination once the settings UI lands and the field
//! count grows - pulling those two crates in for one `u8` was
//! overkill. The loader ignores unknown keys, so adding fields
//! later is forward-compatible without bumping a schema version.
//!
//! Errors are deliberately swallowed on read - a missing,
//! permission-denied, or malformed file falls through to
//! [`Settings::default`] so a broken settings file can never block
//! the emulator from starting. Write errors are surfaced so the
//! caller can log them.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::video::VideoSettings;

const FILE_NAME: &str = "settings.kv";
const HEADER: &str = "# vibenes settings - auto-managed by the emulator. Safe to edit.\n";

/// Persisted user preferences. Add fields here as more settings need
/// to survive across launches; extend [`parse`] and [`serialize`] to
/// match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub scale: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            scale: VideoSettings::default().scale,
        }
    }
}

/// Default file path. `None` only when neither `$XDG_CONFIG_HOME`
/// nor `$HOME` is set - shouldn't happen in a real user session,
/// but we don't assume.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("vibenes").join(FILE_NAME))
}

/// Load settings from the default path. Missing files, malformed
/// lines, and out-of-range values silently fall through to defaults.
pub fn load() -> Settings {
    match default_path().and_then(|p| std::fs::read_to_string(&p).ok()) {
        Some(text) => parse(&text),
        None => Settings::default(),
    }
}

/// Write `settings` to the default path. Creates the parent
/// directory if missing.
pub fn save(settings: &Settings) -> Result<()> {
    let path = default_path()
        .context("no XDG_CONFIG_HOME or HOME - cannot resolve settings path")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating settings dir {}", parent.display()))?;
    }
    std::fs::write(&path, serialize(settings))
        .with_context(|| format!("writing settings file {}", path.display()))?;
    Ok(())
}

fn parse(text: &str) -> Settings {
    let mut s = Settings::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "scale" => {
                if let Ok(n) = v.trim().parse::<u8>() {
                    s.scale = n.clamp(VideoSettings::MIN_SCALE, VideoSettings::MAX_SCALE);
                }
            }
            // Forward-compat: ignore unknown keys so a newer
            // vibenes' file doesn't trip up an older binary.
            _ => {}
        }
    }
    s
}

fn serialize(s: &Settings) -> String {
    let mut out = String::from(HEADER);
    out.push_str(&format!("scale={}\n", s.scale));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recovers_scale() {
        assert_eq!(parse("scale=4\n").scale, 4);
    }

    #[test]
    fn parse_ignores_comments_and_blank_lines() {
        let text = "\n# header\n  scale = 3\n# trailing\n\n";
        assert_eq!(parse(text).scale, 3);
    }

    #[test]
    fn parse_ignores_unknown_keys() {
        let s = parse("scale=2\nfuture_field=99\n");
        assert_eq!(s.scale, 2);
    }

    #[test]
    fn parse_clamps_out_of_range_scale() {
        assert_eq!(parse("scale=99\n").scale, VideoSettings::MAX_SCALE);
        assert_eq!(parse("scale=0\n").scale, VideoSettings::MIN_SCALE);
    }

    #[test]
    fn parse_garbage_falls_back_to_default() {
        assert_eq!(parse("scale=not-a-number\n"), Settings::default());
    }

    #[test]
    fn serialize_roundtrips() {
        let s = Settings { scale: 5 };
        assert_eq!(parse(&serialize(&s)), s);
    }
}
