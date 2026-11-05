//! The `RJ01` recovery-journal record format (RFC-0023 §2, D-064): encoding and replay, pure over bytes.
//!
//! A journal is a stream of self-describing records, each checksummed on its own so a torn tail is
//! detected and the intact prefix still replays:
//!
//! ```text
//! magic "RJ01" | schema_ver u16 | kind u8 | flags u8 | payload_len u32 | header_crc u32   (16 bytes)
//! payload (payload_len bytes)
//! payload_crc u32 | end_magic "rj01"                                                     (8 bytes)
//! ```
//!
//! All integers are little-endian. `header_crc` is CRC32C of the 12 bytes before it, `payload_crc` CRC32C of
//! the payload. `schema_ver` is `major << 8 | minor`: a newer minor only appends payload fields, which a
//! reader skips; a newer major stops replay at that record (INV-ADDITIVE). The first record is always a
//! [`Header`]; its base is either a [`Checkpoint`] record that follows, or a [`Base::Disk`] (the length and
//! SHA-256 of the disk text, for documents over [`LARGE_FILE`]). [`replay`] is the design doc's defensive
//! forward pass: it stops at the first bad record and never skips past one.
//!
//! Payloads (lengths are `u32` unless named `u64`):
//!
//! * Header: `pid u32`, `host bytes`, `start_ms u64`, `path bytes`, `format u8` ([`format_bits`]),
//!   `created_ms u64`, `base u8` (0 = a Checkpoint follows, 1 = disk: `len u64`, `sha256 [32]`).
//! * Checkpoint: `node u64`, `revision u64`, `text` (`u64` length + bytes).
//! * Edit: `seq u64`, `base_revision u64`, `result_revision u64`, `origin u8`, `group_hint u8`,
//!   `timestamp_ms u64`, `kind u8` (0 = fresh: `node u64`, `parent u64`; 1 = navigate: `to_node u64`), then
//!   the forward and the inverse edit lists, each `count u32` and per edit `pos u64`, `del u64`, `ins bytes`.
//! * Meta: `tag u8`: 0 = save point (`saved_node u64`, `len u64`, `sha256 [32]`), 1 = file format
//!   (`format u8`), 2 = rename (`path bytes`).
//! * Marker: `time_ms u64`, written after each fsync.

use std::io;

use ruse_core::change_log::{ChangeEntry, ChangeKind};
use ruse_core::edit::{Edit, EditList};
use ruse_core::transaction::{GroupHint, TransactionOrigin};
use ruse_core::BytePos;

use super::encoding::{Eol, FileFormat};

pub const MAGIC: [u8; 4] = *b"RJ01";
pub const END_MAGIC: [u8; 4] = *b"rj01";
pub const SCHEMA_MAJOR: u8 = 1;
pub const SCHEMA_MINOR: u8 = 0;
/// The `schema_ver` this build writes.
pub const SCHEMA: u16 = (SCHEMA_MAJOR as u16) << 8 | SCHEMA_MINOR as u16;
/// The fixed record header and trailer sizes.
pub const HEAD: usize = 16;
pub const TRAIL: usize = 8;
/// A document larger than this is journaled against its disk text ([`Base::Disk`]) instead of a full
/// Checkpoint (RFC-0023 §5, D-005's threshold).
pub const LARGE_FILE: usize = 32 << 20;

/// Record kinds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Kind {
    Header = 1,
    Checkpoint = 2,
    Edit = 3,
    Meta = 4,
    Marker = 5,
}

impl Kind {
    fn from_u8(b: u8) -> Option<Kind> {
        Some(match b {
            1 => Kind::Header,
            2 => Kind::Checkpoint,
            3 => Kind::Edit,
            4 => Kind::Meta,
            5 => Kind::Marker,
            _ => return None,
        })
    }
}

/// Who wrote a journal: enough to tell a live second editor from a crash (#1193, RFC-0023 §7).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Owner {
    pub pid: u32,
    pub host: Vec<u8>,
    /// The writing process's start, Unix milliseconds (as the process measured it at startup).
    pub start_ms: u64,
}

/// What the edit records of a journal apply to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Base {
    /// A Checkpoint record follows the Header.
    Checkpoint,
    /// The disk file's text, decoded with the Header's format: its length and SHA-256.
    Disk { len: u64, sha256: [u8; 32] },
}

/// The first record of every journal.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Header {
    pub owner: Owner,
    /// The document's canonical absolute path (raw OS bytes); empty for an unnamed buffer.
    pub path: Vec<u8>,
    pub format: FileFormat,
    pub created_ms: u64,
    pub base: Base,
}

/// A Meta record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Meta {
    SavePoint {
        saved_node: u64,
        len: u64,
        sha256: [u8; 32],
    },
    Format(FileFormat),
    Rename(Vec<u8>),
}

