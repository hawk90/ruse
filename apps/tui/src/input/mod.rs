//! The input engine: a small pending-state machine that folds keys into semantic [`Command`]s
//! (`d`, `2w`, `d3w`, `dd`, `cw`=`ce`), plus ex-command (`:…`) parsing. The trace records the resulting
//! commands, so re-keymapping never invalidates a corpus. Pure and unit-tested.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::keymap::{Resolved, UnmatchedKey};
use ruse_core::{
    BlockInsertKind, Command, EditorOption, ForcedWise, GlobalCmd, LineAddr, Mode, Motion, OpKind,
    SearchOp, SelectKind, SubFlags, SubRange, WordCase,
};

/// The outcome of feeding one key to the engine.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Feed {
    /// A completed command to apply.
    Cmd(Command),
    /// `.` (dot-repeat): replay a recorded change as an ORDERED command list — the change's leading
    /// command followed by any insert-session text (F-023). The frontend applies each in turn, exactly as
    /// if the original keys were re-typed at the current cursor. Kept distinct from `Cmd` because one
    /// keypress expands to a compound edit, and because the driver must NOT re-record it as a new change.
    Replay(Vec<Command>),
    /// A completed `:`-line to execute (F-026). The command-line namespace owns the buffer while it is
    /// being typed; on `<CR>` it hands the finished text to the frontend to parse+run as an ex command.
    ExecuteEx(String),
    /// The key was consumed but the command is not complete yet (a count digit, a pending operator, or a
    /// keystroke absorbed into the open command-line buffer).
    Pending,
    /// Nothing bound.
    Ignored,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Delete,
    Change,
    Yank,
    /// `gu` — lowercase the operator span. `gU` = upper, `g~` = toggle.
    CaseLower,
    CaseUpper,
    CaseToggle,
    /// `>` / `<` — indent the operator span's lines right / left (linewise).
    ShiftRight,
    ShiftLeft,
    /// `=` — reindent the operator span's lines to their bracket depth (linewise).
    Reindent,
}

impl Op {
    /// The `WordCase` a case operator applies (`None` for the non-case operators).
    fn case(self) -> Option<WordCase> {
        match self {
            Op::CaseLower => Some(WordCase::Downcase),
            Op::CaseUpper => Some(WordCase::Upcase),
            Op::CaseToggle => Some(WordCase::Toggle),
            _ => None,
        }
    }

    /// Whether this is a shift operator, and its direction (`Some(true)` = left, `Some(false)` = right).
    fn shift(self) -> Option<bool> {
        match self {
            Op::ShiftLeft => Some(true),
            Op::ShiftRight => Some(false),
            _ => None,
        }
    }
}

/// The operator-pending axis: an armed operator (`d`/`c`/`y`) plus the count that preceded it.
#[derive(Clone, Copy, PartialEq, Eq)]
struct OpPending {
    op: Op,
    count: u32,
}

/// The one-shot key-expectation axis — what the *next* key must supply. Held separately from the operator
/// and count axes (per input-engine.md, these are **orthogonal** and must not be crammed into one enum).
/// Exactly one variant holds between keystrokes, so illegal combinations (awaiting a find-target *and* a
/// text-object char at once) are unrepresentable — the class of hierarchy bug that ad-hoc booleans invite.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Awaiting {
    /// A fresh key: a count digit, operator, motion, action, or a pending-initiator (`f`/`F`/`t`/`T`).
    #[default]
    Nothing,
    /// After `f`/`F`/`t`/`T`: the next key is the search target char.
    FindTarget { forward: bool, till: bool },
    /// After `i`/`a`: the next key is the text-object selector (`w`, `(`, `"`, …). Armed only with an
    /// operator present (`diw`) OR in a selection mode (`viw`) — never bare in Normal (invariant, tested).
    TextObjectChar { inner: bool },
    /// After `g`: a second `g` completes `gg` (jump to the first line / `{count}gg`).
    GSecond,
    /// After `r`: the next key is the replacement char.
    ReplaceChar,
    /// After `` ` `` (backtick): the next key names a mark to jump to — `.` (last change) or a named mark
    /// `a`–`z`. Any other key aborts.
    MarkJump,
    /// After `m`: the next key names the mark (`a`–`z`) to SET at the cursor. Any other key aborts.
    SetMarkChar,
    /// After `'` (apostrophe): the next key names a mark to jump to LINEWISE (first non-blank of its line) —
    /// `.` (last change) or `a`–`z`. Any other key aborts.
    MarkJumpLine,
    /// After `"`: the next key is the register NAME (`a`–`z`, or `A`–`Z` to append). It arms a one-shot
    /// pending register that the FOLLOWING yank/delete/change/paste targets — emitted as a
    /// [`Command::SetRegister`] the core applies before that command. `"` itself does not reset the count.
    RegisterSelect,
}

/// The Normal-grammar layer's OWNED state (KL-OBL-4): the three orthogonal transient axes of the
/// Normal / Visual / Select / Operator-pending family, which share one grammar. It is a field of the
/// engine, but it BELONGS to that layer family — it is dropped (`NormalState::default()`) the instant
/// the active namespace leaves the family (into Insert / Replace / Cmdline / Terminal), so the count
/// or armed operator can never survive into another layer. The engine no longer reaches in to reset
/// individual axes on a mode change; the layer's state dies with the layer.
#[derive(Default)]
struct NormalState {
    /// Count axis: the accumulating numeric prefix for the next motion/operator.
    count: u32,
    /// Operator axis: an armed operator awaiting its motion (`None` = none).
    op: Option<OpPending>,
    /// Key-expectation axis: what the next key must supply (the top-priority resolution tier).
    awaiting: Awaiting,
    /// A forced motion wise armed by `v`/`V` after an operator (Vim `o_v`/`o_V`): the NEXT motion
    /// resolves into a [`Command::OpForced`] instead of a plain operator command. `None` unless
    /// mid-`dv…`/`dV…`.
    forced_wise: Option<ForcedWise>,
}

impl NormalState {
    /// A PRISTINE Normal base: no count, no armed operator, no key-expectation, no forced wise. The Native
    /// leader tier (F-013 NAT-2) only arms from here, so a Space mid-construct (`d<Space>`, `2<Space>`)
    /// stays the Vim right-motion — the text grammar is untouched (NAT-1).
    fn is_clean(&self) -> bool {
        self.count == 0
            && self.op.is_none()
            && self.awaiting == Awaiting::Nothing
            && self.forced_wise.is_none()
    }
}

/// A SUSPENDED layer awaiting return (KL-OBL-5): while a one-shot command is borrowed to run in
/// another namespace, this records the ADDRESS to resume — *whence* control came. `i_CTRL-O` suspends
/// Insert to run one Normal command (`resume: Insert`); `t_CTRL-\ CTRL-O` (deferred, no terminal
/// buffers yet) is the SAME construct with `resume: Terminal`. A flat boolean edge cannot record
/// whence; a stack of these can, and nests for free.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Suspended {
    /// The namespace to resume once the borrowed one-shot command completes.
    resume: Ns,
}

/// The Insert layer's OWNED transient state (KL-OBL-4): the one-key `CTRL-G` prefix local to Insert.
/// Dropped (`InsertState::default()`) when the active namespace is not Insert — the layer's state dies
/// with the layer. (The `i_CTRL-O` one-shot is no longer a bool here; its RETURN ADDRESS lives on the
/// engine's activation stack — KL-OBL-5 — because a return spans two layers, not one.)
#[derive(Default)]
struct InsertState {
    /// Insert-mode `CTRL-G` prefix: the next key is expected to be `u` (undo-break,
    /// [`Command::BreakUndo`]). A one-key expectation local to Insert; any other second key aborts it.
    ctrl_g: bool,
}

/// The active input profile (F-012 / RFC-0014, F-013 / RFC-0016). Vim is a MODAL grammar (Normal/Insert/
/// Visual, operator-pending); Emacs is NON-MODAL (always editable, `C-` bindings are commands); Native is
/// the third language, whose TEXT layer REUSES the Vim modal grammar (NAT-1) and layers command-discovery
/// (leader/which-key, NAT-2), transient special-view maps (NAT-3) and a readline line (NAT-4) on top. The
/// profiles dispatch differently, so `feed` branches on this before any modal handling — they are not two
/// keymaps over one state machine. Only Emacs takes the non-modal path; Vim and Native share the modal path
/// (Native's distinctive layers are additive, they do not replace the text grammar). `input.profile`
/// (config-schema) selects it; no config loader exists yet, so it is set at construction (`InputEngine::new`
/// = Vim, `::emacs` = Emacs, `::native` = Native).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputProfile {
    Vim,
    Emacs,
    /// The Native profile (F-013). Modal text = the Vim grammar (NAT-1); command discovery, transient
    /// action maps and a readline line layer on additively. In this slice it is behaviourally Vim plus the
    /// distinct identity — the leader/which-key tier (NAT-2) et al. land in following slices.
    Native,
}

