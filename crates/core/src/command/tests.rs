//! The line-form round-trip tests. Split from `command.rs` (#1384).

use super::*;

#[test]
fn pre_1071_search_word_under_traces_parse_as_the_bare_move() {
    assert_eq!(
        Command::from_line("search_word_under bwd any"),
        Ok(Command::SearchWordUnder {
            forward: false,
            whole_word: false,
            op: SearchOp::Move,
            count: 1,
        })
    );
    assert!(Command::from_line("search_word_under fwd whole bogus 1").is_err());
}

#[test]
fn pre_1595_case_motion_traces_parse_unforced() {
    assert_eq!(
        Command::from_line("case_motion 1 down upcase"),
        Ok(Command::CaseMotion {
            count: 1,
            motion: Motion::Down,
            case: WordCase::Upcase,
            wise: None,
        })
    );
    assert!(Command::from_line("case_motion 1 down upcase bogus").is_err());
}

/// One of each trace verb, plus every resolved Ex edit (RFC-0020; `editor/ex_cmd/samples.rs` lists one of each).
fn samples() -> Vec<Command> {
    let cases = [
        Command::MoveLeft,
        Command::MoveRight,
        Command::MoveUp,
        Command::MoveDown,
        Command::MoveLineStart,
        Command::MoveLineEnd,
        Command::EnterInsert,
        Command::EnterInsertAfter,
        Command::EnterNormal,
        Command::EnterTerminal,
        Command::EnterTerminalNormal,
        Command::InsertLineStart,
        Command::InsertColumnZero,
        Command::AppendLineEnd,
        Command::OpenBelow,
        Command::OpenAbove,
        Command::EnterReplace,
        Command::ReplaceType('z'),
        Command::ReplaceType('가'),
        Command::ReplaceBackspace,
        Command::EnterVirtualReplace,
        Command::VirtualReplaceType('z'),
        Command::VirtualReplaceType('가'),
        Command::VirtualReplaceChar(1, 'z'),
        Command::VirtualReplaceChar(3, 'z'),
        Command::VirtualReplaceChar(1, '가'),
        Command::InsertChar('h'),
        Command::ReplaceChar(1, 'z'),
        Command::ReplaceChar(3, 'z'),
        Command::ReplaceSelectionChar('x'),
        Command::ReplaceSelectionChar('가'),
        Command::ReplaceChar(1, '가'),
        Command::ToggleCase(1),
        Command::ToggleCase(4),
        Command::CaseSelection(WordCase::Upcase),
        Command::CaseSelection(WordCase::Downcase),
        Command::CaseSelection(WordCase::Toggle),
        Command::CaseSelection(WordCase::Rot13),
        Command::CaseMotion {
            count: 1,
            motion: Motion::AParagraph,
            case: WordCase::Rot13,
            wise: None,
        },
        Command::CaseMotion {
            count: 2,
            motion: Motion::Down,
            case: WordCase::Upcase,
            wise: Some(ForcedWise::Charwise),
        },
        Command::CaseMotion {
            count: 1,
            motion: Motion::WordEnd,
            case: WordCase::Toggle,
            wise: Some(ForcedWise::Linewise),
        },
        Command::CaseMotion {
            count: 1,
            motion: Motion::Down,
            case: WordCase::Downcase,
            wise: Some(ForcedWise::Blockwise),
        },
        Command::JoinLines(1),
        Command::JoinLines(3),
        Command::JoinLinesNoSpace(2),
        Command::IncrementNumber(1),
        Command::IncrementNumber(-3),
        Command::IncrementSelection {
            delta: 1,
            sequential: false,
        },
        Command::IncrementSelection {
            delta: -2,
            sequential: true,
        },
        Command::GotoLastChange,
        Command::GotoLastChangeLine,
        Command::GotoChangeMarkStart,
        Command::GotoChangeMarkEnd,
        Command::GotoChangeMarkStartLine,
        Command::GotoChangeMarkEndLine,
        Command::GotoVisualMarkStart,
        Command::GotoVisualMarkEnd,
        Command::GotoVisualMarkStartLine,
        Command::GotoVisualMarkEndLine,
        Command::GotoContextMark,
        Command::GotoContextMarkLine,
        Command::GotoNamedMarkLine('a'),
        Command::GotoNamedMarkLine('z'),
        Command::GotoOlderChange,
        Command::GotoNewerChange,
        Command::GotoOlderJump,
        Command::GotoNewerJump,
        Command::RepeatSubstituteGlobal,
        Command::RepeatSubstituteLine,
        Command::GotoDeclaration { global: false },
        Command::GotoDeclaration { global: true },
        Command::GotoFirstMatch("\\<foo\\>".into()),
        Command::GotoFirstMatch("bar baz".into()),
        Command::ShowKeywordLines {
            above: true,
            list: false,
            count: 1,
        },
        Command::ShowKeywordLines {
            above: false,
            list: false,
            count: 3,
        },
        Command::ShowKeywordLines {
            above: true,
            list: true,
            count: 1,
        },
        Command::ShowKeywordLines {
            above: false,
            list: true,
            count: 1,
        },
        Command::InsertAtLastInsert,
        Command::AsciiInfo,
        Command::Utf8Info,
        Command::FileInfo,
        Command::CursorInfo,
        Command::SetNamedMark('a'),
        Command::SetNamedMark('z'),
        Command::GotoNamedMark('a'),
        Command::GotoNamedMark('z'),
        Command::OpToMark {
            op: MarkOp::Delete,
            name: 'a',
            linewise: false,
        },
        Command::OpToMark {
            op: MarkOp::Yank,
            name: 'z',
            linewise: true,
        },
        Command::OpToMark {
            op: MarkOp::Change,
            name: 'b',
            linewise: false,
        },
        Command::OpToMark {
            op: MarkOp::Case(WordCase::Toggle),
            name: 'c',
            linewise: false,
        },
        Command::OpToMark {
            op: MarkOp::Case(WordCase::Upcase),
            name: 'd',
            linewise: true,
        },
        Command::OpToMark {
            op: MarkOp::Case(WordCase::Rot13),
            name: 'e',
            linewise: false,
        },
        Command::OpToMark {
            op: MarkOp::Shift { left: false },
            name: 'f',
            linewise: false,
        },
        Command::OpToMark {
            op: MarkOp::Shift { left: true },
            name: 'g',
            linewise: true,
        },
        Command::OpToMark {
            op: MarkOp::Reindent,
            name: 'h',
            linewise: false,
        },
        Command::OpToMark {
            op: MarkOp::Format { keep: false },
            name: 'i',
            linewise: true,
        },
        Command::OpToMark {
            op: MarkOp::Format { keep: true },
            name: 'j',
            linewise: false,
        },
        Command::OpToMark {
            op: MarkOp::Comment,
            name: 'k',
            linewise: true,
        },
        Command::Comment {
            count: 2,
            motion: Motion::Line,
        },
        Command::Search {
            op: SearchOp::Comment,
            count: 1,
            pattern: "c".into(),
            backward: false,
            offset: SearchOffset::None,
        },
        Command::BreakUndo,
        Command::ShiftRight(1),
        Command::ShiftRight(3),
        Command::ShiftLeft(1),
        Command::ShiftLeft(2),
        Command::Reindent {
            count: 1,
            motion: Motion::Line,
        },
        Command::Format {
            count: 1,
            motion: Motion::AParagraph,
            keep_cursor: false,
        },
        Command::Format {
            count: 2,
            motion: Motion::Down,
            keep_cursor: true,
        },
        Command::SetIndents {
            first_line: 0,
            last_line: 0,
            levels: vec![],
        },
        Command::SetIndents {
            first_line: 2,
            last_line: 5,
            levels: vec![0, 1, 2, 1],
        },
        Command::SetIndentCols {
            first_line: 1,
            last_line: 3,
            cols: vec![Some(0), None, Some(9)],
        },
        Command::SetIndentCols {
            first_line: 0,
            last_line: 0,
            cols: vec![],
        },
        Command::InsertReindent,
        Command::InsertChar('🎉'),
        Command::InsertChar(' '),
        Command::InsertRegister('"'),
        Command::InsertRegister('0'),
        Command::InsertRegister('a'),
        Command::InsertRegister('-'),
        Command::InsertDeleteWordBack,
        Command::InsertDeleteToLineStart,
        Command::CompleteWord {
            back: 2,
            text: "foobar".into(),
        },
        Command::CompleteWord {
            back: 6,
            text: String::new(),
        },
        Command::InsertIndent,
        Command::InsertDedent,
        Command::InsertDeleteIndent,
        Command::InsertDeleteIndentRestore,
        Command::InsertStartColumnLine { down: true },
        Command::InsertStartColumnLine { down: false },
        Command::InsertCaretStep { right: true },
        Command::InsertCaretStep { right: false },
        Command::InsertTab,
        Command::InsertNewline,
        Command::OpenLineIndent {
            kind: OpenKind::Below,
            level: 0,
        },
        Command::OpenLineIndent {
            kind: OpenKind::Above,
            level: 2,
        },
        Command::OpenLineIndent {
            kind: OpenKind::Split,
            level: 3,
        },
        Command::InsertCloser { ch: '}' },
        Command::InsertCloser { ch: ']' },
        Command::DeleteBack,
        Command::DeleteUnder(1),
        Command::DeleteUnder(3),
        Command::DeleteForward(1),
        Command::DeleteForward(2),
        Command::Move(2, Motion::WordFwd),
        Command::Move(1, Motion::EmacsWordFwd),
        Command::Delete(1, Motion::EmacsWordFwd),
        Command::Move(1, Motion::BigWordFwd),
        Command::Delete(1, Motion::BigWordBack),
        Command::Change(2, Motion::BigWordEnd),
        Command::Delete(1, Motion::Line),
        Command::Delete(1, Motion::InnerWord),
        Command::Change(1, Motion::AWord),
        Command::Delete(1, Motion::InnerBigWord),
        Command::Change(1, Motion::ABigWord),
        Command::Delete(1, Motion::InnerParagraph),
        Command::Yank(1, Motion::AParagraph),
        Command::Delete(1, Motion::InnerSentence),
        Command::Change(1, Motion::ASentence),
        Command::Delete(
            1,
            Motion::Pair {
                open: '(',
                close: ')',
                around: false,
            },
        ),
        Command::Change(
            2,
            Motion::Pair {
                open: '{',
                close: '}',
                around: true,
            },
        ),
        Command::Yank(
            1,
            Motion::Pair {
                open: '<',
                close: '>',
                around: false,
            },
        ),
        Command::Delete(
            1,
            Motion::Quote {
                ch: '"',
                around: true,
            },
        ),
        Command::Change(
            1,
            Motion::Quote {
                ch: '`',
                around: false,
            },
        ),
        Command::Change(3, Motion::WordEnd),
        Command::OpForced {
            op: OpKind::Delete,
            count: 1,
            motion: Motion::Down,
            wise: ForcedWise::Charwise,
        },
        Command::OpForced {
            op: OpKind::Yank,
            count: 2,
            motion: Motion::WordEnd,
            wise: ForcedWise::Linewise,
        },
        Command::OpForced {
            op: OpKind::Change,
            count: 1,
            motion: Motion::WordFwd,
            wise: ForcedWise::Charwise,
        },
        Command::OpForced {
            op: OpKind::Delete,
            count: 1,
            motion: Motion::Down,
            wise: ForcedWise::Blockwise,
        },
        Command::Move(
            1,
            Motion::FindChar {
                ch: 'x',
                forward: true,
                till: false,
                repeat: false,
            },
        ),
        Command::Delete(
            2,
            Motion::FindChar {
                ch: ')',
                forward: true,
                till: true,
                repeat: false,
            },
        ),
        Command::Move(
            1,
            Motion::FindChar {
                ch: '가',
                forward: false,
                till: false,
                repeat: false,
            },
        ),
        Command::Move(
            1,
            Motion::FindChar {
                ch: 'x',
                forward: true,
                till: true,
                repeat: true,
            },
        ),
        Command::Move(1, Motion::LastLine),
        Command::Move(5, Motion::GotoLine),
        Command::Delete(1, Motion::GotoLine),
        Command::Move(7, Motion::GotoByte),
        Command::Delete(3, Motion::GotoByte),
        Command::Move(1, Motion::DownFirstNonBlank),
        Command::Delete(2, Motion::DownFirstNonBlank),
        Command::Move(3, Motion::UpFirstNonBlank),
        Command::Delete(1, Motion::UpFirstNonBlank),
        Command::Move(1, Motion::LineUnderscore),
        Command::Delete(2, Motion::LineUnderscore),
        Command::Move(1, Motion::MatchBracket),
        Command::Delete(1, Motion::MatchBracket),
        Command::Move(50, Motion::GotoPercent),
        Command::Delete(25, Motion::GotoPercent),
        Command::Move(50, Motion::MidLine),
        Command::Delete(25, Motion::MidLine),
        Command::Move(3, Motion::WrapLeft),
        Command::Delete(1, Motion::WrapLeft),
        Command::Move(3, Motion::WrapRight),
        Command::Move(2, Motion::EmacsCharBack),
        Command::Delete(4, Motion::EmacsCharFwd),
        Command::Move(1, Motion::ParagraphFwd),
        Command::Move(2, Motion::ParagraphBack),
        Command::Delete(1, Motion::ParagraphFwd),
        Command::Yank(1, Motion::ParagraphBack),
        Command::Move(1, Motion::SectionFwd),
        Command::Move(2, Motion::SectionBack),
        Command::Delete(1, Motion::SectionEndFwd),
        Command::Yank(3, Motion::SectionEndBack),
        Command::Change(1, Motion::SectionFwd),
        Command::Move(1, Motion::UnmatchedParenBack),
        Command::Move(2, Motion::UnmatchedParenFwd),
        Command::Delete(1, Motion::UnmatchedParenBack),
        Command::Yank(1, Motion::UnmatchedParenFwd),
        Command::Change(2, Motion::UnmatchedBraceBack),
        Command::Delete(1, Motion::UnmatchedBraceFwd),
        Command::Move(1, Motion::MethodStartFwd),
        Command::Move(2, Motion::MethodStartBack),
        Command::Delete(1, Motion::MethodEndFwd),
        Command::Yank(3, Motion::MethodEndBack),
        Command::Yank(1, Motion::Line),
        Command::Yank(2, Motion::WordFwd),
        Command::Paste {
            after: true,
            count: 1,
            move_after: false,
        },
        Command::Paste {
            after: false,
            count: 1,
            move_after: false,
        },
        Command::Paste {
            after: true,
            count: 2,
            move_after: true,
        },
        Command::Paste {
            after: false,
            count: 5,
            move_after: true,
        },
        Command::PasteIndent {
            after: true,
            count: 1,
        },
        Command::PasteIndent {
            after: false,
            count: 3,
        },
        Command::PasteBlockInner {
            after: true,
            count: 2,
        },
        Command::PasteSelection {
            swap: true,
            count: 1,
        },
        Command::PasteSelection {
            swap: false,
            count: 3,
        },
        Command::SetRegister(None),
        Command::SetRegister(Some('a')),
        Command::SetRegister(Some('Z')),
        Command::SetExprRegister("1+2*3".into()),
        Command::SetExprRegister("'foo'.'bar'".into()),
        Command::SetExprRegister(String::new()),
        Command::InsertEval("3.0/2".into()),
        Command::InsertEval("'n='.5".into()),
        Command::InsertPaste("dd:q!\n  x y\t한\u{1b}".into()),
        Command::InsertPaste(String::new()),
        Command::ReplacePaste("a b\nc".into()),
        Command::EmacsPaste("kill\nring".into()),
        Command::EnterVisual {
            kind: SelectKind::Charwise,
        },
        Command::EnterVisual {
            kind: SelectKind::Linewise,
        },
        Command::EnterVisual {
            kind: SelectKind::Blockwise,
        },
        Command::EnterSelect {
            kind: SelectKind::Charwise,
        },
        Command::EnterSelect {
            kind: SelectKind::Linewise,
        },
        Command::EnterSelect {
            kind: SelectKind::Blockwise,
        },
        Command::DeleteSelection,
        Command::YankSelection,
        Command::ChangeSelection,
        Command::SwapSelectionEnds,
        Command::ReselectVisual,
        Command::ReplaceSelection('z'),
        Command::ReplaceSelection('가'),
        Command::ReplaceSelection('🎉'),
        Command::VisualOp(VisualOp::Lines(OpKind::Delete)),
        Command::VisualOp(VisualOp::Lines(OpKind::Yank)),
        Command::VisualOp(VisualOp::BlockToEol(OpKind::Change)),
        Command::VisualOp(VisualOp::Shift {
            left: true,
            count: 3,
        }),
        Command::VisualOp(VisualOp::Shift {
            left: false,
            count: 1,
        }),
        Command::VisualOp(VisualOp::Join { spaces: true }),
        Command::VisualOp(VisualOp::Join { spaces: false }),
        Command::VisualOp(VisualOp::Reindent),
        Command::VisualOp(VisualOp::Comment),
        Command::VisualOp(VisualOp::Format),
        Command::VisualOp(VisualOp::OtherCorner),
        Command::VisualOp(VisualOp::YankTrim),
        Command::VisualOp(VisualOp::LeaveAtStart),
        Command::BlockInsert(BlockInsertKind::Insert),
        Command::BlockInsert(BlockInsertKind::Append),
        Command::BlockInsert(BlockInsertKind::Change),
        Command::SearchNext("foo bar".into()),
        Command::SearchPrev("x".into()),
        Command::Search {
            op: SearchOp::Move,
            count: 2,
            pattern: "foo".into(),
            backward: false,
            offset: SearchOffset::None,
        },
        Command::Search {
            op: SearchOp::Delete,
            count: 1,
            pattern: "world foo".into(),
            backward: false,
            offset: SearchOffset::None,
        },
        Command::Search {
            op: SearchOp::Change,
            count: 1,
            pattern: "x".into(),
            backward: true,
            offset: SearchOffset::None,
        },
        Command::Search {
            op: SearchOp::Yank,
            count: 3,
            pattern: "a b".into(),
            backward: true,
            offset: SearchOffset::None,
        },
        Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: "back words".into(),
            backward: true,
            offset: SearchOffset::None,
        },
        // Offset variants (search-offsets, #475) — every SearchOffset shape, both directions and
        // signs, so the offset token survives to_line/from_line round-trip alongside the pattern.
        Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: "foo".into(),
            backward: false,
            offset: SearchOffset::End(0),
        },
        Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: "foo bar".into(),
            backward: false,
            offset: SearchOffset::End(2),
        },
        Command::Search {
            op: SearchOp::Delete,
            count: 1,
            pattern: "x".into(),
            backward: false,
            offset: SearchOffset::End(-1),
        },
        Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: "foo".into(),
            backward: true,
            offset: SearchOffset::Start(0),
        },
        Command::Search {
            op: SearchOp::Yank,
            count: 2,
            pattern: "a b".into(),
            backward: false,
            offset: SearchOffset::Start(3),
        },
        Command::Search {
            op: SearchOp::Change,
            count: 1,
            pattern: "z".into(),
            backward: true,
            offset: SearchOffset::Start(-2),
        },
        Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: "foobar".into(),
            backward: false,
            offset: SearchOffset::Line(1),
        },
        Command::Search {
            op: SearchOp::Delete,
            count: 1,
            pattern: "end".into(),
            backward: true,
            offset: SearchOffset::Line(-3),
        },
        Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: "x".into(),
            backward: false,
            offset: SearchOffset::Line(0),
        },
        Command::SearchObject {
            op: SearchOp::Move,
            count: 1,
            pattern: "foo".into(),
            backward: false,
        },
        Command::SearchObject {
            op: SearchOp::Change,
            count: 2,
            pattern: "foo bar".into(),
            backward: false,
        },
        Command::SearchObject {
            op: SearchOp::Delete,
            count: 1,
            pattern: "x".into(),
            backward: true,
        },
        Command::SearchObject {
            op: SearchOp::Yank,
            count: 3,
            pattern: "a b".into(),
            backward: true,
        },
        Command::SearchObject {
            op: SearchOp::Case(WordCase::Upcase),
            count: 1,
            pattern: "b".into(),
            backward: false,
        },
        Command::SearchObject {
            op: SearchOp::Shift { left: true },
            count: 2,
            pattern: "b".into(),
            backward: false,
        },
        Command::Search {
            op: SearchOp::Format { keep: true },
            count: 1,
            pattern: "end".into(),
            backward: false,
            offset: SearchOffset::None,
        },
        Command::Search {
            op: SearchOp::Reindent,
            count: 1,
            pattern: "x y".into(),
            backward: true,
            offset: SearchOffset::None,
        },
        Command::SearchWordUnder {
            forward: true,
            whole_word: true,
            op: SearchOp::Move,
            count: 1,
        },
        Command::SearchWordUnder {
            forward: false,
            whole_word: false,
            op: SearchOp::Case(WordCase::Upcase),
            count: 3,
        },
        Command::Undo,
        Command::Redo,
        Command::LineUndo,
        Command::Save,
        Command::Quit,
        Command::EmacsYank { count: 1 },
        Command::EmacsYank { count: 4 },
        Command::EmacsBufferEdge { start: true },
        Command::EmacsBufferEdge { start: false },
        Command::EmacsKillLine,
        Command::EmacsTransposeChars,
        Command::EmacsTransposeWords,
        Command::EmacsKillWord { count: 1 },
        Command::EmacsKillWord { count: 3 },
        Command::EmacsBackwardKillWord { count: 1 },
        Command::EmacsBackwardKillWord { count: 2 },
        Command::EmacsHorizontalSpace { keep_one: true },
        Command::EmacsHorizontalSpace { keep_one: false },
        Command::EmacsOpenLine,
        Command::EmacsMarkWord,
        Command::EmacsKillWholeLine,
        Command::EmacsCaseRegion {
            case: WordCase::Upcase,
        },
        Command::EmacsCaseRegion {
            case: WordCase::Downcase,
        },
        Command::EmacsCaseRegion {
            case: WordCase::Capitalize,
        },
        Command::EmacsCaseWord {
            case: WordCase::Upcase,
        },
        Command::EmacsCaseWord {
            case: WordCase::Downcase,
        },
        Command::EmacsCaseWord {
            case: WordCase::Capitalize,
        },
        Command::EmacsDeleteIndentation,
    ];
    // Every resolved Ex edit (RFC-0020); `editor/ex_cmd/samples.rs` lists one of each.
    cases
        .into_iter()
        .chain(crate::editor::ex_cmd_samples().into_iter().map(Command::Ex))
        .collect()
}