/// The edit-record metadata the writer adds to a drained [`ChangeEntry`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EditMeta {
    /// The writer's own record sequence number (authoritative order).
    pub seq: u64,
    /// Wall clock, Unix milliseconds: display only, never used for ordering.
    pub timestamp_ms: u64,
}

/// One decoded Edit record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EditRecord {
    pub meta: EditMeta,
    pub base_revision: u64,
    pub result_revision: u64,
    pub origin: TransactionOrigin,
    pub group_hint: GroupHint,
    pub kind: ChangeKind,
    pub forward: EditList,
    pub inverse: EditList,
}

/// CRC32C (Castagnoli), table computed once. Not cryptography: it detects torn and corrupt records.
pub fn crc32c(bytes: &[u8]) -> u32 {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (n, slot) in t.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0x82F6_3B78 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        t
    });
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc = table[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// The one-byte encoding of a [`FileFormat`]: bit 0 CRLF, bit 1 BOM, bit 2 final newline.
pub fn format_bits(f: FileFormat) -> u8 {
    u8::from(f.eol == Eol::Crlf) | u8::from(f.bom) << 1 | u8::from(f.final_newline) << 2
}

fn format_from_bits(b: u8) -> FileFormat {
    FileFormat {
        eol: if b & 1 != 0 { Eol::Crlf } else { Eol::Lf },
        bom: b & 2 != 0,
        final_newline: b & 4 != 0,
    }
}

fn origin_u8(o: TransactionOrigin) -> u8 {
    match o {
        TransactionOrigin::UserInput => 0,
        TransactionOrigin::Macro => 1,
        TransactionOrigin::Plugin => 2,
        TransactionOrigin::Lsp => 3,
        TransactionOrigin::AiAgent => 4,
        TransactionOrigin::RemotePeer => 5,
    }
}

fn origin_from(b: u8) -> Option<TransactionOrigin> {
    Some(match b {
        0 => TransactionOrigin::UserInput,
        1 => TransactionOrigin::Macro,
        2 => TransactionOrigin::Plugin,
        3 => TransactionOrigin::Lsp,
        4 => TransactionOrigin::AiAgent,
        5 => TransactionOrigin::RemotePeer,
        _ => return None,
    })
}

fn hint_u8(h: GroupHint) -> u8 {
    match h {
        GroupHint::Continue => 0,
        GroupHint::BreakBefore => 1,
        GroupHint::JoinPrev => 2,
    }
}

fn hint_from(b: u8) -> Option<GroupHint> {
    Some(match b {
        0 => GroupHint::Continue,
        1 => GroupHint::BreakBefore,
        2 => GroupHint::JoinPrev,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------------------
// Encoding

/// Appends one framed record to `out`: the payload is written in place by `body`, then the length and both
/// CRCs are filled in, so a large Checkpoint is copied once. A payload past `u32::MAX` bytes is refused and
/// `out` is left as it was (RFC-0023 §2).
fn frame(out: &mut Vec<u8>, kind: Kind, body: impl FnOnce(&mut Vec<u8>)) -> io::Result<()> {
    let start = out.len();
    out.extend_from_slice(&[0u8; HEAD]);
    body(out);
    let Ok(len) = u32::try_from(out.len() - start - HEAD) else {
        let len = out.len() - start - HEAD;
        out.truncate(start);
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("a journal record of {len} bytes is too large (4 GiB at most)"),
        ));
    };
    let h = &mut out[start..start + HEAD];
    h[..4].copy_from_slice(&MAGIC);
    h[4..6].copy_from_slice(&SCHEMA.to_le_bytes());
    h[6] = kind as u8;
    h[7] = 0;
    h[8..12].copy_from_slice(&len.to_le_bytes());
    let hcrc = crc32c(&h[..12]);
    h[12..16].copy_from_slice(&hcrc.to_le_bytes());
    let pcrc = crc32c(&out[start + HEAD..]);
    out.extend_from_slice(&pcrc.to_le_bytes());
    out.extend_from_slice(&END_MAGIC);
    Ok(())
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// A `u32`-length-prefixed byte string. The frame's own bound refuses a record this would overflow.
fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_u32(out, u32::try_from(b.len()).unwrap_or(u32::MAX));
    out.extend_from_slice(b);
}

fn put_list(out: &mut Vec<u8>, list: &EditList) {
    put_u32(out, u32::try_from(list.edits().len()).unwrap_or(u32::MAX));
    for e in list.edits() {
        put_u64(out, e.pos.0 as u64);
        put_u64(out, e.del as u64);
        put_bytes(out, &e.ins);
    }
}

pub fn encode_header(out: &mut Vec<u8>, h: &Header) -> io::Result<()> {
    frame(out, Kind::Header, |o| {
        put_u32(o, h.owner.pid);
        put_bytes(o, &h.owner.host);
        put_u64(o, h.owner.start_ms);
        put_bytes(o, &h.path);
        o.push(format_bits(h.format));
        put_u64(o, h.created_ms);
        match h.base {
            Base::Checkpoint => o.push(0),
            Base::Disk { len, sha256 } => {
                o.push(1);
                put_u64(o, len);
                o.extend_from_slice(&sha256);
            }
        }
    })
}

pub fn encode_checkpoint(
    out: &mut Vec<u8>,
    node: u64,
    revision: u64,
    text: &[u8],
) -> io::Result<()> {
    frame(out, Kind::Checkpoint, |o| {
        put_u64(o, node);
        put_u64(o, revision);
        put_u64(o, text.len() as u64);
        o.extend_from_slice(text);
    })
}

/// One drained change as an Edit record.
pub fn encode_edit(out: &mut Vec<u8>, meta: EditMeta, e: &ChangeEntry) -> io::Result<()> {
    frame(out, Kind::Edit, |o| {
        put_u64(o, meta.seq);
        put_u64(o, e.base_revision.0);
        put_u64(o, e.result_revision.0);
        o.push(origin_u8(e.origin));
        o.push(hint_u8(e.group_hint));
        put_u64(o, meta.timestamp_ms);
        match e.kind {
            ChangeKind::Fresh { node, parent } => {
                o.push(0);
                put_u64(o, node.0);
                put_u64(o, parent.0);
            }
            ChangeKind::Navigate { to_node } => {
                o.push(1);
                put_u64(o, to_node.0);
            }
        }
        put_list(o, &e.forward);
        put_list(o, &e.inverse);
    })
}

pub fn encode_meta(out: &mut Vec<u8>, m: &Meta) -> io::Result<()> {
    frame(out, Kind::Meta, |o| match m {
        Meta::SavePoint {
            saved_node,
            len,
            sha256,
        } => {
            o.push(0);
            put_u64(o, *saved_node);
            put_u64(o, *len);
            o.extend_from_slice(sha256);
        }
        Meta::Format(f) => {
            o.push(1);
            o.push(format_bits(*f));
        }
        Meta::Rename(p) => {
            o.push(2);
            put_bytes(o, p);
        }
    })
}

pub fn encode_marker(out: &mut Vec<u8>, time_ms: u64) -> io::Result<()> {
    frame(out, Kind::Marker, |o| put_u64(o, time_ms))
}

// ---------------------------------------------------------------------------------------------------------
// Decoding

fn le32(b: &[u8]) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[..4]);
    u32::from_le_bytes(a)
}

