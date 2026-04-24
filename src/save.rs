// SPDX-License-Identifier: GPL-3.0-or-later
//! Battery-backed cartridge save file I/O.
//!
//! Default layout: `$XDG_CONFIG_HOME/vibenes/saves/<rom-stem>.sav`
//! (falling back to `$HOME/.config/vibenes/saves/<rom-stem>.sav` when
//! `XDG_CONFIG_HOME` is unset). Matches Mesen2's `~/.config/Mesen2/
//! Saves/` convention and keeps ROM directories clean. Alternatives
//! via [`SaveStyle::NextToRom`] (FCEUX-style) and
//! [`SaveStyle::ByCrc`] (rename-survives).
//!
//! Writes are atomic: we stage to a `<path>.tmp` file, `fsync`, then
//! [`std::fs::rename`]. POSIX rename is atomic over the same
//! filesystem, so a crash mid-write leaves either the old save or the
//! new one — never a torn half-written file.
//!
//! No `dirs` crate dependency today — XDG resolution is hand-rolled
//! against `XDG_CONFIG_HOME` + `HOME` because those are all we need
//! on Linux. When macOS / Windows support matters, swap in `dirs`
//! and update [`saves_dir`].

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::{SaveConfig, SaveStyle};

/// Extension appended to the ROM stem for battery-RAM save files.
pub const SAVE_EXT: &str = "sav";

/// Extension appended to the ROM stem for FDS disk-save sidecars.
/// Matches Mesen2's `.ips` naming so users can move saves between
/// emulators.
pub const DISK_SAVE_EXT: &str = "ips";

/// Resolve the save-file path for a cartridge loaded from `rom_path`,
/// using the default battery-RAM extension [`SAVE_EXT`].
pub fn save_path_for(rom_path: &Path, crc: u32, cfg: &SaveConfig) -> Option<PathBuf> {
    save_path_for_with_ext(rom_path, crc, cfg, SAVE_EXT)
}

/// Resolve the save-file path for a cartridge loaded from `rom_path`,
/// letting the caller pick the extension. FDS disk saves pass
/// [`DISK_SAVE_EXT`] so `<rom-stem>.ips` lives alongside any battery
/// `.sav` for carts that use both channels. Returns `None` when no
/// sensible path can be produced (no filename on `rom_path`, or the
/// config-dir style is selected but neither `XDG_CONFIG_HOME` nor
/// `HOME` is set).
pub fn save_path_for_with_ext(
    rom_path: &Path,
    crc: u32,
    cfg: &SaveConfig,
    ext: &str,
) -> Option<PathBuf> {
    match cfg.style {
        SaveStyle::ConfigDir => {
            let stem = rom_path.file_stem()?;
            let dir = cfg
                .dir_override
                .clone()
                .or_else(saves_dir)?;
            let mut p = dir;
            p.push(stem);
            p.set_extension(ext);
            Some(p)
        }
        SaveStyle::NextToRom => Some(rom_path.with_extension(ext)),
        SaveStyle::ByCrc => {
            let dir = cfg
                .dir_override
                .clone()
                .or_else(saves_dir)?;
            let mut p = dir;
            p.push(format!("{crc:08X}"));
            p.set_extension(ext);
            Some(p)
        }
    }
}

/// Default saves directory: `$XDG_CONFIG_HOME/vibenes/saves/`, else
/// `$HOME/.config/vibenes/saves/`. Returns `None` if neither env var
/// is set (shouldn't happen on a real user session, but we don't
/// assume).
pub fn saves_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("vibenes").join("saves"))
}

/// Read the save file at `path`. `Ok(None)` when no file exists;
/// `Err` only for permission / I/O errors we want to surface.
pub fn load(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading save file {}", path.display())),
    }
}

/// Atomic-write `data` to `path`. Parent directory is created if
/// missing (one-level only — we don't try to build a deep tree).
/// A preexisting `<path>.tmp` from an interrupted previous write is
/// overwritten.
pub fn write(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating save dir {}", parent.display()))?;
        }
    }
    let tmp = tmp_path(path);
    {
        let mut f = fs::File::create(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(data)
            .with_context(|| format!("writing {}", tmp.display()))?;
        // Flush to disk before the rename so a power-loss between
        // rename and fsync can't leave an empty file. Non-fatal on
        // platforms where sync_all errors for unsupported backends.
        let _ = f.sync_all();
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(".tmp");
    PathBuf::from(os)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempdir::Dir {
        tempdir::Dir::new("vibenes-save")
    }

    #[test]
    fn default_config_dir_path_uses_rom_stem() {
        let d = tempdir();
        let cfg = SaveConfig {
            style: SaveStyle::ConfigDir,
            dir_override: Some(d.path().to_path_buf()),
            ..SaveConfig::default()
        };
        let p = save_path_for(Path::new("/roms/kirby.nes"), 0xDEADBEEF, &cfg).unwrap();
        assert_eq!(p, d.path().join("kirby.sav"));
    }

    #[test]
    fn next_to_rom_path_sits_beside_the_rom() {
        let cfg = SaveConfig {
            style: SaveStyle::NextToRom,
            ..SaveConfig::default()
        };
        let p = save_path_for(Path::new("/roms/kirby.nes"), 0xDEADBEEF, &cfg).unwrap();
        assert_eq!(p, PathBuf::from("/roms/kirby.sav"));
    }

    #[test]
    fn by_crc_uses_hex_crc_as_filename() {
        let d = tempdir();
        let cfg = SaveConfig {
            style: SaveStyle::ByCrc,
            dir_override: Some(d.path().to_path_buf()),
            ..SaveConfig::default()
        };
        let p = save_path_for(Path::new("/roms/whatever.nes"), 0xDEADBEEF, &cfg).unwrap();
        assert_eq!(p, d.path().join("DEADBEEF.sav"));
    }

    #[test]
    fn load_missing_returns_ok_none() {
        let d = tempdir();
        let p = d.path().join("nope.sav");
        assert!(matches!(load(&p), Ok(None)));
    }

    #[test]
    fn write_then_load_roundtrip() {
        let d = tempdir();
        let p = d.path().join("game.sav");
        let data = vec![0xAA, 0xBB, 0xCC, 0xDD];
        write(&p, &data).unwrap();
        assert_eq!(load(&p).unwrap(), Some(data));
    }

    #[test]
    fn write_replaces_existing_file_atomically() {
        let d = tempdir();
        let p = d.path().join("game.sav");
        write(&p, &[1, 2, 3]).unwrap();
        write(&p, &[9, 8, 7, 6]).unwrap();
        assert_eq!(load(&p).unwrap(), Some(vec![9, 8, 7, 6]));
        // Tmp file must be gone after a clean rename.
        let tmp = tmp_path(&p);
        assert!(!tmp.exists(), "temp file should be renamed away, not left behind");
    }
}

// Tiny self-contained temp-dir helper — avoid adding a dev-dep just
// for tests. Drop cleans up the directory recursively.
#[cfg(test)]
mod tempdir {
    use std::path::{Path, PathBuf};

    pub struct Dir {
        path: PathBuf,
    }

    impl Dir {
        pub fn new(prefix: &str) -> Self {
            let base = std::env::temp_dir();
            // Nanosecond-since-epoch + process ID is unique enough
            // for `cargo test`'s small concurrency. Collisions retry
            // would add complexity for no real payoff.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = base.join(format!("{prefix}-{}-{}", std::process::id(), now));
            std::fs::create_dir(&path).expect("mkdir tempdir");
            Self { path }
        }

        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
