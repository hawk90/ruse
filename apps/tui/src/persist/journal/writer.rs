//! The change log written as rj01 records: sync, compaction and health. Split from `journal.rs` (#1389).

use std::io::Write;

use super::{
    canonical, claim, discard_file, fs, header, io, make_private_dir, now_ms, open_regular,
    path_bytes, rj01, whole, write_private, Arc, Base, ChangeDrain, Document, Duration, FileFormat,
    Instant, Journals, OpenOptions, Path, PathBuf, COMPACT_AFTER, HASH_POLL, IDLE, PENDING_CAP,
    SYNC_EVERY,
};

/// The SHA-256 of a large document's base text (RFC-0023 §5), computed off the input path (#1203): about
/// 100 ms for 33 MiB, which the first keystroke would otherwise wait for.
#[derive(Debug)]
enum BaseHash {
    Ready([u8; 32]),
    Running(std::thread::JoinHandle<[u8; 32]>),
}

impl BaseHash {
    /// Start hashing `text` on a thread of its own; inline when no thread can be started.
    fn start(text: Arc<Vec<u8>>) -> BaseHash {
        let t = Arc::clone(&text);
        match std::thread::Builder::new()
            .name("ruse-journal-hash".into())
            .spawn(move || rj01::sha256(&t))
        {
            Ok(h) => BaseHash::Running(h),
            Err(_) => BaseHash::Ready(rj01::sha256(&text)),
        }
    }

    fn is_ready(&self) -> bool {
        match self {
            BaseHash::Ready(_) => true,
            BaseHash::Running(h) => h.is_finished(),
        }
    }

    /// The hash, waiting for the thread if it is still running.
    fn wait(&mut self, text: &[u8]) -> [u8; 32] {
        let h = match std::mem::replace(self, BaseHash::Ready([0; 32])) {
            BaseHash::Ready(h) => h,
            BaseHash::Running(t) => t.join().unwrap_or_else(|_| rj01::sha256(text)),
        };
        *self = BaseHash::Ready(h);
        h
    }
}

/// A large document's journal Header, held back until its base hash is known (#1203). The Edit records
/// buffer behind it; the Header is written first, so no Edit reaches disk before its `DiskBase`.
#[derive(Debug)]
struct HeldHeader {
    base: Arc<Vec<u8>>,
    hash: BaseHash,
    /// The format the base text is decoded with, as the journal began.
    fmt: FileFormat,
}

/// Where a [`Writer`]'s journal is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// No journal: the buffer has been clean since this writer last looked.
    Idle,
    /// The pending bytes are a whole new journal (Header first), not yet on disk.
    Fresh,
    /// The journal on disk is this writer's, `on_disk` bytes long; pending bytes append to it.
    Live,
}

/// Writes one file buffer's journal from its drained change log (RFC-0023 §4). The session calls
/// [`Writer::absorb`] after every command and [`Writer::flush`] when [`Writer::due`] (or before a risky
/// operation).
#[derive(Debug)]
pub struct Writer {
    /// The journal this writer writes: the document's own name, or this process's `<name>.<pid>.rj` when
    /// a live other session owns that one ([`claim`], #1193).
    journal: PathBuf,
    /// The document's own journal name.
    primary: PathBuf,
    /// The canonical document path (the Header's).
    doc: PathBuf,
    /// The buffer's path as the session last showed it, to notice a rename or `:cd` cheaply.
    seen: PathBuf,
    fmt: FileFormat,
    /// The text at the last drain while there was no journal: the base the next changes apply to.
    base: Option<Arc<Vec<u8>>>,
    state: State,
    /// The next flush replaces the journal with Header + Checkpoint of the current text.
    rewrite: bool,
    pending: Vec<u8>,
    pending_changes: usize,
    last_change: Option<Instant>,
    on_disk: u64,
    since_checkpoint: u64,
    checkpoint_len: u64,
    /// The journal's base is the disk text (RFC-0023 §5): no compaction.
    large: bool,
    /// The hash of a large clean text, started when the writer first saw it (#1203).
    hashing: Option<HeldHeader>,
    /// A large document's fresh journal waits for its base hash: its Header is not in `pending` yet.
    held: Option<HeldHeader>,
    seq: u64,
    /// When the last write and sync succeeded, for `:checkhealth` (RFC-0023 §8).
    last_sync: Option<Instant>,
    /// Failed writes since this writer began, and the last one's error.
    failures: u64,
    last_error: Option<String>,
}

