use std::path::PathBuf;

use super::*;
use crate::persist;

/// #619: Emacs's `auto-mode-case-fold` tries an unknown extension again in lower case; the Vim profile
/// does not.
#[test]
fn auto_mode_case_fold_lowers_the_extension() {
    let (mut files, mut hl): (Files, Highlighters) = (Files::new(), HashMap::new());
    let id = DocumentId(1);
    let fmt = persist::encoding::FileFormat::detect(b"");
    attach_file(&mut files, &mut hl, id, PathBuf::from("A.RS"), fmt);
    assert!(!hl.contains_key(&id), "Vim: .RS is no Rust file");
    files.write = crate::app::write_opts::WriteOptions::new(true);
    attach_file(&mut files, &mut hl, id, PathBuf::from("A.RS"), fmt);
    assert!(hl.contains_key(&id), "Emacs: .RS is tried as .rs");
}

/// #619: a file past the large-file threshold loses its highlighter and is marked degraded.
#[test]
fn a_large_file_is_degraded() {
    let (mut files, mut hl): (Files, Highlighters) = (Files::new(), HashMap::new());
    let id = DocumentId(1);
    let fmt = persist::encoding::FileFormat::detect(b"");
    attach_file(&mut files, &mut hl, id, PathBuf::from("a.rs"), fmt);
    mark_large(&mut files, &mut hl, id, persist::rj01::LARGE_FILE);
    assert!(
        hl.contains_key(&id) && !files[&id].large,
        "at the threshold: normal"
    );
    mark_large(&mut files, &mut hl, id, persist::rj01::LARGE_FILE + 1);
    assert!(!hl.contains_key(&id) && files[&id].large);
}

/// #894: attaching a file records its identity and the highlighter its extension selects; a later file
/// with no grammar drops the old highlighter instead of keeping the previous language's.
#[test]
fn attach_file_sets_identity_and_swaps_the_highlighter() {
    let (mut files, mut hl): (Files, Highlighters) = (Files::new(), HashMap::new());
    let id = DocumentId(1);
    let fmt = persist::encoding::FileFormat::detect(b"");
    attach_file(&mut files, &mut hl, id, PathBuf::from("a.rs"), fmt);
    assert_eq!(files[&id].path, PathBuf::from("a.rs"));
    assert!(hl.contains_key(&id), "a .rs file is highlighted");
    attach_file(&mut files, &mut hl, id, PathBuf::from("notes.xyz"), fmt);
    assert_eq!(files[&id].path, PathBuf::from("notes.xyz"));
    assert!(!hl.contains_key(&id), "no grammar, no highlighter");
}
