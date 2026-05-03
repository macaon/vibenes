// SPDX-License-Identifier: GPL-3.0-or-later
//! Discovers RetroArch shader presets on disk and groups them for
//! menu rendering. Pure logic - no UI, no wgpu - so the menu layer
//! can call `Catalog::scan(...)` once at startup (or when the user
//! hits "Rescan") and walk the resulting tree without reaching
//! back into the filesystem.
//!
//! ## Discovery sources
//!
//! Two roots are scanned independently and merged:
//!
//! - **Bundled**: presets shipped inside the `assets/shaders/`
//!   directory next to the binary (or under `$cwd/assets/shaders/`
//!   when running from `cargo run`).
//! - **User**: presets the user dropped under
//!   `$XDG_DATA_HOME/vibenes/shaders/` (Linux convention,
//!   `~/.local/share/vibenes/shaders/` fallback).
//!
//! Each root is walked recursively. Any file ending in `.slangp`,
//! `.glslp`, or `.cgp` becomes one [`ShaderEntry`].
//!
//! ## Naming
//!
//! The `.slangp` format has no `name = "..."` field, so the
//! convention is to derive the display name from the filename stem
//! (matches RetroArch's behaviour). We do a light beautification
//! (replace `-` / `_` with space) but no title-casing - shader
//! authors already pick their preferred capitalisation in the
//! filename.
//!
//! The category is the immediate parent directory relative to the
//! root (e.g., `crt-guest-advanced-fast/crt-guest-advanced-fast.slangp`
//! lives in category `crt-guest-advanced-fast`). Presets directly
//! under the root land in [`ShaderCategory::Uncategorised`].

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Where a preset was discovered. Drives menu grouping (Bundled
/// presets come first, then User presets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ShaderSource {
    Bundled,
    User,
}

/// Category bucket within a source. Either a directory name lifted
/// from the layout, or a sentinel for "lives directly under the
/// root".
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ShaderCategory {
    Named(String),
    Uncategorised,
}

impl ShaderCategory {
    pub fn label(&self) -> &str {
        match self {
            Self::Named(s) => s,
            Self::Uncategorised => "Misc",
        }
    }
}

/// One discovered preset. `path` is the on-disk filename the
/// runtime loads; `display_name` is what the menu shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShaderEntry {
    pub path: PathBuf,
    pub display_name: String,
    pub category: ShaderCategory,
    pub source: ShaderSource,
}

/// Result of scanning the bundled + user roots. Sorted internally
/// so the menu layer can iterate it in stable order.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    entries: Vec<ShaderEntry>,
}

impl Catalog {
    /// Walk both roots and build the entry list. Either root may be
    /// `None` (e.g., the user shaders dir doesn't exist yet) or
    /// missing on disk - both cases are silently ignored.
    pub fn scan(bundled_root: Option<&Path>, user_root: Option<&Path>) -> Self {
        let mut entries = Vec::new();
        if let Some(root) = bundled_root {
            scan_root(root, ShaderSource::Bundled, &mut entries);
        }
        if let Some(root) = user_root {
            scan_root(root, ShaderSource::User, &mut entries);
        }
        // Stable ordering: Bundled before User, then strictly
        // alphabetical by display name. The menu renders this as a
        // flat list under each source heading - users scan by name,
        // not by directory layout, so category-grouped sort is the
        // wrong default. (`grouped()` rebuckets by source+category
        // for callers that want a tree.)
        entries.sort_by(|a, b| {
            a.source
                .cmp(&b.source)
                .then_with(|| a.display_name.cmp(&b.display_name))
        });
        Self { entries }
    }

    pub fn entries(&self) -> &[ShaderEntry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Group entries by `(source, category)` for menu rendering.
    /// Returns a deterministic ordering thanks to `BTreeMap`.
    pub fn grouped(&self) -> BTreeMap<(ShaderSource, ShaderCategory), Vec<&ShaderEntry>> {
        let mut groups: BTreeMap<(ShaderSource, ShaderCategory), Vec<&ShaderEntry>> =
            BTreeMap::new();
        for entry in &self.entries {
            groups
                .entry((entry.source, entry.category.clone()))
                .or_default()
                .push(entry);
        }
        groups
    }

    /// Look up an entry by its on-disk path. Used to highlight the
    /// active shader in the menu after a load.
    pub fn find_by_path(&self, path: &Path) -> Option<&ShaderEntry> {
        self.entries.iter().find(|e| e.path == path)
    }
}

fn scan_root(root: &Path, source: ShaderSource, out: &mut Vec<ShaderEntry>) {
    if !root.is_dir() {
        return;
    }
    walk(root, root, source, out);
}

fn walk(root: &Path, dir: &Path, source: ShaderSource, out: &mut Vec<ShaderEntry>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, source, out);
            continue;
        }
        if !is_preset(&path) {
            continue;
        }
        out.push(ShaderEntry {
            display_name: display_name_from_path(&path),
            category: category_from_path(root, &path),
            path,
            source,
        });
    }
}

