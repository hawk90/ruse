//! A generic modal "special view" picker overlay (view-window-workspace.md §7 VW-OVERLAY / F-013 NAT-3):
//! a typed query, a case-insensitive **fuzzy-subsequence** filtered match list ranked best-first, a
//! selection, and a transient keymap. The command palette, line picker, buffer picker, and file picker
//! are all `Picker<T>` over different payloads (`Command` / byte offset / `DocumentId` / `PathBuf`) and
//! different accept-actions — the caller performs the payload-specific action when a row is accepted.

use crossterm::event::{KeyCode, KeyEvent};

/// Score `needle` (already lowercased) as an ordered subsequence of `hay` (already lowercased), returning
/// `None` when it is not a subsequence. Higher is better: a contiguous run and matches at word starts (after
/// a non-alphanumeric) score well, gaps and a late first match are penalised — so `"beta"` ranks the exact
/// item above `"gamma beta"`, and `"fb"` finds `"foo_bar"`. Greedy left-to-right (fzf-lite): fast, good
/// enough for a picker, and stable. An empty needle matches everything at score 0 (preserving source order).
fn fuzzy_score(hay: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = hay.chars().collect();
    let needle: Vec<char> = needle.chars().collect();
    let mut score = 0i32;
    let mut ni = 0usize;
    let mut prev: Option<usize> = None;
    for (hi, &hc) in hay.iter().enumerate() {
        if ni < needle.len() && hc == needle[ni] {
            if hi == 0 || !hay[hi - 1].is_alphanumeric() {
                score += 10; // start of a word
            }
            match prev {
                Some(p) if p + 1 == hi => score += 15, // contiguous with the last match
                Some(p) => score -= ((hi - p - 1) as i32).min(10), // gap since the last match
                None => score -= (hi as i32).min(10),  // distance to the first match
            }
            score += 1;
            prev = Some(hi);
            ni += 1;
        }
    }
    (ni == needle.len()).then_some(score)
}

/// One selectable row: `search` is matched against the query, `display` is painted, and `payload` is what
/// the caller acts on when the row is accepted.
pub(crate) struct PickItem<T> {
    pub(crate) search: String,
    pub(crate) display: String,
    pub(crate) payload: T,
}

/// The outcome of feeding one key to a [`Picker`]. On `Accept` the caller reads [`Picker::selected`] and
/// performs the action, then closes the overlay; on `Cancel` it just closes.
pub(crate) enum PickOutcome {
    Continue,
    Cancel,
    Accept,
}

/// A generic picker overlay: the shared query/filter/selection/key-nav machinery for every special-view
/// picker. Only the item source (how `PickItem`s are built) and the accept-action differ per picker.
pub(crate) struct Picker<T> {
    /// The typed filter.
    pub(crate) query: String,
    /// All items, in source order.
    items: Vec<PickItem<T>>,
    /// Indices into `items` matching the current query.
    matches: Vec<usize>,
    /// Selected row into `matches`.
    selected: usize,
}

impl<T> Picker<T> {
    /// A picker over `items`, empty query (so every item matches), first row selected.
    pub(crate) fn new(items: Vec<PickItem<T>>) -> Picker<T> {
        let mut p = Picker {
            query: String::new(),
            items,
            matches: Vec::new(),
            selected: 0,
        };
        p.refilter();
        p
    }

    /// Move the selection to the first item whose payload satisfies `pred` (used by the buffer picker to
    /// open on the alternate buffer). No-op if none match.
    pub(crate) fn select_first(&mut self, pred: impl Fn(&T) -> bool) {
        if let Some(pos) = self
            .matches
            .iter()
            .position(|&i| pred(&self.items[i].payload))
        {
            self.selected = pos;
        }
    }

