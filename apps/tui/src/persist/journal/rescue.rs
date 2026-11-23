//! The crash-path rescue. Split from `journal.rs` (#1389).

use std::io::Write;

use super::{
    claim, io, make_private_dir, now_ms, open_regular, owner, rj01, whole, write_private,
    FileFormat, OpenOptions, Path, PathBuf, Read,
};

/// The crash-path write (panic hook, fatal signal, I/O error exit): put the exact current `text` in the
/// journal at `journal`, or, when a live other session owns that one, in this process's own beside it
/// ([`claim`], #1193); the path written. A journal of this process with an intact Header gets a Checkpoint
/// and a Marker appended after its last intact record (a torn tail is cut first: the failure being rescued
/// may have interrupted a write). Anything else (a crashed session's journal, kept until this session's
/// first sync, or a damaged one) is replaced whole and atomically, so the Header names who wrote the text
/// and a failure part-way leaves the old journal. Synced before it returns.
pub fn rescue(
    journal: &Path,
    canon: Option<&Path>,
    format: FileFormat,
    text: &[u8],
    revision: u64,
    node: u64,
) -> io::Result<PathBuf> {
    let journal = claim(journal);
    if let Some(dir) = journal.parent() {
        make_private_dir(dir)?;
    }
    let mut f = open_regular(
        &journal,
        OpenOptions::new().read(true).write(true).create(true),
    )?;
    let mut raw = Vec::new();
    f.read_to_end(&mut raw)?;
    let r = rj01::replay(&raw, None);
    let ours = r.header.as_ref().is_some_and(|h| h.owner == *owner());
    if !ours || r.unreplayable.is_some() {
        drop(f);
        write_private(&journal, &whole(canon, format, text, revision, node)?)?;
        return Ok(journal);
    }
    let mut out = Vec::with_capacity(text.len() + 256);
    rj01::encode_checkpoint(&mut out, node, revision, text)?;
    rj01::encode_marker(&mut out, now_ms())?;
    let at = r.last_good as u64;
    f.set_len(at)?;
    io::Seek::seek(&mut f, io::SeekFrom::Start(at))?;
    let written = f.write_all(&out).and_then(|()| f.sync_data());
    if written.is_err() {
        let _ = f.set_len(at);
    }
    written.map(|()| journal)
}
