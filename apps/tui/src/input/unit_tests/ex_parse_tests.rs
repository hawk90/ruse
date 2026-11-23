use ruse_core::SubFlags;

use super::k;
use crate::input::*;

#[test]
fn parse_quit_and_quit_bang() {
    assert_eq!(parse_ex("q"), Ex::Quit);
    assert_eq!(parse_ex("quit"), Ex::Quit);
    assert_eq!(parse_ex("q!"), Ex::QuitForce);
    assert_eq!(parse_ex("quit!"), Ex::QuitForce);
}

#[test]
fn parse_only() {
    assert_eq!(parse_ex("only"), Ex::Only);
    assert_eq!(parse_ex("on"), Ex::Only);
}

#[test]
fn parse_delete_ranges() {
    use ruse_core::SubRange;
    assert_eq!(parse_ex("d"), Ex::Delete(SubRange::CurrentLine, None, None));
    assert_eq!(
        parse_ex("delete"),
        Ex::Delete(SubRange::CurrentLine, None, None)
    );
    assert_eq!(
        parse_ex("2,5d"),
        Ex::Delete(SubRange::Lines(2, 5), None, None)
    );
    assert_eq!(parse_ex("%d"), Ex::Delete(SubRange::WholeFile, None, None));
    assert_eq!(
        parse_ex("3delete"),
        Ex::Delete(SubRange::Lines(3, 3), None, None)
    );
    // Not a delete verb → falls through (Unknown), not a spurious Delete.
    assert!(matches!(parse_ex("diffthis"), Ex::Unknown(_)));
}

#[test]
fn parse_yank_ranges() {
    use ruse_core::SubRange;
    assert_eq!(parse_ex("y"), Ex::Yank(SubRange::CurrentLine, None));
    assert_eq!(parse_ex("yank"), Ex::Yank(SubRange::CurrentLine, None));
    assert_eq!(parse_ex("1,3y"), Ex::Yank(SubRange::Lines(1, 3), None));
    assert_eq!(parse_ex("%yank"), Ex::Yank(SubRange::WholeFile, None));
}

#[test]
fn parse_move_and_copy() {
    use ruse_core::{LineAddr, SubRange};
    assert_eq!(
        parse_ex("3,5m10"),
        Ex::Move(SubRange::Lines(3, 5), LineAddr::Line(10))
    );
    assert_eq!(
        parse_ex("m0"),
        Ex::Move(SubRange::CurrentLine, LineAddr::Line(0))
    );
    assert_eq!(
        parse_ex("move$"),
        Ex::Move(SubRange::CurrentLine, LineAddr::Last)
    );
    assert_eq!(
        parse_ex(".m$"),
        Ex::Move(SubRange::CurrentLine, LineAddr::Last)
    );
    assert_eq!(
        parse_ex("1,2t0"),
        Ex::Copy(SubRange::Lines(1, 2), LineAddr::Line(0))
    );
    assert_eq!(
        parse_ex("copy."),
        Ex::Copy(SubRange::CurrentLine, LineAddr::Current)
    );
    assert_eq!(
        parse_ex("t5"),
        Ex::Copy(SubRange::CurrentLine, LineAddr::Line(5))
    );
    // A move/copy with no destination is E16, as in nvim.
    assert_eq!(parse_ex("m"), Ex::Fail("E16: Invalid range".into()));
    assert_eq!(parse_ex("t"), Ex::Fail("E16: Invalid range".into()));
}

