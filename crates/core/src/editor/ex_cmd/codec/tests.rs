use crate::command::Command;

#[test]
fn truncated_extra_or_out_of_bound_tokens_are_errors() {
    for bad in [
        "ex_delete 1 2",
        "ex_delete 1 2 - extra",
        "ex_substitute 1 2 q 61 62",
        "ex_substitute 1 2 g 6 62",
        "ex_substitute 1 2 g 61 62 yn",
        "ex_substitute 1 2 g 61 62 c:yx",
        "ex_global x 1",
        "ex_global d 1,,2",
        "ex_shift 1 1 > 1001",
        "ex_shift 1 1 ^ 1",
        "ex_retab 1 1 0 10000",
        "ex_set tabstop 0",
        "ex_set shiftwidth 2147483648",
        "ex_set ignorecase 2",
        "ex_set caretgravity 2",
        "ex_set wrap 1",
        "ex_sort 1 2 z -",
        "ex_sort 1 2 - 61",
        "ex_mark 1 1114112",
        "ex_mark 1 97 x",
        "ex_mark 1 97 ^ ^",
        "ex_begin visual",
        "ex_edits lsp 1:2",
        "ex_edits lsp 1:2:6",
        "ex_edits lsp x:2:-",
        "ex_edits vim 1:2:-",
        "ex_edits lsp 0:0:ff",
        "ex_keepjumps 2",
        "ex_register 65 c 61",
        "ex_register 95 c 61",
        "ex_register - l 61",
        "ex_register - z 61",
        "ex_register - bx 61",
        "ex_register - c",
        "ex_nope",
    ] {
        assert!(Command::from_line(bad).is_err(), "{bad:?} parsed");
    }
}

/// A trace written before #1628 records one `y`/`n` per match, padded with `n`; it still decodes, each
/// answer as it was given.
#[test]
fn a_padded_yes_no_answer_list_still_decodes() {
    use crate::editor::{ExCmd, SubAnswer};
    let Ok(Command::Ex(ExCmd::Substitute { answers, .. })) =
        Command::from_line("ex_substitute 1 5 - 61 78 c:nynnn")
    else {
        panic!("the pre-#1628 form did not decode");
    };
    let (y, n) = (SubAnswer::Yes, SubAnswer::No);
    assert_eq!(answers, Some(vec![n, y, n, n, n]));
}
