//! The open-time recovery offer and its adoption. Split from `journal.rs` (#1389).

use super::{
    alive, canonical, claim, fs, io, is_live_other, is_stale, owner, path_bytes, read_journal,
    read_owner, rescue, rj01, rjs0, unreadable_note, Available, FileFormat, Journals, Path,
    PathBuf, Recovery, SystemTime, UNIX_EPOCH,
};

impl Journals {
    /// The newest unnamed-buffer journal left by a ruse that is no longer running, for a pathless start.
    fn orphaned_unnamed(&self) -> Option<PathBuf> {
        let me = owner().pid;
        fs::read_dir(&self.dir)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let pid: u32 = name
                    .strip_prefix("unnamed.")?
                    .split('.')
                    .next()?
                    .parse()
                    .ok()?;
                let mtime = e.metadata().ok()?.modified().ok()?;
                let gone = read_owner(&e.path())
                    .map_or(!alive(pid), |o| o != *owner() && !is_live_other(&o));
                (name.ends_with(".rj") && pid != me && gone).then(|| (mtime, e.path()))
            })
            .max()
            .map(|(_, p)| p)
    }

    /// The journals of `doc`: its own name first, then every `<name>.<pid>.rj` a second session wrote
    /// beside it ([`claim`]), newest first.
    pub fn candidates(&self, doc: &Path) -> Vec<PathBuf> {
        let primary = self.path_for(doc);
        let mut out = vec![primary.clone()];
        let Some(stem) = primary
            .file_name()
            .map(|n| path_bytes(Path::new(n)))
            .and_then(|n| n.strip_suffix(b".rj").map(<[u8]>::to_vec))
        else {
            return out;
        };
        let mut alts: Vec<(SystemTime, PathBuf)> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = path_bytes(Path::new(&e.file_name()));
                let pid = name.strip_prefix(stem.as_slice())?.strip_prefix(b".")?;
                let pid = pid.strip_suffix(b".rj")?;
                (!pid.is_empty() && pid.iter().all(u8::is_ascii_digit)).then_some(())?;
                let mtime = e.metadata().ok()?.modified().ok()?;
                Some((mtime, e.path()))
            })
            .collect();
        alts.sort_by_key(|a| std::cmp::Reverse(a.0));
        out.extend(alts.into_iter().map(|(_, p)| p));
        out
    }

    /// What the journals of `doc` (or, for `None`, an orphaned unnamed buffer's) offer at open time, given
    /// the buffer's `disk` text (the file decoded with its detected format; empty for no file). A journal
    /// whose owner is still running on this host makes the file [`Recovery::InUse`]; a crashed one beside
    /// it is offered with it (#1210). A journal replaying to the disk text offers nothing; a journal that
    /// cannot be used says why.
    pub fn offer(&self, doc: Option<&Path>, disk: &[u8]) -> Recovery {
        let journals = match doc {
            Some(d) => self.candidates(d),
            None => self.orphaned_unnamed().into_iter().collect(),
        };
        // A journal to recover wins over one that only gives a note (a newer ruse's beside this one's).
        let (mut available, mut note, mut in_use) = (None, None, None);
        for journal in journals {
            if let Some(o) = read_owner(&journal).filter(is_live_other) {
                in_use.get_or_insert((o, journal));
                continue;
            }
            if available.is_some() {
                continue;
            }
            match self.offer_one(doc, &journal, disk) {
                Recovery::None => {}
                r @ Recovery::Available(_) => available = Some(r),
                r => {
                    note.get_or_insert(r);
                }
            }
        }
        if let Some((owner, journal)) = in_use {
            return Recovery::InUse {
                owner,
                journal,
                crashed: match available {
                    Some(Recovery::Available(a)) => Some(a),
                    _ => None,
                },
                note: match note {
                    Some(Recovery::Unreplayable { reason }) => Some(reason),
                    _ => None,
                },
            };
        }
        available
            .or(note)
            .unwrap_or_else(|| self.offer_legacy(doc, disk))
    }

    /// The offer of the one journal at `journal` (its owner is not a live other session).
    fn offer_one(&self, doc: Option<&Path>, journal: &Path, disk: &[u8]) -> Recovery {
        let raw = match read_journal(journal) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Recovery::None,
            Err(e) => {
                return Recovery::Unreplayable {
                    reason: unreadable_note(journal, &e),
                }
            }
        };
        let r = rj01::replay(&raw, Some(disk));
        match r.unreplayable {
            Some(rj01::Unreplayable::NewerMajor(v)) => {
                return Recovery::Unreplayable {
                    reason: format!(
                    "recovery journal {} is from a newer ruse (format {v}.x); kept, not offered",
                    journal.display()
                ),
                }
            }
            Some(rj01::Unreplayable::DiskChanged { changes }) => {
                let kept = self.archive(journal).map_or_else(
                    |e| format!("not archived: {e}"),
                    |p| format!("archived as {}", p.display()),
                );
                return Recovery::Unreplayable {
                    reason: format!(
                        "the file changed on disk since the journal began; the {changes} changes \
                         cannot be replayed (journal {kept})"
                    ),
                };
            }
            None => {}
        }
        // A journal an older build wrote may hold a last line without its newline; the buffer text is
        // line-terminated (RFC-0024 §2), as `disk` already is.
        match r.text.map(crate::persist::encoding::terminated) {
            Some(text) if text != disk => Recovery::Available(Box::new(Available {
                recovered: text,
                base: r.header.as_ref().map(|h| h.base),
                changes: Some(r.changes),
                last_change: r.last_change_ms,
                up_to: r.last_marker_ms,
                lost_tail: r.lost_tail,
                stale: doc.and_then(|d| stale_since(d, journal)),
                owner: r.header.map(|h| h.owner),
                journal: journal.to_path_buf(),
                legacy: false,
                format: r.format,
            })),
            _ => Recovery::None,
        }
    }

    /// A sidecar from an older ruse.
    fn offer_legacy(&self, doc: Option<&Path>, disk: &[u8]) -> Recovery {
        let sidecar = rjs0::sidecar_path(doc);
        match read_journal(&sidecar) {
            Ok(raw) => {
                match rjs0::last_snapshot(&raw)
                    .map(|t| crate::persist::encoding::terminated(t.to_vec()))
                {
                    Some(text) if text != disk => Recovery::Available(Box::new(Available {
                        recovered: text,
                        base: None,
                        changes: None,
                        last_change: None,
                        up_to: None,
                        lost_tail: false,
                        stale: doc.and_then(|d| stale_since(d, &sidecar)),
                        owner: None,
                        journal: sidecar,
                        legacy: true,
                        format: None,
                    })),
                    // Nothing in it to recover: archive it so it stops sitting in the project tree.
                    Some(_) => {
                        let _ = self.archive(&sidecar);
                        Recovery::None
                    }
                    None => Recovery::None,
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Recovery::None,
            Err(e) => Recovery::Unreplayable {
                reason: unreadable_note(&sidecar, &e),
            },
        }
    }

    /// `r` at the prompt (RFC-0023 §7): this process takes the recovered text over as its own journal at
    /// once, at the name it will write (`doc`'s, or this process's unnamed one), so a crash, `:q!` or a
    /// normal quit of this session finds a journal it owns. A journal of another name it came from (a
    /// legacy sidecar, an orphaned unnamed one, a second session's `<name>.<pid>.rj`) is archived. Nothing
    /// is lost when a step fails: the old journal is moved only once the new one is written.
    pub fn adopt(
        &self,
        offer: &Available,
        doc: Option<&Path>,
        fmt: FileFormat,
    ) -> io::Result<PathBuf> {
        let canon = doc.map(canonical);
        let target = claim(&self.path_of(doc));
        let written = rescue(&target, canon.as_deref(), fmt, &offer.recovered, 0, 0)?;
        if written != offer.journal {
            self.archive(&offer.journal)?;
        }
        Ok(written)
    }
}

/// When `doc` was modified on disk after its journal at `journal` was last written (#897), its modification
/// time (Unix milliseconds): recovering would replace a newer file.
fn stale_since(doc: &Path, journal: &Path) -> Option<u64> {
    if !is_stale(doc, journal) {
        return None;
    }
    let t = fs::metadata(doc).and_then(|m| m.modified()).ok()?;
    Some(
        t.duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
    )
}
