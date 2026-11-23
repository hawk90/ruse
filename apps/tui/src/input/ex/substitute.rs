//! `:s`, `:&`/`:&&` and `:g`/`:v` parsing. Split from `ex.rs` (#1385).

use ruse_core::SubFlags;

use super::{
    parse_sub_range, skip_unescaped, split_bar, Ex, GlobalCmd, GlobalPayload, GlobalSpec, SubRange,
    SubSpec,
};

/// Why the flags and count of a `:s` line are wrong. `Display` is nvim's message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SubError {
    /// `E488`: text after the flags and count.
    Trailing(String),
    /// `E939`: a zero count.
    ZeroCount,
}

impl std::fmt::Display for SubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubError::Trailing(t) => write!(f, "E488: Trailing characters: {t}"),
            SubError::ZeroCount => f.write_str("E939: Positive count required"),
        }
    }
}

/// Parse a `:[range]s/pat/rep/[flags] [count]` line into a [`SubSpec`], or `None` if it is not a
/// substitute. `gdefault` inverts the meaning of the `g` flag (Vim `'gdefault'`); pass it through so the
/// caller's config toggles it. The flags are read the way nvim's `do_sub` reads them: `&` first keeps the
/// previous flags, `g`, `c` and `e` toggle, `n` counts only, `i`/`I` set the case (the last one wins), `r`
/// is accepted, and `p`/`#`/`l` print the last line; then a count, then only a `"` comment may follow.
///
/// # Errors
/// [`SubError::Trailing`] (`E488`) for any other character after the flags and count,
/// [`SubError::ZeroCount`] (`E939`) for a zero count.
pub(crate) fn parse_substitute(line: &str, gdefault: bool) -> Option<Result<SubSpec, SubError>> {
    // Split the leading range prefix (chars `[0-9,%.$]`) from the command.
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, rest) = line.split_at(split);
    // The command verb is `s` or `substitute`, then a single-char delimiter.
    let rest = rest
        .strip_prefix("substitute")
        .or_else(|| rest.strip_prefix('s'))?;
    let delim = rest.chars().next()?;
    if !delim.is_ascii_punctuation() {
        return None; // e.g. `:sort` — `o` is not a delimiter
    }
    let body = &rest[delim.len_utf8()..];
    // The pattern ends where Vim's `skip_regexp` says (a delimiter inside `[...]` is part of it, #803); the
    // replacement runs to the next UNESCAPED delimiter, and the rest is the flags run. Escapes stay as typed
    // (`\/` is a literal `/` to the pattern reader and the replacement expander alike).
    let pend = ruse_core::pattern::skip_regexp(body, delim, true);
    let pattern = body[..pend].to_string();
    let (replacement, flags) = match body[pend..].strip_prefix(delim) {
        Some(after) => match skip_unescaped(after, 0, delim, 1) {
            Some(end) => (&after[..end - delim.len_utf8()], &after[end..]),
            None => (after, ""),
        },
        None => ("", ""),
    };
    let replacement = replacement.to_string();
    let range = parse_sub_range(range_str)?;
    let (keep_flags, mut f) = match flags.strip_prefix('&') {
        Some(r) => (true, r),
        None => (false, flags),
    };
    let mut spec = SubSpec {
        range,
        pattern,
        replacement,
        global: gdefault && !keep_flags,
        ignore_case: None,
        confirm: false,
        count_only: false,
        quiet: false,
        keep_flags,
        count: None,
        print: None,
    };
    while let Some(c) = f.chars().next() {
        match c {
            'g' => spec.global = !spec.global,
            'c' => spec.confirm = !spec.confirm,
            'n' => spec.count_only = true,
            'e' => spec.quiet = !spec.quiet,
            'r' => {}
            'p' => {
                spec.print.get_or_insert_default();
            }
            '#' => spec.print.get_or_insert_default().number = true,
            'l' => spec.print.get_or_insert_default().list = true,
            'i' => spec.ignore_case = Some(true),
            'I' => spec.ignore_case = Some(false),
            _ => break,
        }
        f = &f[1..];
    }
    // `n` reports only, so it never asks.
    if spec.count_only {
        spec.confirm = false;
    }
    let f = f.trim_start();
    let digits = f.find(|c: char| !c.is_ascii_digit()).unwrap_or(f.len());
    let f = if digits > 0 {
        let n: usize = f[..digits].parse().unwrap_or(usize::MAX);
        if n == 0 {
            return Some(Err(SubError::ZeroCount));
        }
        spec.count = Some(n);
        f[digits..].trim_start()
    } else {
        f
    };
    if !f.is_empty() && !f.starts_with('"') {
        return Some(Err(SubError::Trailing(f.to_string())));
    }
    Some(Ok(spec))
}

