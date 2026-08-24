//! Command-line history rings and the recall walk (`:help cmdline-history`). Session-scoped frontend
//! state: a `CmdHistory` ring holds ACCEPTED lines (oldest first, most-recent last), and a `HistWalk`
//! holds the transient cursor + saved draft for one open prompt. Kept out of `crates/core` — this is
//! UI/session state, never buffer content. Vim keeps `:` (ex) and `/`+`?` (search) as SEPARATE rings;
//! the engine owns one of each (`InputEngine::ex_history` / `search_history`).
//!
//! Behaviour verified against nvim v0.12.4 (see the unit tests): `<Up>`/`<Down>` recall PREFIX-FILTERED
//! by the draft typed before the walk began; `<C-p>`/`<C-n>` walk the RAW ring unfiltered; `<Down>` past
//! the newest restores the saved draft; an immediate repeat is de-duplicated by moving it to the end;
//! empty lines are not stored; the ring evicts oldest-first at its cap.

/// The default history length. Vim's 'history' default is 10000; v0 uses a smaller cap (the ring is
/// in-memory only — no shada/viminfo persistence yet), which is ample for a session and cheap to hold.
pub(crate) const DEFAULT_CAP: usize = 200;

/// One history ring: accepted command/search lines, oldest first and most-recent LAST. Bounded at `cap`
/// (oldest evicted first). De-duplicates an immediate repeat the Vim way — a re-entered identical line is
/// removed from its old position and re-appended, so it becomes the most-recent single entry.
pub(crate) struct CmdHistory {
    entries: Vec<String>,
    cap: usize,
}

/// The transient recall cursor for ONE open prompt. `draft` is the text typed before the walk began
/// (captured lazily on the first recall, so it is the true "bottom" the walk returns to); `pos` is the
/// index into the ring of the currently-shown entry, or `None` when showing the draft.
#[derive(Default)]
pub(crate) struct HistWalk {
    draft: Option<String>,
    pos: Option<usize>,
}

impl HistWalk {
    /// Capture `current` as the walk's draft on the FIRST recall only. Later recalls keep the original
    /// draft (so the prefix filter and the draft-restore both anchor to what was typed, not to a recalled
    /// entry). Idempotent after the first call.
    fn ensure_draft(&mut self, current: &str) {
        if self.draft.is_none() {
            self.draft = Some(current.to_string());
        }
    }
}

impl CmdHistory {
    pub(crate) fn new(cap: usize) -> CmdHistory {
        CmdHistory {
            entries: Vec::new(),
            cap: cap.max(1),
        }
    }

    #[cfg(test)]
    pub(crate) fn entries(&self) -> &[String] {
        &self.entries
    }

    /// The ring's entries (oldest first, most-recent last) — the content the command-line window mirrors
    /// (`:help cmdwin`). Read-only; the window never mutates the ring except via the normal accept path.
    pub(crate) fn entries_ref(&self) -> &[String] {
        &self.entries
    }

    /// Push an ACCEPTED line onto the ring (`<CR>`). Empty lines are not stored. An identical existing
    /// entry is removed first, so a re-entered line moves to the most-recent slot (Vim de-dup). Evicts the
    /// oldest entry when over `cap`.
    pub(crate) fn push(&mut self, line: &str) {
        if line.is_empty() {
            return;
        }
        if let Some(i) = self.entries.iter().position(|e| e == line) {
            self.entries.remove(i);
        }
        self.entries.push(line.to_string());
        while self.entries.len() > self.cap {
            self.entries.remove(0);
        }
    }

    /// Recall the previous (OLDER) entry. `filter` selects Vim's two behaviours: `true` for `<Up>` —
    /// return only entries whose start matches the saved draft prefix; `false` for `<C-p>` — walk the raw
    /// ring. Returns the entry to show, or `None` if there is no older match (the walk stays put). The
    /// draft is captured from `current` on the first recall of the walk.
    pub(crate) fn recall_prev(
        &self,
        walk: &mut HistWalk,
        current: &str,
        filter: bool,
    ) -> Option<String> {
        walk.ensure_draft(current);
        let prefix = if filter { walk.draft.as_deref() } else { None };
        let start = walk.pos.unwrap_or(self.entries.len());
        let mut i = start;
        while i > 0 {
            i -= 1;
            if prefix.is_none_or(|p| self.entries[i].starts_with(p)) {
                walk.pos = Some(i);
                return Some(self.entries[i].clone());
            }
        }
        None
    }