/// The Normal/Visual input state, held as three **orthogonal axes** — `count`, the operator-pending `op`,
/// and the one-shot `awaiting` key-expectation — plus sticky repeat state. `feed` resolves them in a fixed
/// precedence (mode → awaiting tier → base keys), so the hierarchy is explicit, not encoded in field order.
pub struct InputEngine {
    /// Which input profile is active — Vim (modal grammar) or Emacs (non-modal). Chosen at construction.
    input_profile: InputProfile,
    /// The active profile's layers. Built once — resolution must not allocate per keystroke.
    profile: VimProfile,
    /// The Normal-family grammar layer's owned transient state (count / operator / awaiting / forced
    /// wise). Dropped when the family deactivates — KL-OBL-4.
    normal: NormalState,
    /// Sticky (survives command completion): the last char-search `(ch, forward, till)`, for `;`/`,`.
    last_find: Option<(char, bool, bool)>,
    /// Sticky: the last search pattern, for `n`/`N`.
    last_search: Option<String>,
    /// Sticky: the last completed change, replayed by `.` (dot-repeat). `None` until the first change, so
    /// a bare `.` before any edit is a clean no-op.
    last_change: Option<ChangeIntent>,
    /// An in-flight change being recorded: set when an insert-entering command fires, then extended with the
    /// insert-session commands until the terminating `<Esc>` (`EnterNormal`) closes it into `last_change`.
    recording: Option<ChangeIntent>,
    /// The register named by the most recent `"x`, held only until the NEXT recorded change picks it up (so
    /// `.` replays it). Cleared by any intervening non-register command — a stray `"x` then a motion forgets.
    pending_record_register: Option<char>,
    /// The operator+count armed when `/` opened the search line, held across the minibuffer's pattern entry
    /// so `submit_search` can fold them into the finished command (`d/pat`, `2/pat`). `/` captures this
    /// BEFORE `reset()` wipes the axes; `None` between searches. See [`InputEngine::submit_search`].
    pending_search: Option<(SearchOp, u32)>,
    /// The Insert layer's owned transient state (the `CTRL-G` prefix). Dropped when the active
    /// namespace is not Insert — KL-OBL-4.
    insert: InsertState,
    /// The activation stack (KL-OBL-5): return ADDRESSES for one-shot commands one layer borrows to
    /// run in another. `i_CTRL-O` pushes `Suspended{resume: Insert}`; the completing command pops it,
    /// resuming Insert. Empty in steady state. A stack rather than a bool so it records *whence* and
    /// nests for free (the general form; `i_CTRL-O` is its depth-1 case, `t_CTRL-\ CTRL-O` its second).
    activations: Vec<Suspended>,
    /// The command-line namespace (F-026): while `Some`, keys are routed into its owned line buffer
    /// rather than the Normal grammar. `None` = not on the command line. This is the engine owning the
    /// line, not an ad-hoc text buffer on the UI (anti-pattern command-line P2).
    cmdline: Option<CmdLine>,
    /// The active Lang-Arg language map (`lmap`, F-027): a char→char rewrite applied by the pre-dispatch
    /// translation stage. Populated by `:lmap` at runtime (the persistent form is `keymap.lang`; no
    /// config-file loader exists for any `keymap.*` key yet). MVP restricts both sides to a single char.
    lang_map: HashMap<char, char>,
    /// Whether the language map is currently active (Vim `iminsert`/`imsearch`, toggled by `i_CTRL-^`).
    /// One boolean for MVP (RFC-0013); the per-context iminsert/imsearch model is a follow-up. `false`
    /// by default so a configured map never silently rewrites the command line you type to define it.
    lang_active: bool,
    /// The Emacs prefix argument being read (F-012 / D-049): `Some` while `C-u`/digits accumulate an
    /// argument that the NEXT command consumes. `None` in steady state and always `None` under the Vim
    /// profile (that profile folds the count into its own grammar). The value is held OPAQUE — each
    /// command decides how to fold it (motions multiply); this is the raw channel D-049 resolved.
    emacs_arg: Option<EmacsArg>,
    /// The pending Emacs prefix key (F-012): `Some('x')` after `C-x`, so the NEXT key resolves inside that
    /// prefix's map (`C-x C-s` = save) rather than the global map. `None` in steady state. This is the
    /// depth-1 case of the multi-key dispatch the nine-tier stack generalises; more prefixes (`C-c`, `C-h`)
    /// slot in by tag. Always `None` under the Vim profile.
    emacs_prefix: Option<char>,
    /// The Emacs profile's nine-tier keymap (F-012 / D-045). Built once; consulted only on the Emacs path.
    /// Present regardless of profile, mirroring `profile: VimProfile` — both are cheap to build.
    emacs: EmacsProfile,
    /// The Native profile's leader (which-key) tier is ARMED (F-013 NAT-2): `<leader>` (Space) was pressed
    /// from a clean Normal base, so the NEXT key resolves in the leader map rather than the Vim grammar.
    /// `false` in steady state and ALWAYS `false` under the Vim/Emacs profiles (only the Native+Normal path
    /// ever sets it). The depth-1 case of the discovery tier; nested groups (`<leader>g …`) layer on by tag.
    leader: bool,
}

/// A short human label for a key, for the palette's binding column (F-004 #2).
fn key_label(code: KeyCode) -> String {
    match code {
        KeyCode::Esc => "Esc".into(),
        KeyCode::Enter => "CR".into(),
        KeyCode::Backspace => "BS".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::Char(c) => c.to_string(),
        other => format!("{other:?}"),
    }
}

impl InputEngine {
    #[must_use]
    pub fn new() -> InputEngine {
        Self::with_profile(InputProfile::Vim)
    }

    /// An Emacs-profile engine (F-012). Same state; `feed` takes the non-modal Emacs path.
    #[must_use]
    pub fn emacs() -> InputEngine {
        Self::with_profile(InputProfile::Emacs)
    }

    /// A Native-profile engine (F-013). Same state; `feed` takes the MODAL path — Native's text layer is the
    /// Vim grammar (NAT-1). Its distinctive command-discovery/transient/readline layers are additive follow-
    /// ups, so in this slice a Native engine drives text exactly as Vim does, under its own profile identity.
    #[must_use]
    pub fn native() -> InputEngine {
        Self::with_profile(InputProfile::Native)
    }

    #[must_use]
    fn with_profile(input_profile: InputProfile) -> InputEngine {
        InputEngine {
            input_profile,
            profile: VimProfile::new(),
            normal: NormalState::default(),
            last_find: None,
            last_search: None,
            last_change: None,
            recording: None,
            pending_record_register: None,
            pending_search: None,
            insert: InsertState::default(),
            activations: Vec::new(),
            cmdline: None,
            lang_map: HashMap::new(),
            lang_active: false,
            emacs_arg: None,
            emacs_prefix: None,
            emacs: EmacsProfile::new(),
            leader: false,
        }
    }

    /// Whether a one-shot command borrowed from another layer is in flight (`i_CTRL-O`): keys resolve
    /// through the Normal grammar until it completes and pops its return address (KL-OBL-5).
    fn in_one_shot(&self) -> bool {
        !self.activations.is_empty()
    }

    /// A human label for `command`'s STATIC keymap-layer binding (F-004 #2 palette column), or `None`
    /// if it is not bound in any namespace's layer table. Grammar-driven commands (motions, operators
    /// built by `feed`, not the layer tables) and ex commands have no static binding here — the
    /// deliberate MVP scope (static layer bindings only), so most Normal commands return `None`.
    #[must_use]
    pub fn binding_label(&self, command: &Command) -> Option<String> {
        for ns in VimProfile::all()
            .into_iter()
            .chain(std::iter::once(Ns::Replace))
        {
            if let Some(code) = self.profile.stack(ns).key_for(command) {
                return Some(key_label(*code));
            }
        }
        None
    }

    /// The active command-line as `(prefix, text, cursor)` for the frontend to render — `None` when
    /// not on the command line. The frontend reads this instead of owning any line buffer (F-026).
    #[must_use]
    pub fn cmdline(&self) -> Option<(char, &str, usize)> {
        self.cmdline
            .as_ref()
            .map(|c| (c.prefix, c.buffer.as_str(), c.cursor))
    }

    /// The Native leader (which-key) discovery hint (F-013 NAT-2) as a one-line `"w:write  q:quit  …"`
    /// string for the status/command line, or `None` unless the leader tier is armed. `Some` iff
    /// `<leader>` is pending, so it doubles as the pending-state query; formatting lives here so the
    /// frontend stays a thin renderer. A structured multi-column which-key popup is a later render slice.
    #[must_use]
    pub fn leader_hint(&self) -> Option<String> {
        if !self.leader {
            return None;
        }
        Some(
            NATIVE_LEADER_MENU
                .iter()
                .map(|(k, label, _)| format!("{k}:{label}"))
                .collect::<Vec<_>>()
                .join("  "),
        )
    }

