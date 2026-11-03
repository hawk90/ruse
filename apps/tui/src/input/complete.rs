//! Command-line completion (`:help cmdline-completion`, #613): what the text before the caret asks to
//! complete, and the sources that answer it.
//!
//! [`context`] reads an Ex line the way nvim's `set_one_cmd_context` does — command modifiers, a range,
//! the command word, then its argument (after the last `|` that separates commands) — and names an
//! [`Expand`] context plus the byte where the word being completed starts. A [`Completer`] then asks its
//! [`CompletionSource`]s in order; the first that claims the context answers. The built-in sources
//! complete Ex command names, file and directory names, buffer names, and `:set` option names and values.
//! A later source (LSP, project files) plugs in with [`Completer::register`], ahead of the built-ins.
//!
//! The engine has no documents, buffers or options, so everything outside the line comes through a
//! [`CompletionContext`] the frontend supplies when a completion starts.

use std::path::{Path, PathBuf};

/// One completion candidate.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Item {
    /// The text that replaces the word being completed (file names escaped, a directory with its `/`).
    pub text: String,
    /// What the popup menu shows: nvim shows a file's tail without its directory, still escaped.
    pub pum: String,
    /// What `c_CTRL-D` lists: a file's tail, unescaped.
    pub list: String,
}

impl Item {
    /// A candidate shown as it is inserted.
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Item {
        let text = text.into();
        Item {
            pum: text.clone(),
            list: text.clone(),
            text,
        }
    }
}

/// A source's answer: its candidates in the order shown, and where in the word the popup menu's text
/// column starts (a file's tail, past the directory typed before it).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Matches {
    pub items: Vec<Item>,
    /// A byte offset into the completed word: the popup menu is aligned with the text from here.
    pub pum_at: usize,
}

/// What the word before the caret is (Vim's `EXPAND_*` contexts that ruse completes).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Expand<'a> {
    /// An Ex command name (`:wri`).
    Command,
    /// A file name argument (`:e`, `:w`, `:r`, `:sp`); `dirs_only` for a directory argument.
    File { dirs_only: bool },
    /// A buffer name (`:b`, `:bd`).
    Buffer,
    /// A `:set` option name; `bool_only` after a `no`/`inv` prefix.
    Option { bool_only: bool },
    /// The value after `:set {name}=` (`name` is the option's long name).
    OptionValue { name: &'static str },
    /// The argument of `command` (its full name), which no built-in source completes.
    Arg { command: &'a str },
}

/// One completion request: the context and the word typed so far (still escaped as on the line).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Request<'a> {
    pub expand: Expand<'a>,
    pub word: &'a str,
}