fn le64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

/// A bounds-checked cursor over one payload: every read checks the remaining bytes first, so a crafted
/// length never allocates or reads past the record (RFC-0023 Security impact).
struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.i.checked_add(n)?;
        let s = self.b.get(self.i..end)?;
        self.i = end;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(le32)
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(le64)
    }
    fn usize64(&mut self) -> Option<usize> {
        usize::try_from(self.u64()?).ok()
    }
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn sha(&mut self) -> Option<[u8; 32]> {
        self.take(32).map(|s| {
            let mut a = [0u8; 32];
            a.copy_from_slice(s);
            a
        })
    }
    fn rest(&self) -> usize {
        self.b.len() - self.i
    }
    fn list(&mut self) -> Option<EditList> {
        let n = self.u32()? as usize;
        // Each edit takes at least 20 bytes (two u64 and a u32 length), so a count the payload cannot hold
        // is refused before anything is reserved.
        if n > self.rest() / 20 {
            return None;
        }
        let mut edits = Vec::with_capacity(n);
        for _ in 0..n {
            let pos = self.usize64()?;
            let del = self.usize64()?;
            let ins = self.bytes()?.to_vec();
            edits.push(Edit::replace(BytePos(pos), del, ins));
        }
        EditList::new(edits).ok()
    }
}

fn decode_header(c: &mut Cursor) -> Option<Header> {
    let pid = c.u32()?;
    let host = c.bytes()?.to_vec();
    let start_ms = c.u64()?;
    let path = c.bytes()?.to_vec();
    let format = format_from_bits(c.u8()?);
    let created_ms = c.u64()?;
    let base = match c.u8()? {
        0 => Base::Checkpoint,
        1 => Base::Disk {
            len: c.u64()?,
            sha256: c.sha()?,
        },
        _ => return None,
    };
    Some(Header {
        owner: Owner {
            pid,
            host,
            start_ms,
        },
        path,
        format,
        created_ms,
        base,
    })
}

