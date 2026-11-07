//! Structured diagnostic logging (D-040 / stability §3, §4), kept strictly separate from the replay `Trace`:
//! the Trace is the deterministic command record (a product artifact); this is operator-facing telemetry for
//! "why did it misbehave". Off by default — set `RUSE_LOG=<file>` (optionally `RUSE_LOG_LEVEL=debug`) to
//! append structured events to a file. Never writes to the terminal (that surface belongs to the editor).
//!
//! Every event goes through one function below (#966): each has a fixed `event` name, a fixed field set and
//! a fixed level (stability §4.1). A call site never writes its own `tracing::` macro, so the field set and
//! the privacy rules hold in one place.
//!
//! **Privacy (stability §10, D-017's direction).** No field ever carries buffer text, the text of a message
//! shown to the user (it can quote buffer text, a pattern or a command line), an environment variable, or
//! a path: a file is logged as [`path_id`], a stable 16-hex-digit hash of its path, plus its extension. An
//! error is logged by its [`io::ErrorKind`] and OS error number, never its message (a custom `io::Error`
//! can name a path). The one exception is a panic's message and location, which the code itself writes
//! (stability §8, #866).
//!
//! **Levels.** `info`: a lifecycle change (`save`, `lsp.spawned`, `recover.chosen`); `warn`: recovered with
//! possible user impact (a failure the user was told about, a degrade); `error`: a component or request
//! failed (`recover.failed`, `lsp.exited` with a crash). `tracing` has no `fatal`; a fatal event is logged
//! at `error` with `severity="fatal"` (only `panic`).
//!
//! **Cost when off.** With no `RUSE_LOG` no subscriber is installed: every `tracing` callsite is disabled,
//! and a field's value (a hash, a kind) is never computed, so a call is a branch on a cached flag.

use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::app::messages::Kind;
use crate::persist::atomic::SaveError;

/// A cloneable file sink so `tracing_subscriber`'s `MakeWriter` can hand out a writer per event.
#[derive(Clone)]
struct FileSink(Arc<Mutex<std::fs::File>>);

impl std::io::Write for FileSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().map_or(Ok(buf.len()), |mut f| f.write(buf))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().map_or(Ok(()), |mut f| f.flush())
    }
}