/// One journal's health, as `:checkhealth` reports it (RFC-0023 §8, #938).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Health {
    /// The journal file.
    pub journal: PathBuf,
    /// Its size on disk; 0 when this writer has none there.
    pub size: u64,
    /// Records wait to be written.
    pub unsynced: bool,
    /// How long ago the last successful sync was; `None` before the first.
    pub synced_ago: Option<Duration>,
    pub failures: u64,
    /// The last failure's error, while the last write failed.
    pub failing: Option<String>,
}

impl Writer {
    /// A writer for the buffer whose file is `path`, in `journals`.
    pub fn new(journals: &Journals, path: &Path, fmt: FileFormat) -> Writer {
        let doc = canonical(path);
        let primary = journals.path_for_canonical(&doc);
        Writer {
            journal: claim(&primary),
            primary,
            doc,
            seen: path.to_path_buf(),
            fmt,
            base: None,
            state: State::Idle,
            rewrite: false,
            pending: Vec::new(),
            pending_changes: 0,
            last_change: None,
            on_disk: 0,
            since_checkpoint: 0,
            checkpoint_len: 0,
            large: false,
            hashing: None,
            held: None,
            seq: 0,
            last_sync: None,
            failures: 0,
            last_error: None,
        }
    }

    pub fn journal(&self) -> &Path {
        &self.journal
    }

    /// The journal's health at `now`.
    pub fn health(&self, now: Instant) -> Health {
        Health {
            journal: self.journal.clone(),
            size: if self.state == State::Live {
                self.on_disk
            } else {
                0
            },
            unsynced: self.unsynced(),
            synced_ago: self.last_sync.map(|t| now.saturating_duration_since(t)),
            failures: self.failures,
            failing: self.last_error.clone(),
        }
    }

    pub fn doc(&self) -> &Path {
        &self.doc
    }

    /// Follow the buffer's path: a `:cd` that only respells it changes nothing; a rename (`:saveas`,
    /// `:file`) moves the journal to the new name and records a Meta rename.
    pub fn follow(&mut self, journals: &Journals, path: &Path) {
        if path == self.seen {
            return;
        }
        self.seen = path.to_path_buf();
        let doc = canonical(path);
        if doc == self.doc {
            return;
        }
        let primary = journals.path_for_canonical(&doc);
        let journal = claim(&primary);
        match self.state {
            State::Idle => {}
            State::Fresh => self.rewrite = true,
            State::Live => {
                if fs::rename(&self.journal, &journal).is_ok() {
                    let _ =
                        rj01::encode_meta(&mut self.pending, &rj01::Meta::Rename(path_bytes(&doc)));
                } else {
                    discard_file(&self.journal);
                    self.rewrite = true;
                }
            }
        }
        self.doc = doc;
        self.journal = journal;
        self.primary = primary;
    }

    /// Drain `doc`'s change log into the pending records (`now` is the drain's time). Turns recording on
    /// first, also after `:e!` replaced the document and reset it. A clean buffer drops its journal.
    pub fn absorb(&mut self, doc: &mut Document, fmt: FileFormat, now: Instant) {
        if !doc.is_recording_changes() {
            doc.record_changes(true);
        }
        let drain = doc.drain_changes();
        if !doc.is_modified() {
            self.discard();
            let text = doc.text_arc();
            if text.len() <= rj01::LARGE_FILE {
                self.hashing = None;
            } else if !self
                .hashing
                .as_ref()
                .is_some_and(|h| Arc::ptr_eq(&h.base, &text))
            {
                // Hash a large text now, off the input path, for the journal its first change starts.
                self.hashing = Some(HeldHeader {
                    hash: BaseHash::start(Arc::clone(&text)),
                    base: Arc::clone(&text),
                    fmt: self.fmt,
                });
            }
            self.base = Some(text);
            return;
        }
        if fmt != self.fmt {
            self.fmt = fmt;
            if self.state != State::Idle && !self.rewrite {
                let _ = rj01::encode_meta(&mut self.pending, &rj01::Meta::Format(fmt));
            }
        }
        match drain {
            ChangeDrain::Checkpoint { .. } => {
                self.rewrite = true;
                if self.state == State::Idle {
                    self.state = State::Fresh;
                }
                self.touch(now, 1);
            }
            ChangeDrain::Changes(entries) if entries.is_empty() => {}
            ChangeDrain::Changes(entries) => {
                if self.state == State::Idle {
                    self.start(entries.first());
                }
                if !self.rewrite {
                    for e in &entries {
                        self.seq += 1;
                        let meta = rj01::EditMeta {
                            seq: self.seq,
                            timestamp_ms: now_ms(),
                        };
                        let before = self.pending.len();
                        if rj01::encode_edit(&mut self.pending, meta, e).is_err() {
                            self.rewrite = true;
                            break;
                        }
                        self.since_checkpoint += (self.pending.len() - before) as u64;
                    }
                }
                self.touch(now, entries.len());
            }
        }
        if self.pending.len() > PENDING_CAP {
            self.rewrite = true;
        }
        if self.rewrite {
            self.pending = Vec::new();
        }
    }