fn decode_edit(c: &mut Cursor) -> Option<EditRecord> {
    let seq = c.u64()?;
    let base_revision = c.u64()?;
    let result_revision = c.u64()?;
    let origin = origin_from(c.u8()?)?;
    let group_hint = hint_from(c.u8()?)?;
    let timestamp_ms = c.u64()?;
    use ruse_core::undo::MonotonicSeq;
    let kind = match c.u8()? {
        0 => ChangeKind::Fresh {
            node: MonotonicSeq(c.u64()?),
            parent: MonotonicSeq(c.u64()?),
        },
        1 => ChangeKind::Navigate {
            to_node: MonotonicSeq(c.u64()?),
        },
        _ => return None,
    };
    let forward = c.list()?;
    let inverse = c.list()?;
    Some(EditRecord {
        meta: EditMeta { seq, timestamp_ms },
        base_revision,
        result_revision,
        origin,
        group_hint,
        kind,
        forward,
        inverse,
    })
}

fn decode_meta(c: &mut Cursor) -> Option<Meta> {
    Some(match c.u8()? {
        0 => Meta::SavePoint {
            saved_node: c.u64()?,
            len: c.u64()?,
            sha256: c.sha()?,
        },
        1 => Meta::Format(format_from_bits(c.u8()?)),
        2 => Meta::Rename(c.bytes()?.to_vec()),
        _ => return None,
    })
}

/// A decoded record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Record {
    Header(Header),
    Checkpoint {
        node: u64,
        revision: u64,
        text: Vec<u8>,
    },
    Edit(EditRecord),
    Meta(Meta),
    Marker {
        time_ms: u64,
    },
    /// A record of a newer minor version whose kind this build does not know: skipped.
    Skipped,
}

/// Why reading a record at some offset stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stop {
    /// Fewer bytes than a header, a payload or a trailer: a torn write.
    Torn,
    /// A magic, CRC, end magic or payload that does not decode: corruption.
    Corrupt,
    /// A record of a newer major schema version: not readable by this build.
    NewerMajor(u8),
}

/// Decode the record at `raw[at..]`, returning it and the offset after it.
pub fn read_record(raw: &[u8], at: usize) -> Result<(Record, usize), Stop> {
    let rest = &raw[at.min(raw.len())..];
    if rest.len() < HEAD {
        return Err(Stop::Torn);
    }
    let h = &rest[..HEAD];
    if h[..4] != MAGIC {
        return Err(Stop::Corrupt);
    }
    if crc32c(&h[..12]) != le32(&h[12..16]) {
        return Err(Stop::Corrupt);
    }
    let ver = u16::from_le_bytes([h[4], h[5]]);
    let (major, minor) = ((ver >> 8) as u8, ver as u8);
    if major != SCHEMA_MAJOR {
        return Err(Stop::NewerMajor(major));
    }
    let len = le32(&h[8..12]) as usize;
    let Some(end) = HEAD.checked_add(len).and_then(|e| e.checked_add(TRAIL)) else {
        return Err(Stop::Torn);
    };
    if rest.len() < end {
        return Err(Stop::Torn);
    }
    let payload = &rest[HEAD..HEAD + len];
    let trail = &rest[HEAD + len..end];
    if crc32c(payload) != le32(&trail[..4]) || trail[4..] != END_MAGIC {
        return Err(Stop::Corrupt);
    }
    let newer_minor = minor > SCHEMA_MINOR;
    let mut c = Cursor { b: payload, i: 0 };
    let rec = match Kind::from_u8(h[6]) {
        None if newer_minor => Some(Record::Skipped),
        None => None,
        Some(Kind::Header) => decode_header(&mut c).map(Record::Header),
        Some(Kind::Checkpoint) => (|| {
            let node = c.u64()?;
            let revision = c.u64()?;
            let n = c.usize64()?;
            Some(Record::Checkpoint {
                node,
                revision,
                text: c.take(n)?.to_vec(),
            })
        })(),
        Some(Kind::Edit) => decode_edit(&mut c).map(Record::Edit),
        Some(Kind::Meta) => decode_meta(&mut c).map(Record::Meta),
        Some(Kind::Marker) => c.u64().map(|time_ms| Record::Marker { time_ms }),
    };
    match rec {
        // Trailing payload is a newer minor's appended fields, skipped; in this minor it is corruption.
        Some(r) if c.rest() == 0 || newer_minor || matches!(r, Record::Skipped) => {
            Ok((r, at + end))
        }
        _ => Err(Stop::Corrupt),
    }
}

// ---------------------------------------------------------------------------------------------------------
// Replay

/// Why a journal's text cannot be rebuilt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unreplayable {
    /// The journal was written by a newer major schema version (it is kept for that version).
    NewerMajor(u8),
    /// A [`Base::Disk`] journal whose disk text is not the one it began from: `changes` edits cannot be
    /// replayed onto other text (RFC-0023 §5).
    DiskChanged { changes: usize },
}