/// Initialize file logging if `RUSE_LOG` is set. Best-effort: a bad path or level silently disables logging
/// rather than blocking startup (external-failure degrade, §7). Safe to call once at startup.
pub fn init() {
    let Ok(path) = std::env::var("RUSE_LOG") else {
        return;
    };
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let level = std::env::var("RUSE_LOG_LEVEL").unwrap_or_else(|_| "info".into());
    let filter = tracing_subscriber::EnvFilter::try_new(&level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let sink = FileSink(Arc::new(Mutex::new(file)));
    let _ = tracing_subscriber::fmt()
        .with_writer(move || sink.clone())
        .with_ansi(false)
        .with_target(false)
        .with_env_filter(filter)
        .try_init();
}

/// A file as the log names it: the 64-bit FNV-1a hash of its path's bytes, as 16 hex digits. Stable across
/// runs and builds, so events about one file line up, and it never shows the path.
pub(crate) fn path_id(path: &Path) -> String {
    let bytes = crate::persist::journal::path_bytes(path);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// A document's id for the log: [`path_id`], or `unnamed` for the unnamed buffer.
fn doc_id(path: Option<&Path>) -> String {
    path.map_or_else(|| "unnamed".to_string(), path_id)
}

/// A document's extension (`rs`, `md`), empty when it has none: what decides its syntax and server.
fn ext(path: Option<&Path>) -> &str {
    path.and_then(Path::extension)
        .and_then(|e| e.to_str())
        .unwrap_or("")
}

/// The `E486` / `W10` code a message starts with, `-` when it has none. The rest of the text is not logged.
fn message_code(text: &str) -> &str {
    let code = text.split(':').next().unwrap_or("");
    let digits = code.get(1..).unwrap_or("");
    if (code.starts_with('E') || code.starts_with('W'))
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        code
    } else {
        "-"
    }
}

/// An `io::Error` as two fields: its kind and its OS error number (0 when it has none).
fn error_kind(e: &io::Error) -> String {
    format!("{:?}", e.kind())
}
fn os_error(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(0)
}

/// `message`: an error or a warning was shown to the user (`Status::set`, once per message given). Logs its
/// kind, its code and its length, never its text.
pub(crate) fn message(kind: Kind, text: &str) {
    match kind {
        Kind::Error => {
            tracing::warn!(
                event = "message",
                kind = "error",
                code = message_code(text),
                len = text.len()
            )
        }
        Kind::Warning => {
            tracing::warn!(
                event = "message",
                kind = "warning",
                code = message_code(text),
                len = text.len()
            )
        }
        Kind::Info | Kind::Echo => {}
    }
}

/// `panic`: a core panic (fatal: an invariant broke). `recovered` is how many buffers went to their
/// journals, `None` when the buffer store was busy and nothing could be journaled.
pub(crate) fn panic(message: &str, location: &str, recovered: Option<usize>) {
    match recovered {
        Some(n) => tracing::error!(
            event = "panic",
            severity = "fatal",
            recovered = n,
            location,
            panic_message = message,
            "core panic"
        ),
        None => tracing::error!(
            event = "panic",
            severity = "fatal",
            store = "busy",
            location,
            panic_message = message,
            "core panic"
        ),
    }
}

/// `recover.written`: a modified buffer's text went to its journal on a crash path (`reason`: `panic`,
/// `signal`, `io_error`).
pub(crate) fn recover_written(reason: &str, doc: Option<&Path>, journal: &Path) {
    tracing::warn!(
        event = "recover.written",
        reason,
        doc = doc_id(doc),
        ext = ext(doc),
        journal = path_id(journal)
    );
}

/// `recover.failed`: a step of crash recovery failed. `stage` is `rescue` (writing a journal on a crash
/// path), `adopt` (taking a recovered journal over), `archive` (moving one to the archive) or `offer` (a
/// journal that exists but cannot be offered).
pub(crate) fn recover_failed(
    stage: &str,
    doc: Option<&Path>,
    journal: Option<&Path>,
    e: &io::Error,
) {
    tracing::error!(
        event = "recover.failed",
        stage,
        doc = doc_id(doc),
        journal = journal.map_or_else(String::new, path_id),
        error_kind = error_kind(e),
        os_error = os_error(e)
    );
}

/// `recover.unreplayable`: a journal exists for `doc` but cannot be offered (newer version, unreadable, or a
/// large-file journal whose disk text changed). The reason is the status line, not logged.
pub(crate) fn recover_unreplayable(doc: Option<&Path>) {
    tracing::warn!(event = "recover.unreplayable", doc = doc_id(doc));
}

/// `recover.found`: opening `doc` found a journal to offer. `offer` is `available` (unsaved work of a gone
/// session) or `in_use` (another live session; `crashed` when a gone one left work beside it).
pub(crate) fn recover_found(offer: &str, doc: Option<&Path>, journal: &Path, crashed: bool) {
    tracing::info!(
        event = "recover.found",
        offer,
        crashed,
        doc = doc_id(doc),
        ext = ext(doc),
        journal = path_id(journal)
    );
}

/// `recover.chosen`: the user answered the recovery prompt for `doc` (`recover`, `edit_disk`, `discard`,
/// `quit`, `read_only`, `edit_anyway`, `recover_crashed`).
pub(crate) fn recover_chosen(choice: &str, doc: Option<&Path>) {
    tracing::info!(event = "recover.chosen", choice, doc = doc_id(doc));
}

/// `journal.failed`: the per-frame journal sync of `doc` failed (logged once until it succeeds again).
pub(crate) fn journal_failed(doc: &Path, e: &io::Error) {
    tracing::warn!(
        event = "journal.failed",
        doc = path_id(doc),
        error_kind = error_kind(e),
        os_error = os_error(e)
    );
}

/// `open.failed`: a file could not be read (`site`: `startup`, `edit`, `reload`, `read`).
pub(crate) fn open_failed(site: &str, doc: &Path, e: &io::Error) {
    tracing::warn!(
        event = "open.failed",
        site,
        doc = path_id(doc),
        ext = ext(Some(doc)),
        error_kind = error_kind(e),
        os_error = os_error(e)
    );
}

/// `save`: `bytes` bytes of `doc` reached the disk.
pub(crate) fn save(doc: &Path, bytes: usize) {
    tracing::info!(
        event = "save",
        doc = path_id(doc),
        ext = ext(Some(doc)),
        bytes
    );
}

/// `save.failed`: writing `doc` failed; the buffer stays modified. `failure` is how far the save got
/// (`read_only`, `not_regular`, `untouched`, `partial`, `not_durable`, `backup`).
pub(crate) fn save_failed(doc: &Path, e: &SaveError) {
    let (failure, io) = match e {
        SaveError::ReadOnly(_) => ("read_only", None),
        SaveError::NotRegular(_) => ("not_regular", None),
        SaveError::Untouched(err) => ("untouched", Some(err)),
        SaveError::Partial { err, .. } => ("partial", Some(err)),
        SaveError::NotDurable { err, .. } => ("not_durable", Some(err)),
        SaveError::Backup(b) => ("backup", Some(&b.err)),
        SaveError::Kept { err, .. } => return save_failed(doc, err),
    };
    tracing::warn!(
        event = "save.failed",
        doc = path_id(doc),
        failure,
        error_kind = io.map_or_else(String::new, error_kind),
        os_error = io.map_or(0, os_error)
    );
}

/// `save.backup_failed`: a forced write (`:w!`) of `doc` went ahead without the backup it could not make.
pub(crate) fn backup_failed(doc: &Path, e: &io::Error) {
    tracing::warn!(
        event = "save.backup_failed",
        doc = path_id(doc),
        error_kind = error_kind(e),
        os_error = os_error(e)
    );
}

/// `save.stale_temp_removed`: a dead save's temp file beside `doc` was removed.
pub(crate) fn stale_temp_removed(temp: &Path) {
    tracing::info!(event = "save.stale_temp_removed", temp = path_id(temp));
}

/// `lsp.spawned`: the language server `server` (a fixed key such as `rust-analyzer`) started as `pid`.
pub(crate) fn lsp_spawned(server: &str, pid: u32) {
    tracing::info!(event = "lsp.spawned", server, pid);
}

/// `lsp.spawn_failed`: the language server `server` could not be started (most often: not installed).
pub(crate) fn lsp_spawn_failed(server: &str, e: &io::Error) {
    tracing::warn!(
        event = "lsp.spawn_failed",
        server,
        error_kind = error_kind(e),
        os_error = os_error(e)
    );
}

/// `lsp.exited`: the language server `server` ended on its own while the editor ran. A crash (a signal or a
/// non-zero status) is an `error`, a clean exit a `warn`; either way its features stop for the session.
pub(crate) fn lsp_exited(server: &str, status: std::process::ExitStatus) {
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status).unwrap_or(0);
    #[cfg(not(unix))]
    let signal = 0;
    let code = status.code().unwrap_or(-1);
    if status.success() {
        tracing::warn!(event = "lsp.exited", server, crashed = false, code, signal);
    } else {
        tracing::error!(event = "lsp.exited", server, crashed = true, code, signal);
    }
}

