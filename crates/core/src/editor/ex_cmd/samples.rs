use super::{ExCmd, GroupKind};
use crate::editor::{CaretGravity, EditorOption, GlobalCmd, SortKind, SortOptions, SubFlags};
use crate::pos::BytePos;
use crate::register::Register;
use crate::transaction::TransactionOrigin;

/// One of each [`ExCmd`] shape, for the codec round-trip test (`round_trips_every_variant`).
#[cfg(test)]
pub(crate) fn samples() -> Vec<ExCmd> {
    let sub = |answers| ExCmd::Substitute {
        first: 1,
        last: 12,
        pattern: "a b\\n".into(),
        replacement: String::new(),
        flags: SubFlags {
            global: true,
            ignore_case: Some(false),
        },
        answers,
    };
    vec![
        sub(None),
        sub(Some(Vec::new())),
        sub(Some(vec![true, false, true])),
        ExCmd::Global {
            marks: vec![1, 3, 5],
            cmd: GlobalCmd::Delete,
        },
        ExCmd::Global {
            marks: Vec::new(),
            cmd: GlobalCmd::Substitute {
                pattern: "x".into(),
                replacement: "가".into(),
                flags: SubFlags::default(),
            },
        },
        ExCmd::Delete {
            first: 3,
            last: 3,
            reg: Some('"'),
        },
        ExCmd::Delete {
            first: 1,
            last: 2,
            reg: None,
        },
        ExCmd::Yank {
            first: 1,
            last: 2,
            reg: Some('a'),
        },
        ExCmd::Join {
            first: 1,
            last: 4,
            no_space: true,
        },
        ExCmd::Shift {
            first: 2,
            last: 3,
            left: true,
            levels: 3,
        },
        ExCmd::Shift {
            first: 2,
            last: 3,
            left: false,
            levels: 1,
        },
        ExCmd::Move {
            first: 2,
            last: 3,
            dest: 0,
        },
        ExCmd::Copy {
            first: 1,
            last: 1,
            dest: 9,
        },
        ExCmd::Put {
            dest: 4,
            reg: Some('+'),
        },
        ExCmd::Sort {
            first: 1,
            last: 9,
            opts: SortOptions::default(),
        },
        ExCmd::Sort {
            first: 1,
            last: 9,
            opts: SortOptions {
                reverse: true,
                key: SortKind::Hex,
                unique: true,
                ignore_case: true,
                pattern: Some("\\d\\+ ".into()),
                use_match: true,
            },
        },
        ExCmd::Sort {
            first: 1,
            last: 9,
            opts: SortOptions {
                pattern: Some(String::new()),
                ..SortOptions::default()
            },
        },
        ExCmd::Retab {
            first: 1,
            last: 2,
            bang: true,
            tabstop: crate::cells::VarStops::parse("8"),
        },
        ExCmd::Retab {
            first: 1,
            last: 2,
            bang: false,
            tabstop: None,
        },
        ExCmd::ReadText {
            dest: 7,
            cursor_on_last: true,
            bytes: b"hex\x00\xff\n".to_vec(),
        },
        ExCmd::ReadText {
            dest: 0,
            cursor_on_last: false,
            bytes: Vec::new(),
        },
        ExCmd::FilterReplace {
            first: 1,
            last: 3,
            bytes: b"a\nb\n".to_vec(),
        },
        ExCmd::Mark {
            line: 2,
            name: 'a',
            nonblank: false,
        },
        ExCmd::Mark {
            line: 2,
            name: '<',
            nonblank: true,
        },
        ExCmd::Set(EditorOption::IgnoreCase(true)),
        ExCmd::Set(EditorOption::SmartCase(false)),
        ExCmd::Set(EditorOption::TabStop(8)),
        ExCmd::Set(EditorOption::ShiftWidth(0)),
        ExCmd::Set(EditorOption::ExpandTab(false)),
        ExCmd::Set(EditorOption::TextWidth(79)),
        ExCmd::Set(EditorOption::SoftTabStop(-1)),
        ExCmd::Set(EditorOption::SoftTabStop(4)),
        ExCmd::Set(EditorOption::VarTabStop("4,8".into())),
        ExCmd::Set(EditorOption::VarTabStop(String::new())),
        ExCmd::Set(EditorOption::VarSoftTabStop("2,4".into())),
        ExCmd::Set(EditorOption::AutoIndent(false)),
        ExCmd::Set(EditorOption::SmartIndent(true)),
        ExCmd::Set(EditorOption::Lisp(true)),
        ExCmd::Set(EditorOption::LispWords("defun,let*".into())),
        ExCmd::Set(EditorOption::CIndent(true)),
        ExCmd::Set(EditorOption::CinOptions(">2s,(0".into())),
        ExCmd::Set(EditorOption::CinKeys("0{,0},:".into())),
        ExCmd::Set(EditorOption::CinScopeDecls("public,signals".into())),
        ExCmd::Set(EditorOption::CopyIndent(true)),
        ExCmd::Set(EditorOption::PreserveIndent(true)),
        ExCmd::Set(EditorOption::SmartTab(false)),
        ExCmd::Set(EditorOption::ShiftRound(true)),
        ExCmd::Set(EditorOption::CaretGravity(CaretGravity::OnChar)),
        ExCmd::Set(EditorOption::CaretGravity(CaretGravity::BetweenChar)),
        ExCmd::Place(BytePos(17)),
        ExCmd::Want(3),
        ExCmd::Want(super::super::range::MAXCOL),
        ExCmd::UndoSync,
        ExCmd::Edits {
            origin: TransactionOrigin::Lsp,
            edits: vec![
                (BytePos(0), BytePos(3), "가 b".into()),
                (BytePos(5), BytePos(5), String::new()),
            ],
        },
        ExCmd::Edits {
            origin: TransactionOrigin::UserInput,
            edits: Vec::new(),
        },
        ExCmd::Edits {
            origin: TransactionOrigin::Reload,
            edits: vec![(BytePos(0), BytePos(1), "z".into())],
        },
        ExCmd::KeepJumps(true),
        ExCmd::KeepJumps(false),
        ExCmd::Modifiable(false),
        ExCmd::Modifiable(true),
        ExCmd::Modified(true),
        ExCmd::Modified(false),
        ExCmd::Register {
            name: None,
            value: Register::linewise(b"a\n".to_vec()),
        },
        ExCmd::Register {
            name: Some('/'),
            value: Register::charwise(Vec::new()),
        },
        ExCmd::Register {
            name: Some('+'),
            value: Register::charwise(b"x\x00 y".to_vec()),
        },
        ExCmd::Register {
            name: Some('a'),
            value: Register::blockwise_width(b"ab\nc".to_vec(), 4),
        },
        ExCmd::Register {
            name: Some('='),
            value: Register::expr_result(b"x\n".to_vec()),
        },
        ExCmd::Begin(GroupKind::Normal),
        ExCmd::Begin(GroupKind::Global),
        ExCmd::Begin(GroupKind::Macro),
        ExCmd::End,
    ]
}