/// The result of [`replay`].
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Replay {
    /// The journal's Header, when its first record is an intact one.
    pub header: Option<Header>,
    /// The rebuilt text at the last intact record; `None` when there is none to give.
    pub text: Option<Vec<u8>>,
    /// Set when the text cannot be rebuilt.
    pub unreplayable: Option<Unreplayable>,
    /// Edit records since the base (the last Checkpoint, or the disk text).
    pub changes: usize,
    /// The timestamp of the last replayed edit, Unix milliseconds.
    pub last_change_ms: Option<u64>,
    /// The time of the last Marker (fsync point): what the journal is "recovered up to".
    pub last_marker_ms: Option<u64>,
    /// Bytes followed the last intact record: a torn or corrupt tail was dropped.
    pub lost_tail: bool,
    /// The offset after the last intact record: where the next append must go (the safe truncation point).
    pub last_good: usize,
    /// The latest file format (the Header's, or a later Meta's).
    pub format: Option<FileFormat>,
    /// The latest save point recorded.
    pub saved: Option<Meta>,
}

/// The replay's text: a gap buffer, so an edit costs the distance from the previous one rather than a
/// copy of the whole text. Typing edits one place after another, so a long session replays in one pass.
struct Gap {
    buf: Vec<u8>,
    /// The gap is `buf[start..end]`.
    start: usize,
    end: usize,
}

impl Gap {
    fn new(text: Vec<u8>) -> Gap {
        let n = text.len();
        Gap {
            buf: text,
            start: n,
            end: n,
        }
    }

    fn len(&self) -> usize {
        self.buf.len() - (self.end - self.start)
    }

    /// Put the gap at text offset `at`.
    fn move_to(&mut self, at: usize) {
        if at < self.start {
            let n = self.start - at;
            self.buf.copy_within(at..self.start, self.end - n);
            self.start = at;
            self.end -= n;
        } else if at > self.start {
            let n = at - self.start;
            self.buf.copy_within(self.end..self.end + n, self.start);
            self.start += n;
            self.end += n;
        }
    }

    /// Make the gap at least `n` bytes wide (doubling the buffer, so growth is amortized).
    fn reserve(&mut self, n: usize) {
        let gap = self.end - self.start;
        if gap >= n {
            return;
        }
        let grow = (n - gap).max(self.buf.len()).max(64);
        let tail = self.buf.len() - self.end;
        self.buf.resize(self.buf.len() + grow, 0);
        let len = self.buf.len();
        self.buf.copy_within(self.end..self.end + tail, len - tail);
        self.end += grow;
    }

    /// Apply a checked, ascending edit list, last edit first so the earlier positions stay valid.
    fn apply(&mut self, list: &EditList) {
        for e in list.edits().iter().rev() {
            self.move_to(e.pos.0 + e.del);
            self.start -= e.del;
            self.reserve(e.ins.len());
            self.buf[self.start..self.start + e.ins.len()].copy_from_slice(&e.ins);
            self.start += e.ins.len();
        }
    }

    fn into_vec(mut self) -> Vec<u8> {
        self.buf.drain(self.start..self.end);
        self.buf
    }
}

/// SHA-256 of `bytes`.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