    /// Open the command-line namespace with `prefix` (`:`/`/`), optionally as `gQ` Ex mode.
    fn open_cmdline(&mut self, prefix: char, ex_mode: bool) {
        self.cmdline = Some(CmdLine {
            prefix,
            buffer: String::new(),
            cursor: 0,
            ex_mode,
            mx: false,
        });
    }

    /// Open the Emacs `M-x` minibuffer (F-012): the same command-line namespace, reading a command NAME.
    /// The prompt glyph is a placeholder here; the frontend shows the `M-x ` prompt (a rendering follow-up).
    fn open_minibuffer(&mut self) {
        self.cmdline = Some(CmdLine {
            prefix: ':',
            buffer: String::new(),
            cursor: 0,
            ex_mode: false,
            mx: true,
        });
    }

    /// Route a key into the open command-line buffer (F-026). The namespace owns the buffer: a printable
    /// key appends (open/append policy), `<BS>` deletes back, `<Esc>` aborts, `<CR>` finalises — a
    /// search folds through [`Self::submit_search`], an ex line becomes [`Feed::ExecuteEx`]. In `gQ` Ex
    /// mode the line re-opens after `<CR>` until `:visual`/`:vi`/an empty line exits.
    fn feed_cmdline(&mut self, key: KeyEvent) -> Feed {
        let Some(cl) = self.cmdline.as_mut() else {
            return Feed::Ignored;
        };
        match key.code {
            KeyCode::Esc => {
                self.cmdline = None;
                Feed::Ignored
            }
            KeyCode::Backspace => {
                cl.buffer.pop();
                cl.cursor = cl.buffer.chars().count();
                Feed::Pending
            }
            KeyCode::Char(c) => {
                cl.buffer.push(c);
                cl.cursor = cl.buffer.chars().count();
                Feed::Pending
            }
            KeyCode::Enter => {
                let prefix = cl.prefix;
                let ex_mode = cl.ex_mode;
                let mx = cl.mx;
                let text = std::mem::take(&mut cl.buffer);
                if mx {
                    // `M-x <name> <CR>`: resolve the command name against the registry; an unknown name is a
                    // no-op (Emacs shows "[No match]"). Completion / history are deferred (F-004).
                    self.cmdline = None;
                    return match emacs_command_by_name(&text) {
                        Some(cmd) => Feed::Cmd(cmd),
                        None => Feed::Ignored,
                    };
                }
                if ex_mode {
                    // Ex mode: `:visual`/`:vi`/empty leaves it; anything else runs and re-prompts.
                    if text.is_empty() || text == "visual" || text == "vi" {
                        self.cmdline = None;
                        return Feed::Ignored;
                    }
                    cl.cursor = 0;
                    return Feed::ExecuteEx(text);
                }
                self.cmdline = None;
                if prefix == '/' {
                    self.submit_search(text)
                } else {
                    Feed::ExecuteEx(text)
                }
            }
            _ => Feed::Pending,
        }
    }

    /// Apply a namespace's declared unmatched-key policy to a key nothing bound.
    ///
    /// This is the replacement for the shared `Feed::Ignored` fallthrough. The behaviour is
    /// deliberately unchanged today — `Ignore` and `Abort` both clear the transient state and yield
    /// `Ignored` — but the DECISION now comes from the layer that was actually consulted instead of
    /// from one arm at the bottom of `feed`. Separating them properly needs KL-OBL-4 (a layer owns
    /// its state and dies with it), which this engine does not model yet: count/operator/awaiting are
    /// still engine-wide, so `reset()` is the only available approximation of "the layer went away".
    fn unmatched(&mut self, ns: Ns, key: KeyEvent) -> Feed {
        let policy = match self.profile.stack(ns).resolve(&key.code) {
            Resolved::Bound { .. } => {
                // Reachable only if a caller routes a bound key here; treat as unhandled rather than
                // guessing, so a wiring mistake shows up instead of silently doing something.
                self.reset();
                return Feed::Ignored;
            }
            Resolved::Unmatched { policy, .. } => policy,
            // An empty stack is a construction bug (see `VimProfile::new`), never a policy.
            Resolved::NoLayer => unreachable!("every Vim namespace declares exactly one layer"),
        };
        match policy {
            UnmatchedKey::Insert => {
                self.reset();
                match key.code {
                    KeyCode::Char(c) => Feed::Cmd(Command::InsertChar(c)),
                    // `open/insert` is about PRINTABLE keys; a non-printable unmatched key does
                    // nothing, which is not the same statement as `closed/ignore`.
                    _ => Feed::Ignored,
                }
            }
            UnmatchedKey::Ignore | UnmatchedKey::Abort => {
                self.reset();
                Feed::Ignored
            }
            UnmatchedKey::ReplaceSelection => {
                self.reset();
                match key.code {
                    // Vim Select: a printable key deletes the selection, inserts the char, enters Insert.
                    // The core (`Command::ReplaceSelection`) performs all three as one edit.
                    KeyCode::Char(c) => Feed::Cmd(Command::ReplaceSelection(c)),
                    // `open/replace-selection` is about PRINTABLE keys; a non-printable unmatched key
                    // does nothing (it is NOT `closed/ignore`, but the observable result here matches).
                    _ => Feed::Ignored,
                }
            }
            // The remaining open policies belong to namespaces this engine does not reach yet
            // (Cmdline/Terminal/Lang). Reaching one means a namespace was wired without its handler,
            // and failing loudly beats inventing a behaviour.
            other => unreachable!("namespace {ns:?} has unimplemented policy {other:?}"),
        }
    }

    /// Complete a `/pattern` line (called by the command-line namespace on `<CR>`). Folds the pattern
    /// into the operator/count captured when `/` was pressed and yields the finished [`Command::Search`]:
    /// bare (`/pat`, `2/pat`) moves, or `d/pat`/`c/pat`/`y/pat` deletes/changes/yanks `[cursor, match)`.
    /// Also records the pattern for `n`/`N`. An empty pattern aborts (Vim's `<CR>` on an empty line is a
    /// no-op / reuse-last, which v0 treats as inert) — the armed operator is dropped, nothing is emitted.
    pub fn submit_search(&mut self, pattern: String) -> Feed {
        self.cmdline = None; // completing a search closes the command-line namespace (F-026)
        let (op, count) = self.pending_search.take().unwrap_or((SearchOp::Move, 1));
        if pattern.is_empty() {
            return Feed::Ignored;
        }
        self.last_search = Some(pattern.clone());
        Feed::Cmd(Command::Search { op, count, pattern })
    }

    /// Record `pattern` as the last search so `n`/`N` repeat it. Used by the frontend after it resolves a
    /// `*`/`#` (word-under-cursor) search, whose pattern is only known once the buffer is read.
    pub fn set_last_search(&mut self, pattern: String) {
        self.last_search = Some(pattern);
    }

    /// End the current Normal-grammar sequence: the Normal-family layer drops its OWN transient state
    /// (count / operator / awaiting / forced-wise) at a command boundary. This is the layer resetting
    /// itself, not the engine reaching into a foreign layer (KL-OBL-4) — sticky repeat state survives.
    /// Every non-`Pending` outcome runs through here, so no partial sequence leaks into the next command.
    fn reset(&mut self) {
        self.normal = NormalState::default();
        // A borrowed one-shot command is consumed the instant it completes (every completion runs
        // through here): pop its return address off the activation stack, resuming the layer it came
        // from (KL-OBL-5). A no-op when no one-shot is in flight (the stack is empty).
        self.activations.pop();
    }

    /// Take the pending Normal count (0 if none) and clear the transient state. For a frontend intercept
    /// (`H`/`M`/`L`, `C-d`, …) that consumes a key the engine would otherwise never see: it reads the
    /// count the digits accumulated and resets, so the count can't leak onto the next command.
    pub fn take_count(&mut self) -> u32 {
        let n = self.normal.count;
        self.reset();
        n
    }

    fn mcount(&self) -> u32 {
        self.normal.count.max(1)
    }