    /// Recompute `matches` from the query: keep items whose `search` text contains the query as a
    /// case-insensitive subsequence, ranked best score first. Ties keep source order (stable by index).
    fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        let mut scored: Vec<(i32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| fuzzy_score(&it.search.to_lowercase(), &q).map(|s| (s, i)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.matches = scored.into_iter().map(|(_, i)| i).collect();
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    /// The selected item's payload, or `None` when nothing matches.
    pub(crate) fn selected(&self) -> Option<&T> {
        self.matches
            .get(self.selected)
            .map(|&i| &self.items[i].payload)
    }

    /// The overlay's match rows (`display`, selected flag) for the above-the-status-line paint slot.
    pub(crate) fn rows(&self) -> Vec<(String, bool)> {
        self.matches
            .iter()
            .enumerate()
            .map(|(row, &i)| (self.items[i].display.clone(), row == self.selected))
            .collect()
    }

    /// Feed one key: Up/Down move, Backspace/Char edit + refilter, Enter = Accept, Esc = Cancel.
    pub(crate) fn on_key(&mut self, key: KeyEvent) -> PickOutcome {
        match key.code {
            KeyCode::Esc => return PickOutcome::Cancel,
            KeyCode::Enter => return PickOutcome::Accept,
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down if self.selected + 1 < self.matches.len() => self.selected += 1,
            KeyCode::Backspace => {
                self.query.pop();
                self.refilter();
            }
            KeyCode::Char(c) => {
                self.query.push(c);
                self.refilter();
            }
            _ => {}
        }
        PickOutcome::Continue
    }
}

#[cfg(test)]
mod picker_tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn items() -> Vec<PickItem<u32>> {
        ["alpha", "beta", "gamma beta"]
            .iter()
            .enumerate()
            .map(|(i, s)| PickItem {
                search: s.to_string(),
                display: s.to_string(),
                payload: i as u32,
            })
            .collect()
    }

    #[test]
    fn filters_and_ranks_and_tracks_selection() {
        let mut p = Picker::new(items());
        assert_eq!(p.rows().len(), 3, "empty query keeps all");
        assert_eq!(p.selected(), Some(&0));

        // Type "beta" → two matches (beta, gamma beta); the exact word ranks first, selection at row 0.
        for c in "beta".chars() {
            assert!(matches!(
                p.on_key(key(KeyCode::Char(c))),
                PickOutcome::Continue
            ));
        }
        assert_eq!(p.rows().len(), 2);
        assert_eq!(p.selected(), Some(&1));
        // Down moves; Down again is bounded.
        p.on_key(key(KeyCode::Down));
        assert_eq!(p.selected(), Some(&2));
        p.on_key(key(KeyCode::Down));
        assert_eq!(p.selected(), Some(&2), "Down is bounded to the last match");
    }

    #[test]
    fn matches_non_contiguous_subsequence() {
        // "gb" is a subsequence of "gamma beta" (g…b) but of neither "alpha" nor "beta".
        let mut p = Picker::new(items());
        for c in "gb".chars() {
            p.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            p.rows().len(),
            1,
            "gb matches only 'gamma beta' as a subsequence"
        );
        assert_eq!(p.selected(), Some(&2));
    }

    #[test]
    fn ranks_word_start_over_mid_word() {
        // "b" hits the start of "beta" (word boundary) and mid-word... "beta" and "gamma beta" both
        // contain 'b' at a word start, but "alpha" has none. The two 'b' items sort by score then order.
        let extra = vec![
            PickItem {
                search: "abbey".into(),
                display: "abbey".into(),
                payload: 9u32,
            },
            PickItem {
                search: "bee".into(),
                display: "bee".into(),
                payload: 8u32,
            },
        ];
        let mut p = Picker::new(extra);
        p.on_key(key(KeyCode::Char('b')));
        // "bee" starts with b (word-start bonus, no leading distance) → ranks above "abbey" (b at idx1).
        assert_eq!(
            p.selected(),
            Some(&8),
            "a leading-b word outranks a mid-word b"
        );
    }

    #[test]
    fn enter_accepts_esc_cancels_and_select_first_preselects() {
        let mut p = Picker::new(items());
        p.select_first(|&payload| payload == 2);
        assert_eq!(p.selected(), Some(&2), "select_first preselects by payload");
        assert!(matches!(p.on_key(key(KeyCode::Enter)), PickOutcome::Accept));
        assert!(matches!(p.on_key(key(KeyCode::Esc)), PickOutcome::Cancel));

        // A no-match query yields no selection.
        for c in "zzz".chars() {
            p.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(p.selected(), None);
    }
}