fn is_preset(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("slangp") | Some("glslp") | Some("cgp")
    )
}

fn display_name_from_path(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("(unknown)");
    beautify(stem)
}

/// Light beautification: replace `-` and `_` with space. No title
/// casing - shader authors pick their own capitalisation, and
/// upper-casing tokens like `xBR` or `2xSaI` would mangle them.
fn beautify(stem: &str) -> String {
    stem.replace(['-', '_'], " ")
}

fn category_from_path(root: &Path, path: &Path) -> ShaderCategory {
    let parent = path.parent();
    match parent {
        Some(p) if p == root => ShaderCategory::Uncategorised,
        Some(p) => {
            // Use only the immediate parent directory's name
            // relative to the root. Deeper nesting collapses into
            // one category - keeps the menu shallow.
            p.file_name()
                .and_then(|s| s.to_str())
                .map(|s| ShaderCategory::Named(beautify(s)))
                .unwrap_or(ShaderCategory::Uncategorised)
        }
        None => ShaderCategory::Uncategorised,
    }
}

/// Resolves the bundled `assets/shaders/` root by trying a few
/// well-known locations. Returns the first that exists, or `None`
/// if none of them do (e.g., a stripped-down install without
/// bundled shaders).
///
/// Search order:
///
/// 1. `$VIBENES_SHADERS_DIR` (explicit override)
/// 2. `<exe_dir>/assets/shaders` (Cargo build artefact alongside
///    the source tree)
/// 3. `<exe_dir>/../share/vibenes/shaders` (Linux system install)
/// 4. `<cwd>/assets/shaders` (running from the repo root)
pub fn bundled_shaders_dir() -> Option<PathBuf> {
    if let Ok(env) = std::env::var("VIBENES_SHADERS_DIR") {
        let p = PathBuf::from(env);
        if p.is_dir() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let local = exe_dir.join("assets").join("shaders");
            if local.is_dir() {
                return Some(local);
            }
            let system = exe_dir
                .parent()
                .map(|p| p.join("share/vibenes/shaders"))
                .filter(|p| p.is_dir());
            if let Some(p) = system {
                return Some(p);
            }
        }
    }
    let cwd_assets = PathBuf::from("assets/shaders");
    if cwd_assets.is_dir() {
        return Some(cwd_assets);
    }
    None
}

