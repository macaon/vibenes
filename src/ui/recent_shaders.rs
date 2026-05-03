// SPDX-License-Identifier: GPL-3.0-or-later
//! Disk-persisted ring of recently-loaded shader presets, surfaced
//! by the View > Shader > Recent submenu.
//!
//! Lives next to the input bindings under
//! `$XDG_CONFIG_HOME/vibenes/shaders.toml` (or the
//! `$HOME/.config/...` fallback). Format is intentionally simple so
//! a user can hand-edit it - the file is just a list of absolute
//! paths.
//!
//! ```toml
//! recent = [
//!   "/home/user/Git/slang-shaders/crt/crt-easymode.slangp",
//!   "/home/user/.local/share/vibenes/shaders/foo.slangp",
//! ]
//! ```
//!
//! Persistence is best-effort: read failures fall back to an empty
//! list, write failures are logged but don't bubble up. The list is
//! sibling to [`crate::ui::recent::RecentRoms`] but has its own
//! storage so a corrupted shader-history doesn't take the ROM
//! history down with it.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone)]
pub struct RecentShaders {
    paths: VecDeque<PathBuf>,
    /// Last shader path the user explicitly activated. `None`
    /// represents either "user has never picked one" or "user
    /// turned the shader off". The distinction doesn't matter at
    /// startup - both end with passthrough rendering. Persisted in
    /// the same file as `paths` so they share a fate (corrupted
    /// state, missing storage, etc.).
    active: Option<PathBuf>,
    /// Disk-persistence target, or `None` when running in an
    /// environment without `$HOME` (CI containers, sandbox tests).
    /// `push` is a no-op write in that case but the in-memory list
    /// still works.
    storage_path: Option<PathBuf>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RecentShadersFile {
    /// Last shader the user enabled. Skipped on serialise when
    /// `None` so an "off" session writes the field as absent
    /// rather than `active = ""` (cleaner for the user inspecting
    /// the file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<PathBuf>,
    #[serde(default)]
    recent: Vec<PathBuf>,
}

impl RecentShaders {
    /// Cap matches Mesen2's "Recent" lists - long enough to remember
    /// a session's worth of experimentation, short enough that the
    /// menu stays scannable.
    pub const MAX: usize = 10;

    /// Try to load the persisted file at `storage_path`. Missing or
    /// malformed files yield an empty state - we never refuse to
    /// start because of recent-list state.
    pub fn load_or_init(storage_path: Option<PathBuf>) -> Self {
        let parsed = storage_path
            .as_ref()
            .and_then(|p| read_file(p).ok())
            .unwrap_or_default();
        Self {
            paths: parsed.recent.into_iter().take(Self::MAX).collect(),
            active: parsed.active,
            storage_path,
        }
    }

    /// The shader the user most recently enabled, if it still
    /// exists on disk. `None` covers three cases the menu wires up
    /// the same way: user never picked one, user explicitly turned
    /// the shader off, or the file was deleted between sessions.
    pub fn active(&self) -> Option<&Path> {
        match self.active.as_ref() {
            Some(p) if p.exists() => Some(p.as_path()),
            _ => None,
        }
    }

    /// Mark `path` as the active shader. Persisted immediately so
    /// the choice survives a SIGKILL.
    pub fn set_active(&mut self, path: PathBuf) {
        self.active = Some(path);
        if let Err(e) = self.persist() {
            log::warn!(
                "failed to persist active shader at {:?}: {e:#}",
                self.storage_path
            );
        }
    }

    /// Mark "no active shader" (None / Off). Persists.
    pub fn clear_active(&mut self) {
        self.active = None;
        if let Err(e) = self.persist() {
            log::warn!(
                "failed to clear active shader at {:?}: {e:#}",
                self.storage_path
            );
        }
    }

    /// Default disk path: `$XDG_CONFIG_HOME/vibenes/shaders.toml`
    /// with the standard `~/.config/...` fallback. Same resolution
    /// the input bindings use - keeps every per-user config under
    /// one directory.
    pub fn default_storage_path() -> Option<PathBuf> {
        crate::save::saves_dir().and_then(|d| d.parent().map(|p| p.join("shaders.toml")))
    }

    /// Promote `path` to the front of the list, dedupe by exact
    /// path, evict overflow, and write the result back to disk.
    /// Disk-write failures are logged - we never panic on a
    /// best-effort persistence path.
    pub fn push(&mut self, path: PathBuf) {
        self.paths.retain(|p| p != &path);
        self.paths.push_front(path);
        while self.paths.len() > Self::MAX {
            self.paths.pop_back();
        }
        if let Err(e) = self.persist() {
            log::warn!(
                "failed to persist shader recent list at {:?}: {e:#}",
                self.storage_path
            );
        }
    }