/// What completion needs from outside the command line. The frontend implements it over its workspace;
/// every method has a neutral default so a test (or a source) supplies only what it uses.
pub trait CompletionContext {
    /// The listed buffers' names, in buffer-number order.
    fn buffer_names(&self) -> Vec<String> {
        Vec::new()
    }
    /// The current file name (`%`).
    fn current_file(&self) -> Option<String> {
        None
    }
    /// The alternate file name (`#`).
    fn alternate_file(&self) -> Option<String> {
        None
    }
    /// Every buffer's number and file name, for `#{n}`.
    fn buffer_files(&self) -> Vec<(u64, String)> {
        Vec::new()
    }
    /// Whether `path` names a directory.
    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }
    /// The current value of option `long` as `:set {long}?` would print it, for the options ruse has.
    fn option_value(&self, _long: &str) -> Option<String> {
        None
    }
    /// The entries of directory `dir` as `(name, is_dir)`; unreadable is empty.
    fn read_dir(&self, dir: &Path) -> Vec<(String, bool)> {
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| {
                        let is_dir = std::fs::metadata(e.path()).is_ok_and(|m| m.is_dir());
                        (e.file_name().to_string_lossy().into_owned(), is_dir)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    /// The directory `~` names.
    fn home(&self) -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// A context with nothing outside the line: files come from the real file system.
pub struct NoContext;
impl CompletionContext for NoContext {}

/// A completion source. It answers `None` for a context it does not handle, so the next source is asked;
/// `Some` with no items means "handled, nothing matches".
pub trait CompletionSource {
    fn complete(&self, req: &Request<'_>, ctx: &dyn CompletionContext) -> Option<Matches>;
}

/// A finished completion: where the completed word starts on the line, what kind it was, and the matches.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Completion {
    /// The byte offset of the completed word in the line.
    pub start: usize,
    /// The word was a file name (`<Up>`/`<Down>` then walk directories).
    pub files: bool,
    pub matches: Matches,
}

/// The ordered list of sources the command line completes from.
pub struct Completer {
    sources: Vec<Box<dyn CompletionSource>>,
}

impl Default for Completer {
    fn default() -> Completer {
        Completer {
            sources: vec![
                Box::new(CommandNames),
                Box::new(FileNames),
                Box::new(BufferNames),
                Box::new(OptionNames),
            ],
        }
    }
}

impl Completer {
    /// Add `source` ahead of the ones already registered, so it can claim a context before them.
    pub fn register(&mut self, source: Box<dyn CompletionSource>) {
        self.sources.insert(0, source);
    }

    /// Complete the Ex line `before` (the text before the caret). `None` when the word is in no context
    /// any source handles — there the wildchar is typed literally, as in nvim (`:s/a/<Tab>`).
    #[must_use]
    pub fn complete(&self, before: &str, ctx: &dyn CompletionContext) -> Option<Completion> {
        let (start, expand) = context(before)?;
        let req = Request {
            expand,
            word: &before[start..],
        };
        let matches = self.sources.iter().find_map(|s| s.complete(&req, ctx))?;
        Some(Completion {
            start,
            files: matches!(expand, Expand::File { .. }),
            matches,
        })
    }
}

/// Commands whose argument may contain `|` (it does not start another command).
const BAR_IN_ARG: &[&str] = &["normal", "global", "vglobal"];

/// Commands that take file names.
const FILE_COMMANDS: &[&str] = &[
    "edit", "write", "read", "split", "vsplit", "wq", "xit", "exit", "update", "saveas",
];

/// Commands that take a buffer name.
const BUFFER_COMMANDS: &[&str] = &["buffer", "bdelete"];

/// The completion context of the Ex line `line` (the text before the caret) and the byte where its word
/// starts, or `None` when there is nothing to complete there (a pattern, a shell command, a count).
#[must_use]
pub fn context(line: &str) -> Option<(usize, Expand<'static>)> {
    let mut base = 0;
    loop {
        let seg = &line[base..];
        let mut i = skip_modifiers(seg);
        i += super::ex_cmd::range_len(&seg[i..]);
        let wlen = seg[i..]
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(seg.len() - i);
        let wend = i + wlen;
        if wend == seg.len() {
            return Some((base + i, Expand::Command));
        }
        if wlen == 0 {
            return None; // `:!cmd`, `:&&`, `:>`: nothing to complete
        }
        let name = super::ex_cmd::full_command_name(&seg[i..wend])?;
        let mut j = wend;
        if seg[j..].starts_with('!') {
            j += 1;
        }
        let rest = &seg[j..];
        let arg_off = rest.len() - rest.trim_start_matches([' ', '\t']).len();
        if arg_off == 0 {
            return None; // `:b2`, `:s/a/`: no blank before an argument
        }
        let arg_start = j + arg_off;
        if !BAR_IN_ARG.contains(&name) {
            if let Some(p) = unescaped_bar(&seg[arg_start..]) {
                base += arg_start + p + 1;
                continue;
            }
        }
        let arg = &seg[arg_start..];
        let piece = last_piece(arg);
        let at = base + arg_start + piece;
        let expand = if FILE_COMMANDS.contains(&name) {
            Expand::File { dirs_only: false }
        } else if BUFFER_COMMANDS.contains(&name) {
            Expand::Buffer
        } else if name == "set" {
            return set_context(&arg[piece..]).map(|(off, e)| (at + off, e));
        } else {
            Expand::Arg { command: name }
        };
        return Some((at, expand));
    }
}

/// The length of the command modifiers (`:sil[ent][!]`, `:verb[ose]`, …) and blanks before a command.
fn skip_modifiers(seg: &str) -> usize {
    let mut i = seg.len() - seg.trim_start_matches([' ', '\t', ':']).len();
    loop {
        let rest = &seg[i..];
        let len = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        if len == 0 || len == rest.len() || !super::ex_cmd::is_modifier(&rest[..len]) {
            return i;
        }
        let mut after = &rest[len..];
        after = after.strip_prefix('!').unwrap_or(after);
        if !after.starts_with([' ', '\t', ':']) {
            return i;
        }
        let blanks = after.len() - after.trim_start_matches([' ', '\t', ':']).len();
        i += rest.len() - after.len() + blanks;
    }
}

/// The byte offset of the first `|` in `arg` not escaped by a backslash.
fn unescaped_bar(arg: &str) -> Option<usize> {
    let b = arg.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'|' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// The byte offset of the last blank-separated piece of `arg` (a blank escaped with `\` does not
/// separate: `a\ b` is one file name).
fn last_piece(arg: &str) -> usize {
    let b = arg.as_bytes();
    let (mut i, mut start) = (0, 0);
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b' ' | b'\t' => {
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    start.min(arg.len())
}

/// The context of one `:set` argument `piece` and the offset of its word within it.
fn set_context(piece: &str) -> Option<(usize, Expand<'static>)> {
    if let Some(op) = piece.find(['=', ':']) {
        let name = piece[..op].trim_end_matches(['+', '-', '^']);
        let (long, _) = find_option(name)?;
        if is_boolean(long) {
            return None; // `:set ic=<Tab>` types the Tab
        }
        return Some((op + 1, Expand::OptionValue { name: long }));
    }
    if piece.contains(['?', '&', '!']) {
        return None;
    }
    // No option name starts with `no` or `inv`, so either prefix always negates (`:set nonu`).
    match ["no", "inv"].into_iter().find(|p| piece.starts_with(p)) {
        Some(p) => Some((p.len(), Expand::Option { bool_only: true })),
        None => Some((0, Expand::Option { bool_only: false })),
    }
}

/// The options `:set` completes, as `(long, short)` in nvim's order.
fn options() -> impl Iterator<Item = (&'static str, &'static str)> {
    super::nvim_options::NVIM_SET_COMPLETION
        .split(' ')
        .filter_map(|e| e.split_once(':'))
}

/// The `(long, short)` names of the option `name` (either spelling).
fn find_option(name: &str) -> Option<(&'static str, &'static str)> {
    options().find(|&(l, s)| l == name || (!s.is_empty() && s == name))
}

fn is_boolean(long: &str) -> bool {
    super::nvim_options::NVIM_BOOLEAN_OPTIONS
        .split(' ')
        .any(|o| o == long)
}

/// The fixed values of the string options ruse implements, in nvim's order (`:set fdm=<Tab>`).
fn option_values(long: &str) -> &'static [&'static str] {
    match long {
        "foldmethod" => &["manual", "expr", "marker", "indent", "syntax", "diff"],
        _ => &[],
    }
}

/// Whether file names compare ignoring case: nvim's `'fileignorecase'` default (on for macOS and
/// Windows, off elsewhere).
fn file_ignore_case() -> bool {
    cfg!(any(target_os = "macos", windows))
}

fn starts_with_fic(name: &str, prefix: &str) -> bool {
    if file_ignore_case() {
        name.len() >= prefix.len()
            && name
                .get(..prefix.len())
                .is_some_and(|h| h.eq_ignore_ascii_case(prefix))
    } else {
        name.starts_with(prefix)
    }
}

/// The characters escaped with `\` in a completed file name (nvim's `PATH_ESC_CHARS`).
const PATH_ESC_CHARS: &str = " \t\n*?[{`$\\%#'\"|!<";

/// `name` with the characters a file argument would read specially escaped.
#[must_use]
pub fn escape_file(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if PATH_ESC_CHARS.contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `word` with its backslash escapes removed (`a\ b` → `a b`).
fn unescape(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut chars = word.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.extend(chars.next());
        } else {
            out.push(c);
        }
    }
    out
}

/// Ex command names: the commands ruse implements and its own, sorted as nvim sorts them (byte order,
/// so `bNext` comes before `bdelete`).
struct CommandNames;
impl CompletionSource for CommandNames {
    fn complete(&self, req: &Request<'_>, _: &dyn CompletionContext) -> Option<Matches> {
        if req.expand != Expand::Command {
            return None;
        }
        let mut names: Vec<&str> = super::ex_cmd::command_names()
            .filter(|n| n.starts_with(req.word))
            .collect();
        names.sort_unstable();
        names.dedup();
        Some(Matches {
            items: names.into_iter().map(Item::plain).collect(),
            pum_at: 0,
        })
    }
}

/// The suffixes nvim's `'suffixes'` default sorts after other files.
const SUFFIXES: &[&str] = &[".bak", "~", ".o", ".h", ".info", ".swp", ".obj"];

/// File and directory names, relative to the directory typed so far. `%` and `#` complete to the
/// current and alternate file. Hidden entries match only a `.` prefix; names sort byte-wise with
/// `'suffixes'` last, and a directory gets its `/`.
struct FileNames;
impl CompletionSource for FileNames {
    fn complete(&self, req: &Request<'_>, ctx: &dyn CompletionContext) -> Option<Matches> {
        let Expand::File { dirs_only } = req.expand else {
            return None;
        };
        let word = req.word;
        if word.starts_with(['%', '#']) {
            // `%`, `#`, `#{n}` with any file-name modifiers (`%:h`): the one name they expand to, as the
            // Ex command would read it; a directory gets its `/` (nvim `:e %:h<Tab>` gives `sub/`).
            let (cur, alt, buffers) =
                (ctx.current_file(), ctx.alternate_file(), ctx.buffer_files());
            let names = super::ex::FileNames {
                current: cur.as_deref(),
                alternate: alt.as_deref(),
                buffers: &buffers,
            };
            let item = super::ex::expand_filename(word, &names).ok().map(|n| {
                let slash = !n.ends_with('/') && ctx.is_dir(Path::new(&n));
                Item::plain(format!(
                    "{}{}",
                    escape_file(&n),
                    if slash { "/" } else { "" }
                ))
            });
            return Some(Matches {
                items: item.into_iter().collect(),
                pum_at: 0,
            });
        }
        // The directory part as typed (kept verbatim on the line) and the name prefix after it.
        let split = word.rfind('/').map_or(0, |i| i + 1);
        let (dir_typed, prefix) = (&word[..split], unescape(&word[split..]));
        let dir = unescape(dir_typed);
        let path = if dir.is_empty() {
            PathBuf::from(".")
        } else if let Some(rest) = dir.strip_prefix("~/") {
            ctx.home()?.join(rest)
        } else {
            PathBuf::from(&dir)
        };
        let mut entries = ctx.read_dir(&path);
        if prefix.starts_with('.') {
            entries.push((".".into(), true));
            entries.push(("..".into(), true));
        }
        let mut found: Vec<(String, bool)> = entries
            .into_iter()
            .filter(|(n, is_dir)| {
                (prefix.starts_with('.') || !n.starts_with('.'))
                    && starts_with_fic(n, &prefix)
                    && (*is_dir || !dirs_only)
            })
            .collect();
        let demoted = |(n, d): &(String, bool)| !d && SUFFIXES.iter().any(|s| n.ends_with(s));
        found.sort_by(|a, b| demoted(a).cmp(&demoted(b)).then_with(|| a.0.cmp(&b.0)));
        found.dedup();
        let items = found
            .into_iter()
            .map(|(n, d)| {
                let slash = if d { "/" } else { "" };
                let tail = escape_file(&n);
                Item {
                    text: format!("{dir_typed}{tail}{slash}"),
                    pum: format!("{tail}{slash}"),
                    list: format!("{n}{slash}"),
                }
            })
            .collect();
        Some(Matches {
            items,
            pum_at: dir_typed.len(),
        })
    }
}

/// Buffer names containing the typed text, in buffer-number order (nvim matches anywhere in the name).
struct BufferNames;
impl CompletionSource for BufferNames {
    fn complete(&self, req: &Request<'_>, ctx: &dyn CompletionContext) -> Option<Matches> {
        if req.expand != Expand::Buffer {
            return None;
        }
        let pat = unescape(req.word);
        let items = ctx
            .buffer_names()
            .into_iter()
            .filter(|n| n != "[No Name]")
            .filter(|n| {
                if file_ignore_case() {
                    n.to_lowercase().contains(&pat.to_lowercase())
                } else {
                    n.contains(&pat)
                }
            })
            .map(|n| Item::plain(escape_file(&n)))
            .collect();
        Some(Matches { items, pum_at: 0 })
    }
}

/// `:set` option names (long names, matched by long or short name, in nvim's order with `all` first)
/// and values: the current value for an empty value, then a string option's fixed values.
struct OptionNames;
impl CompletionSource for OptionNames {
    fn complete(&self, req: &Request<'_>, ctx: &dyn CompletionContext) -> Option<Matches> {
        let word = req.word;
        match req.expand {
            Expand::Option { bool_only } => {
                let mut items = Vec::new();
                if !bool_only && "all".starts_with(word) {
                    items.push(Item::plain("all"));
                }
                items.extend(
                    options()
                        .filter(|&(l, s)| {
                            l.starts_with(word) || (!s.is_empty() && s.starts_with(word))
                        })
                        .filter(|&(l, _)| !bool_only || is_boolean(l))
                        .map(|(l, _)| Item::plain(l)),
                );
                Some(Matches { items, pum_at: 0 })
            }
            Expand::OptionValue { name } => {
                let values = option_values(name);
                let mut items: Vec<Item> = Vec::new();
                if word.is_empty() {
                    if let Some(v) = ctx.option_value(name) {
                        items.push(Item::plain(v));
                    }
                } else if values.is_empty() {
                    return None; // `:set ts=4<Tab>` types the Tab
                }
                for v in values.iter().filter(|v| v.starts_with(word)) {
                    if !items.iter().any(|i| i.text == *v) {
                        items.push(Item::plain(*v));
                    }
                }
                (!items.is_empty() || !values.is_empty()).then_some(Matches { items, pum_at: 0 })
            }
            _ => None,
        }
    }
}

/// The word `<Up>` in the file wildmenu completes next: the directory above the one `word` is in
/// (nvim's `wildmenu_process_key_filenames`). `dir/sub/x` → `dir/`; `dir/x` and `dir/sub/` → `` (the
/// current directory); `x` → `../`; `../x` → `../../`.
#[must_use]
pub fn parent_word(word: &str) -> String {
    let b = word.as_bytes();
    // The separator before the last component, skipping a directory's own trailing `/`.
    let Some(j) = (1..b.len().saturating_sub(1)).rev().find(|&j| b[j] == b'/') else {
        return if b.first() == Some(&b'/') { "/" } else { "../" }.into();
    };
    if word[..j].ends_with("..") && (j == 2 || b[j - 3] == b'/') {
        return format!("{}../", &word[..=j]);
    }
    match (1..j).rev().find(|&k| b[k] == b'/') {
        Some(k) => word[..=k].to_string(),
        None if b[0] == b'/' => "/".into(),
        None => String::new(),
    }
}

/// The longest prefix every candidate shares (`c_CTRL-L`), compared ignoring case for file names under
/// `'fileignorecase'`; the first candidate's spelling is kept.
#[must_use]
pub fn longest_common(items: &[Item], files: bool) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let fold = files && file_ignore_case();
    let mut len = first.text.len();
    for it in &items[1..] {
        let common = first
            .text
            .char_indices()
            .zip(it.text.chars())
            .find(|((_, a), b)| {
                if fold {
                    !a.eq_ignore_ascii_case(b)
                } else {
                    a != b
                }
            })
            .map_or(first.text.len().min(it.text.len()), |((i, _), _)| i);
        len = len.min(common);
    }
    first.text[..len].to_string()
}

/// `c_CTRL-D`'s listing of `items` in columns for a `width`-cell screen, filled down each column first
/// as nvim does; each column is the widest entry plus two cells.
#[must_use]
pub fn list_columns(items: &[Item], width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;
    let col_w = items.iter().map(|i| i.list.width()).max().unwrap_or(0) + 2;
    let cols = ((width + 1) / col_w).max(1);
    let rows = items.len().div_ceil(cols);
    (0..rows)
        .map(|r| {
            let mut line = String::new();
            for c in 0..cols {
                let Some(it) = items.get(r + c * rows) else {
                    break;
                };
                line.push_str(&it.list);
                if items.get(r + (c + 1) * rows).is_some() {
                    line.push_str(&" ".repeat(col_w - it.list.width()));
                }
            }
            line
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed directory tree and workspace.
    struct Fake;
    impl CompletionContext for Fake {
        fn buffer_names(&self) -> Vec<String> {
            vec!["dir/foo.txt".into(), "dir/bar.c".into(), "sub/a1".into()]
        }
        fn current_file(&self) -> Option<String> {
            Some("sub/a1".into())
        }
        fn alternate_file(&self) -> Option<String> {
            Some("dir/bar.c".into())
        }
        fn buffer_files(&self) -> Vec<(u64, String)> {
            vec![(1, "dir/foo.txt".into()), (3, "sub/a1".into())]
        }
        fn is_dir(&self, path: &Path) -> bool {
            matches!(path.to_str(), Some("dir" | "sub"))
        }
        fn option_value(&self, long: &str) -> Option<String> {
            match long {
                "tabstop" => Some("8".into()),
                "foldmethod" => Some("indent".into()),
                _ => None,
            }
        }
        fn read_dir(&self, dir: &Path) -> Vec<(String, bool)> {
            let e = |n: &str, d: bool| (n.to_string(), d);
            match dir.to_str() {
                Some(".") => vec![
                    e("x.o", false),
                    e("dir", true),
                    e("x.c", false),
                    e(".hidden", false),
                    e("x.txt", false),
                    e("sub", true),
                    e("x.h", false),
                ],
                Some("dir/") => vec![
                    e("foobar.rs", false),
                    e("food", true),
                    e("bar.c", false),
                    e("fo o.md", false),
                    e("foo.txt", false),
                ],
                Some("/home/u/") => vec![e("notes", false)],
                _ => Vec::new(),
            }
        }
        fn home(&self) -> Option<PathBuf> {
            Some(PathBuf::from("/home/u"))
        }
    }

    fn texts(line: &str) -> Option<Vec<String>> {
        Completer::default()
            .complete(line, &Fake)
            .map(|c| c.matches.items.into_iter().map(|i| i.text).collect())
    }

    #[test]
    fn contexts_follow_the_command() {
        assert_eq!(context("wri"), Some((0, Expand::Command)));
        assert_eq!(context("%wri"), Some((1, Expand::Command)));
        assert_eq!(context("sil 2,3d"), Some((7, Expand::Command)));
        assert_eq!(context("e foo | wri"), Some((8, Expand::Command)));
        assert_eq!(
            context("e! dir/f"),
            Some((3, Expand::File { dirs_only: false }))
        );
        assert_eq!(
            context("e a b\\ c"),
            Some((4, Expand::File { dirs_only: false }))
        );
        assert_eq!(context("b o"), Some((2, Expand::Buffer)));
        assert_eq!(
            context("set nu ts"),
            Some((7, Expand::Option { bool_only: false }))
        );
        assert_eq!(
            context("set nonu"),
            Some((6, Expand::Option { bool_only: true }))
        );
        assert_eq!(
            context("set fdm="),
            Some((8, Expand::OptionValue { name: "foldmethod" }))
        );
        // Nothing to complete: a pattern, a shell command, a boolean's value, `:normal` keys.
        for line in ["s/a/", "!ls ", "set ic=", "set ic&", "b2", "normal x|e "] {
            assert!(
                !matches!(
                    context(line),
                    Some((_, Expand::File { .. } | Expand::Command))
                ),
                ":{line}"
            );
        }
        assert_eq!(
            context("normal "),
            Some((7, Expand::Arg { command: "normal" }))
        );
    }

    #[test]
    fn file_names_match_nvim() {
        // nvim v0.12.6 `:e dir/fo<Tab>`: `dir/fo\ o.md|dir/foo.txt|dir/foobar.rs|dir/food/`.
        assert_eq!(
            texts("e dir/fo").unwrap(),
            ["dir/fo\\ o.md", "dir/foo.txt", "dir/foobar.rs", "dir/food/"]
        );
        // `:e <Tab>`: hidden files left out, `'suffixes'` last: `dir/|sub/|x.c|x.txt|x.h|x.o`.
        assert_eq!(
            texts("e ").unwrap(),
            ["dir/", "sub/", "x.c", "x.txt", "x.h", "x.o"]
        );
        // `:e .<Tab>`: `./|../|.hidden`.
        assert_eq!(texts("e .").unwrap(), ["./", "../", ".hidden"]);
        assert_eq!(texts("e dir/zz").unwrap(), Vec::<String>::new());
        assert_eq!(texts("e %").unwrap(), ["sub/a1"]);
        assert_eq!(texts("e #").unwrap(), ["dir/bar.c"]);
        assert_eq!(texts("e #1").unwrap(), ["dir/foo.txt"]);
        // nvim `:e %:h<Tab>` on `sub/a1`: `sub/`.
        assert_eq!(texts("e %:h").unwrap(), ["sub/"]);
        assert_eq!(texts("e %:t").unwrap(), ["a1"]);
        assert_eq!(texts("e ~/n").unwrap(), ["~/notes"]);
        let c = Completer::default().complete("e dir/fo", &Fake).unwrap();
        assert_eq!((c.start, c.files, c.matches.pum_at), (2, true, 4));
        assert_eq!(c.matches.items[0].pum, "fo\\ o.md");
        assert_eq!(c.matches.items[0].list, "fo o.md");
    }

    #[test]
    fn commands_buffers_and_options() {
        assert_eq!(texts("wri").unwrap(), ["write"]);
        let b = texts("b").unwrap();
        assert!(
            b.starts_with(&["bNext".to_string(), "bdelete".into()]),
            "{b:?}"
        );
        assert_eq!(texts("zzz").unwrap(), Vec::<String>::new());
        assert_eq!(texts("b ").unwrap(), ["dir/foo.txt", "dir/bar.c", "sub/a1"]);
        assert_eq!(texts("b ba").unwrap(), ["dir/bar.c"]);
        assert_eq!(texts("set nu").unwrap(), ["number", "numberwidth"]);
        assert_eq!(
            texts("set ts").unwrap(),
            ["tabstop", "thesaurus", "thesaurusfunc"]
        );
        assert_eq!(texts("set nonu").unwrap(), ["number"]);
        assert_eq!(texts("se").unwrap(), ["set"]);
        assert_eq!(
            texts("set ").unwrap()[..3],
            ["all", "allowrevins", "ambiwidth"]
        );
        assert!(!texts("set no").unwrap().iter().any(|o| o == "all"));
        assert_eq!(texts("set ts=").unwrap(), ["8"]);
        assert_eq!(texts("set ts=4"), None);
        assert_eq!(
            texts("set fdm=").unwrap(),
            ["indent", "manual", "expr", "marker", "syntax", "diff"]
        );
        assert_eq!(texts("set fdm=i").unwrap(), ["indent"]);
        assert_eq!(texts("s/a/"), None);
    }

    #[test]
    fn up_goes_to_the_parent_directory() {
        assert_eq!(parent_word("dir/sub/x"), "dir/");
        assert_eq!(parent_word("dir/sub/"), "");
        assert_eq!(parent_word("dir/fo\\ o.md"), "");
        assert_eq!(parent_word("x.c"), "../");
        assert_eq!(parent_word("../x"), "../../");
        assert_eq!(parent_word("/usr/x"), "/");
    }

    #[test]
    fn longest_and_listing() {
        let items: Vec<Item> = ["sub/a1", "sub/a1.c", "sub/a1.d/"]
            .into_iter()
            .map(Item::plain)
            .collect();
        assert_eq!(longest_common(&items, true), "sub/a1");
        let items: Vec<Item> = ["a1", "a1.c", "a1.d/", "a2", "a3/"]
            .into_iter()
            .map(Item::plain)
            .collect();
        // nvim `:e sub/a<C-d>` on 60 columns: `a1     a1.c   a1.d/  a2     a3/`.
        assert_eq!(
            list_columns(&items, 60),
            ["a1     a1.c   a1.d/  a2     a3/"]
        );
        // Narrow: filled down each column first.
        assert_eq!(
            list_columns(&items, 15),
            ["a1     a2", "a1.c   a3/", "a1.d/"]
        );
    }
}
