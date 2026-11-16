//! The value-assignment tests: nvim's number, flag and list rules. Split from `set_cmd.rs` (#1388).

use super::*;

#[test]
fn values_follow_nvim_rules() {
    let ts = opt_def(Opt::TabStop);
    let sw = opt_def(Opt::ShiftWidth);
    let so = opt_def(Opt::ScrollOff);
    let num = |n| RawVal::Num(n);
    let set = AssignOp::Set;
    assert_eq!(
        assign_value(ts, OptVal::Num(8), set, &num(0)),
        Err(ValueError::Positive)
    );
    assert_eq!(
        assign_value(ts, OptVal::Num(8), set, &num(-1)),
        Err(ValueError::Positive)
    );
    assert_eq!(
        assign_value(ts, OptVal::Num(8), set, &num(10000)),
        Err(ValueError::Invalid)
    );
    assert_eq!(
        assign_value(ts, OptVal::Num(8), set, &num(9999)),
        Ok(OptVal::Num(9999))
    );
    assert_eq!(
        assign_value(ts, OptVal::Num(8), set, &num(99_999_999_999)),
        Err(ValueError::Invalid)
    );
    assert_eq!(
        assign_value(sw, OptVal::Num(8), set, &num(-1)),
        Err(ValueError::Positive)
    );
    assert_eq!(
        assign_value(sw, OptVal::Num(4), AssignOp::Sub, &num(100)),
        Err(ValueError::Positive)
    );
    assert_eq!(
        assign_value(sw, OptVal::Num(i32::MAX.into()), AssignOp::Add, &num(1)),
        Err(ValueError::Invalid)
    );
    assert_eq!(
        assign_value(sw, OptVal::Num(5), AssignOp::Prepend, &num(3)),
        Ok(OptVal::Num(15))
    );
    assert_eq!(
        assign_value(so, OptVal::Num(0), set, &num(-2)),
        Ok(OptVal::Num(-2))
    );
    let fdm = opt_def(Opt::FoldMethod);
    let s = |v: &str| RawVal::Str(v.into());
    let cur = OptVal::Str("manual");
    assert_eq!(
        assign_value(fdm, cur.clone(), set, &s("bogus")),
        Err(ValueError::Invalid)
    );
    assert_eq!(
        assign_value(fdm, cur.clone(), set, &s("")),
        Err(ValueError::Invalid)
    );
    assert_eq!(
        assign_value(fdm, cur.clone(), AssignOp::Add, &s("")),
        Ok(cur.clone())
    );
    assert_eq!(
        assign_value(fdm, cur.clone(), AssignOp::Add, &s("x")),
        Err(ValueError::Invalid)
    );
    assert_eq!(
        assign_value(fdm, cur.clone(), AssignOp::Sub, &s("al")),
        Err(ValueError::Invalid)
    );
    assert_eq!(
        assign_value(fdm, cur.clone(), set, &s("marker")),
        Ok(OptVal::Str("marker"))
    );
}

#[test]
fn string_options_combine_and_check_like_nvim() {
    let set = AssignOp::Set;
    let s = |v: &str| RawVal::Str(v.into());
    let text = |v: &str| OptVal::Text(Cow::Owned(v.into()));
    let fo = opt_def(Opt::FormatOptions);
    // nvim v0.12.6: a flag list drops the flags already there, unless the value is already in it.
    assert_eq!(
        assign_value(fo, text("tcqj"), AssignOp::Add, &s("jr")),
        Ok(text("tcqjr"))
    );
    assert_eq!(
        assign_value(fo, text("tcqj"), AssignOp::Add, &s("qj")),
        Ok(text("tcqj"))
    );
    assert_eq!(
        assign_value(fo, text("tcqj"), AssignOp::Add, &s("cq")),
        Ok(text("tjcq"))
    );
    assert_eq!(
        assign_value(fo, text("tcqj"), AssignOp::Sub, &s("cq")),
        Ok(text("tj"))
    );
    assert_eq!(
        assign_value(fo, text("tcqj"), AssignOp::Sub, &s("qc")),
        Ok(text("tcqj"))
    );
    assert_eq!(
        assign_value(fo, text("tcqj"), AssignOp::Prepend, &s("r")),
        Ok(text("rtcqj"))
    );
    assert_eq!(
        assign_value(fo, text("tcqj"), set, &s("xyz")),
        Err(ValueError::Message("E539: Illegal character <x>".into()))
    );
    let com = opt_def(Opt::Comments);
    assert_eq!(
        assign_value(com, text("b:x"), AssignOp::Add, &s(":;")),
        Ok(text("b:x,:;"))
    );
    assert_eq!(
        assign_value(com, text("b:x,:;"), AssignOp::Add, &s(":;")),
        Ok(text("b:x,:;"))
    );
    assert_eq!(
        assign_value(com, text("b:x,:;"), AssignOp::Sub, &s("b:x")),
        Ok(text(":;"))
    );
    assert_eq!(
        assign_value(com, text("b:x"), AssignOp::Prepend, &s(":;")),
        Ok(text(":;,b:x"))
    );
    assert_eq!(
        assign_value(com, text(""), set, &s(":,")),
        Err(ValueError::Message("E525: Zero length string".into()))
    );
    let cms = opt_def(Opt::CommentString);
    assert_eq!(assign_value(cms, text(""), set, &s("#%s")), Ok(text("#%s")));
    assert!(matches!(
        assign_value(cms, text(""), AssignOp::Add, &s("x")),
        Err(ValueError::Message(m)) if m.starts_with("E537")
    ));
    let fp = opt_def(Opt::FormatPrg);
    assert_eq!(
        assign_value(fp, text(""), AssignOp::Add, &s(" -w")),
        Ok(text(" -w"))
    );
}