#[test]
fn parse_put() {
    use ruse_core::LineAddr;
    // Bare `:put` / `:pu` → the unnamed register after the current line.
    assert_eq!(
        parse_ex("put"),
        Ex::Put {
            addr: LineAddr::Current,
            reg: None,
            above: false
        }
    );
    assert_eq!(
        parse_ex("pu"),
        Ex::Put {
            addr: LineAddr::Current,
            reg: None,
            above: false
        }
    );
    // Addressed puts: `:2put` (after line 2), `:0put` (top), `:$put` (last line), `:.put` (current).
    assert_eq!(
        parse_ex("2put"),
        Ex::Put {
            addr: LineAddr::Line(2),
            reg: None,
            above: false
        }
    );
    assert_eq!(
        parse_ex("0put"),
        Ex::Put {
            addr: LineAddr::Line(0),
            reg: None,
            above: false
        }
    );
    assert_eq!(
        parse_ex("$put"),
        Ex::Put {
            addr: LineAddr::Last,
            reg: None,
            above: false
        }
    );
    assert_eq!(
        parse_ex(".put"),
        Ex::Put {
            addr: LineAddr::Current,
            reg: None,
            above: false
        }
    );
    // A register argument (whitespace-separated): named, numbered, and the clipboard register.
    assert_eq!(
        parse_ex("put a"),
        Ex::Put {
            addr: LineAddr::Current,
            reg: Some('a'),
            above: false
        }
    );
    assert_eq!(
        parse_ex("2put a"),
        Ex::Put {
            addr: LineAddr::Line(2),
            reg: Some('a'),
            above: false
        }
    );
    assert_eq!(
        parse_ex("put +"),
        Ex::Put {
            addr: LineAddr::Current,
            reg: Some('+'),
            above: false
        }
    );
    assert_eq!(
        parse_ex("pu 0"),
        Ex::Put {
            addr: LineAddr::Current,
            reg: Some('0'),
            above: false
        }
    );
    // `:put!` puts above.
    assert_eq!(
        parse_ex("2put! a"),
        Ex::Put {
            addr: LineAddr::Line(2),
            reg: Some('a'),
            above: true
        }
    );
    // The register must be separated by whitespace — `:puta` is not `:put a` (Vim E492) → Unknown.
    assert!(matches!(parse_ex("puta"), Ex::Unknown(_)));
    // A two-char register argument is not the single-char form → Unknown.
    assert!(matches!(parse_ex("put ab"), Ex::Unknown(_)));
}

