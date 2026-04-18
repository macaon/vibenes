//! Most-recently-opened ROM list for the File menu. In-memory for
//! sub-phase 2 — disk persistence (to `~/.config/vibenes/recent.json`)
//! is a follow-up.

use std::collections::VecDeque;
use std::path::PathBuf;

/// Ring of recent ROM paths, most-recent first. Duplicates are moved
/// to the front rather than inserted twice, so re-opening a ROM
/// already in the list doesn't evict older entries.
#[derive(Debug, Default, Clone)]
pub struct RecentRoms {
    paths: VecDeque<PathBuf>,
}

impl RecentRoms {
    pub const MAX: usize = 8;

    pub fn push(&mut self, path: PathBuf) {
        self.paths.retain(|p| p != &path);
        self.paths.push_front(path);
        while self.paths.len() > Self::MAX {
            self.paths.pop_back();
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &PathBuf> {
        self.paths.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn p(s: &str) -> PathBuf {
        Path::new(s).to_path_buf()
    }

    #[test]
    fn push_adds_to_front() {
        let mut r = RecentRoms::default();
        r.push(p("a.nes"));
        r.push(p("b.nes"));
        let got: Vec<_> = r.iter().cloned().collect();
        assert_eq!(got, vec![p("b.nes"), p("a.nes")]);
    }

    #[test]
    fn push_dedupes_by_moving_to_front() {
        let mut r = RecentRoms::default();
        r.push(p("a.nes"));
        r.push(p("b.nes"));
        r.push(p("c.nes"));
        // Re-push a — it should move to the front without losing b or c.
        r.push(p("a.nes"));
        let got: Vec<_> = r.iter().cloned().collect();
        assert_eq!(got, vec![p("a.nes"), p("c.nes"), p("b.nes")]);
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn capped_at_max_entries() {
        let mut r = RecentRoms::default();
        for i in 0..(RecentRoms::MAX + 3) {
            r.push(p(&format!("{i}.nes")));
        }
        assert_eq!(r.len(), RecentRoms::MAX);
        // Oldest entries (0, 1, 2) evicted; newest stays at front.
        let front = r.iter().next().unwrap();
        assert_eq!(front, &p(&format!("{}.nes", RecentRoms::MAX + 2)));
    }

    #[test]
    fn is_empty_on_default() {
        let r = RecentRoms::default();
        assert!(r.is_empty());
    }
}
