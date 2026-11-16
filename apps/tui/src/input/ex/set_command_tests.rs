use super::super::set_cmd::{Opt, SetArg, SetScope};
use super::*;

fn set(line: &str) -> super::super::set_cmd::SetLine {
    match parse_ex(line) {
        Ex::SetOptions(l) => l,
        other => panic!("{line}: {other:?}"),
    }
}

#[test]
fn set_words_reach_the_set_grammar() {
    assert_eq!(set("set hls").args[0].0, SetArg::Bool(Opt::HlSearch, true));
    assert_eq!(
        set("set nofixeol").args[0].0,
        SetArg::Bool(Opt::FixEol, false)
    );
    assert_eq!(set("setlocal ts=2").scope, SetScope::Local);
    assert_eq!(set("setglobal ts?").scope, SetScope::Global);
    // A bare `:set` lists the changed options.
    assert!(set("set").args.is_empty() && set("set").error.is_none());
}
