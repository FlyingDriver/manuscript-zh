//! Browser-style navigation history. Pure logic (no GTK types) so it can be
//! unit-tested: a back stack and a forward stack of [`Location`]s.

use std::path::{Path, PathBuf};

/// Maximum entries kept per stack.
const CAP: usize = 50;

/// Scroll position: char offset of the TextBuffer position at the top of the
/// viewport. An offset rather than a line: a rendered paragraph is one buffer
/// line however many screen lines it wraps to (soft breaks are joined), so a
/// line would snap back to the paragraph's start. `source` distinguishes the
/// source buffer (Edit / Split) from the rendered buffer (View); the two are
/// not interchangeable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pos {
    pub offset: i32,
    /// Pixels from the top of `offset`'s display line down to the viewport
    /// top. A tall line (image, rich table) is a single display line, so
    /// without this a position inside it would snap back to its top.
    pub delta: i32,
    pub source: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub pos: Pos,
}

#[derive(Default)]
pub struct History {
    back: Vec<Location>,
    forward: Vec<Location>,
}

fn push_capped(stack: &mut Vec<Location>, loc: Location) {
    if stack.last() == Some(&loc) {
        return;
    }
    stack.push(loc);
    if stack.len() > CAP {
        stack.remove(0);
    }
}

/// Pop entries until one still exists and differs from `current`.
fn pop_valid(
    stack: &mut Vec<Location>,
    current: &Location,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<Location> {
    while let Some(loc) = stack.pop() {
        if &loc != current && exists(&loc.path) {
            return Some(loc);
        }
    }
    None
}

impl History {
    /// Record a new navigation away from `loc`: push onto back, clear forward.
    pub fn push(&mut self, loc: Location) {
        push_capped(&mut self.back, loc);
        self.forward.clear();
    }

    /// Step back from `current`. Returns the location to go to.
    pub fn back(
        &mut self,
        current: Location,
        exists: &dyn Fn(&Path) -> bool,
    ) -> Option<Location> {
        let target = pop_valid(&mut self.back, &current, exists)?;
        push_capped(&mut self.forward, current);
        Some(target)
    }

    /// Step forward from `current`. Returns the location to go to.
    pub fn forward(
        &mut self,
        current: Location,
        exists: &dyn Fn(&Path) -> bool,
    ) -> Option<Location> {
        let target = pop_valid(&mut self.forward, &current, exists)?;
        push_capped(&mut self.back, current);
        Some(target)
    }

    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(p: &str, offset: i32) -> Location {
        Location {
            path: PathBuf::from(p),
            pos: Pos { offset, delta: 0, source: false },
        }
    }

    fn all(_: &Path) -> bool {
        true
    }

    #[test]
    fn push_back_forward_roundtrip() {
        let mut h = History::default();
        h.push(loc("a", 0));
        h.push(loc("b", 5));
        assert!(h.can_back() && !h.can_forward());
        assert_eq!(h.back(loc("c", 1), &all), Some(loc("b", 5)));
        assert!(h.can_forward());
        assert_eq!(h.back(loc("b", 5), &all), Some(loc("a", 0)));
        assert_eq!(h.forward(loc("a", 0), &all), Some(loc("b", 5)));
        assert_eq!(h.forward(loc("b", 5), &all), Some(loc("c", 1)));
        assert!(h.forward(loc("c", 1), &all).is_none());
    }

    #[test]
    fn push_clears_forward_and_dedupes() {
        let mut h = History::default();
        h.push(loc("a", 0));
        h.push(loc("a", 0));
        assert_eq!(h.back.len(), 1);
        h.back(loc("b", 0), &all);
        assert!(h.can_forward());
        h.push(loc("x", 0));
        assert!(!h.can_forward());
    }

    #[test]
    fn missing_files_are_dropped() {
        let mut h = History::default();
        h.push(loc("a", 0));
        h.push(loc("gone", 0));
        let exists = |p: &Path| p != Path::new("gone");
        assert_eq!(h.back(loc("c", 0), &exists), Some(loc("a", 0)));
        assert!(!h.can_back());
        assert!(h.back(loc("a", 0), &exists).is_none());
    }

    #[test]
    fn entry_equal_to_current_is_skipped() {
        let mut h = History::default();
        h.push(loc("a", 3));
        assert!(h.back(loc("a", 3), &all).is_none());
        assert!(!h.can_forward());
    }

    #[test]
    fn cap_is_enforced() {
        let mut h = History::default();
        for i in 0..80 {
            h.push(loc("a", i));
        }
        assert_eq!(h.back.len(), CAP);
        assert_eq!(h.back[0], loc("a", 30));
    }
}