    /// Return only paths that still exist on disk, in MRU order.
    /// Used by the menu so missing files (user moved or deleted a
    /// preset) don't clutter the list with broken entries. The
    /// persisted file isn't pruned - if the file comes back, it
    /// still pops up in Recent next session.
    pub fn iter_existing(&self) -> impl Iterator<Item = &PathBuf> {
        self.paths.iter().filter(|p| p.exists())
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// All persisted entries, even ones that don't currently exist.
    /// Mostly useful for tests; the UI prefers `iter_existing`.
    #[allow(dead_code)]
    pub fn iter_all(&self) -> impl Iterator<Item = &PathBuf> {
        self.paths.iter()
    }

    fn persist(&self) -> anyhow::Result<()> {
        let Some(target) = self.storage_path.as_ref() else {
            return Ok(());
        };
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let payload = RecentShadersFile {
            active: self.active.clone(),
            recent: self.paths.iter().cloned().collect(),
        };
        let serialized = toml::to_string_pretty(&payload)?;
        // Atomic-ish write: write to a sibling tmp file, then
        // rename. Avoids partial-writes leaving a half-broken
        // shaders.toml behind on a SIGKILL mid-write.
        let mut tmp = target.clone();
        tmp.as_mut_os_string().push(".tmp");
        std::fs::write(&tmp, serialized)?;
        std::fs::rename(&tmp, target)?;
        Ok(())
    }
}

fn read_file(path: &Path) -> anyhow::Result<RecentShadersFile> {
    let s = std::fs::read_to_string(path)?;
    let parsed: RecentShadersFile = toml::from_str(&s)?;
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_storage(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("vibenes-recent-shaders-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p.join("shaders.toml")
    }

    fn touch(p: &Path) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, b"").unwrap();
    }

    #[test]
    fn push_dedupes_and_moves_to_front() {
        let mut r = RecentShaders::default();
        r.push(PathBuf::from("/a.slangp"));
        r.push(PathBuf::from("/b.slangp"));
        r.push(PathBuf::from("/a.slangp"));
        let got: Vec<_> = r.iter_all().cloned().collect();
        assert_eq!(got, vec![PathBuf::from("/a.slangp"), PathBuf::from("/b.slangp")]);
    }

    #[test]
    fn push_caps_at_max() {
        let mut r = RecentShaders::default();
        for i in 0..(RecentShaders::MAX + 5) {
            r.push(PathBuf::from(format!("/{i}.slangp")));
        }
        assert_eq!(r.iter_all().count(), RecentShaders::MAX);
    }

    #[test]
    fn iter_existing_filters_missing() {
        let dir = std::env::temp_dir().join(format!(
            "vibenes-recent-existing-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.slangp");
        touch(&real);
        let fake = dir.join("missing.slangp");
        let mut r = RecentShaders::default();
        r.push(fake);
        r.push(real.clone());
        let got: Vec<_> = r.iter_existing().cloned().collect();
        assert_eq!(got, vec![real]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trips_through_disk() {
        let storage = tmp_storage("rt");
        let mut writer = RecentShaders::load_or_init(Some(storage.clone()));
        writer.push(PathBuf::from("/a.slangp"));
        writer.push(PathBuf::from("/b.slangp"));
        // Re-read from disk in a fresh instance.
        let reader = RecentShaders::load_or_init(Some(storage.clone()));
        let got: Vec<_> = reader.iter_all().cloned().collect();
        assert_eq!(
            got,
            vec![PathBuf::from("/b.slangp"), PathBuf::from("/a.slangp")]
        );
        let _ = std::fs::remove_dir_all(storage.parent().unwrap());
    }

    #[test]
    fn malformed_file_falls_back_to_empty() {
        let storage = tmp_storage("malformed");
        std::fs::write(&storage, b"this is not valid toml = = =").unwrap();
        let r = RecentShaders::load_or_init(Some(storage.clone()));
        assert!(r.is_empty());
        let _ = std::fs::remove_dir_all(storage.parent().unwrap());
    }

    #[test]
    fn set_and_clear_active_round_trip_through_disk() {
        let storage = tmp_storage("active");
        let dir = storage.parent().unwrap().to_path_buf();
        let preset = dir.join("real.slangp");
        touch(&preset);

        let mut writer = RecentShaders::load_or_init(Some(storage.clone()));
        writer.set_active(preset.clone());
        assert_eq!(writer.active(), Some(preset.as_path()));

        // Round-trip through disk and confirm active() returns the
        // path again.
        let reader = RecentShaders::load_or_init(Some(storage.clone()));
        assert_eq!(reader.active(), Some(preset.as_path()));

        // Now clear and verify the cleared state survives a reload.
        let mut writer = RecentShaders::load_or_init(Some(storage.clone()));
        writer.clear_active();
        assert!(writer.active().is_none());

        let reader = RecentShaders::load_or_init(Some(storage.clone()));
        assert!(reader.active().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_is_none_when_persisted_path_missing() {
        let storage = tmp_storage("ghost");
        let dir = storage.parent().unwrap().to_path_buf();

        // Persist an active path that doesn't exist on disk - mimics
        // the user moving a preset away between sessions.
        let mut writer = RecentShaders::load_or_init(Some(storage.clone()));
        writer.set_active(dir.join("ghost.slangp"));

        let reader = RecentShaders::load_or_init(Some(storage.clone()));
        assert!(
            reader.active().is_none(),
            "active() should be None when the persisted path no longer exists"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
