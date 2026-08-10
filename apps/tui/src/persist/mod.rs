//! Durable persistence & crash recovery (F-008). MVP-minimal, honest against all four acceptance
//! criteria; the full four-state / inverse-edit / incremental journal in
//! `docs/design/persistence-and-recovery.md` is the post-MVP C-PERSIST elaboration.
//!
//!   * [`atomic`]   — temp + fsync + rename + directory fsync (durable "Saved"). (#1)
//!   * [`encoding`] — detect the original EOL/BOM and restore it on save. (#2)
//!   * [`journal`]  — append-only framed recovery records; a torn tail is discarded. (#4)
//!   * [`assess_recovery`] — the pure open-time decision; recovery never auto-overwrites. (#3, #4)
//!
//! The pure cores are terminal-free and file-free (they take/return bytes) so the whole persistence
//! contract is unit-testable; only the thin `append`/`replay`/`save` wrappers touch the disk.

pub mod atomic;
pub mod encoding;
pub mod journal;

/// What a document's journal offers at open time, once compared against the on-disk file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Recovery {
    /// No journal, or its latest record already matches the disk file — nothing to recover.
    None,
    /// Unsaved changes were found that differ from disk. The user chooses: keep the recovered
    /// buffer, or discard it and use the disk file. The original file is NEVER auto-overwritten —
    /// recovered bytes go into the editor buffer only, so a save is an explicit later act.
    Available(Vec<u8>),
}

/// The open-time recovery decision (F-008 #3/#4), pure over the two byte sources so it is testable
/// without a filesystem. `disk` is the file's current content (the editor's cold-open buffer, after
/// [`encoding::FileFormat::to_buffer`]); `recovered` is [`journal::replay`]'s latest valid payload.
///
/// A recovery is offered only when the journal holds work that genuinely differs from disk — a stale
/// journal left after a clean exit (its last record == disk) yields `None`, so the user is not
/// nagged about nothing.
pub fn assess_recovery(disk: &[u8], recovered: Option<&[u8]>) -> Recovery {
    match recovered {
        Some(r) if r != disk => Recovery::Available(r.to_vec()),
        _ => Recovery::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_journal_means_no_recovery() {
        assert_eq!(assess_recovery(b"disk", None), Recovery::None);
    }

    #[test]
    fn journal_matching_disk_is_not_offered() {
        // A clean exit can leave a journal whose last record equals what was saved — do not nag.
        assert_eq!(assess_recovery(b"same", Some(b"same")), Recovery::None);
    }

    #[test]
    fn diverging_unsaved_work_is_offered() {
        assert_eq!(
            assess_recovery(b"on disk", Some(b"unsaved edits")),
            Recovery::Available(b"unsaved edits".to_vec())
        );
    }
}