/// The `:s`/`:&` repeat-substitute flag chars this MVP honors, mirroring [`parse_substitute`]: `g` (all
/// matches), `i`/`I` (case override). `c` (confirm) is the interactive-loop form and is NOT wired through the
/// repeat path (it needs the frontend confirm loop, and no confirm state is stored in the `:s` history).
const REPEAT_SUB_FLAG_CHARS: &[char] = &['g', 'i', 'I'];

/// Build [`ruse_core::SubFlags`] from a flag run (used for `:s {flags}`). `I` wins over `i` if both appear
/// (last one set); `g` sets global. `gdefault` inverts `g` exactly like [`parse_substitute`].
fn repeat_sub_flags(flags: &str, gdefault: bool) -> ruse_core::SubFlags {
    let mut global = flags.contains('g');
    if gdefault {
        global = !global;
    }
    let ignore_case = if flags.contains('i') {
        Some(true)
    } else if flags.contains('I') {
        Some(false)
    } else {
        None
    };
    ruse_core::SubFlags {
        global,
        ignore_case,
    }
}

/// Parse the ex repeat-substitute forms — bare `:[range]s`, `:[range]s {flags}`, `:[range]&`, `:[range]&&` —
/// into an [`Ex::RepeatSubstitute`], or `None` if the line is not one of them. Runs AFTER [`parse_substitute`]
/// in the dispatch chain, so a real `:s/pat/rep/` (with a delimiter) is already consumed here. `gdefault`
/// inverts the `g` flag for the `:s {flags}` form, matching [`parse_substitute`].
pub(super) fn parse_repeat_substitute(line: &str, gdefault: bool) -> Option<Ex> {
    // Split the leading range prefix (chars `[0-9,%.$]`) from the verb, exactly like the other range verbs.
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, rest) = line.split_at(split);

    // `:&` / `:&&` — the verb is a run of ONLY `&` (like `parse_shift`'s `>`/`<` run). One `&` drops the flags
    // (like `:s`/`&`); two or more keep them (`:&&`). No collision with `:>`/`:<` (different verb char).
    let verb = rest.trim();
    if !verb.is_empty() && verb.chars().all(|c| c == '&') {
        let range = parse_sub_range(range_str)?;
        let flags = if verb.len() == 1 {
            Some(ruse_core::SubFlags::default()) // `:&` — drop flags
        } else {
            None // `:&&` — keep the stored flags
        };
        return Some(Ex::RepeatSubstitute { range, flags });
    }

    // Bare `:s` / `:s {flags}` — the verb is `s`/`substitute` (matching `parse_substitute`) followed by ONLY
    // flag chars (optionally space-separated: `:s g` and `:sg` both work in nvim). A real `:s/pat/…/` was
    // already taken by `parse_substitute`; anything with a non-flag char (`:sort`, `:s 3`) falls through.
    let after_verb = rest
        .strip_prefix("substitute")
        .or_else(|| rest.strip_prefix('s'))?;
    // Vim reads `:si` followed by `g`/`l`/`m` as another command's abbreviation (`:sig[n]`, `:sil[ent]`,
    // `:sim[alt]`), never as `:s` with flags (`find_ex_command`), so `:sig` is not a repeat-substitute (#601).
    // Only the unspaced form: `:s ig` is still `:s` with the flags `ig`.
    if rest.starts_with("si") && !rest.starts_with("substitute") && after_verb.starts_with("ig") {
        return None;
    }
    let flag_run = after_verb.trim();
    if !flag_run.chars().all(|c| REPEAT_SUB_FLAG_CHARS.contains(&c)) {
        return None;
    }
    let range = parse_sub_range(range_str)?;
    Some(Ex::RepeatSubstitute {
        range,
        flags: Some(repeat_sub_flags(flag_run, gdefault)),
    })
}

