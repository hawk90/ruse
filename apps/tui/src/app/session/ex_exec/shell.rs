//! The Ex commands that run a shell or read from outside the buffer (#1500): `:r`, `:{range}!`,
//! `:!` and `:terminal`.

#[cfg(unix)]
use ruse_core::Command;
use ruse_core::{ExCmd, LineAddr, SubRange};

#[cfg(unix)]
use crate::app::dispatch::run_cmd;
use crate::app::dispatch::{lines_outcome, run_ex};
use crate::app::session::{read_text, with_shell_failure, FrameCtx, Session};
use crate::input::ReadSource;
use crate::persist;
#[cfg(unix)]
use crate::term_buffer;

impl Session {
    /// `:[addr]r {file}` / `:[addr]r !{cmd}` — read a file's contents (pure IO) or a shell
    /// command's stdout below the addressed line. The read is IMPURE (fs / process) and stays in
    /// the frontend; core only splices the resulting bytes. The cursor lands on the
    /// FIRST inserted line for a file read, the LAST for a command read (nvim v0.12.4 quirk).
    pub(in crate::app::session) fn ex_read(&mut self, addr: LineAddr, source: ReadSource) {
        let Session {
            ws,
            recorded,
            status,
            ..
        } = self;
        match source {
            ReadSource::File(path) => match persist::read_file(std::path::Path::new(&path)) {
                Ok(raw) => {
                    // Strip the read file's own BOM / CRLF as opening it would; the target
                    // buffer's format is applied on save (#860).
                    let clean = persist::encoding::FileFormat::normalize(&raw);
                    let n = read_text(ws, recorded, addr, clean.to_vec(), false);
                    status.echo(format!("\"{path}\" {n}L, {}B", raw.len()));
                }
                Err(e) => {
                    crate::log::open_failed("read", std::path::Path::new(&path), &e);
                    status.error(format!("E484: can't open {path}: {e}"));
                }
            },
            ReadSource::Command(cmd) => match crate::shell::capture(&cmd) {
                Ok(out) => {
                    let n = read_text(ws, recorded, addr, out.text.clone().into_bytes(), true);
                    let lines = if n == 1 {
                        "1 more line".to_string()
                    } else {
                        format!("{n} more lines")
                    };
                    status.info(with_shell_failure(&out, lines));
                }
                Err(e) => status.error(e.to_string()),
            },
        }
    }

    /// `:{range}!{cmd}` — FILTER: pipe the range's lines through the shell command and replace
    /// them with its output. Pull the range text out FIRST (read-only), shell out, then splice.
    /// As in nvim the output (stdout + stderr) replaces the range even when the command fails,
    /// and the status line says `shell returned {n}` (#861).
    pub(in crate::app::session) fn ex_filter(&mut self, range: SubRange, cmd: String) {
        let Session {
            ws,
            files,
            recorded,
            status,
            ..
        } = self;
        match ws.range_text(range) {
            Some(input) => match crate::shell::filter(&cmd, {
                // The range's last line goes out as the file ends it (nvim: `:%!od -c` of `a\nb`
                // without `'eol'` reads three bytes, RFC-0024 §3).
                if crate::app::file_cmds::reaches_last_line(ws, range) {
                    let fmt = crate::app::file_cmds::buffer_format(files, ws.focused_buffer());
                    files.write.lines_out(&input, fmt)
                } else {
                    &input
                }
            }) {
                Ok(out) => {
                    // Recorded with the filter's output as data: replay never runs the command.
                    let (first, last) = ws.resolve_range(range);
                    let ex = ExCmd::FilterReplace {
                        first,
                        last,
                        bytes: out.raw.clone(),
                    };
                    let n = lines_outcome(&run_ex(ex, ws, recorded));
                    let lines = if n <= 1 {
                        String::new()
                    } else {
                        format!("{n} lines filtered")
                    };
                    status.info(with_shell_failure(&out, lines));
                }
                Err(e) => status.error(e.to_string()),
            },
            None => status.error("E749: empty buffer"),
        }
    }

    /// `:terminal` (F-011): spawn a shell in a new PTY-backed buffer, sized to the focused
    /// window, and enter Terminal mode. Unix-only in slice 1.
    pub(in crate::app::session) fn ex_terminal(&mut self, frame: &FrameCtx) {
        let Session {
            ws,
            files,
            recorded,
            quit,
            terminals,
            status,
            ..
        } = self;
        let cols = frame.cols;
        let text_rows = frame.text_rows;
        let rects = &frame.rects;
        #[cfg(unix)]
        {
            let rect = rects.get(ws.focus());
            let cols = rect.map_or(cols, |r| r.w).max(1);
            let rows = rect.map_or(text_rows, |r| r.h).max(1);
            match term_buffer::Terminal::spawn(rows, cols) {
                Ok(term) => {
                    let id = ws.add_buffer(Vec::new(), Some("[terminal]".to_string()));
                    ws.focus_buffer(id);
                    terminals.insert(id, term);
                    run_cmd(Command::EnterTerminal, ws, files, recorded, status, quit);
                    status.echo("terminal started (CTRL-\\ CTRL-N to leave)");
                }
                Err(e) => status.error(format!("E: cannot start terminal: {e}")),
            }
        }
        #[cfg(not(unix))]
        {
            status.error("terminal is not supported on this platform");
        }
    }

    /// `:!{cmd}` — run the command and show its output on the status line; NO buffer change.
    /// Multi-line output is collapsed to one status line (a scrolling message pane is a
    /// follow-up); an empty result echoes the command so the user sees it ran.
    pub(in crate::app::session) fn ex_shell(&mut self, cmd: String) {
        let Session { status, .. } = self;
        match crate::shell::capture(&cmd) {
            Ok(out) => {
                let joined = out.text.trim_end_matches('\n').replace('\n', " | ");
                let shown = if joined.is_empty() {
                    format!(":!{cmd}")
                } else {
                    joined
                };
                status.echo(match out.failure() {
                    Some(note) => format!("{shown} | {note}"),
                    None => shown,
                });
            }
            Err(e) => status.error(e.to_string()),
        }
    }
}
