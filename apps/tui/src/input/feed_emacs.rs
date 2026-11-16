//! The Emacs profile's key path (#1356): `feed_emacs` (prefix argument, `ESC`-as-Meta, the nine-tier
//! keymap) and `feed_emacs_prefix` (the `C-x` / `C-c` maps). Moved out of `input/mod.rs` as an
//! `impl InputEngine` block; the keymap tables and data types live in [`emacs`](super::emacs).

use super::*;

impl InputEngine {
    /// The Emacs profile's dispatch (F-012, minimal slice). Non-modal: the global-map's `C-` motions
    /// resolve to commands; an unmodified printable key inserts literally. A `C-u`/digit prefix argument
    /// (D-049) accumulates ahead of the command it modifies. This is the seam — the nine-tier stack, the
    /// `C-x` prefix maps, the kill ring and the mark ring layer on from here (RFC-0014). Motions work while
    /// the buffer stays editable because a `Move*` command keeps the current mode and only moves the cursor,
    /// so "move + insert in one state" needs no new mode.
    pub(super) fn feed_emacs(&mut self, mut key: KeyEvent) -> Feed {
        // The `M-x` minibuffer owns the keystream while open (F-026 command-line namespace), reused verbatim
        // from the Vim `:`-line handler — its `<CR>` resolves the command NAME (the `cl.mx` branch).
        if self.cmdline.is_some() {
            return self.feed_cmdline(key);
        }
        // ESC is the meta prefix: the key after a bare ESC is that key with Meta held, however much later it
        // arrives (`ESC u` = `M-u`). Not while a `C-x` prefix is pending: that prefix owns its next key.
        if self.emacs_prefix.is_none() {
            if std::mem::take(&mut self.emacs_meta) {
                key.modifiers |= KeyModifiers::ALT;
            } else if key.code == KeyCode::Esc && key.modifiers.is_empty() {
                self.emacs_meta = true;
                return Feed::Pending;
            }
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // A pending prefix key owns the NEXT keystroke: resolve it in that prefix's map before any global
        // dispatch. `C-g` (keyboard-quit) or any unbound key cancels the prefix (Emacs beeps). The prefix
        // is always cleared here, so a single stray key can never leave the engine wedged in a prefix.
        if let Some(prefix) = self.emacs_prefix.take() {
            return self.feed_emacs_prefix(prefix, key);
        }
        // `C-u` (universal argument): seed the prefix argument, or multiply an in-progress one by four.
        // It never completes a command — it always leaves the argument pending for the next key.
        if ctrl && key.code == KeyCode::Char('u') {
            self.emacs_arg = Some(match self.emacs_arg {
                Some(arg) => arg.times_four(),
                None => EmacsArg::ctrl_u(),
            });
            return Feed::Pending;
        }
        // While an argument is being read, an unmodified digit extends it (an explicit numeric count) rather
        // than self-inserting. Only when an argument is already pending — a bare digit is ordinary text.
        if self.emacs_arg.is_some() && !ctrl {
            if let KeyCode::Char(d @ '0'..='9') = key.code {
                let arg = self.emacs_arg.unwrap_or_else(EmacsArg::ctrl_u);
                self.emacs_arg = Some(arg.push_digit(d as u32 - '0' as u32));
                return Feed::Pending;
            }
        }
        // Any other key completes the argument: take the accumulated count (default 1) and resolve the key
        // through the nine-tier stack (F-012 / D-045). The count is folded per the bound key's policy.
        let count = self.emacs_arg.take().map(EmacsArg::count).unwrap_or(1);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Resolve, then drop the borrow of `self.emacs` before mutating engine state (the prefix map).
        let step = match self.emacs.map.resolve(&EmacsKey::of(key)) {
            Resolved::Bound { value, .. } => Step::Bound(value.clone()),
            // global-map's `self-insert-command`: an UNMODIFIED printable key inserts. A `C-`/`M-` key that
            // reached here is unbound, not text, so it stays inert.
            Resolved::Unmatched {
                policy: UnmatchedKey::Insert,
                ..
            } if !ctrl && !alt => match key.code {
                KeyCode::Char(c) => Step::SelfInsert(c),
                _ => Step::Ignore,
            },
            _ => Step::Ignore,
        };
        match step {
            Step::Bound(EmacsBinding::Prefix(p)) => {
                // Enter the prefix map: the next key resolves there. Any pending argument is dropped in this
                // slice (arg-passthrough to a prefixed command is a follow-up).
                self.emacs_prefix = Some(p);
                Feed::Pending
            }
            Step::Bound(EmacsBinding::Minibuffer) => {
                // M-x: open the minibuffer; subsequent keys route through `feed_cmdline` until `<CR>`.
                self.open_minibuffer();
                Feed::Pending
            }
            Step::Bound(binding) => fold_emacs_count(&binding, count),
            Step::SelfInsert(c) => emacs_repeat(Command::InsertChar(c), count),
            Step::Ignore => Feed::Ignored,
        }
    }

    /// Resolve the second key of an Emacs prefix sequence (F-012). Only the `C-x` map exists in this slice:
    /// [`emacs::EMACS_CX_MAP`] (`C-x C-s` saves, `C-x C-c` quits, `C-x u` undoes, …). An unbound key
    /// (including `C-g`) cancels the prefix and is inert — the prefix was already cleared by the caller, so
    /// the engine is never left wedged.
    fn feed_emacs_prefix(&mut self, prefix: char, key: KeyEvent) -> Feed {
        let plain = |c: char| key.code == KeyCode::Char(c) && key.modifiers.is_empty();
        match prefix {
            // `C-x x` (`ctl-x-x-map`): `C-x x g` is `revert-buffer-quick`.
            'x' if plain('x') => {
                self.emacs_prefix = Some('X');
                Feed::Pending
            }
            'X' if plain('g') => Feed::EmacsAction(emacs::EmacsAction::RevertBufferQuick),
            'x' => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                if let KeyCode::Char(c) = key.code {
                    if let Some(&(_, _, a)) = emacs::EMACS_CX_ACTIONS
                        .iter()
                        .find(|(k, kc, _)| *k == c && *kc == ctrl)
                    {
                        return Feed::EmacsAction(a);
                    }
                }
                emacs::emacs_cx_command(key).map_or(Feed::Ignored, Feed::Cmd)
            }
            _ => Feed::Ignored,
        }
    }
}