    /// Begin a journal at the first change: Header + the base the change applied to.
    fn start(&mut self, first: Option<&ruse_core::ChangeEntry>) {
        self.state = State::Fresh;
        self.pending.clear();
        self.since_checkpoint = 0;
        let Some(base) = self.base.take() else {
            self.rewrite = true;
            return;
        };
        let (revision, node) = first.map_or((0, 0), |e| {
            let node = match e.kind {
                ruse_core::ChangeKind::Fresh { parent, .. } => parent.0,
                ruse_core::ChangeKind::Navigate { .. } => 0,
            };
            (e.base_revision.0, node)
        });
        self.large = base.len() > rj01::LARGE_FILE;
        if self.large {
            // The Header waits for the base hash, which runs off the input path (#1203).
            let fmt = self.fmt;
            self.held = Some(match self.hashing.take() {
                Some(h) if Arc::ptr_eq(&h.base, &base) && h.fmt == fmt => h,
                _ => HeldHeader {
                    hash: BaseHash::start(Arc::clone(&base)),
                    base,
                    fmt,
                },
            });
            self.checkpoint_len = 0;
            return;
        }
        let ok = rj01::encode_header(
            &mut self.pending,
            &header(Some(&self.doc), self.fmt, Base::Checkpoint),
        )
        .is_ok()
            && rj01::encode_checkpoint(&mut self.pending, node, revision, &base).is_ok();
        if !ok {
            self.rewrite = true;
        }
        self.checkpoint_len = base.len() as u64;
    }

    fn touch(&mut self, now: Instant, n: usize) {
        self.pending_changes += n;
        self.last_change = Some(now);
    }

    /// Whether records wait to be written.
    pub fn unsynced(&self) -> bool {
        self.state != State::Idle
            && (self.rewrite || self.held.is_some() || !self.pending.is_empty())
    }

    /// Whether a large document's journal still waits for its base hash.
    fn hash_running(&self) -> bool {
        !self.rewrite && self.held.as_ref().is_some_and(|h| !h.hash.is_ready())
    }

    /// Whether the sync policy asks for a flush now: [`IDLE`] since the last change, or [`SYNC_EVERY`]
    /// changes buffered.
    /// A journal whose base hash is still running is not due yet; a risky operation's
    /// [`Writer::flush`] waits for it.
    pub fn due(&self, now: Instant) -> bool {
        self.due_every(now, SYNC_EVERY)
    }

    /// [`Writer::due`] with `every` changes in place of [`SYNC_EVERY`] (Vim's `'updatecount'`, #619).
    pub fn due_every(&self, now: Instant, every: usize) -> bool {
        self.unsynced()
            && !self.hash_running()
            && (self.pending_changes >= every.max(1)
                || self
                    .last_change
                    .is_none_or(|t| now.saturating_duration_since(t) >= IDLE))
    }

    /// How long until [`Writer::due`] turns true with no further change; `None` when nothing waits.
    pub fn wake_in(&self, now: Instant) -> Option<Duration> {
        if !self.unsynced() {
            return None;
        }
        let wait = self.last_change.map_or(Duration::ZERO, |t| {
            IDLE.saturating_sub(now.saturating_duration_since(t))
        });
        Some(if self.hash_running() {
            wait.max(HASH_POLL)
        } else {
            wait
        })
    }