    /// Emit `m` — an operator command if one is armed, else a bare move — then clear the transient state.
    fn motion(&mut self, m: Motion) -> Feed {
        let cmd = match self.normal.op {
            Some(OpPending { op, count }) => {
                let total = count.max(1) * self.mcount();
                // `gu`/`gU`/`g~` recase the operator span (no forced-wise / `cw`->`ce` rewrites apply).
                if let Some(case) = op.case() {
                    Command::CaseMotion {
                        count: total,
                        motion: m,
                        case,
                    }
                } else if let Some(left) = op.shift() {
                    // `>`/`<` over a motion — the planner shifts the motion's LINES (always linewise).
                    Command::ShiftMotion {
                        left,
                        count: total,
                        motion: m,
                    }
                } else if op == Op::Reindent {
                    // `=` over a motion — the planner reindents the motion's LINES (always linewise).
                    Command::Reindent {
                        count: total,
                        motion: m,
                    }
                } else if let Some(wise) = self.normal.forced_wise {
                    let opk = match op {
                        Op::Delete => OpKind::Delete,
                        Op::Change => OpKind::Change,
                        Op::Yank => OpKind::Yank,
                        // Case ops are handled by the `op.case()` branch above.
                        Op::CaseLower
                        | Op::CaseUpper
                        | Op::CaseToggle
                        | Op::ShiftRight
                        | Op::ShiftLeft
                        | Op::Reindent => {
                            unreachable!()
                        }
                    };
                    Command::OpForced {
                        op: opk,
                        count: total,
                        motion: m,
                        wise,
                    }
                } else {
                    match op {
                        Op::Delete => Command::Delete(total, m),
                        // Vim `cw`/`cW` behave like `ce`/`cE` (do not eat the trailing space).
                        Op::Change => Command::Change(
                            total,
                            match m {
                                Motion::WordFwd => Motion::WordEnd,
                                Motion::BigWordFwd => Motion::BigWordEnd,
                                other => other,
                            },
                        ),
                        Op::Yank => Command::Yank(total, m),
                        // Case ops are handled by the `op.case()` branch above.
                        Op::CaseLower
                        | Op::CaseUpper
                        | Op::CaseToggle
                        | Op::ShiftRight
                        | Op::ShiftLeft
                        | Op::Reindent => {
                            unreachable!()
                        }
                    }
                }
            }
            None => Command::Move(self.mcount(), m),
        };
        self.reset();
        Feed::Cmd(cmd)
    }

    fn action(&mut self, cmd: Command) -> Feed {
        self.reset();
        Feed::Cmd(cmd)
    }

    /// Arm a case operator (`gu`/`gU`/`g~`) so the next motion recases its span. No doubling detection
    /// here — the linewise `guu`/`gUU`/`g~~` form is handled where the second key is dispatched.
    fn arm_case_op(&mut self, op: Op) -> Feed {
        self.normal.op = Some(OpPending {
            op,
            count: self.mcount(),
        });
        self.normal.count = 0;
        Feed::Pending
    }

    /// Emit the LINEWISE case command for a doubled case operator (`guu`/`gUU`/`g~~`) — recase the current
    /// `count` lines. Called only with a case operator armed.
    fn case_linewise(&mut self) -> Feed {
        let cmd = match self.normal.op {
            Some(OpPending { op, count }) => {
                let case = op.case().expect("a case operator is armed");
                Command::CaseMotion {
                    count: count.max(1) * self.mcount(),
                    motion: Motion::Line,
                    case,
                }
            }
            None => unreachable!("case_linewise called without an armed case operator"),
        };
        self.reset();
        Feed::Cmd(cmd)
    }

    /// Arm an operator, or emit its linewise form when doubled (`dd`/`cc`/`yy`). `linewise` builds the
    /// linewise command from the operator's count.
    fn operator(&mut self, op: Op, linewise: fn(u32, Motion) -> Command) -> Feed {
        if let Some(OpPending { op: armed, count }) = self.normal.op {
            if armed == op {
                let n = count.max(1);
                self.reset();
                return Feed::Cmd(linewise(n, Motion::Line));
            }
        }
        self.normal.op = Some(OpPending {
            op,
            count: self.mcount(),
        });
        self.normal.count = 0;
        Feed::Pending
    }

    /// Feed one key given the current mode. Resolves the key into an outcome, then folds that outcome into
    /// the dot-repeat record (so `.` can later replay the last change). The two steps are split so the
    /// resolution grammar stays untouched by the recording concern.
    /// Install/replace one Lang-Arg mapping (`:lmap {lhs} {rhs}`, F-027). MVP is single-char → single-char.
    pub fn set_lang_mapping(&mut self, lhs: char, rhs: char) {
        self.lang_map.insert(lhs, rhs);
    }

    /// Remove one Lang-Arg mapping (`:lunmap {lhs}`). A no-op if it was not mapped.
    pub fn clear_lang_mapping(&mut self, lhs: char) {
        self.lang_map.remove(&lhs);
    }

    /// Whether `mode` (plus the engine's live state) is a Lang-Arg-eligible context: the Command-line or
    /// Insert namespace, or a command reading a single character (`f`/`t`/`F`/`T`/`r`). Everything else —
    /// Normal, Visual, Operator-pending, Replace — is inert, which is the whole point of a SEPARATE Lang
    /// namespace (F-027 acceptance #2: "and to nothing else"): operators and motions are never translated.
    fn lang_eligible(&self, mode: Mode) -> bool {
        // Command-line namespace (typing on the `:`/`/` line).
        if self.cmdline.is_some() {
            return true;
        }
        // A command reading a single character as its argument: `f`/`t`/`F`/`T` (find target) or `r`
        // (replace). Vim's Lang-Arg translates that argument regardless of how the command was reached.
        if matches!(
            self.normal.awaiting,
            Awaiting::FindTarget { .. } | Awaiting::ReplaceChar
        ) {
            return true;
        }
        // Insert namespace — but NOT mid-`CTRL-G` prefix (its second key is a command selector, not
        // text) and NOT while an `i_CTRL-O` one-shot has borrowed the Normal grammar (that key is a
        // Normal command, not inserted text).
        mode == Mode::Insert && !self.insert.ctrl_g && !self.in_one_shot()
    }

    /// The Lang-Arg TRANSLATION STAGE (F-027, D-048 / RFC-0013). Rewrites a decoded key through the
    /// active language map BEFORE any dispatch, in the Lang-Arg contexts only, applying AT MOST ONE
    /// substitution: the mapped key is returned and dispatched literally, never fed back through this
    /// stage — so a cyclic map (`a→b`, `b→a`) cannot loop, resolution stays TOTAL, and the work stays
    /// BOUNDED (INV-FAIL-BOUNDED). The stage lives ABOVE the layer stack (D-045 `resolve` is untouched):
    /// it is a preprocessor that always yields a concrete key, not a resolution layer that yields.
    ///
    /// Terminal-side IME composition is a DIFFERENT, disjoint mechanism (acceptance #3): the terminal
    /// composes keystrokes into a finished CHARACTER and delivers it as TEXT on the paste/IME path, which
    /// never reaches `feed` as a single decoded `KeyEvent`. So this stage only ever sees a bare printable
    /// key with no CTRL/ALT modifier, and a given unit of input is translated by AT MOST ONE of {terminal
    /// IME, lmap} — never both.
    fn translate_lang(&self, key: KeyEvent, mode: Mode) -> KeyEvent {
        if !self.lang_active || self.lang_map.is_empty() {
            return key;
        }
        // Only a bare printable key is a candidate; a modified key (CTRL-*/ALT-*) is a command, and
        // composed IME text never arrives here at all.
        let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
        let KeyCode::Char(c) = key.code else {
            return key;
        };
        if !plain || !self.lang_eligible(mode) {
            return key;
        }
        match self.lang_map.get(&c) {
            Some(&mapped) => KeyEvent {
                code: KeyCode::Char(mapped),
                ..key
            },
            None => key,
        }
    }

    pub fn feed(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        // Lang-Arg translation stage (F-027 / D-048): rewrite the key through the active language map
        // before ANY dispatch, in the three Lang-Arg contexts only. One substitution, then literal —
        // the translated key flows through normal dispatch and never re-enters this stage.
        let key = self.translate_lang(key, mode);
        // Emacs is NON-MODAL: no Normal/Insert grammar, no operator-pending, no dot-repeat recorder — a
        // key is either a `C-`/`M-` command or literal text. So it takes its own dispatch, never the Vim
        // modal path below (F-012 / RFC-0014). Native is NOT here on purpose: its text layer IS the Vim
        // modal grammar (F-013 NAT-1), so it falls through to the modal path and only its additive layers
        // (leader/which-key, transient maps) — landing in later slices — will branch above it.
        if self.input_profile == InputProfile::Emacs {
            return self.feed_emacs(key);
        }
        // The command-line namespace owns the keystream while open (F-026); its typing is not a
        // dot-repeatable change, so it bypasses the recorder.
        if self.cmdline.is_some() {
            return self.feed_cmdline(key);
        }
        let out = self.feed_impl(key, mode);
        self.record(&out, mode);
        out
    }