/// `lsp.stopped`: the editor shut the language server `server` down.
pub(crate) fn lsp_stopped(server: &str) {
    tracing::info!(event = "lsp.stopped", server);
}

/// `lsp.frame_dropped`: a message from a language server was over the size limit and was skipped.
pub(crate) fn lsp_frame_dropped(len: usize, limit: usize) {
    tracing::warn!(event = "lsp.frame_dropped", len, limit);
}

/// `degrade`: a feature dropped to a lower level instead of failing (INV-CAP-DEGRADE). `component` is what
/// degraded (`syntax`, `terminal`, `config` in safe mode), `reason` a fixed word for why, `ext` the file type when it applies.
pub(crate) fn degrade(component: &str, reason: &str, ext: &str) {
    tracing::warn!(event = "degrade", component, reason, ext);
}

/// `config.diagnostic`: a config file problem, by its code (the message, which can quote a value or a path,
/// is shown on the status line, not logged).
pub(crate) fn config_diagnostic(code: &str) {
    tracing::warn!(event = "config.diagnostic", code);
}

/// A debug build's way for an end-to-end test to make the editor panic (#966): when `RUSE_DEBUG_PANIC_FILE`
/// names a file that exists, the main loop panics. Not compiled into a release build.
#[cfg(debug_assertions)]
pub(crate) fn debug_panic_if_requested() {
    static FILE: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    let file = FILE.get_or_init(|| std::env::var_os("RUSE_DEBUG_PANIC_FILE").map(Into::into));
    if file.as_deref().is_some_and(Path::exists) {
        panic!("RUSE_DEBUG_PANIC_FILE exists");
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A `tracing` subscriber for this thread that writes what `init` would write into a buffer, and the
    /// buffer, as one string per event.
    #[derive(Clone, Default)]
    pub(crate) struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("capture").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Capture {
        /// Run `f` with a subscriber that records every event at `trace` and above, the format `init` uses;
        /// return the lines it logged.
        pub(crate) fn run(f: impl FnOnce()) -> Vec<String> {
            let cap = Capture::default();
            let sink = cap.clone();
            let sub = tracing_subscriber::fmt()
                .with_writer(move || sink.clone())
                .with_ansi(false)
                .with_target(false)
                .with_max_level(tracing::Level::TRACE)
                .finish();
            tracing::subscriber::with_default(sub, f);
            let bytes = cap.0.lock().expect("capture").clone();
            String::from_utf8_lossy(&bytes)
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    /// The one line naming `event`, or a panic listing what was logged.
    pub(crate) fn line<'a>(lines: &'a [String], event: &str) -> &'a str {
        let want = format!("event=\"{event}\"");
        lines
            .iter()
            .find(|l| l.contains(&want))
            .unwrap_or_else(|| panic!("no {event} event in {lines:#?}"))
    }

    fn eacces() -> io::Error {
        io::Error::from_raw_os_error(libc::EACCES)
    }

    #[test]
    fn path_id_is_stable_and_never_the_path() {
        let p = Path::new("/home/someone/secret-project/plan.txt");
        assert_eq!(path_id(p), path_id(p));
        assert_ne!(path_id(p), path_id(Path::new("/home/someone/other.txt")));
        assert_eq!(path_id(p).len(), 16);
        assert!(!path_id(p).contains("plan"));
        // FNV-1a's offset basis for the empty path: the hash is the documented one, not a per-build one.
        assert_eq!(path_id(Path::new("")), "cbf29ce484222325");
    }

    #[test]
    fn message_codes_are_taken_and_the_rest_left_out() {
        assert_eq!(message_code("E486: Pattern not found: hunter2"), "E486");
        assert_eq!(
            message_code("W10: Warning: Changing a readonly file"),
            "W10"
        );
        assert_eq!(message_code("Every: thing else"), "-");
        assert_eq!(message_code("E: no digits"), "-");
        assert_eq!(message_code("E12x: not a code"), "-");
    }

    /// One call per event class, with buffer text, a message quoting it, and paths naming it as inputs.
    /// Every event comes out, and none of the secret, nor a path, is in any field.
    #[test]
    fn every_event_logs_its_fields_and_never_buffer_text_or_a_path() {
        const SECRET: &str = "hunter2-buffer-text";
        let doc = Path::new("/home/someone/hunter2-buffer-text/notes.rs");
        let journal = Path::new("/state/ruse/journal/notes.rs.0123456789abcdef.rj");
        let lines = Capture::run(|| {
            message(Kind::Error, &format!("E486: Pattern not found: {SECRET}"));
            message(Kind::Warning, &format!("W10: {SECRET}"));
            message(Kind::Info, &format!("3 lines yanked {SECRET}"));
            panic("index out of bounds", "core/src/x.rs:1:2", Some(1));
            recover_written("panic", Some(doc), journal);
            recover_failed("adopt", Some(doc), Some(journal), &eacces());
            recover_unreplayable(Some(doc));
            recover_found("available", Some(doc), journal, false);
            recover_chosen("recover", Some(doc));
            journal_failed(doc, &eacces());
            open_failed("edit", doc, &eacces());
            save(doc, 12);
            save_failed(
                doc,
                &SaveError::Partial {
                    path: doc.to_path_buf(),
                    err: io::Error::other(format!("cannot write {}", doc.display())),
                },
            );
            stale_temp_removed(doc);
            lsp_spawned("rust-analyzer", 42);
            lsp_spawn_failed("rust-analyzer", &io::Error::from(io::ErrorKind::NotFound));
            lsp_stopped("rust-analyzer");
            lsp_frame_dropped(10, 5);
            degrade("syntax", "parse_failed", "rs");
            config_diagnostic("config.unknown_key");
        });
        for event in [
            "message",
            "panic",
            "recover.written",
            "recover.failed",
            "recover.unreplayable",
            "recover.found",
            "recover.chosen",
            "journal.failed",
            "open.failed",
            "save",
            "save.failed",
            "save.stale_temp_removed",
            "lsp.spawned",
            "lsp.spawn_failed",
            "lsp.stopped",
            "lsp.frame_dropped",
            "degrade",
            "config.diagnostic",
        ] {
            line(&lines, event);
        }
        // An info message is not logged: two `message` events, the error and the warning.
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.contains("event=\"message\""))
                .count(),
            2
        );
        assert!(line(&lines, "message").contains("code=\"E486\""));
        let all = lines.join("\n");
        assert!(!all.contains(SECRET), "{all}");
        assert!(!all.contains("/home/someone"), "{all}");
        assert!(!all.contains("notes.rs.0123"), "{all}");
        assert!(line(&lines, "save").contains(&format!("doc=\"{}\"", path_id(doc))));
        assert!(line(&lines, "save").contains("ext=\"rs\""));
        assert!(line(&lines, "open.failed").contains("error_kind=\"PermissionDenied\""));
    }

    #[test]
    fn a_panic_is_fatal_with_its_message_and_location() {
        let lines = Capture::run(|| panic("boom", "a.rs:1:2", None));
        let l = line(&lines, "panic");
        assert!(l.contains("ERROR"), "{l}");
        assert!(l.contains("severity=\"fatal\""), "{l}");
        assert!(l.contains("panic_message=\"boom\""), "{l}");
        assert!(l.contains("location=\"a.rs:1:2\""), "{l}");
        assert!(l.contains("store=\"busy\""), "{l}");
    }

    #[cfg(unix)]
    #[test]
    fn a_crashed_server_is_an_error_and_a_clean_exit_a_warning() {
        use std::os::unix::process::ExitStatusExt;
        let lines = Capture::run(|| {
            lsp_exited("gopls", std::process::ExitStatus::from_raw(libc::SIGSEGV));
            lsp_exited("clangd", std::process::ExitStatus::from_raw(0));
        });
        let crash = lines.iter().find(|l| l.contains("gopls")).expect("gopls");
        assert!(
            crash.contains("ERROR") && crash.contains("crashed=true"),
            "{crash}"
        );
        assert!(
            crash.contains(&format!("signal={}", libc::SIGSEGV)),
            "{crash}"
        );
        let clean = lines.iter().find(|l| l.contains("clangd")).expect("clangd");
        assert!(
            clean.contains("WARN") && clean.contains("crashed=false"),
            "{clean}"
        );
    }

    /// With no subscriber a call computes nothing: a field whose value would panic is never evaluated.
    #[test]
    fn with_no_subscriber_no_field_is_computed() {
        struct Loud;
        impl std::fmt::Display for Loud {
            fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                panic!("formatted with no subscriber")
            }
        }
        tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
            tracing::warn!(event = "probe", value = %Loud);
        });
    }
}
