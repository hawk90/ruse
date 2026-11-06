//! `:trace save` writes only a trace that replays to the document it follows (#690, F-022).
//!
//! A trace follows one document, the buffer the session started in. Every edit the session makes to it goes
//! through the recorder (`dispatch::run_cmd`/`run_ex`, RFC-0020), but some session events have no recorded
//! form: `:e!` re-reads the file (new text from disk, a fresh undo history), and commands typed in another
//! buffer are recorded into the same list, so a replay would apply them to this one. [`TraceWatch`] notes
//! those events as they happen, and [`check`] replays the trace before it is written: a trace whose replay
//! does not give the start buffer's text is refused, with the events that explain it. So a saved trace
//! replays to the same document, or there is no saved trace.
//!
//! A window switch on the start buffer has a recorded form: the focused window's cursor and wanted column,
//! as an `ex_place` and an `ex_want` ([`TraceWatch::after_input`]), so the commands after it replay from
//! where they ran.

use ruse_core::{Command, DocumentId, ExCmd, Trace, ViewId, Workspace};

use crate::app::dispatch::{place_cursor, run_ex, Files};

/// What the session did to the trace's document that the trace cannot hold.
#[derive(Clone, Debug)]
pub(crate) struct TraceWatch {
    /// The buffer the trace follows.
    pub(crate) start: DocumentId,
    /// The start buffer's [`crate::app::dispatch::BufferFile::reloads`] when the trace began.
    start_reloads: u32,
    /// The focused window after the last key (its View and tile index), to record the cursor a window
    /// switch brings. The View tells windows apart where the index does not: closing the focused first
    /// window keeps the index at 0 and focuses another View (#1272).
    window: (ViewId, usize),
    /// A key recorded commands while another buffer was focused.
    other_buffer: bool,
}

impl TraceWatch {
    /// Watch a trace that starts now, in the focused buffer.
    pub(crate) fn new(ws: &Workspace, files: &Files) -> TraceWatch {
        let start = ws.focused_buffer();
        TraceWatch {
            start,
            start_reloads: files.get(&start).map_or(0, |bf| bf.reloads),
            window: (ws.focused_view(), ws.focus()),
            other_buffer: false,
        }
    }

    /// After one key or paste: `before` is the recorded length and the focused buffer when it began. A key
    /// that recorded commands in another buffer is noted; a switch to another window on the start buffer
    /// records that window's cursor, so a replay continues from it.
    pub(crate) fn after_input(
        &mut self,
        before: (usize, DocumentId),
        ws: &mut Workspace,
        recorded: &mut Vec<Command>,
    ) {
        let (len, buf) = before;
        let here = ws.focused_buffer();
        if recorded.len() > len && (buf != self.start || here != self.start) {
            self.other_buffer = true;
        }
        let window = (ws.focused_view(), ws.focus());
        if window != self.window {
            self.window = window;
            if here == self.start {
                // The window's cursor and its wanted column: the `j`/`k`/`G` after the switch read
                // that window's column, not the one the last window left (#1272).
                let view = ws.focused().view;
                let (at, want) = (view.cursor().0, view.curswant());
                place_cursor(ws, recorded, at);
                run_ex(ExCmd::Want(want), ws, recorded);
            }
        }
    }

    /// The events that the trace cannot hold, for the refusal message.
    fn reasons(&self, files: &Files) -> Vec<&'static str> {
        let mut out = Vec::new();
        if files
            .get(&self.start)
            .is_some_and(|bf| bf.reloads != self.start_reloads)
        {
            out.push("the file was re-read from disk (:e!)");
        }
        if self.other_buffer {
            out.push("commands ran in another buffer");
        }
        out
    }
}

/// Why `:trace save` refused to write a trace. Its `Display` is the status line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The buffer the trace follows was closed (`:bd`).
    Closed,
    /// The trace does not replay at all.
    NoReplay(ruse_core::TraceError),
    /// The replay gives another text, with the events the session saw that explain it (none: an edit
    /// outside the recorder it could not name).
    Differs(Vec<&'static str>),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("trace save refused: ")?;
        match self {
            Refusal::Closed => f.write_str("the buffer the trace follows was closed"),
            Refusal::NoReplay(e) => write!(f, "the trace does not replay ({e:?})"),
            Refusal::Differs(why) if why.is_empty() => {
                f.write_str("an edit the trace cannot record")
            }
            Refusal::Differs(why) => f.write_str(&why.join(", ")),
        }
    }
}

/// Whether `trace` may be written: its replay onto `initial` gives the start buffer's text.
pub(crate) fn check(
    trace: &Trace,
    initial: &[u8],
    ws: &Workspace,
    files: &Files,
    watch: &TraceWatch,
) -> Result<(), Refusal> {
    let doc = ws.document(watch.start).ok_or(Refusal::Closed)?;
    let replayed = trace.replay(initial).map_err(Refusal::NoReplay)?;
    if replayed.bytes() == doc.bytes() {
        Ok(())
    } else {
        Err(Refusal::Differs(watch.reasons(files)))
    }
}