    /// The Emacs profile's dispatch (F-012, minimal slice). Non-modal: the global-map's `C-` motions
    /// resolve to commands; an unmodified printable key inserts literally. A `C-u`/digit prefix argument
    /// (D-049) accumulates ahead of the command it modifies. This is the seam — the nine-tier stack, the
    /// `C-x` prefix maps, the kill ring and the mark ring layer on from here (RFC-0014). Motions work while
    /// the buffer stays editable because a `Move*` command keeps the current mode and only moves the cursor,
    /// so "move + insert in one state" needs no new mode.
    fn feed_emacs(&mut self, key: KeyEvent) -> Feed {
        // The `M-x` minibuffer owns the keystream while open (F-026 command-line namespace), reused verbatim
        // from the Vim `:`-line handler — its `<CR>` resolves the command NAME (the `cl.mx` branch).
        if self.cmdline.is_some() {
            return self.feed_cmdline(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // A pending prefix key owns the NEXT keystroke: resolve it in that prefix's map before any global
        // dispatch. `C-g` (keyboard-quit) or any unbound key cancels the prefix (Emacs beeps). The prefix
        // is always cleared here, so a single stray key can never leave the engine wedged in a prefix.
        if let Some(prefix) = self.emacs_prefix.take() {
            return self.feed_emacs_prefix(prefix, key, ctrl);
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
    /// `C-x C-s` saves, `C-x C-c` quits, `C-x u` undoes, `C-x C-x` exchanges point and mark. An unbound key
    /// (including `C-g`) cancels the prefix and is inert — the prefix was already cleared by the caller, so
    /// the engine is never left wedged.
    fn feed_emacs_prefix(&mut self, prefix: char, key: KeyEvent, ctrl: bool) -> Feed {
        if prefix == 'x' {
            let cmd = match key.code {
                KeyCode::Char('s') if ctrl => Command::Save, // C-x C-s — save-buffer
                KeyCode::Char('c') if ctrl => Command::Quit, // C-x C-c — save-buffers-kill-terminal
                KeyCode::Char('u') if !ctrl => Command::Undo, // C-x u — undo
                KeyCode::Char('x') if ctrl => Command::ExchangePointMark, // C-x C-x — exchange point/mark
                KeyCode::Char('u') if ctrl => Command::EmacsCaseRegion {
                    case: WordCase::Upcase, // C-x C-u — upcase-region
                },
                KeyCode::Char('l') if ctrl => Command::EmacsCaseRegion {
                    case: WordCase::Downcase, // C-x C-l — downcase-region
                },
                _ => return Feed::Ignored,
            };
            return Feed::Cmd(cmd);
        }
        Feed::Ignored
    }

    /// Fold a just-produced outcome into the dot-repeat record. In Insert mode, extend the in-flight change
    /// until `<Esc>` closes it; in Normal/Visual, an insert-entering command opens a recording, a
    /// self-contained edit becomes the record outright, and anything else leaves the record intact.
    /// `Pending`/`Ignored`/`Replay`/`Open*` never touch it — which is what makes `..` repeat one change.
    fn record(&mut self, out: &Feed, mode: Mode) {
        let Feed::Cmd(cmd) = out else {
            return;
        };
        if mode == Mode::Insert {
            if let Some(rec) = self.recording.as_mut() {
                rec.insert.push(cmd.clone());
                // The `<Esc>` that leaves Insert (recorded here so replay leaves Insert too) closes the change.
                if *cmd == Command::EnterNormal {
                    self.last_change = self.recording.take();
                }
            }
            return;
        }
        // `"x` selects the register the NEXT recorded change should carry — remembered, not itself a change.
        if let Command::SetRegister(name) = cmd {
            self.pending_record_register = *name;
            return;
        }
        match change_kind(cmd) {
            ChangeKind::InsertEntering => {
                self.recording = Some(ChangeIntent {
                    lead: cmd.clone(),
                    insert: Vec::new(),
                    register: self.pending_record_register.take(),
                });
            }
            ChangeKind::Immediate => {
                self.recording = None;
                self.last_change = Some(ChangeIntent {
                    lead: cmd.clone(),
                    insert: Vec::new(),
                    register: self.pending_record_register.take(),
                });
            }
            // A non-change (motion, mode switch, yank) forgets a dangling register selection.
            ChangeKind::NotAChange => self.pending_record_register = None,
        }
    }

    /// Feed one key given the current mode.
    /// The Insert namespace (mode is Insert, no one-shot in flight). Two multi-key sequences resolve
    /// before the layer — `CTRL-G u` (undo-break), `i_CTRL-^` (Lang-Arg toggle), `CTRL-O` (one-shot
    /// Normal), `CTRL-G` (prefix) — then the Insert layer binds, else the `open/insert` policy applies.
    fn feed_insert(&mut self, key: KeyEvent) -> Feed {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // `CTRL-G` prefix: consume the second key. `u` (or `U`) breaks the undo group; anything else
        // aborts the prefix without inserting (Vim beeps). Checked before the layer so the printable
        // path never sees the prefixed key.
        if self.insert.ctrl_g {
            self.insert.ctrl_g = false;
            return match key.code {
                // `action` clears the transient axes (like every other completed key), so no partial
                // Normal state can survive an Insert key.
                KeyCode::Char('u') | KeyCode::Char('U') => self.action(Command::BreakUndo),
                _ => {
                    self.reset();
                    Feed::Ignored
                }
            };
        }
        // `i_CTRL-^` toggles the language map (Lang-Arg / lmap) on or off within Insert (F-027 / D-048).
        // MVP flips one boolean; the per-context iminsert/imsearch model is a follow-up. Checked before
        // the printable path so `^`/`6` under CTRL never reach text insertion.
        if ctrl && matches!(key.code, KeyCode::Char('^') | KeyCode::Char('6')) {
            self.reset();
            self.lang_active = !self.lang_active;
            return Feed::Pending;
        }
        if ctrl && key.code == KeyCode::Char('o') {
            // Push a one-shot activation whose RETURN ADDRESS is Insert (KL-OBL-5): the NEXT keys resolve
            // through the Normal grammar; on completion `reset()` pops the address and Insert routing
            // resumes. Core mode stays Insert throughout. Reset first so the one-shot begins from a clean
            // count/operator/awaiting state (the pop is a no-op — the stack is empty here).
            self.reset();
            self.activations.push(Suspended { resume: Ns::Insert });
            return Feed::Pending;
        }
        if ctrl && key.code == KeyCode::Char('g') {
            self.reset();
            self.insert.ctrl_g = true;
            return Feed::Pending;
        }
        if let Resolved::Bound { value, .. } = self.profile.stack(Ns::Insert).resolve(&key.code) {
            let cmd = value.clone();
            self.reset();
            return Feed::Cmd(cmd);
        }
        self.unmatched(Ns::Insert, key)
    }

    /// Replace (`R`) / Virtual Replace (`gR`): the `open/overwrite` namespace. Bindings (Esc/BS/CR)
    /// resolve through the Replace LAYER; an unmatched printable key hits the layer's declared overwrite
    /// policy (KL-OBL-2), applied as the mode-appropriate overwrite command (tab-aware in Virtual Replace).
    fn feed_replace(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        if let Resolved::Bound { value, .. } = self.profile.stack(Ns::Replace).resolve(&key.code) {
            let cmd = value.clone();
            return self.action(cmd);
        }
        // open/overwrite: a printable key overwrites; non-printable does nothing (NOT closed/ignore).
        debug_assert!(
            matches!(
                self.profile.stack(Ns::Replace).resolve(&key.code),
                Resolved::Unmatched {
                    policy: UnmatchedKey::Overwrite,
                    ..
                }
            ),
            "the Replace namespace must declare open/overwrite"
        );
        self.reset();
        match key.code {
            KeyCode::Char(c) if mode == Mode::VirtualReplace => {
                Feed::Cmd(Command::VirtualReplaceType(c))
            }
            KeyCode::Char(c) => Feed::Cmd(Command::ReplaceType(c)),
            _ => Feed::Ignored,
        }
    }

    /// The base Normal-family dispatch, reached only with `awaiting == Nothing` (the tier above
    /// returned for the pending cases). Shared char-search initiators, then the count/operator/motion
    /// grammar and the mode-specific keys.
    fn feed_base(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        // `CTRL-G` toggles Visual<->Select over the SAME selection (Vim's documented behaviour). Handled
        // here, before the shared `g` initiator below, so it is never mistaken for the start of `gg` — and
        // fully consumed in every mode: outside a selection nothing is bound (Vim's file-info `CTRL-G` is
        // not implemented), which is inert, NOT the start of `gg`.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('g') {
            return match mode {
                Mode::Visual { kind } => self.action(Command::EnterSelect { kind }),
                Mode::Select { kind } => self.action(Command::EnterVisual { kind }),
                _ => {
                    self.reset();
                    Feed::Ignored
                }
            };
        }
        // --- Shared initiators (char-search + `;`/`,`): work in Normal and Visual, preserving the operator
        // axis (so `dfx` / `d;` work). Reachable only with `awaiting == Nothing` — the tier above already
        // returned for the pending cases — so a text object in flight can never be hijacked by `f`/`t`. ---
        match key.code {
            KeyCode::Char('f') => {
                self.normal.awaiting = Awaiting::FindTarget {
                    forward: true,
                    till: false,
                };
                return Feed::Pending;
            }
            KeyCode::Char('F') => {
                self.normal.awaiting = Awaiting::FindTarget {
                    forward: false,
                    till: false,
                };
                return Feed::Pending;
            }
            KeyCode::Char('t') => {
                self.normal.awaiting = Awaiting::FindTarget {
                    forward: true,
                    till: true,
                };
                return Feed::Pending;
            }
            KeyCode::Char('T') => {
                self.normal.awaiting = Awaiting::FindTarget {
                    forward: false,
                    till: true,
                };
                return Feed::Pending;
            }
            KeyCode::Char(';') => {
                if let Some((ch, forward, till)) = self.last_find {
                    return self.motion(Motion::FindChar { ch, forward, till });
                }
            }
            KeyCode::Char(',') => {
                if let Some((ch, forward, till)) = self.last_find {
                    // `,` repeats in the opposite direction.
                    return self.motion(Motion::FindChar {
                        ch,
                        forward: !forward,
                        till,
                    });
                }
            }
            // Line jumps: `g` arms `gg`; `G` jumps to `{count}` (or the last line when no count).
            KeyCode::Char('g') => {
                self.normal.awaiting = Awaiting::GSecond;
                return Feed::Pending;
            }
            KeyCode::Char('G') => {
                return if self.normal.count > 0 {
                    self.motion(Motion::GotoLine)
                } else {
                    self.motion(Motion::LastLine)
                };
            }
            KeyCode::Char('%') => return self.motion(Motion::MatchBracket),
            // `"x` — arm register selection. Deliberately does NOT reset the count/operator axes, so a
            // count typed after it still lands (`"a3yy`). The next key is the register name (see the
            // `RegisterSelect` tier above). Shared by Normal and Visual (Vim supports `"ayiw` and `"xy`).
            KeyCode::Char('"') => {
                self.normal.awaiting = Awaiting::RegisterSelect;
                return Feed::Pending;
            }
            // `` ` `` — arm a mark jump; the next key names the mark (`.` = last change, or `a`–`z`).
            KeyCode::Char('`') => {
                self.normal.awaiting = Awaiting::MarkJump;
                return Feed::Pending;
            }
            // `m` — arm a mark SET; the next key names the mark (`a`–`z`).
            KeyCode::Char('m') => {
                self.normal.awaiting = Awaiting::SetMarkChar;
                return Feed::Pending;
            }
            // `'` — arm a LINEWISE mark jump; the next key names the mark (`.` or `a`–`z`).
            KeyCode::Char('\'') => {
                self.normal.awaiting = Awaiting::MarkJumpLine;
                return Feed::Pending;
            }
            _ => {}
        }
        // Visual and Select: the selection already exists, so operators act on it directly and motions
        // extend it. The two share every matched key here (identical selection state); they diverge ONLY
        // in the unmatched-key fallthrough — Visual ignores, Select replaces-and-inserts.
        //
        // `gv` (restore the previous selection) IS wired — it re-selects the depth-1 `last_visual` (handled
        // in the `g`-initiator tier above); the full C-ANCHOR position history stays deferred.
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Mode::Visual { kind } | Mode::Select { kind } = mode {
            // `v`/`V`/`CTRL-V` switch the selection SHAPE: pressing the key of the current shape leaves the
            // namespace (to Normal), any other switches to that shape (F-025 c1). `i`/`a` (lowercase) begin
            // a text object in every shape; blockwise-only `I`/`A` (and block `c`/`s`) arm an insert-
            // replicate session instead of the plain charwise change.
            let is_block = kind == SelectKind::Blockwise;
            let shape_toggle = |target: SelectKind| {
                if kind == target {
                    Command::EnterNormal
                } else {
                    Command::EnterVisual { kind: target }
                }
            };
            match key.code {
                KeyCode::Esc => return self.action(Command::EnterNormal),
                KeyCode::Char('v') if ctrl => {
                    return self.action(shape_toggle(SelectKind::Blockwise))
                }
                KeyCode::Char('v') => return self.action(shape_toggle(SelectKind::Charwise)),
                KeyCode::Char('V') => return self.action(shape_toggle(SelectKind::Linewise)),
                // Blockwise insert-replicate: `I` at the left edge, `A` at the right edge, `c`/`s` delete
                // the block then insert at the left edge — each replicates on `<Esc>` (blockwise slice 2).
                KeyCode::Char('I') if is_block => {
                    return self.action(Command::BlockInsert(BlockInsertKind::Insert))
                }
                KeyCode::Char('A') if is_block => {
                    return self.action(Command::BlockInsert(BlockInsertKind::Append))
                }
                KeyCode::Char('c') | KeyCode::Char('s') if is_block => {
                    return self.action(Command::BlockInsert(BlockInsertKind::Change))
                }
                KeyCode::Char('d') | KeyCode::Char('x') => {
                    return self.action(Command::DeleteSelection)
                }
                KeyCode::Char('y') => return self.action(Command::YankSelection),
                KeyCode::Char('c') | KeyCode::Char('s') => {
                    return self.action(Command::ChangeSelection)
                }
                // `o` swaps the selection's ends (cursor <-> anchor); the SAME text stays selected but a
                // later motion extends the other end. In Normal `o` is OpenBelow — here it is the swap.
                KeyCode::Char('o') => return self.action(Command::SwapSelectionEnds),
                // In a selection, `i`/`a` always begin a text object (there is no insert here); the next key
                // is its selector. The completed object re-spans the selection (see the core's `Move` arm).
                KeyCode::Char('i') => {
                    self.normal.awaiting = Awaiting::TextObjectChar { inner: true };
                    return Feed::Pending;
                }
                KeyCode::Char('a') => {
                    self.normal.awaiting = Awaiting::TextObjectChar { inner: false };
                    return Feed::Pending;
                }
                // Count digits and motions extend the selection; an unmatched key hits the namespace's
                // own policy — `closed/ignore` for Visual, `open/replace-selection` for Select.
                KeyCode::Char('1'..='9') => {}
                KeyCode::Char('0') if self.normal.count > 0 => {}
                KeyCode::Char('0') => return self.motion(Motion::LineStart),
                _ if motion_key(key.code).is_some() => {}
                _ => {
                    let ns = if matches!(mode, Mode::Select { .. }) {
                        Ns::Select
                    } else {
                        Ns::Visual
                    };
                    return self.unmatched(ns, key);
                }
            }
            // fall through to shared count/motion handling below (op is never set in Visual/Select)
        }
        match key.code {
            KeyCode::Char(d @ '1'..='9') => {
                self.normal.count = self.normal.count.saturating_mul(10) + (d as u32 - '0' as u32);
                Feed::Pending
            }
            KeyCode::Char('0') if self.normal.count > 0 => {
                self.normal.count = self.normal.count.saturating_mul(10);
                Feed::Pending
            }
            KeyCode::Char('0') => self.motion(Motion::LineStart),
            code if motion_key(code).is_some() => {
                self.motion(motion_key(code).expect("guarded by is_some"))
            }
            // With an operator armed, `v`/`V`/`CTRL-V` FORCE the next motion's wise (Vim `o_v`/`o_V`/
            // `o_CTRL-V`): `dvj`, `dVe`, `d<C-v>j`. They stay operator-pending (the motion still follows);
            // `motion` emits `OpForced`. Bare (no operator) they enter Visual/Visual-line/Visual-block.
            KeyCode::Char('v') if self.normal.op.is_some() && ctrl => {
                self.normal.forced_wise = Some(ForcedWise::Blockwise);
                Feed::Pending
            }
            KeyCode::Char('v') if self.normal.op.is_some() => {
                self.normal.forced_wise = Some(ForcedWise::Charwise);
                Feed::Pending
            }
            KeyCode::Char('V') if self.normal.op.is_some() => {
                self.normal.forced_wise = Some(ForcedWise::Linewise);
                Feed::Pending
            }
            KeyCode::Char('v') if ctrl => self.action(Command::EnterVisual {
                kind: SelectKind::Blockwise,
            }),
            KeyCode::Char('v') => self.action(Command::EnterVisual {
                kind: SelectKind::Charwise,
            }),
            KeyCode::Char('V') => self.action(Command::EnterVisual {
                kind: SelectKind::Linewise,
            }),
            KeyCode::Char('d') => self.operator(Op::Delete, Command::Delete),
            KeyCode::Char('c') => self.operator(Op::Change, Command::Change),
            KeyCode::Char('y') => self.operator(Op::Yank, Command::Yank),
            // `>`/`<` are operators: doubled (`>>`) shifts the current `count` lines, and over a motion
            // (`>j`, `>ap`) shifts the motion's lines (always linewise).
            KeyCode::Char('>') => self.operator(Op::ShiftRight, |n, _| Command::ShiftRight(n)),
            KeyCode::Char('<') => self.operator(Op::ShiftLeft, |n, _| Command::ShiftLeft(n)),
            // `=` reindents (bracket-depth): doubled `==` is linewise, `=motion`/`=ap` over a motion.
            KeyCode::Char('=') => self.operator(Op::Reindent, |n, _| Command::Reindent {
                count: n,
                motion: Motion::Line,
            }),
            KeyCode::Char('p') => self.action(Command::Paste {
                after: true,
                count: self.mcount(),
                move_after: false,
            }),
            KeyCode::Char('P') => self.action(Command::Paste {
                after: false,
                count: self.mcount(),
                move_after: false,
            }),
            // Line-operator synonyms: `D`=`d$`, `C`=`c$`, `Y`=`y$` (nvim 0.6+ charwise), `S`=`cc`.
            // Each is the existing operator applied to an implicit motion, routed through the same
            // plan/commit path (so register geometry, cursor clamping, and dot-replayability match).
            KeyCode::Char('D') => self.action(Command::Delete(self.mcount(), Motion::LineEnd)),
            KeyCode::Char('C') => self.action(Command::Change(self.mcount(), Motion::LineEnd)),
            KeyCode::Char('Y') => self.action(Command::Yank(self.mcount(), Motion::LineEnd)),
            KeyCode::Char('S') => self.action(Command::Change(self.mcount(), Motion::Line)),
            // `s` (substitute char) = `cl`: change `count` chars rightward and enter Insert. Same
            // `Change(count, Right)` a `c l` produces, so register geometry and dot-repeat match. Guarded
            // to Normal (no pending operator) so `ds` still aborts — `s` is not a motion.
            KeyCode::Char('s') if self.normal.op.is_none() => {
                self.action(Command::Change(self.mcount(), Motion::Right))
            }
            // `CTRL-A` / `CTRL-X` — increment / decrement the number under-or-after the cursor by the count.
            // Placed before the plain `a`/`x` arms so the ctrl-modified keys are not swallowed by them.
            KeyCode::Char('a') if ctrl => {
                self.action(Command::IncrementNumber(i64::from(self.mcount())))
            }
            KeyCode::Char('x') if ctrl => {
                self.action(Command::IncrementNumber(-i64::from(self.mcount())))
            }
            // `CTRL-O` / `CTRL-I` — walk the jumplist back / forward (before the plain `o`/`i` arms).
            KeyCode::Char('o') if ctrl => self.action(Command::GotoOlderJump),
            KeyCode::Char('i') if ctrl => self.action(Command::GotoNewerJump),
            KeyCode::Char('i') if self.normal.op.is_some() => {
                self.normal.awaiting = Awaiting::TextObjectChar { inner: true };
                Feed::Pending
            }
            KeyCode::Char('a') if self.normal.op.is_some() => {
                self.normal.awaiting = Awaiting::TextObjectChar { inner: false };
                Feed::Pending
            }
            KeyCode::Char('i') => self.action(Command::EnterInsert),
            KeyCode::Char('a') => self.action(Command::EnterInsertAfter),
            KeyCode::Char('I') => self.action(Command::InsertLineStart),
            KeyCode::Char('A') => self.action(Command::AppendLineEnd),
            KeyCode::Char('o') => self.action(Command::OpenBelow),
            KeyCode::Char('O') => self.action(Command::OpenAbove),
            KeyCode::Char('x') => self.action(Command::DeleteUnder(self.mcount())),
            // Doubled case operators (`guu` / `gUU` / `g~~`): the second key repeats the operator to make
            // it linewise. These guards must precede the plain `u`/`~` handlers.
            KeyCode::Char('u')
                if matches!(
                    self.normal.op,
                    Some(OpPending {
                        op: Op::CaseLower,
                        ..
                    })
                ) =>
            {
                self.case_linewise()
            }
            KeyCode::Char('U')
                if matches!(
                    self.normal.op,
                    Some(OpPending {
                        op: Op::CaseUpper,
                        ..
                    })
                ) =>
            {
                self.case_linewise()
            }
            KeyCode::Char('~')
                if matches!(
                    self.normal.op,
                    Some(OpPending {
                        op: Op::CaseToggle,
                        ..
                    })
                ) =>
            {
                self.case_linewise()
            }
            KeyCode::Char('u') => self.action(Command::Undo),
            KeyCode::Char('r') if ctrl => self.action(Command::Redo),
            KeyCode::Tab => self.action(Command::GotoNewerJump),
            KeyCode::Char('r') => {
                self.normal.awaiting = Awaiting::ReplaceChar;
                Feed::Pending
            }
            KeyCode::Char('R') => self.action(Command::EnterReplace),
            KeyCode::Char('~') => self.action(Command::ToggleCase(self.mcount())),
            KeyCode::Char('J') => self.action(Command::JoinLines),
            KeyCode::Char('n') => match self.last_search.clone() {
                Some(p) => self.action(Command::SearchNext(p)),
                None => self.unmatched(Ns::Normal, key),
            },
            KeyCode::Char('N') => match self.last_search.clone() {
                Some(p) => self.action(Command::SearchPrev(p)),
                None => self.unmatched(Ns::Normal, key),
            },
            // `*`/`#` — search the whole keyword under the cursor forward / backward. The frontend reads
            // the word from the buffer and rewrites this to a concrete search (the engine has no buffer).
            KeyCode::Char('*') => self.action(Command::SearchWordUnder {
                forward: true,
                whole_word: true,
            }),
            KeyCode::Char('#') => self.action(Command::SearchWordUnder {
                forward: false,
                whole_word: true,
            }),
            // Dot-repeat: replay the last recorded change at the current cursor (D-047). A leading `N`
            // overrides the change's count (Vim `3.`). `.` itself never rewrites the record, so `..`
            // repeats the same change; with no prior change it is a clean no-op (the Normal namespace's
            // `closed/ignore` policy — Vim rings the bell).
            KeyCode::Char('.') => {
                if let Some(intent) = self.last_change.clone() {
                    let count = (self.normal.count > 0).then_some(self.normal.count);
                    self.reset();
                    Feed::Replay(intent.replay(count))
                } else {
                    self.unmatched(Ns::Normal, key)
                }
            }
            KeyCode::Char('/') => {
                // `/` is a MOTION, so an armed operator/count must survive the minibuffer (`d/pat`,
                // `2/pat`). Capture them (folded like `motion()` does: op-count × pending count) BEFORE
                // `reset()` clears the axes, then hand off to the frontend, which collects the pattern and
                // calls `submit_search` to build the finished command.
                let op = match self.normal.op {
                    Some(OpPending { op, .. }) => match op {
                        Op::Delete => SearchOp::Delete,
                        Op::Change => SearchOp::Change,
                        Op::Yank => SearchOp::Yank,
                        // Recase-/shift-to-search (`gu/pat`, `>/pat`) are not modeled; degrade to a plain
                        // search motion.
                        Op::CaseLower
                        | Op::CaseUpper
                        | Op::CaseToggle
                        | Op::ShiftRight
                        | Op::ShiftLeft
                        | Op::Reindent => SearchOp::Move,
                    },
                    None => SearchOp::Move,
                };
                let count = match self.normal.op {
                    Some(OpPending { count, .. }) => count.max(1) * self.mcount(),
                    None => self.mcount(),
                };
                self.pending_search = Some((op, count));
                self.reset();
                self.open_cmdline('/', false);
                Feed::Pending
            }
            KeyCode::Char(':') => {
                self.reset();
                self.open_cmdline(':', false);
                Feed::Pending
            }
            // The base namespace's own declared policy — not a shared fallthrough. In Visual this
            // line is unreachable (the Visual arm above returns first), which is why the two are
            // separate calls rather than one `mode`-derived namespace.
            _ => self.unmatched(Ns::Normal, key),
        }
    }

    fn feed_impl(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        // KL-OBL-4: a layer's owned state is destroyed when the layer deactivates — the engine does not
        // reach in to reset foreign fields.
        //
        // The Insert layer owns the `CTRL-G` prefix; it dies the moment the active namespace is not
        // Insert (a key in any other mode means the insert context is gone). A pending Insert one-shot
        // return (KL-OBL-5) is likewise abandoned — its resume address no longer applies.
        if mode != Mode::Insert {
            self.insert = InsertState::default();
            self.activations.retain(|a| a.resume != Ns::Insert);
        }
        // The Normal-family grammar layer (Normal / Visual / Select, with operator-pending as its
        // sub-state) owns count / operator / awaiting / forced-wise. They die when neither the family
        // nor an `i_CTRL-O` one-shot — which runs a single Normal command from WITHIN Insert, so the
        // family is momentarily active there — is in effect.
        let normal_family = matches!(
            mode,
            Mode::Normal | Mode::Visual { .. } | Mode::Select { .. }
        );
        if !normal_family && !self.in_one_shot() {
            self.normal = NormalState::default();
        }
        // Insert resolves through its LAYER, not through an early return ahead of everything else.
        // The bindings and the `open/insert` policy both live in `VimProfile`, so the namespace is
        // addressable in its own right (KL-OBL-1) and its policy is declared (KL-OBL-2).
        //
        // Two multi-key insert sequences are handled BEFORE the layer: `CTRL-O` (push a one-shot Normal
        // activation, then fall through to the Normal grammar for the rest of this and following keys until
        // it completes) and `CTRL-G u` (undo-break). A `CTRL-O` already in flight (on the activation stack)
        // skips the insert branch entirely so the pending Normal command keeps resolving.
        if mode == Mode::Insert && !self.in_one_shot() {
            return self.feed_insert(key);
        }
        if mode == Mode::Replace || mode == Mode::VirtualReplace {
            return self.feed_replace(key, mode);
        }
        // --- Native leader tier (F-013 NAT-2), ABOVE the Vim grammar and gated to the Native profile. ---
        // An ARMED leader consumes this key as its which-key selection (a bound command, or a which-key
        // abort). `self.leader` is only ever set on the Native+Normal path, so this is inert elsewhere.
        if self.leader {
            self.leader = false;
            return native_leader_command(key).map_or(Feed::Ignored, Feed::Cmd);
        }
        // ARM the leader from a CLEAN Normal base: `<leader>` (Space) opens the menu. Gated to Native +
        // Normal + clean so Vim's Space=MoveRight and a mid-construct Space-as-motion stay intact (NAT-1).
        if self.input_profile == InputProfile::Native
            && mode == Mode::Normal
            && self.normal.is_clean()
            && key.code == KeyCode::Char(' ')
            && key.modifiers.is_empty()
        {
            self.leader = true;
            return Feed::Pending;
        }
        // --- Top-priority tier: a one-shot key-expectation resolves before any base-key handling. ---
        match self.normal.awaiting {
            Awaiting::FindTarget { forward, till } => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    KeyCode::Char(ch) => {
                        self.last_find = Some((ch, forward, till));
                        self.motion(Motion::FindChar { ch, forward, till })
                    }
                    // A pending construct is in flight, so this is `closed/abort` — the policy
                    // that distinguishes operator-pending from Normal (VS-OBL-3).
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::TextObjectChar { inner } => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    // Under an operator this composes (`diw`/`da(`/`ci"`); in a selection `self.motion`
                    // emits a bare `Move` whose text-object shape the core turns into a selection (`viw`).
                    KeyCode::Char(ch) if text_object(ch, inner).is_some() => {
                        self.motion(text_object(ch, inner).expect("guarded by is_some"))
                    }
                    // Not a text object (includes the deferred `t`/`T` tag objects): a pending construct is
                    // in flight, so this is `closed/abort` — the operator-pending policy (VS-OBL-3).
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::GSecond => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    KeyCode::Char('g') => self.motion(Motion::GotoLine),
                    // `ge` / `gE` — backward to the end of the previous word / WORD (operator-aware via
                    // `motion`, so `dge` deletes back through the previous word-end).
                    KeyCode::Char('e') => self.motion(Motion::WordEndBack),
                    KeyCode::Char('E') => self.motion(Motion::BigWordEndBack),
                    // `g_` — to the last non-blank char of the line (`{count}g_` = count-1 lines down).
                    KeyCode::Char('_') => self.motion(Motion::LineLastNonBlank),
                    // `g*` / `g#` — like `*`/`#` but match the word ANYWHERE (no `\<…\>` boundaries).
                    KeyCode::Char('*') => self.action(Command::SearchWordUnder {
                        forward: true,
                        whole_word: false,
                    }),
                    KeyCode::Char('#') => self.action(Command::SearchWordUnder {
                        forward: false,
                        whole_word: false,
                    }),
                    // `gu` / `gU` / `g~` — arm a case operator (lower / upper / toggle) over the next
                    // motion. Only from Normal (no operator already pending): `dgu` is not a Vim command.
                    KeyCode::Char('u') if self.normal.op.is_none() => {
                        self.arm_case_op(Op::CaseLower)
                    }
                    KeyCode::Char('U') if self.normal.op.is_none() => {
                        self.arm_case_op(Op::CaseUpper)
                    }
                    KeyCode::Char('~') if self.normal.op.is_none() => {
                        self.arm_case_op(Op::CaseToggle)
                    }
                    // `gJ` — join with the next line WITHOUT inserting a space (Vim `gJ`).
                    KeyCode::Char('J') => self.action(Command::JoinLinesNoSpace),
                    // `g;` / `g,` — walk the change list to older / newer change positions (Vim).
                    KeyCode::Char(';') => self.action(Command::GotoOlderChange),
                    KeyCode::Char(',') => self.action(Command::GotoNewerChange),
                    // `gi` — resume Insert at the last-insert position (Vim `` `^ ``).
                    KeyCode::Char('i') => self.action(Command::InsertAtLastInsert),
                    // `gp` / `gP` — paste like `p`/`P` but leave the cursor JUST AFTER the pasted text.
                    KeyCode::Char('p') => self.action(Command::Paste {
                        after: true,
                        count: self.mcount(),
                        move_after: true,
                    }),
                    KeyCode::Char('P') => self.action(Command::Paste {
                        after: false,
                        count: self.mcount(),
                        move_after: true,
                    }),
                    // `g&` — repeat the last `:s` over the whole file with its flags (frontend-resolved).
                    KeyCode::Char('&') => self.action(Command::RepeatSubstituteGlobal),
                    // `gv` — re-select the last visual selection (D-027 depth-1 slice).
                    KeyCode::Char('v') => self.action(Command::ReselectVisual),
                    // `gR` — enter Virtual Replace mode (tab-aware overwrite).
                    KeyCode::Char('R') => self.action(Command::EnterVirtualReplace),
                    // `g-` / `g+` — chronological undo-time travel across branches (F-005 #3).
                    KeyCode::Char('-') => self.action(Command::UndoOlder),
                    KeyCode::Char('+') => self.action(Command::UndoNewer),
                    // `gQ` — enter Ex mode (F-026 #3). `Q` alone is NOT Ex at the pinned Neovim
                    // revision, so only this two-key form opens it; the line re-prompts until `:visual`.
                    KeyCode::Char('Q') => {
                        self.open_cmdline(':', true);
                        Feed::Pending
                    }
                    // A pending construct is in flight, so this is `closed/abort` — the policy
                    // that distinguishes operator-pending from Normal (VS-OBL-3).
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::ReplaceChar => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    // The count accumulated before `r` is still live (the `r` arm did not reset it).
                    KeyCode::Char(c) => self.action(Command::ReplaceChar(self.mcount(), c)),
                    // A pending construct is in flight, so this is `closed/abort` — the policy
                    // that distinguishes operator-pending from Normal (VS-OBL-3).
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::MarkJump => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    // `` `. `` — jump to the last-change mark.
                    KeyCode::Char('.') => self.action(Command::GotoLastChange),
                    // `` `{a-z} `` — jump to a named mark.
                    KeyCode::Char(c @ 'a'..='z') => self.action(Command::GotoNamedMark(c)),
                    // Any other mark name is not wired — abort the pending construct.
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::SetMarkChar => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    // `m{a-z}` — set a named mark at the cursor.
                    KeyCode::Char(c @ 'a'..='z') => self.action(Command::SetNamedMark(c)),
                    // Uppercase/global marks and specials are deferred — abort.
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::MarkJumpLine => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    // `'.` — linewise to the last-change line.
                    KeyCode::Char('.') => self.action(Command::GotoLastChangeLine),
                    // `'{a-z}` — linewise to a named mark's line.
                    KeyCode::Char(c @ 'a'..='z') => self.action(Command::GotoNamedMarkLine(c)),
                    // Any other mark name is not wired — abort.
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::RegisterSelect => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    // A register name (`a`–`z` / `A`–`Z`, or the yank register `0`): emit `SetRegister` for
                    // the core to hold as the pending register the next yank/delete/change/paste reads.
                    // `action` clears the transient axes — which is why the register PREFIX must precede a
                    // count (`"a3yy`, as in Vim). `"0p` pastes the last yank (`"0` is read-only from edits).
                    KeyCode::Char(c) if c.is_ascii_alphabetic() || c == '0' => {
                        self.action(Command::SetRegister(Some(c)))
                    }
                    // The numbered delete-ring (`"1`–`"9`) and other registers are not modelled yet; a
                    // pending construct is in flight, so an unusable name is `closed/abort` (operator-
                    // pending), leaking no state.
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::Nothing => {}
        }
        self.feed_base(key, mode)
    }
}

impl Default for InputEngine {
    /// Hand-written, NOT derived. A derived `Default` would build an empty layer set, every
    /// `resolve` would return `NoLayer`, and the policies would be silently disabled — the exact
    /// class of invisible regression the layer model exists to prevent.
    fn default() -> InputEngine {
        InputEngine::new()
    }
}

mod vim;
pub(crate) use vim::{motion_key, text_object, Ns, VimProfile};

mod native;
pub(crate) use native::{native_leader_command, NATIVE_LEADER_MENU};

mod emacs;
pub use emacs::emacs_command_by_name;
use emacs::{emacs_repeat, fold_emacs_count, EmacsArg, EmacsBinding, EmacsKey, EmacsProfile, Step};

mod cmdline;
use cmdline::CmdLine;

mod repeat;
use repeat::{change_kind, ChangeIntent, ChangeKind};

mod ex;
pub use ex::{parse_ex, BufTarget, Ex};
#[cfg(test)]
pub(crate) use ex::{parse_substitute, GlobalSpec, SubSpec};

#[cfg(test)]
mod unit_tests;
