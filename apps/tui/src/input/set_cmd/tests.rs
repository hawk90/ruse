//! The `:set` grammar tests: argument forms, errors, and command words. Split from `set_cmd.rs` (#1388).

use super::*;

fn args(s: &str) -> Vec<SetArg> {
    let l = parse_args(SetScope::Both, s);
    assert_eq!(l.error, None, "{s}");
    l.args.into_iter().map(|(a, _)| a).collect()
}

fn err(s: &str) -> String {
    parse_args(SetScope::Both, s).error.expect(s)
}

#[test]
fn forms() {
    use SetArg::*;
    assert_eq!(args("ic"), [Bool(Opt::IgnoreCase, true)]);
    assert_eq!(args("noic"), [Bool(Opt::IgnoreCase, false)]);
    assert_eq!(args("invic"), [Toggle(Opt::IgnoreCase)]);
    assert_eq!(args("ic!"), [Toggle(Opt::IgnoreCase)]);
    assert_eq!(args("noic!"), [Toggle(Opt::IgnoreCase)]);
    assert_eq!(args("ic?"), [Show(Opt::IgnoreCase)]);
    assert_eq!(args("noic?"), [Show(Opt::IgnoreCase)]);
    assert_eq!(args("ts"), [Show(Opt::TabStop)]);
    assert_eq!(args("ts?"), [Show(Opt::TabStop)]);
    assert_eq!(args("ts ?"), [Show(Opt::TabStop)]);
    assert_eq!(
        args("ts&"),
        [Default {
            opt: Opt::TabStop,
            vim: false
        }]
    );
    assert_eq!(
        args("ts&vim"),
        [Default {
            opt: Opt::TabStop,
            vim: true
        }]
    );
    assert_eq!(args("ts<"), [FromGlobal(Opt::TabStop)]);
    let assign = |opt, op, n| Assign {
        opt,
        op,
        val: RawVal::Num(n),
    };
    assert_eq!(args("ts=4"), [assign(Opt::TabStop, AssignOp::Set, 4)]);
    assert_eq!(args("ts:4"), [assign(Opt::TabStop, AssignOp::Set, 4)]);
    assert_eq!(args("ts =4"), [assign(Opt::TabStop, AssignOp::Set, 4)]);
    assert_eq!(args("sw+=2"), [assign(Opt::ShiftWidth, AssignOp::Add, 2)]);
    assert_eq!(args("sw-=2"), [assign(Opt::ShiftWidth, AssignOp::Sub, 2)]);
    assert_eq!(
        args("sw^=2"),
        [assign(Opt::ShiftWidth, AssignOp::Prepend, 2)]
    );
    assert_eq!(args("ts=0x10"), [assign(Opt::TabStop, AssignOp::Set, 16)]);
    assert_eq!(args("ts=010"), [assign(Opt::TabStop, AssignOp::Set, 8)]);
    assert_eq!(args("ts=019"), [assign(Opt::TabStop, AssignOp::Set, 19)]);
    assert_eq!(args("ts=0b101"), [assign(Opt::TabStop, AssignOp::Set, 5)]);
    assert_eq!(args("ts=0o7"), [assign(Opt::TabStop, AssignOp::Set, 7)]);
    assert_eq!(args("so=-2"), [assign(Opt::ScrollOff, AssignOp::Set, -2)]);
    assert_eq!(
        args("fdm=indent"),
        [Assign {
            opt: Opt::FoldMethod,
            op: AssignOp::Set,
            val: RawVal::Str("indent".into())
        }]
    );
    assert_eq!(
        args("ts=4 ic sw?"),
        [
            assign(Opt::TabStop, AssignOp::Set, 4),
            Bool(Opt::IgnoreCase, true),
            Show(Opt::ShiftWidth)
        ]
    );
    assert_eq!(
        args("ic  ic?"),
        [Bool(Opt::IgnoreCase, true), Show(Opt::IgnoreCase)]
    );
    assert_eq!(args("all"), [ShowAll]);
    assert_eq!(args("all&"), [ResetAll]);
    assert_eq!(args("t_xx=1 <F1>"), [Ignored, Ignored]);
    assert_eq!(args("wrap"), [Bool(Opt::Wrap, true)]);
    assert_eq!(args("spell"), [Unsupported("spell".into())]);
    // Comments.
    assert_eq!(args("\"x"), []);
    assert_eq!(args("ic\"x"), [Bool(Opt::IgnoreCase, true)]);
    assert_eq!(
        args("ts=4 \"c sw=2"),
        [assign(Opt::TabStop, AssignOp::Set, 4)]
    );
    assert_eq!(args("ts=4\"c"), [assign(Opt::TabStop, AssignOp::Set, 4)]);
}