    /// Write the pending records and a Marker, then `fdatasync`. On failure the records stay pending and
    /// the next try waits for the next trigger.
    pub fn flush(&mut self, doc: &Document) -> io::Result<()> {
        if !self.unsynced() {
            return Ok(());
        }
        if self.state == State::Live
            && !self.large
            && self.since_checkpoint > COMPACT_AFTER
            && self.since_checkpoint > self.checkpoint_len
        {
            self.rewrite = true;
            self.pending = Vec::new();
        }
        let result = self.write_out(doc);
        match &result {
            Ok(()) => {
                self.pending = Vec::new();
                self.rewrite = false;
                self.state = State::Live;
                self.last_sync = Some(Instant::now());
                self.last_error = None;
            }
            Err(e) => {
                self.last_change = Some(Instant::now());
                self.failures += 1;
                self.last_error = Some(e.to_string());
            }
        }
        self.pending_changes = 0;
        result
    }

    fn write_out(&mut self, doc: &Document) -> io::Result<()> {
        if self.state == State::Live && !self.rewrite {
            if self.append()? {
                return Ok(());
            }
            self.rewrite = true; // the journal on disk is not the one this writer left
        }
        if self.state != State::Live {
            // No journal of this writer on disk yet: a session that started since may own the name now.
            self.journal = claim(&self.primary);
        }
        if let Some(dir) = self.journal.parent() {
            make_private_dir(dir)?;
        }
        if self.rewrite {
            let text = doc.bytes();
            let bytes = whole(
                Some(&self.doc),
                self.fmt,
                text,
                doc.revision().0,
                doc.history().current_seq().0,
            )?;
            write_private(&self.journal, &bytes)?;
            self.held = None;
            self.large = false;
            self.since_checkpoint = 0;
            self.checkpoint_len = text.len() as u64;
            self.on_disk = bytes.len() as u64;
            return Ok(());
        }
        if let Some(held) = self.held.as_mut() {
            // A large document's fresh journal: its Header and `DiskBase` first, then the records.
            let hash = held.hash.wait(&held.base);
            let base = Base::Disk {
                len: held.base.len() as u64,
                sha256: hash,
            };
            let mut out = Vec::with_capacity(self.pending.len() + 256);
            rj01::encode_header(&mut out, &header(Some(&self.doc), held.fmt, base))?;
            out.extend_from_slice(&self.pending);
            rj01::encode_marker(&mut out, now_ms())?;
            write_private(&self.journal, &out)?;
            self.held = None;
            self.on_disk = out.len() as u64;
            return Ok(());
        }
        // A fresh journal: the pending records start with its Header.
        let len = self.pending.len();
        rj01::encode_marker(&mut self.pending, now_ms())?;
        let written = write_private(&self.journal, &self.pending);
        if written.is_err() {
            self.pending.truncate(len); // keep the records, not the Marker, for the retry
        }
        written?;
        self.on_disk = self.pending.len() as u64;
        Ok(())
    }

    /// Append the pending records and a Marker to this writer's journal on disk. `Ok(false)` when the
    /// journal is not the length this writer left it (deleted, truncated, written by someone else): nothing
    /// is appended, and the caller rewrites it whole instead.
    fn append(&mut self) -> io::Result<bool> {
        let mut f = match open_regular(&self.journal, OpenOptions::new().append(true)) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        if f.metadata()?.len() != self.on_disk {
            return Ok(false);
        }
        let mut tail = Vec::new();
        rj01::encode_marker(&mut tail, now_ms())?;
        let written = f
            .write_all(&self.pending)
            .and_then(|()| f.write_all(&tail))
            .and_then(|()| f.sync_data());
        if let Err(e) = written {
            let _ = f.set_len(self.on_disk);
            return Err(e);
        }
        self.on_disk += (self.pending.len() + tail.len()) as u64;
        Ok(true)
    }

    /// Drop the journal (the buffer is clean, or its changes were let go) and start over.
    pub fn discard(&mut self) {
        if self.state != State::Idle {
            discard_file(&self.journal);
        }
        self.state = State::Idle;
        self.rewrite = false;
        self.held = None;
        self.pending = Vec::new();
        self.pending_changes = 0;
        self.last_change = None;
        self.on_disk = 0;
        self.since_checkpoint = 0;
        self.last_error = None;
    }
}

#[cfg(test)]
mod tests;
