//! The file-shape family: the focused buffer's line endings (`'fileformat'`), `'binary'`, its last line
//! (`'endofline'`, `'endoffile'`) and its BOM (`'bomb'`).

use ruse_core::{DocumentId, OptionLevel};

use super::OptionHost;
use crate::input::set_cmd::{Opt, OptVal};

impl OptionHost<'_> {
    /// The value of a file-shape option for `buf`.
    pub(super) fn get_file_shape(&self, opt: Opt, buf: DocumentId) -> OptVal {
        match opt {
            Opt::EndOfLine => OptVal::Bool(self.file_format(buf).final_newline),
            Opt::FileFormat => OptVal::Str(self.file_format(buf).eol.name()),
            Opt::Binary => OptVal::Bool(self.file_format(buf).binary),
            Opt::EndOfFile => OptVal::Bool(self.file_format(buf).eof),
            Opt::Bomb => OptVal::Bool(self.file_format(buf).bom),
            _ => unreachable!(),
        }
    }

    /// The shape of the focused buffer's file: its line endings, `'binary'`, its last line and its BOM.
    pub(super) fn put_file_shape(
        &mut self,
        opt: Opt,
        level: OptionLevel,
        val: OptVal,
        on: bool,
        buf: DocumentId,
    ) {
        match (opt, val) {
            // `'fileformat'` changes how the file is written, so the buffer is modified (nvim); `:setglobal`
            // sets the format of buffers without a file.
            (Opt::FileFormat, OptVal::Str(name)) => {
                let eol = crate::persist::encoding::Eol::from_name(name).unwrap_or_default();
                if level == OptionLevel::Global {
                    self.files.write.fileformat = eol;
                } else {
                    let before = self.file_format(buf);
                    self.set_file_format(buf, |f| f.eol = eol);
                    if before.eol != eol && !before.binary {
                        self.ws.focused_doc_mut().mark_unsaved();
                    }
                    if level == OptionLevel::Both && !self.files.contains_key(&buf) {
                        self.files.write.fileformat = eol;
                    }
                }
            }
            // nvim's `'binary'`: on, `'textwidth'` and `'wrapmargin'` go to 0 and `'expandtab'` off, and come
            // back when it goes off again.
            (Opt::Binary, _) => {
                let was = self.file_format(buf).binary;
                self.set_file_format(buf, |f| f.binary = on);
                if on && !was {
                    let num = |v: OptVal| match v {
                        OptVal::Num(n) => n,
                        _ => 0,
                    };
                    let saved = (
                        num(self.get(Opt::TextWidth, false)),
                        num(self.get(Opt::WrapMargin, false)),
                        matches!(self.get(Opt::ExpandTab, false), OptVal::Bool(true)),
                    );
                    self.files.write.bin_saved = Some(saved);
                    self.put(Opt::TextWidth, level, OptVal::Num(0));
                    self.put(Opt::WrapMargin, level, OptVal::Num(0));
                    self.put(Opt::ExpandTab, level, OptVal::Bool(false));
                } else if !on && was {
                    if let Some((tw, wm, et)) = self.files.write.bin_saved.take() {
                        self.put(Opt::TextWidth, level, OptVal::Num(tw));
                        self.put(Opt::WrapMargin, level, OptVal::Num(wm));
                        self.put(Opt::ExpandTab, level, OptVal::Bool(et));
                    }
                }
            }
            // The file's shape is the buffer's. Changing its BOM changes the file, so the buffer is modified
            // (nvim); `'endofline'` and `'endoffile'` do not modify it.
            (Opt::EndOfLine | Opt::EndOfFile | Opt::Bomb, _) => {
                let before = self.file_format(buf).bom;
                self.set_file_format(buf, |fmt| match opt {
                    Opt::EndOfLine => fmt.final_newline = on,
                    Opt::EndOfFile => fmt.eof = on,
                    _ => fmt.bom = on,
                });
                if opt == Opt::Bomb && before != on {
                    self.ws.focused_doc_mut().mark_unsaved();
                }
            }
            _ => {}
        }
    }

    /// The shape of `buf`'s file (`'endofline'`, `'endoffile'`, `'bomb'`): its file's, or the one a buffer
    /// without a file will be written with.
    fn file_format(&self, buf: DocumentId) -> crate::persist::encoding::FileFormat {
        self.files.get(&buf).map(|bf| bf.fmt).unwrap_or_else(|| {
            self.files
                .write
                .nofile_fmt
                .get(&buf)
                .copied()
                .unwrap_or_else(|| self.files.write.new_format())
        })
    }

    /// Change the shape of `buf`'s file (or of the one a buffer without a file will be written with).
    fn set_file_format(
        &mut self,
        buf: DocumentId,
        f: impl FnOnce(&mut crate::persist::encoding::FileFormat),
    ) {
        let mut fmt = self.file_format(buf);
        f(&mut fmt);
        match self.files.get_mut(&buf) {
            Some(bf) => bf.fmt = fmt,
            None => {
                self.files.write.nofile_fmt.insert(buf, fmt);
            }
        }
    }
}