/// User shaders dir: `$XDG_DATA_HOME/vibenes/shaders/`, falling
/// back to `~/.local/share/vibenes/shaders/`. Returns `None` when
/// `$HOME` isn't set (rare; CI containers). Does NOT create the
/// directory - the caller decides whether to mkdir on first use.
pub fn user_shaders_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("vibenes").join("shaders"));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("vibenes")
            .join("shaders"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        // Minimum viable preset content - the scanner doesn't read
        // file bodies, only filenames, but writing something keeps
        // behaviour realistic if a future test wants to parse them.
        f.write_all(b"shaders = 0\n").unwrap();
    }

    fn tmp(label: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("vibenes-shader-test-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn beautify_replaces_dashes_and_underscores_with_spaces() {
        assert_eq!(beautify("crt-tetchi-grill"), "crt tetchi grill");
        assert_eq!(beautify("ntsc_256px_composite"), "ntsc 256px composite");
        assert_eq!(beautify("xBR-lv2"), "xBR lv2");
    }

    #[test]
    fn beautify_keeps_authoring_casing() {
        // `2xSaI` and `xBR` should survive verbatim - no title-case.
        assert_eq!(beautify("2xSaI"), "2xSaI");
        assert_eq!(beautify("super-2xSaI"), "super 2xSaI");
    }

    #[test]
    fn is_preset_recognises_all_three_extensions() {
        assert!(is_preset(Path::new("a.slangp")));
        assert!(is_preset(Path::new("a.glslp")));
        assert!(is_preset(Path::new("a.cgp")));
        assert!(!is_preset(Path::new("a.slang"))); // shader source, not preset
        assert!(!is_preset(Path::new("a.png")));
        assert!(!is_preset(Path::new("a")));
    }

    #[test]
    fn scan_skips_missing_root_silently() {
        let absent = PathBuf::from("/definitely/does/not/exist/here");
        let cat = Catalog::scan(Some(&absent), None);
        assert!(cat.is_empty());
    }

    #[test]
    fn scan_finds_top_level_preset_as_uncategorised() {
        let root = tmp("toplvl");
        touch(&root.join("simple.slangp"));
        let cat = Catalog::scan(Some(&root), None);
        let entries = cat.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].display_name, "simple");
        assert_eq!(entries[0].category, ShaderCategory::Uncategorised);
        assert_eq!(entries[0].source, ShaderSource::Bundled);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_uses_immediate_parent_dir_as_category() {
        let root = tmp("nested");
        touch(&root.join("crt-guest-fast/crt-guest-fast.slangp"));
        touch(&root.join("ntsc/ntsc-256px.slangp"));
        let cat = Catalog::scan(Some(&root), None);
        let entries = cat.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].category,
            ShaderCategory::Named("crt guest fast".to_string())
        );
        assert_eq!(
            entries[1].category,
            ShaderCategory::Named("ntsc".to_string())
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_ignores_shader_source_files_and_luts() {
        let root = tmp("noise");
        touch(&root.join("preset.slangp"));
        touch(&root.join("preset.slang"));
        touch(&root.join("trinitron-lut.png"));
        touch(&root.join("LICENSE"));
        let cat = Catalog::scan(Some(&root), None);
        assert_eq!(cat.entries().len(), 1);
        assert_eq!(cat.entries()[0].display_name, "preset");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_merges_bundled_and_user_with_bundled_first() {
        let bundled = tmp("bundled");
        let user = tmp("user");
        touch(&bundled.join("a.slangp"));
        touch(&user.join("z.slangp"));
        let cat = Catalog::scan(Some(&bundled), Some(&user));
        let entries = cat.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].source, ShaderSource::Bundled);
        assert_eq!(entries[1].source, ShaderSource::User);
        let _ = fs::remove_dir_all(&bundled);
        let _ = fs::remove_dir_all(&user);
    }

    #[test]
    fn grouped_buckets_by_source_then_category() {
        let bundled = tmp("group-b");
        let user = tmp("group-u");
        touch(&bundled.join("crt/a.slangp"));
        touch(&bundled.join("crt/b.slangp"));
        touch(&bundled.join("ntsc/c.slangp"));
        touch(&user.join("custom/d.slangp"));
        let cat = Catalog::scan(Some(&bundled), Some(&user));
        let groups = cat.grouped();
        let crt_bundle = groups
            .get(&(
                ShaderSource::Bundled,
                ShaderCategory::Named("crt".to_string()),
            ))
            .unwrap();
        assert_eq!(crt_bundle.len(), 2);
        let custom_user = groups
            .get(&(
                ShaderSource::User,
                ShaderCategory::Named("custom".to_string()),
            ))
            .unwrap();
        assert_eq!(custom_user.len(), 1);
        let _ = fs::remove_dir_all(&bundled);
        let _ = fs::remove_dir_all(&user);
    }

    #[test]
    fn entries_sort_alphabetically_within_source_regardless_of_category() {
        let root = tmp("flat-sort");
        // Files spread across two categories - if the sort still
        // bucketed by category we'd see eagle's entries first; with
        // a pure alphabetical sort by display name the order
        // interleaves.
        touch(&root.join("eagle/2xsai.slangp"));
        touch(&root.join("eagle/super-2xsai.slangp"));
        touch(&root.join("hqx/hq2x.slangp"));
        touch(&root.join("ntsc/blargg.slangp"));
        let cat = Catalog::scan(Some(&root), None);
        let names: Vec<_> = cat.entries().iter().map(|e| e.display_name.as_str()).collect();
        assert_eq!(names, vec!["2xsai", "blargg", "hq2x", "super 2xsai"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn find_by_path_returns_matching_entry() {
        let root = tmp("findpath");
        let preset = root.join("foo.slangp");
        touch(&preset);
        let cat = Catalog::scan(Some(&root), None);
        let hit = cat.find_by_path(&preset);
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().display_name, "foo");
        let _ = fs::remove_dir_all(&root);
    }
}
