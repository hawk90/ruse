//! Puts and registers: `p`/`P` and their indent and block variants, and setting a register.
//!
//! Split out of [`super::plan_cmd`] (#1349); the arms are as they were there.

use super::*;

pub(super) fn plan(st: &EditorState, b: &[u8], cur: usize, cmd: &Command) -> Plan {
    match cmd {
        // Paste reads the pending register (`"xp`) or the unnamed slot; `commit` clears the pending slot.
        Command::Paste {
            after,
            count,
            move_after,
        } => paste(
            b,
            cur,
            st.view.mode,
            st.view.registers.get(st.view.pending_register),
            *after,
            *count,
            st.view.caret,
            *move_after,
            true,
            st.view.indent.tabs(),
        ),
        // `zp` / `zP` — `p` / `P`, but a block's short rows get no trailing spaces (nvim `PUT_BLOCK_INNER`).
        Command::PasteBlockInner { after, count } => paste(
            b,
            cur,
            st.view.mode,
            st.view.registers.get(st.view.pending_register),
            *after,
            *count,
            st.view.caret,
            false,
            false,
            st.view.indent.tabs(),
        ),
        // `]p`/`[p` — indent-adjusting paste. For a linewise register, reindent its lines to the current
        // line's indent before pasting; a charwise/blockwise register pastes unchanged (Vim `]p` = `p`).
        Command::PasteIndent { after, count } => {
            let reg = st.view.registers.get(st.view.pending_register);
            if reg.is_linewise() {
                let (target_cols, _) = indent_cols(
                    &b[line_start(b, cur)..line_end(b, cur)],
                    st.view.indent.tabs(),
                );
                let adjusted = reindent_register(
                    reg.text(),
                    target_cols,
                    st.view.indent.tabs(),
                    st.view.indent.style,
                );
                let tmp = Register::linewise(adjusted);
                paste(
                    b,
                    cur,
                    st.view.mode,
                    &tmp,
                    *after,
                    *count,
                    st.view.caret,
                    false,
                    true,
                    st.view.indent.tabs(),
                )
            } else {
                paste(
                    b,
                    cur,
                    st.view.mode,
                    reg,
                    *after,
                    *count,
                    st.view.caret,
                    false,
                    true,
                    st.view.indent.tabs(),
                )
            }
        }
        // `"x` — install the one-shot pending register. A pure state set: no edit, no cursor/mode change.
        Command::SetRegister(name) => Plan {
            action: Action::SetPending(*name),
            cursor: cur,
            mode: st.view.mode,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        // `"=<expr><CR>` — evaluate the expression, store the formatted result in the `"=` slot, and arm `=`
        // as the pending register so the following `p`/`P` pastes it. An empty result (malformed/unsupported
        // expression) makes that paste a no-op (Vim's degrade). Like `SetRegister`, a pure state set.
        Command::SetExprRegister(e) => Plan {
            action: Action::SetExprPending(crate::expr::eval_or_empty(e)),
            cursor: cur,
            mode: st.view.mode,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        _ => unreachable!("planner::put plans only its own commands"),
    }
}