/// #1342: `:put ={expr}` takes the rest of the line as the expression, as nvim v0.12.6 reads it (the
/// `i1342_*` oracle fixtures): the `=` may follow the verb or its `!` directly, an unescaped `"` starts a
/// comment, and `\"` / `\|` lose their backslash.
#[test]
fn parse_put_expression() {
    use ruse_core::LineAddr;
    let put = |addr, expr: &str, above| Ex::PutExpr {
        addr,
        expr: expr.into(),
        above,
    };
    assert_eq!(parse_ex("put ='z'"), put(LineAddr::Current, "'z'", false));
    assert_eq!(parse_ex("put='z'"), put(LineAddr::Current, "'z'", false));
    assert_eq!(parse_ex("pu ='z'"), put(LineAddr::Current, "'z'", false));
    assert_eq!(parse_ex("0put ='z'"), put(LineAddr::Line(0), "'z'", false));
    assert_eq!(parse_ex("$put ='z'"), put(LineAddr::Last, "'z'", false));
    assert_eq!(parse_ex("put! ='z'"), put(LineAddr::Current, "'z'", true));
    assert_eq!(parse_ex("put!='z'"), put(LineAddr::Current, "'z'", true));
    assert_eq!(
        parse_ex("put ='x'..'y'"),
        put(LineAddr::Current, "'x'..'y'", false)
    );
    // Blanks after the `=` belong to the expression; the empty one is the last expression.
    assert_eq!(parse_ex("put = 'z'"), put(LineAddr::Current, " 'z'", false));
    assert_eq!(parse_ex("put ="), put(LineAddr::Current, "", false));
    // `"` starts a comment (with the blanks before it gone); `\"` is a quote and `\|` a bar.
    assert_eq!(parse_ex("put =\"a\""), put(LineAddr::Current, "", false));
    assert_eq!(
        parse_ex("put ='a' \" c"),
        put(LineAddr::Current, "'a'", false)
    );
    assert_eq!(
        parse_ex(r#"put =\"a\nb\""#),
        put(LineAddr::Current, r#""a\nb""#, false)
    );
    assert_eq!(
        parse_ex(r#"put =\"a\\\""#),
        put(LineAddr::Current, r#""a\\""#, false)
    );
    assert_eq!(
        parse_ex(r"put ='a'\|'b'"),
        put(LineAddr::Current, "'a'|'b'", false)
    );
}

/// #1234: the command name is the whole run of letters, so a word that only starts like a command is
/// E492 (nvim v0.12.6, the `exn_*` oracle fixtures) and never runs that command.
#[test]
fn parse_whole_word_command_names() {
    for line in [
        "nosuch",
        "nox",
        "sefoo",
        "sexy",
        "wqq",
        "wqx",
        "nosuch arg",
        "nosuch!",
        "2nosuch",
        "Xfoo",
        "W",
        "dx",
        "deletex",
        "yx",
        "jx",
        "mx",
        "copyx",
        "normx",
        "%sortx",
        "nohx",
        "keex",
        "trace savex",
    ] {
        assert!(matches!(parse_ex(line), Ex::Unknown(_)), ":{line}");
    }
    // `:k` alone takes its argument glued on, but not as `:kee…` (`:keepmarks`).
    assert_eq!(
        parse_ex("3ke"),
        Ex::Mark {
            range: SubRange::Lines(3, 3),
            name: 'e'
        }
    );
    for (line, rest) in [("kfoo", "foo"), ("3kax", "ax"), ("k ax", "ax")] {
        assert_eq!(
            parse_ex(line),
            Ex::Fail(format!("E488: Trailing characters: {rest}")),
            ":{line}"
        );
    }
    let (_, rest) = strip_modifiers("kee d");
    assert_eq!(rest, "d");
    let (_, rest) = strip_modifiers("keepmarks 2d");
    assert_eq!(rest, "2d");
}

#[test]
fn parse_edit_reload() {
    use crate::input::{EditSpec, FileOpts};
    let reload = Ex::Edit(EditSpec {
        bang: true,
        file: None,
        cmd: None,
        opts: FileOpts::default(),
    });
    assert_eq!(parse_ex("e!"), reload);
    assert_eq!(parse_ex("edit!"), reload);
}

#[test]
fn parse_time_travel() {
    assert_eq!(parse_ex("earlier"), Ex::Earlier(1));
    assert_eq!(parse_ex("ea 3"), Ex::Earlier(3));
    assert_eq!(parse_ex("later"), Ex::Later(1));
    assert_eq!(parse_ex("lat 5"), Ex::Later(5));
    // A non-numeric count is not understood → Unknown.
    assert!(matches!(parse_ex("earlier x"), Ex::Unknown(_)));
}

/// The `:set` arguments `line` parses to, and the error that stopped it.
fn set_args(line: &str) -> (Vec<crate::input::set_cmd::SetArg>, Option<String>) {
    match parse_ex(line) {
        Ex::SetOptions(l) => (l.args.into_iter().map(|(a, _)| a).collect(), l.error),
        other => panic!("{line}: {other:?}"),
    }
}

#[test]
fn parse_set_options() {
    use crate::input::set_cmd::{AssignOp, Opt, RawVal, SetArg};
    let one = |line: &str| {
        let (args, err) = set_args(line);
        assert_eq!(err, None, "{line}");
        args.into_iter().next().expect(line)
    };
    let num = |opt, n| SetArg::Assign {
        opt,
        op: AssignOp::Set,
        val: RawVal::Num(n),
    };
    assert_eq!(one("set ic"), SetArg::Bool(Opt::IgnoreCase, true));
    assert_eq!(
        one("set noignorecase"),
        SetArg::Bool(Opt::IgnoreCase, false)
    );
    assert_eq!(one("set scs"), SetArg::Bool(Opt::SmartCase, true));
    assert_eq!(one("set expandtab"), SetArg::Bool(Opt::ExpandTab, true));
    assert_eq!(one("set noet"), SetArg::Bool(Opt::ExpandTab, false));
    assert_eq!(one("set sw=2"), num(Opt::ShiftWidth, 2));
    assert_eq!(one("set shiftwidth=8"), num(Opt::ShiftWidth, 8));
    // `tabstop` is its own option, not an alias of `shiftwidth` (#601).
    assert_eq!(one("set ts=8"), num(Opt::TabStop, 8));
    assert_eq!(one("set tabstop=2"), num(Opt::TabStop, 2));
    // An option nvim does not know is E518 (#1087); a misused or mistyped one E474/E521; one nvim
    // knows but ruse does not implement only says so.
    let err = |line: &str| set_args(line).1;
    assert_eq!(
        err("set bogus").as_deref(),
        Some("E518: Unknown option: bogus")
    );
    assert_eq!(
        err("set nobogus").as_deref(),
        Some("E518: Unknown option: nobogus")
    );
    assert_eq!(
        err("set nosw").as_deref(),
        Some("E474: Invalid argument: nosw")
    );
    assert_eq!(
        err("set sw=x").as_deref(),
        Some("E521: Number required after =: sw=x")
    );
    assert_eq!(one("set nonumber"), SetArg::Bool(Opt::Number, false));
    assert_eq!(one("set nospell"), SetArg::Unsupported("spell".into()));
    assert_eq!(one("set invwrap"), SetArg::Toggle(Opt::Wrap));
    // A bare `:set` lists the changed options.
    assert_eq!(set_args("set"), (vec![], None));
}

#[test]
fn parse_buffer_delete() {
    assert_eq!(parse_ex("bd"), Ex::BufferDelete { force: false });
    assert_eq!(parse_ex("bdelete"), Ex::BufferDelete { force: false });
    assert_eq!(parse_ex("bd!"), Ex::BufferDelete { force: true });
    assert_eq!(parse_ex("bdelete!"), Ex::BufferDelete { force: true });
}

#[test]
fn parse_sort_flags() {
    use ruse_core::SubRange;
    // No range → whole file; flags parse; `!` = reverse.
    match parse_ex("sort! n") {
        Ex::Sort(r, s) => {
            assert_eq!(r, SubRange::WholeFile);
            assert!(s.reverse && s.key == ruse_core::SortKind::Decimal && !s.unique);
        }
        other => panic!("expected Sort, got {other:?}"),
    }
    match parse_ex("1,3sort u") {
        Ex::Sort(r, s) => {
            assert_eq!(r, SubRange::Lines(1, 3));
            assert!(!s.reverse && s.key == ruse_core::SortKind::Text && s.unique);
        }
        other => panic!("expected Sort, got {other:?}"),
    }
    // `i` = case-insensitive.
    match parse_ex("sort i") {
        Ex::Sort(_, s) => assert!(s.ignore_case && s.pattern.is_none()),
        other => panic!("expected Sort, got {other:?}"),
    }
    // `/pattern/` — the pattern is captured (delimiters stripped); `r` alone (no pattern) stays inert.
    match parse_ex("sort /id=/") {
        Ex::Sort(_, s) => {
            assert_eq!(s.pattern.as_deref(), Some("id="));
            assert!(!s.use_match);
        }
        other => panic!("expected Sort, got {other:?}"),
    }
    // `/pattern/ r` — sort on the matched text (order-independent: flags precede the pattern in Vim, so
    // this is written `sort r /pat/`).
    match parse_ex("sort rn /\\d\\+/") {
        Ex::Sort(_, s) => {
            assert_eq!(s.pattern.as_deref(), Some("\\d\\+"));
            assert!(s.use_match && s.key == ruse_core::SortKind::Decimal);
        }
        other => panic!("expected Sort, got {other:?}"),
    }
}

#[test]
fn set_textwidth_parses() {
    use crate::input::set_cmd::{AssignOp, Opt, RawVal, SetArg};
    let tw = |n| SetArg::Assign {
        opt: Opt::TextWidth,
        op: AssignOp::Set,
        val: RawVal::Num(n),
    };
    assert_eq!(set_args("set textwidth=72"), (vec![tw(72)], None));
    assert_eq!(set_args("set tw=0"), (vec![tw(0)], None));
}

#[test]
fn ascii_ex_command_parses() {
    use crate::input::ex::{parse_ex, Ex};
    assert_eq!(parse_ex("ascii"), Ex::Ascii);
    assert_eq!(parse_ex("as"), Ex::Ascii);
}

#[test]
fn insert_mode_and_ex() {
    let mut e = InputEngine::new();
    assert_eq!(
        e.feed(k('z'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('z'))
    );
    assert_eq!(e.feed(k(':'), Mode::Normal), Feed::Pending);
    assert!(matches!(
        parse_ex("wq"),
        Ex::Write(crate::input::WriteSpec { quit: true, .. })
    ));
    assert_eq!(
        parse_ex("trace save t.trace"),
        Ex::SaveTrace("t.trace".into())
    );
    // `:checkhealth` and its `:che` prefix both parse (F-030).
    assert_eq!(parse_ex("checkhealth"), Ex::CheckHealth);
    assert_eq!(parse_ex("che"), Ex::CheckHealth);
    assert_eq!(parse_ex("checktime"), Ex::CheckTime);
    assert_eq!(parse_ex("checkt"), Ex::CheckTime);
    // `:rename {new}` / `:rn {new}` (F-014 LSP rename); a bare verb or a whitespaced name stays Unknown.
    assert_eq!(parse_ex("rename widget"), Ex::Rename("widget".into()));
    assert_eq!(parse_ex("rn widget"), Ex::Rename("widget".into()));
    assert_eq!(
        parse_ex("rename  spaced_out  "),
        Ex::Rename("spaced_out".into())
    );
    assert!(matches!(parse_ex("rename"), Ex::Unknown(_)));
    assert!(matches!(parse_ex("rename a b"), Ex::Unknown(_)));
    assert!(matches!(parse_ex("rn"), Ex::Unknown(_)));
    // `:references` / `:refs` / `:ref` (F-014 LSP references).
    assert_eq!(parse_ex("references"), Ex::References);
    assert_eq!(parse_ex("refs"), Ex::References);
    assert_eq!(parse_ex("ref"), Ex::References);
    // `:codeaction` (F-014 LSP code actions); `:ca` is nvim's `:cabbrev`, not an alias (#609).
    assert_eq!(parse_ex("codeaction"), Ex::CodeAction);
    assert!(matches!(parse_ex("ca"), Ex::Unknown(_)));
    // `:diagnostics` / `:diags` / `:diag` (F-014 diagnostics list); `:d` is still delete.
    assert_eq!(parse_ex("diagnostics"), Ex::Diagnostics);
    assert_eq!(parse_ex("diags"), Ex::Diagnostics);
    assert_eq!(parse_ex("diag"), Ex::Diagnostics);
    assert_eq!(parse_ex("trust"), Ex::Trust);
    assert_eq!(parse_ex("untrust"), Ex::Untrust);
    assert_eq!(parse_ex("trust ++remove"), Ex::Untrust);
    assert_eq!(parse_ex("inspect config"), Ex::InspectConfig(None));
    assert_eq!(
        parse_ex("inspect config editor.tab_width"),
        Ex::InspectConfig(Some("editor.tab_width".into()))
    );
    assert_eq!(parse_ex("registers"), Ex::Registers);
    assert_eq!(parse_ex("reg"), Ex::Registers);
    assert_eq!(parse_ex("display"), Ex::Registers);
    assert_eq!(parse_ex("marks"), Ex::Marks);
    assert_eq!(parse_ex("jumps"), Ex::Jumps);
    assert_eq!(parse_ex("changes"), Ex::Changes);
    assert_eq!(parse_ex("d"), Ex::Delete(SubRange::CurrentLine, None, None));
}

/// F-007 multi-buffer ex commands parse: `:enew`, `:ls`, `:bn`/`:bp`, `:b {n}` (with/without space),
/// `:b#`. A non-numeric `:b` argument and a bare verb that only prefixes a buffer word stay Unknown.
#[test]
fn buffer_ex_commands_parse() {
    use crate::input::BufTarget;
    assert_eq!(parse_ex("enew"), Ex::Enew);
    assert_eq!(parse_ex("ene"), Ex::Enew);
    assert_eq!(parse_ex("ls"), Ex::Buffers);
    assert_eq!(parse_ex("buffers"), Ex::Buffers);
    assert_eq!(parse_ex("bn"), Ex::BufferNext);
    assert_eq!(parse_ex("bnext"), Ex::BufferNext);
    assert_eq!(parse_ex("bp"), Ex::BufferPrev);
    assert_eq!(parse_ex("bprevious"), Ex::BufferPrev);
    assert_eq!(parse_ex("b 3"), Ex::Buffer(BufTarget::Number(3)));
    assert_eq!(parse_ex("b2"), Ex::Buffer(BufTarget::Number(2)));
    assert_eq!(parse_ex("buffer 5"), Ex::Buffer(BufTarget::Number(5)));
    assert_eq!(parse_ex("b#"), Ex::Buffer(BufTarget::Alternate));
    assert!(matches!(parse_ex("b foo"), Ex::Unknown(_)));
    assert!(matches!(parse_ex("bx"), Ex::Unknown(_)));
    // `:e {file}` / `:edit {file}` open a file; `:enew`/`:ene` are not edits, and a bare `:e` re-edits the
    // buffer's own file (#610).
    let edit = |file: Option<&str>| {
        Ex::Edit(crate::input::EditSpec {
            bang: false,
            file: file.map(str::to_string),
            cmd: None,
            opts: crate::input::FileOpts::default(),
        })
    };
    assert_eq!(parse_ex("e src/main.rs"), edit(Some("src/main.rs")));
    assert_eq!(parse_ex("edit a.txt"), edit(Some("a.txt")));
    assert_eq!(parse_ex("enew"), Ex::Enew);
    assert_eq!(parse_ex("ene"), Ex::Enew);
    assert_eq!(parse_ex("e"), edit(None));
}

fn sub(line: &str) -> SubSpec {
    match parse_ex(line) {
        Ex::Substitute(s) => s,
        other => panic!("expected Substitute, got {other:?}"),
    }
}

#[test]
fn plain_substitute_current_line() {
    let s = sub("s/foo/bar/");
    assert_eq!(s.range, SubRange::CurrentLine);
    assert_eq!(s.pattern, "foo");
    assert_eq!(s.replacement, "bar");
    assert!(!s.global && s.ignore_case.is_none() && !s.confirm);
}

#[test]
fn whole_file_global() {
    let s = sub("%s/a/b/g");
    assert_eq!(s.range, SubRange::WholeFile);
    assert!(s.global);
}

#[test]
fn line_range_and_flags() {
    let s = sub("2,5s/a/b/gi");
    assert_eq!(s.range, SubRange::Lines(2, 5));
    assert!(s.global);
    assert_eq!(s.ignore_case, Some(true));
}

#[test]
fn capital_i_forces_case_sensitive_and_c_is_confirm() {
    let s = sub("s/a/b/Ic");
    assert_eq!(s.ignore_case, Some(false));
    assert!(s.confirm);
}

#[test]
fn gdefault_inverts_the_g_flag() {
    // With gdefault ON, a BARE `:s///` is global and `:s///g` is single.
    assert!(parse_substitute("s/a/b/", true).unwrap().unwrap().global);
    assert!(!parse_substitute("s/a/b/g", true).unwrap().unwrap().global);
}

#[test]
fn escaped_delimiter_stays_in_the_pattern() {
    let s = sub("s/a\\/b/c/");
    assert_eq!(s.pattern, "a\\/b");
    assert_eq!(s.replacement, "c");
}

#[test]
fn sort_is_not_a_substitute() {
    // `:sort` is its own command (the `o` is not an `:s` delimiter), never a substitute.
    assert!(matches!(parse_ex("sort"), Ex::Sort(_, _)));
    assert!(!matches!(parse_ex("sort"), Ex::Substitute(_)));
}

fn glob(line: &str) -> GlobalSpec {
    match parse_ex(line) {
        Ex::Global(g) => g,
        other => panic!("expected Global, got {other:?}"),
    }
}

#[test]
fn global_delete_defaults_to_whole_file() {
    let g = glob("g/foo/d");
    assert_eq!(g.range, SubRange::WholeFile);
    assert_eq!(g.pattern, "foo");
    assert!(!g.negate);
    assert_eq!(g.cmd, GlobalPayload::Core(GlobalCmd::Delete));
}

#[test]
fn bang_and_v_negate() {
    assert!(glob("g!/foo/d").negate);
    assert!(glob("v/foo/d").negate);
    assert!(glob("vglobal/foo/d").negate);
}

#[test]
fn global_substitute_subcommand() {
    let g = glob("g/foo/s/x/y/g");
    assert_eq!(g.pattern, "foo");
    assert_eq!(
        g.cmd,
        GlobalPayload::Core(GlobalCmd::Substitute {
            pattern: "x".into(),
            replacement: "y".into(),
            flags: SubFlags {
                global: true,
                ignore_case: None
            },
        })
    );
}

#[test]
fn ranged_global() {
    let g = glob("2,5g/foo/d");
    assert_eq!(g.range, SubRange::Lines(2, 5));
}

#[test]
fn vsplit_is_not_a_global() {
    // `:vsplit` must stay the window command, not be parsed as `:v`-global.
    assert_eq!(parse_ex("vsplit"), Ex::VSplit);
    assert_eq!(parse_ex("vs"), Ex::VSplit);
}

#[test]
fn global_normal_payload_parses() {
    let g = glob("g/foo/normal A;");
    assert_eq!(g.pattern, "foo");
    assert!(!g.negate);
    assert_eq!(
        g.cmd,
        GlobalPayload::Normal {
            bang: false,
            keys: "A;".into(),
        }
    );
    // Only the FIRST space delimits the payload — further spaces are part of the verbatim keys.
    assert_eq!(
        glob("g/foo/normal f x").cmd,
        GlobalPayload::Normal {
            bang: false,
            keys: "f x".into(),
        }
    );
    // Vim's `norm`/`norma` abbreviations resolve the same, and `<>` key-notation stays verbatim.
    assert_eq!(
        glob("g/foo/norm Ihi<Esc>").cmd,
        GlobalPayload::Normal {
            bang: false,
            keys: "Ihi<Esc>".into(),
        }
    );
}

#[test]
fn global_normal_negation_and_bang() {
    // `:v/` and `:g!/` mark the NON-matching lines; the payload `!` (ignore mappings) is parsed.
    assert!(glob("v/foo/normal A;").negate);
    assert!(glob("g!/foo/normal A;").negate);
    assert_eq!(
        glob("g/foo/normal! A;").cmd,
        GlobalPayload::Normal {
            bang: true,
            keys: "A;".into(),
        }
    );
}

#[test]
fn global_normalx_is_not_the_normal_verb() {
    // `normalx` is not `normal x`: it is some other command, which the Ex dispatch runs (and refuses)
    // per marked line (#1245).
    assert_eq!(
        glob("g/foo/normalx").cmd,
        GlobalPayload::Ex("normalx".into())
    );
    // A bare `normal` (no delimiting space/keys) is likewise not a `normal` payload.
    assert_eq!(glob("g/foo/normal").cmd, GlobalPayload::Ex("normal".into()));
}

/// #1245: only a bare `d` and a plain `s///` run in core's one pass; every other command is the
/// verbatim text the Ex dispatch runs per marked line, `|` and all.
#[test]
fn other_global_commands_run_through_the_ex_dispatch() {
    let ex = |line: &str| glob(line).cmd;
    for (line, cmd) in [
        ("g/b/d 2", "d 2"),
        ("g/b/d a", "d a"),
        ("g/b/j", "j"),
        ("g/^/m0", "m0"),
        ("g/a/>2", ">2"),
        ("g/a/d|d", "d|d"),
        ("g/a/s/1/X/ 2", "s/1/X/ 2"),
        ("g/a/s/1/X/n", "s/1/X/n"),
        ("g/a/s/1/X/&", "s/1/X/&"),
        ("g/a/s/1/X/p", "s/1/X/p"),
        ("g/./s/1/X/|s/a/Y/", "s/1/X/|s/a/Y/"),
        ("g/a/g/1/d", "g/1/d"),
        ("g/a/foo", "foo"),
    ] {
        assert_eq!(ex(line), GlobalPayload::Ex(cmd.into()), "{line}");
    }
    assert_eq!(ex("g/a/delete"), GlobalPayload::Core(GlobalCmd::Delete));
    assert!(matches!(ex("g/a/s/1/X/g"), GlobalPayload::Core(_)));
    assert!(matches!(ex("g/a/s/1/X/e"), GlobalPayload::Core(_)));
}

/// The keys of a `:g/pat/normal` are verbatim to the end of the line, as `:normal`'s are: a trailing
/// blank or `<C-v><CR>` survives.
#[test]
fn global_normal_keys_keep_their_trailing_chars() {
    assert_eq!(
        glob("g/a/normal :.d\r").cmd,
        GlobalPayload::Normal {
            bang: false,
            keys: ":.d\r".into()
        }
    );
    assert_eq!(
        glob("g/a/normal A ").cmd,
        GlobalPayload::Normal {
            bang: false,
            keys: "A ".into()
        }
    );
}

#[test]
fn noh_variants_parse() {
    assert_eq!(parse_ex("noh"), Ex::NoHighlight);
    assert_eq!(parse_ex("nohl"), Ex::NoHighlight);
    assert_eq!(parse_ex("nohlsearch"), Ex::NoHighlight);
}