/// Parse a `:[range]g/pat/cmd` line (or `:g!` / `:v` for the inverse) into a [`GlobalSpec`], or `None`
/// if it is not a global command. The `:g` default range is the WHOLE FILE. MVP commands: `d` and
/// `s/pat/rep/flags`.
pub(super) fn parse_global(line: &str) -> Option<GlobalSpec> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, rest) = line.split_at(split);
    let (rest, negate) = strip_global_verb(rest)?;
    let delim = rest.chars().next()?;
    if !delim.is_ascii_punctuation() {
        return None;
    }
    let body = &rest[delim.len_utf8()..];
    // Vim's `skip_regexp`: a delimiter inside `[...]` does not end the pattern (#803).
    let pend = ruse_core::pattern::skip_regexp(body, delim, true);
    let pattern = body[..pend].to_string();
    let cmd_str = body[pend..].strip_prefix(delim).unwrap_or(&body[pend..]);
    let cmd = parse_global_cmd(cmd_str)?;
    let range = if range_str.is_empty() {
        SubRange::WholeFile
    } else {
        parse_sub_range(range_str)?
    };
    Some(GlobalSpec {
        range,
        pattern,
        negate,
        cmd,
    })
}

/// Strip the `:g` command verb — `vglobal` / `global` / `g!` / `g` / `v` (longest first; `!` and `v`
/// mark the inverse) — returning the remainder and whether the selection is negated, or `None` if the
/// line is not a global command.
fn strip_global_verb(rest: &str) -> Option<(&str, bool)> {
    if let Some(r) = rest.strip_prefix("vglobal") {
        Some((r, true))
    } else if let Some(r) = rest.strip_prefix("global") {
        Some((r, false))
    } else if let Some(r) = rest.strip_prefix("g!") {
        Some((r, true))
    } else if let Some(r) = rest.strip_prefix('g') {
        Some((r, false))
    } else if let Some(r) = rest.strip_prefix('v') {
        Some((r, true))
    } else {
        None
    }
}

/// Parse the command that follows `:g/pat/`: `normal[!] {keys}` (frontend-executed), a bare `d`/`delete`
/// or a plain `s/pat/rep/flags` substitute (both core-executed, one pass over the marks), the print
/// commands, or any other command line, run per marked line through the Ex dispatch (#1245).
fn parse_global_cmd(raw: &str) -> Option<GlobalPayload> {
    let resolved = super::ex_cmd::resolve_command_word(raw);
    let cmd = &*resolved;
    // `normal[!] {keys}` is recognized BEFORE the `.trim()` below so the key payload keeps its internal
    // spaces — only the single delimiting space after the verb is consumed (as `:normal` does).
    if let Some(payload) = parse_global_normal(cmd) {
        return Some(payload);
    }
    if let Some(Ex::Print {
        range: SubRange::CurrentLine,
        flags,
    }) = super::ex_extra::parse(cmd.trim())
    {
        return Some(GlobalPayload::Print(flags));
    }
    match cmd.trim() {
        // A bare `:g/pat/` prints the marked lines (`:help :g`).
        "" => Some(GlobalPayload::Print(super::PrintFlags::default())),
        "d" | "delete" => Some(GlobalPayload::Core(GlobalCmd::Delete)),
        other => Some(match parse_substitute(other, false) {
            // Core's pass knows the pattern, the replacement and the `g`/`i`/`I` flags. Anything more (a
            // count, `n`, `&`, a print flag, a `|` after it) is one `:s` per line through the Ex dispatch.
            // `c` stays in core's pass, which does not ask: unchanged by #1245.
            Some(Ok(spec))
                if spec.count.is_none()
                    && !spec.count_only
                    && !spec.keep_flags
                    && spec.print.is_none()
                    && split_bar(other).len() == 1 =>
            {
                GlobalPayload::Core(GlobalCmd::Substitute {
                    pattern: spec.pattern,
                    replacement: spec.replacement,
                    flags: SubFlags {
                        global: spec.global,
                        ignore_case: spec.ignore_case,
                    },
                })
            }
            _ => GlobalPayload::Ex(raw.to_string()),
        }),
    }
}

/// Parse `:g/pat/normal[!] {keys}` — the `normal`/`norma`/`norm` verb (Vim's abbreviations, longest first)
/// after optional leading whitespace and an optional `!`, then exactly ONE delimiting space, then the
/// VERBATIM key payload. `None` (→ falls through to `d`/`s`) unless the payload is a `normal`. A bare
/// `normal` with no space/keys is not a valid invocation. `normalx` is not the verb (no `!`/space follows).
fn parse_global_normal(cmd: &str) -> Option<GlobalPayload> {
    let rest = cmd.trim_start();
    let rest = ["normal", "norma", "norm"]
        .into_iter()
        .find_map(|v| rest.strip_prefix(v))?;
    let (bang, rest) = match rest.strip_prefix('!') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let keys = rest.strip_prefix(' ')?;
    Some(GlobalPayload::Normal {
        bang,
        keys: keys.to_string(),
    })
}
