//! Battery-backed cartridge save file I/O.
//!
//! Path resolution today is "next to the ROM": `kirby.nes` pairs with
//! `kirby.sav` in the same directory. This is what FCEUX / Nestopia /
//! Mesen do by default and what collection-management tools expect.
//!
//! Writes are atomic: we stage to a `<path>.tmp` file, `fsync`, then
//! [`std::fs::rename`]. POSIX rename is atomic over the same
//! filesystem, so a crash mid-write leaves either the old save or the
//! new one — never a torn half-written file.
//!
//! **Future work**: [`crate::config::SaveStyle::ByCrc`] routes saves
//! to `<data_dir>/<CRC>.sav` instead. Fallback from `NextToRom` to
//! `ByCrc` when the ROM folder is read-only is also planned but not
//! wired yet — today a write failure to the ROM folder is just logged
//! and skipped.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::{SaveConfig, SaveStyle};

/// Extension appended to the ROM stem for save files.
pub const SAVE_EXT: &str = "sav";

/// Resolve the save-file path for a cartridge loaded from `rom_path`.
/// Returns `None` if no sensible path can be produced (`rom_path` has
/// no filename) — the caller should log and skip rather than panic.
///
/// Currently honors only [`SaveStyle::NextToRom`]; the `ByCrc` branch
/// is a stub that falls through to next-to-rom while the data-dir
/// resolution lives behind the future `dirs`-crate plumbing.
pub fn save_path_for(rom_path: &Path, _crc: u32, cfg: &SaveConfig) -> Option<PathBuf> {
    match cfg.style {
        SaveStyle::NextToRom => Some(rom_path.with_extension(SAVE_EXT)),
        SaveStyle::ByCrc => {
            // TODO: when the settings UI lands, resolve via
            //   cfg.dir_override.clone().unwrap_or_else(default_saves_dir)
            // with default_saves_dir() returning
            //   dirs::data_dir() / "vibenes" / "saves"
            // For now silently fall back so the plumbing still works.
            Some(rom_path.with_extension(SAVE_EXT))
        }
    }
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
    fn path_is_rom_with_sav_extension() {
        let cfg = SaveConfig::default();
        let p = save_path_for(Path::new("/roms/kirby.nes"), 0xDEADBEEF, &cfg).unwrap();
        assert_eq!(p, PathBuf::from("/roms/kirby.sav"));
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
