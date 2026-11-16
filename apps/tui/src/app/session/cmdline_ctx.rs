//! What the command line reads from the session (#1496): the `:` completion context (#613) and the
//! sync of the read-only special registers before a dispatch. Split from `session.rs`.

use ruse_core::{Command, DocumentId, Workspace};

use crate::app::dispatch::{write_register, Files};
use crate::input::InputEngine;

/// What `:` completion reads from the session (#613): buffer names, the current and alternate file,
/// and the values of the options ruse keeps. Files come from the real file system (the default).
pub(super) struct ExCompletion<'a> {
    pub(super) ws: &'a Workspace,
    pub(super) files: &'a Files,
    pub(super) alt_file: &'a Option<crate::app::ex_more::AltFile>,
    pub(super) fold_method: Option<crate::input::FoldMethod>,
    pub(super) foldlevel: Option<usize>,
}

impl ExCompletion<'_> {
    fn file_name(&self, id: Option<DocumentId>) -> Option<String> {
        id.and_then(|id| self.files.get(&id))
            .map(|bf| bf.path.to_string_lossy().into_owned())
    }
}

impl crate::input::complete::CompletionContext for ExCompletion<'_> {
    fn buffer_names(&self) -> Vec<String> {
        self.ws.buffers().into_iter().map(|b| b.name).collect()
    }
    fn current_file(&self) -> Option<String> {
        self.file_name(Some(self.ws.focused_buffer()))
    }
    fn alternate_file(&self) -> Option<String> {
        crate::app::ex_more::alternate_name(self.alt_file, self.ws, self.files)
    }
    fn buffer_files(&self) -> Vec<(u64, String)> {
        self.files
            .iter()
            .map(|(id, bf)| (id.0, bf.path.to_string_lossy().into_owned()))
            .collect()
    }
    fn option_value(&self, long: &str) -> Option<String> {
        match long {
            "tabstop" => Some(self.ws.focused().view.tab_width().to_string()),
            "foldlevel" => Some(self.foldlevel.unwrap_or(0).to_string()),
            "foldmethod" => Some(
                match self.fold_method.unwrap_or(crate::input::FoldMethod::Manual) {
                    crate::input::FoldMethod::Indent => "indent",
                    crate::input::FoldMethod::Manual => "manual",
                }
                .into(),
            ),
            _ => None,
        }
    }
}

/// Sync the read-only registers `"/ ": ". "% "#` from frontend state. Each one whose text changed is
/// written through the trace (RFC-0020 R6), since no recorded command produced it; an unchanged one is
/// not recorded again.
pub(super) fn sync_special_registers(
    ws: &mut Workspace,
    recorded: &mut Vec<Command>,
    engine: &InputEngine,
    last_ex_line: &Option<String>,
    files: &Files,
    alt_file: &Option<crate::app::ex_more::AltFile>,
) {
    // `"%` shows the file name as it was opened (a relative path, matching Vim's default `%`); an unnamed
    // buffer has no `Files` entry, so the slot stays empty.
    let file = files
        .get(&ws.focused_buffer())
        .map(|bf| bf.path.display().to_string());
    // `"#` is the alternate file name (#1022): the one a `:w {file}`, `:sav` or `:file` left (#610), or the
    // alternate buffer's file, shown the same way.
    let alternate = crate::app::ex_more::alternate_name(alt_file, ws, files);
    for (name, value) in [
        ('/', engine.last_search().map(str::to_string)),
        (':', last_ex_line.clone()),
        ('.', engine.last_inserted_text()),
        ('%', file),
        ('#', alternate),
    ] {
        // A pattern spells a byte that is not UTF-8 as the regex engine's escape char (`*` on such a
        // word, #1243); `"/` holds the byte itself, as nvim's does.
        let bytes = match (name, value) {
            ('/', Some(pat)) => ruse_core::pattern::unspell(&pat).into_owned(),
            (_, value) => value.unwrap_or_default().into_bytes(),
        };
        // Compared by text: an empty slot never written reads as an empty one (an empty special register
        // is "not set", E29/E30/E32/E35, whichever way it is empty).
        if ws.registers().get(Some(name)).text() != bytes.as_slice() {
            write_register(
                ws,
                recorded,
                Some(name),
                ruse_core::Register::charwise(bytes),
            );
        }
    }
}