/// Replay a raw journal forward to its last intact record (design doc §2). `disk` is the document's disk
/// text decoded with the detected format; it is needed only by a [`Base::Disk`] journal, whose length and
/// hash it must match.
pub fn replay(raw: &[u8], disk: Option<&[u8]>) -> Replay {
    let mut r = Replay::default();
    let mut at = 0;
    let mut text: Option<Gap> = None;
    loop {
        if at >= raw.len() {
            break;
        }
        let (rec, next) = match read_record(raw, at) {
            Ok(x) => x,
            Err(Stop::NewerMajor(v)) if r.header.is_none() => {
                r.unreplayable = Some(Unreplayable::NewerMajor(v));
                break;
            }
            Err(_) => {
                r.lost_tail = true;
                break;
            }
        };
        let ok = match rec {
            Record::Header(h) if r.header.is_none() => {
                r.format = Some(h.format);
                if let Base::Disk { len, sha256: want } = h.base {
                    match disk {
                        Some(d) if d.len() as u64 == len && sha256(d) == want => {
                            text = Some(Gap::new(d.to_vec()));
                        }
                        _ => r.unreplayable = Some(Unreplayable::DiskChanged { changes: 0 }),
                    }
                }
                r.header = Some(h);
                true
            }
            // Anything before the Header, or a second Header, is not a journal this build wrote.
            _ if r.header.is_none() => false,
            Record::Header(_) => false,
            Record::Checkpoint { text: t, .. } => {
                text = Some(Gap::new(t));
                r.unreplayable = None;
                r.changes = 0;
                true
            }
            Record::Edit(e) => {
                r.changes += 1;
                r.last_change_ms = Some(e.meta.timestamp_ms);
                if let Some(Unreplayable::DiskChanged { changes }) = &mut r.unreplayable {
                    *changes += 1;
                    true
                } else if let Some(t) = text.as_mut() {
                    // An edit that does not fit the text is corruption, not something to apply.
                    if e.forward.check_bounds(t.len()).is_ok() {
                        t.apply(&e.forward);
                        true
                    } else {
                        false
                    }
                } else {
                    false // an edit with no base before it
                }
            }
            Record::Meta(m) => {
                match &m {
                    Meta::Format(f) => r.format = Some(*f),
                    Meta::SavePoint { .. } => r.saved = Some(m.clone()),
                    Meta::Rename(p) => {
                        if let Some(h) = r.header.as_mut() {
                            h.path.clone_from(p);
                        }
                    }
                }
                true
            }
            Record::Marker { time_ms } => {
                r.last_marker_ms = Some(time_ms);
                true
            }
            Record::Skipped => true,
        };
        if !ok {
            r.lost_tail = true;
            break;
        }
        at = next;
        r.last_good = at;
    }
    if r.unreplayable.is_none() {
        r.text = text.map(Gap::into_vec);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruse_core::pos::Revision;
    use ruse_core::undo::MonotonicSeq;

    pub(crate) fn header(base: Base) -> Header {
        Header {
            owner: Owner {
                pid: 42,
                host: b"host".to_vec(),
                start_ms: 7,
            },
            path: b"/tmp/f.txt".to_vec(),
            format: FileFormat::default(),
            created_ms: 9,
            base,
        }
    }

    fn entry(base: u64, forward: Vec<Edit>, before: &[u8]) -> ChangeEntry {
        let forward = EditList::new(forward).unwrap();
        let inverse = forward.inverse(before);
        ChangeEntry {
            base_revision: Revision(base),
            result_revision: Revision(base + 1),
            origin: TransactionOrigin::UserInput,
            group_hint: GroupHint::Continue,
            forward,
            inverse,
            kind: ChangeKind::Fresh {
                node: MonotonicSeq(base + 1),
                parent: MonotonicSeq(base),
            },
        }
    }

    /// Header + Checkpoint "abc\n" + two edits + a Marker; returns the journal and the record offsets.
    fn sample() -> (Vec<u8>, Vec<usize>) {
        let mut j = Vec::new();
        let mut ends = Vec::new();
        encode_header(&mut j, &header(Base::Checkpoint)).unwrap();
        ends.push(j.len());
        encode_checkpoint(&mut j, 0, 1, b"abc\n").unwrap();
        ends.push(j.len());
        let meta = |seq| EditMeta {
            seq,
            timestamp_ms: 1000 + seq,
        };
        encode_edit(
            &mut j,
            meta(1),
            &entry(1, vec![Edit::insert(BytePos(0), "x")], b"abc\n"),
        )
        .unwrap();
        ends.push(j.len());
        encode_edit(
            &mut j,
            meta(2),
            &entry(2, vec![Edit::delete(BytePos(3), 1)], b"xabc\n"),
        )
        .unwrap();
        ends.push(j.len());
        encode_marker(&mut j, 5000).unwrap();
        ends.push(j.len());
        (j, ends)
    }

    /// The gap buffer applies edit lists exactly as `EditList::apply_to` does.
    #[test]
    fn the_gap_buffer_applies_like_apply_to() {
        let mut want = b"0123456789".to_vec();
        let mut g = Gap::new(want.clone());
        let lists = [
            vec![
                Edit::insert(BytePos(10), "end"),
                Edit::insert(BytePos(0), "start"),
            ],
            vec![
                Edit::replace(BytePos(3), 4, "MID"),
                Edit::delete(BytePos(12), 2),
            ],
            vec![Edit::insert(BytePos(1), "x".repeat(300))],
            vec![Edit::delete(BytePos(0), 5)],
        ];
        for l in lists {
            let l = EditList::new(l).unwrap();
            assert!(l.check_bounds(g.len()).is_ok());
            want = l.apply_to(&want);
            g.apply(&l);
            assert_eq!(g.len(), want.len());
        }
        assert_eq!(g.into_vec(), want);
    }

    #[test]
    fn crc32c_matches_the_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn records_round_trip_and_replay() {
        let (j, ends) = sample();
        let r = replay(&j, None);
        assert_eq!(r.text.as_deref(), Some(&b"xab\n"[..]));
        assert_eq!(r.changes, 2);
        assert_eq!(r.last_marker_ms, Some(5000));
        assert_eq!(r.last_change_ms, Some(1002));
        assert!(!r.lost_tail);
        assert_eq!(r.last_good, j.len());
        assert_eq!(r.header, Some(header(Base::Checkpoint)));
        // Every record decodes to what was written; the edit keeps its inverse.
        let (rec, _) = read_record(&j, ends[1]).unwrap();
        let Record::Edit(e) = rec else {
            panic!("{rec:?}")
        };
        assert_eq!(e.inverse.apply_to(&e.forward.apply_to(b"abc\n")), b"abc\n");
        assert_eq!(e.meta.seq, 1);
        assert_eq!(e.origin, TransactionOrigin::UserInput);
    }

    #[test]
    fn meta_records_round_trip() {
        let mut j = Vec::new();
        encode_header(&mut j, &header(Base::Checkpoint)).unwrap();
        encode_checkpoint(&mut j, 0, 1, b"t").unwrap();
        let crlf = FileFormat {
            eol: Eol::Crlf,
            bom: true,
            final_newline: false,
        };
        let save = Meta::SavePoint {
            saved_node: 3,
            len: 1,
            sha256: sha256(b"t"),
        };
        for m in [
            save.clone(),
            Meta::Format(crlf),
            Meta::Rename(b"/new".to_vec()),
        ] {
            let at = j.len();
            encode_meta(&mut j, &m).unwrap();
            assert_eq!(read_record(&j, at).unwrap().0, Record::Meta(m));
        }
        let r = replay(&j, None);
        assert_eq!(r.format, Some(crlf));
        assert_eq!(r.saved, Some(save));
        assert_eq!(r.header.unwrap().path, b"/new");
        assert_eq!(r.text.as_deref(), Some(&b"t"[..]));
    }

    /// The torn-write guarantee at every byte: cutting the last record anywhere replays the record before.
    #[test]
    fn a_torn_last_record_at_every_offset_replays_the_prefix() {
        let (j, ends) = sample();
        let (before, last) = (ends[2], ends[3]); // the second edit
        let mut j = j[..last].to_vec();
        for cut in before..last {
            let r = replay(&j[..cut], None);
            assert_eq!(r.text.as_deref(), Some(&b"xabc\n"[..]), "cut at {cut}");
            assert_eq!(r.last_good, before, "cut at {cut}");
            assert_eq!(r.lost_tail, cut > before, "cut at {cut}");
        }
        j.truncate(last);
        assert_eq!(replay(&j, None).text.as_deref(), Some(&b"xab\n"[..]));
    }

    #[test]
    fn a_corrupt_middle_record_stops_replay_there() {
        let (mut j, ends) = sample();
        j[ends[1] + HEAD + 3] ^= 0x40; // inside the first edit's payload
        let r = replay(&j, None);
        assert_eq!(r.text.as_deref(), Some(&b"abc\n"[..]));
        assert!(r.lost_tail);
        assert_eq!(r.last_good, ends[1]);
        assert_eq!(
            r.last_marker_ms, None,
            "nothing after the bad record is read"
        );
    }

    fn set_version(j: &mut [u8], at: usize, ver: u16) {
        j[at + 4..at + 6].copy_from_slice(&ver.to_le_bytes());
        let crc = crc32c(&j[at..at + 12]);
        j[at + 12..at + 16].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn an_unknown_major_version_stops_replay() {
        let (mut j, ends) = sample();
        set_version(&mut j, 0, 2 << 8);
        let r = replay(&j, None);
        assert_eq!(r.unreplayable, Some(Unreplayable::NewerMajor(2)));
        assert_eq!(r.text, None);
        // Mid-stream, replay stops at that record and keeps the prefix.
        let (mut j, _) = sample();
        set_version(&mut j, ends[2], 2 << 8);
        let r = replay(&j, None);
        assert_eq!(r.text.as_deref(), Some(&b"xabc\n"[..]));
        assert!(r.lost_tail);
    }

    /// A newer minor version appends payload fields; this reader skips them, and skips a kind it does not
    /// know. In the current minor, trailing payload is corruption.
    #[test]
    fn a_newer_minor_version_with_extra_payload_is_read() {
        let mut j = Vec::new();
        encode_header(&mut j, &header(Base::Checkpoint)).unwrap();
        encode_checkpoint(&mut j, 0, 1, b"base").unwrap();
        let mark = j.len();
        // A Marker with 4 extra payload bytes, then a record of an unknown kind 9.
        frame(&mut j, Kind::Marker, |o| {
            put_u64(o, 77);
            o.extend_from_slice(b"more");
        })
        .unwrap();
        let unknown = j.len();
        frame(&mut j, Kind::Marker, |o| o.extend_from_slice(b"new kind")).unwrap();
        j[unknown + 6] = 9;
        set_version(&mut j, unknown, SCHEMA + 1); // header CRC recomputed over the new kind too
        let current = replay(&j, None);
        assert!(current.lost_tail, "extra payload in this minor is corrupt");
        assert_eq!(current.last_good, mark);
        set_version(&mut j, mark, SCHEMA + 1);
        let r = replay(&j, None);
        assert!(!r.lost_tail, "{r:?}");
        assert_eq!(r.last_marker_ms, Some(77));
        assert_eq!(r.last_good, j.len());
        assert_eq!(r.text.as_deref(), Some(&b"base"[..]));
    }

    #[test]
    fn a_disk_base_replays_onto_the_matching_disk_text_only() {
        let disk = b"large disk text\n";
        let mut j = Vec::new();
        encode_header(
            &mut j,
            &header(Base::Disk {
                len: disk.len() as u64,
                sha256: sha256(disk),
            }),
        )
        .unwrap();
        let e = entry(1, vec![Edit::insert(BytePos(0), "+")], disk);
        encode_edit(
            &mut j,
            EditMeta {
                seq: 1,
                timestamp_ms: 1,
            },
            &e,
        )
        .unwrap();
        let r = replay(&j, Some(disk));
        assert_eq!(r.text.as_deref(), Some(&b"+large disk text\n"[..]));
        // A changed disk file refuses: no text, and the edit count to report.
        let r = replay(&j, Some(b"large disk TEXT\n"));
        assert_eq!(r.text, None);
        assert_eq!(
            r.unreplayable,
            Some(Unreplayable::DiskChanged { changes: 1 })
        );
        assert_eq!(replay(&j, None).text, None);
    }

    #[test]
    fn junk_and_empty_journals_give_nothing() {
        assert_eq!(replay(b"", None).text, None);
        assert!(!replay(b"", None).lost_tail);
        let r = replay(&[0u8; 64], None);
        assert_eq!((r.text, r.lost_tail), (None, true));
        // An edit with no base, or a record before the Header, is not replayed.
        let (j, ends) = sample();
        assert_eq!(replay(&j[ends[0]..], None).text, None);
    }

    /// A crafted length never allocates: a Checkpoint whose text length exceeds its payload is corrupt, and
    /// a header claiming a payload larger than the file is a torn tail.
    #[test]
    fn crafted_lengths_are_refused_before_allocating() {
        let mut j = Vec::new();
        encode_header(&mut j, &header(Base::Checkpoint)).unwrap();
        let at = j.len();
        encode_checkpoint(&mut j, 0, 0, b"x").unwrap();
        // Claim an 8 EiB checkpoint text inside an intact record: the length check refuses it.
        let text_len_at = at + HEAD + 16;
        j[text_len_at..text_len_at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        let pl = u32::from_le_bytes(j[at + 8..at + 12].try_into().unwrap()) as usize;
        let crc = crc32c(&j[at + HEAD..at + HEAD + pl]);
        j[at + HEAD + pl..at + HEAD + pl + 4].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(read_record(&j, at).unwrap_err(), Stop::Corrupt);
        let mut h = j[..HEAD].to_vec();
        h[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        let crc = crc32c(&h[..12]);
        h[12..16].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(read_record(&h, 0).unwrap_err(), Stop::Torn);
    }

    #[test]
    fn an_edit_count_the_payload_cannot_hold_is_refused() {
        let mut j = Vec::new();
        frame(&mut j, Kind::Edit, |o| {
            for _ in 0..3 {
                put_u64(o, 1);
            }
            o.extend_from_slice(&[0, 0]);
            put_u64(o, 1);
            o.push(1);
            put_u64(o, 1);
            put_u32(o, u32::MAX); // four billion edits in a few bytes
        })
        .unwrap();
        assert_eq!(read_record(&j, 0).unwrap_err(), Stop::Corrupt);
    }

    mod fuzz {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(crate::test_cases(256))]

            /// Arbitrary bytes, and any single flipped byte of a real journal, never panic replay, and a
            /// flipped journal replays one of the texts the journal passed through, or none.
            #[test]
            fn replay_never_panics_and_never_invents_text(
                raw in proptest::collection::vec(any::<u8>(), 0..256),
                at in any::<proptest::sample::Index>(),
                xor in 1u8..=255,
            ) {
                let _ = replay(&raw, Some(&raw));
                let mut j = MAGIC.to_vec();
                j.extend_from_slice(&raw);
                let _ = replay(&j, None);
                let (mut j, _) = sample();
                let i = at.index(j.len());
                j[i] ^= xor;
                let got = replay(&j, None).text;
                let ok = [None, Some(&b"abc\n"[..]), Some(&b"xabc\n"[..]), Some(&b"xab\n"[..])];
                prop_assert!(ok.contains(&got.as_deref()), "{:?}", got);
            }
        }
    }
}