    /// Recall the next (MORE RECENT) entry. `filter` mirrors [`Self::recall_prev`] (`<Down>` vs `<C-n>`).
    /// Walking past the newest match restores the saved draft (Vim: `<Down>` past the newest returns what
    /// you were typing). Returns the entry/draft to show, or `None` if already at the draft (bottom).
    pub(crate) fn recall_next(
        &self,
        walk: &mut HistWalk,
        current: &str,
        filter: bool,
    ) -> Option<String> {
        walk.ensure_draft(current);
        let Some(start) = walk.pos else {
            return None; // already at the draft
        };
        let prefix = if filter { walk.draft.as_deref() } else { None };
        let mut i = start + 1;
        while i < self.entries.len() {
            if prefix.is_none_or(|p| self.entries[i].starts_with(p)) {
                walk.pos = Some(i);
                return Some(self.entries[i].clone());
            }
            i += 1;
        }
        // Past the newest match: return to the draft.
        walk.pos = None;
        Some(walk.draft.clone().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(items: &[&str]) -> CmdHistory {
        let mut h = CmdHistory::new(DEFAULT_CAP);
        for it in items {
            h.push(it);
        }
        h
    }

    #[test]
    fn empty_line_is_not_stored() {
        let mut h = CmdHistory::new(DEFAULT_CAP);
        h.push("");
        assert!(h.entries().is_empty());
    }

    #[test]
    fn dedup_moves_repeat_to_end() {
        // Vim: re-entering an identical line removes the old occurrence and re-appends it.
        let h = ring(&["foo", "bar", "foo"]);
        assert_eq!(h.entries(), &["bar", "foo"]);
    }

    #[test]
    fn cap_evicts_oldest_first() {
        let mut h = CmdHistory::new(2);
        h.push("a");
        h.push("b");
        h.push("c");
        assert_eq!(h.entries(), &["b", "c"]);
    }

    #[test]
    fn up_walks_newest_to_oldest_unfiltered() {
        // Matches nvim: ':' <Up> = newest, <Up> again = next older.
        let h = ring(&["set foo", "echo x", "edit a", "echo y", "edit b"]);
        let mut w = HistWalk::default();
        assert_eq!(h.recall_prev(&mut w, "", false).as_deref(), Some("edit b"));
        assert_eq!(h.recall_prev(&mut w, "", false).as_deref(), Some("echo y"));
    }

    #[test]
    fn up_prefix_filters_by_draft() {
        // nvim: ':edit' <Up> -> 'edit b', <Up> -> 'edit a' (skips 'echo y'); prefix stays the ORIGINAL
        // draft ('edit'), not the recalled line.
        let h = ring(&["set foo", "echo x", "edit a", "echo y", "edit b"]);
        let mut w = HistWalk::default();
        assert_eq!(
            h.recall_prev(&mut w, "edit", true).as_deref(),
            Some("edit b")
        );
        assert_eq!(
            h.recall_prev(&mut w, "edit", true).as_deref(),
            Some("edit a")
        );
        // No older 'edit…' entry: the walk stays put.
        assert_eq!(h.recall_prev(&mut w, "edit", true), None);
    }

    #[test]
    fn ctrl_p_is_unfiltered_even_with_prefix() {
        // nvim VERIFIED distinction: <C-p> ignores the typed prefix and walks the raw ring.
        // ':edit' <C-p> -> 'edit b' (raw newest), <C-p> -> 'echo y' (raw next, NOT prefix-filtered).
        let h = ring(&["set foo", "echo x", "edit a", "echo y", "edit b"]);
        let mut w = HistWalk::default();
        assert_eq!(
            h.recall_prev(&mut w, "edit", false).as_deref(),
            Some("edit b")
        );
        assert_eq!(
            h.recall_prev(&mut w, "edit", false).as_deref(),
            Some("echo y")
        );
    }

    #[test]
    fn down_restores_the_typed_draft() {
        // nvim: ':edit' <Up> <Down> -> 'edit' (the draft comes back).
        let h = ring(&["set foo", "echo x", "edit a", "echo y", "edit b"]);
        let mut w = HistWalk::default();
        assert_eq!(
            h.recall_prev(&mut w, "edit", true).as_deref(),
            Some("edit b")
        );
        assert_eq!(h.recall_next(&mut w, "edit", true).as_deref(), Some("edit"));
        // Already at the draft: another <Down> is a no-op.
        assert_eq!(h.recall_next(&mut w, "edit", true), None);
    }

    #[test]
    fn down_walks_back_toward_newest() {
        // nvim: ':edit' <Up><Up> -> 'edit a', <Down> -> 'edit b'.
        let h = ring(&["set foo", "echo x", "edit a", "echo y", "edit b"]);
        let mut w = HistWalk::default();
        h.recall_prev(&mut w, "edit", true);
        h.recall_prev(&mut w, "edit", true);
        assert_eq!(
            h.recall_next(&mut w, "edit", true).as_deref(),
            Some("edit b")
        );
    }

    #[test]
    fn recall_next_from_draft_is_noop() {
        let h = ring(&["foo"]);
        let mut w = HistWalk::default();
        assert_eq!(h.recall_next(&mut w, "", true), None);
    }

    #[test]
    fn empty_ring_prev_is_noop() {
        let h = ring(&[]);
        let mut w = HistWalk::default();
        assert_eq!(h.recall_prev(&mut w, "e", true), None);
    }
}