/// The messages nvim v0.12.6 gives (probed headless with `execute()`), including the argument text it
/// quotes: the argument and the blanks after it.
#[test]
fn errors_quote_nvim() {
    assert_eq!(err("bogus"), "E518: Unknown option: bogus");
    assert_eq!(err("ts=4 bogus sw=2"), "E518: Unknown option: bogus ");
    assert_eq!(err("bogus?"), "E518: Unknown option: bogus?");
    assert_eq!(err("nobogus"), "E518: Unknown option: nobogus");
    assert_eq!(err("no"), "E518: Unknown option: no");
    assert_eq!(err("4"), "E518: Unknown option: 4");
    assert_eq!(err("=4"), "E518: Unknown option: =4");
    assert_eq!(err("all&vim"), "E518: Unknown option: vim");
    assert_eq!(err("ts!"), "E488: Trailing characters: ts!");
    assert_eq!(err("ts! sw=3"), "E488: Trailing characters: ts! ");
    assert_eq!(err("ic?ts"), "E488: Trailing characters: ic?ts");
    assert_eq!(err("ic&x"), "E488: Trailing characters: ic&x");
    assert_eq!(err("ts&vim&"), "E488: Trailing characters: ts&vim&");
    assert_eq!(err("ts<4"), "E488: Trailing characters: ts<4");
    assert_eq!(err("ic!x"), "E488: Trailing characters: ic!x");
    assert_eq!(err("sw*=2"), "E488: Trailing characters: sw*=2");
    assert_eq!(err("ic=1"), "E474: Invalid argument: ic=1");
    assert_eq!(err("ic=1 ts=5"), "E474: Invalid argument: ic=1 ");
    assert_eq!(err("ic+=1"), "E474: Invalid argument: ic+=1");
    assert_eq!(err("noic=1"), "E474: Invalid argument: noic=1");
    assert_eq!(err("nosw"), "E474: Invalid argument: nosw");
    assert_eq!(err("invsw"), "E474: Invalid argument: invsw");
    assert_eq!(err("nowrapscan=1"), "E474: Invalid argument: nowrapscan=1");
    assert_eq!(err("ts=abc"), "E521: Number required after =: ts=abc");
    assert_eq!(err("ts= 4"), "E521: Number required after =: ts= ");
    assert_eq!(err("ts=4x"), "E521: Number required after =: ts=4x");
    assert_eq!(err("ts=4,"), "E521: Number required after =: ts=4,");
    assert_eq!(err("ts="), "E521: Number required after =: ts=");
    assert_eq!(err("ts:"), "E521: Number required after =: ts:");
    assert_eq!(err("ts=+4"), "E521: Number required after =: ts=+4");
    assert_eq!(err("ts+=abc"), "E521: Number required after =: ts+=abc");
    assert_eq!(err("ts==4"), "E521: Number required after =: ts==4");
    assert_eq!(err("ts  =  4"), "E521: Number required after =: ts  =  ");
    assert_eq!(err("ts=4\\ "), "E521: Number required after =: ts=4\\ ");
    assert_eq!(
        err("sw=3 sw=x sw=5"),
        "E521: Number required after =: sw=x "
    );
}

#[test]
fn earlier_arguments_survive_an_error() {
    let l = parse_args(SetScope::Both, "sw=3 sw=x sw=5");
    assert_eq!(l.args.len(), 1);
    assert!(l.error.is_some());
}

#[test]
fn command_words() {
    assert_eq!(
        parse_set_command("set").map(|l| l.scope),
        Some(SetScope::Both)
    );
    assert_eq!(
        parse_set_command("setlocal ts=2").map(|l| l.scope),
        Some(SetScope::Local)
    );
    assert_eq!(
        parse_set_command("setglobal").map(|l| l.scope),
        Some(SetScope::Global)
    );
    assert_eq!(parse_set_command("setx"), None);
    assert_eq!(parse_set_command("sort"), None);
}