#[test]
fn round_trips_every_variant() {
    for c in samples() {
        assert_eq!(Command::from_line(&c.to_line()), Ok(c.clone()), "{c:?}");
    }
}

#[test]
fn a_corrupted_token_never_panics_the_parser() {
    // #720: `--replay` reads untrusted trace files. Replace each token of every verb's line with a
    // hostile one; from_line may reject it but must not panic.
    const HOSTILE: [&str; 9] = [
        "",
        "가",
        "é5",
        "\u{0}",
        "-",
        "+",
        "99999999999999999999",
        "s가",
        "none가",
    ];
    for c in samples() {
        let line = c.to_line();
        let toks: Vec<&str> = line.split(' ').collect();
        for i in 0..toks.len() {
            for h in HOSTILE {
                let mut t = toks.clone();
                t[i] = h;
                let _ = Command::from_line(&t.join(" "));
            }
        }
    }
}

#[test]
fn search_offset_parses_vim_grammar() {
    use SearchOffset::{End, Line, None as Off, Start};
    // No offset.
    assert_eq!(SearchOffset::parse(""), Off);
    // End offsets: `e`, `e+N`, `e-N`, and unsigned `eN` == `e+N`.
    assert_eq!(SearchOffset::parse("e"), End(0));
    assert_eq!(SearchOffset::parse("e+1"), End(1));
    assert_eq!(SearchOffset::parse("e-1"), End(-1));
    assert_eq!(SearchOffset::parse("e2"), End(2));
    // Start offsets: `s`, `s±N`, and the `b` (begin) synonym.
    assert_eq!(SearchOffset::parse("s"), Start(0));
    assert_eq!(SearchOffset::parse("s+2"), Start(2));
    assert_eq!(SearchOffset::parse("s-1"), Start(-1));
    assert_eq!(SearchOffset::parse("s3"), Start(3));
    assert_eq!(SearchOffset::parse("b"), Start(0));
    assert_eq!(SearchOffset::parse("b+1"), Start(1));
    assert_eq!(SearchOffset::parse("b-2"), Start(-2));
    // Line offsets: bare number (unsigned == positive), and signed.
    assert_eq!(SearchOffset::parse("1"), Line(1));
    assert_eq!(SearchOffset::parse("+1"), Line(1));
    assert_eq!(SearchOffset::parse("-3"), Line(-3));
    assert_eq!(SearchOffset::parse("0"), Line(0));
    // A lone sign means magnitude 0 (a line offset of 0).
    assert_eq!(SearchOffset::parse("+"), Line(0));
    assert_eq!(SearchOffset::parse("-"), Line(0));
    // Surrounding whitespace is trimmed.
    assert_eq!(SearchOffset::parse("  e+1  "), End(1));
}

#[test]
fn rejects_unknown_and_bad() {
    assert_eq!(Command::from_line(""), Err(CommandParseError::Empty));
    assert!(matches!(
        Command::from_line("frobnicate"),
        Err(CommandParseError::UnknownVerb(_))
    ));
    assert!(matches!(
        Command::from_line("insert_char zzz"),
        Err(CommandParseError::BadArgument(_))
    ));
}

#[test]
fn a_malformed_search_offset_is_an_error_not_a_panic() {
    // #720: a multi-byte first char and an empty token used to panic in from_token.
    for line in [
        "search move 1 fwd 가 x",
        "search move 1 fwd  x",
        "search move 1 fwd é5 x",
    ] {
        assert!(Command::from_line(line).is_err(), "{line:?}");
    }
}
