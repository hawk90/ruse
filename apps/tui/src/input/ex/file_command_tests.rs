use super::*;

fn write(line: &str) -> WriteSpec {
    match parse_ex(line) {
        Ex::Write(spec) => spec,
        other => panic!(":{line} is not a write: {other:?}"),
    }
}

/// #610: the `:w` family's forms, as `:help :w` lists them.
#[test]
fn write_forms_parse() {
    let w = write("w");
    assert_eq!(w.target, WriteTarget::File(None));
    assert!(!w.bang && !w.quit && !w.if_modified && w.range.is_none());
    assert_eq!(
        write("write! a.txt").target,
        WriteTarget::File(Some("a.txt".into()))
    );
    assert!(write("w! a.txt").bang);
    assert_eq!(
        write("w >> a.txt").target,
        WriteTarget::Append(Some("a.txt".into()))
    );
    assert_eq!(write("w>>").target, WriteTarget::Append(None));
    assert_eq!(
        write("w !wc -l").target,
        WriteTarget::Command("wc -l".into())
    );
    assert_eq!(write("2,3w b.txt").range, Some(SubRange::Lines(2, 3)));
    assert_eq!(write("%w").range, Some(SubRange::WholeFile));
    assert_eq!(
        write(r"w my\ file").target,
        WriteTarget::File(Some("my file".into()))
    );
    assert_eq!(write("w ++ff=dos d.txt").opts.ff, Some(FileFormatOpt::Dos));
    let up = write("up");
    assert!(up.if_modified && !up.quit);
    let wq = write("wq! x.txt");
    assert!(wq.quit && wq.bang && !wq.if_modified);
    for x in ["x", "xi", "xit", "exi", "exit"] {
        let spec = write(x);
        assert!(spec.quit && spec.if_modified, ":{x}");
    }
    assert_eq!(
        parse_ex("w a b"),
        Ex::Fail("E172: Only one file name allowed".into())
    );
    assert_eq!(write("w ++ff=mac a").opts.ff, Some(FileFormatOpt::Mac));
    assert_eq!(write("w ++bin a").opts.bin, Some(true));
    assert_eq!(write("w ++nobin a").opts.bin, Some(false));
    assert_eq!(
        parse_ex("w ++ff=foo a"),
        Ex::Fail("E474: Invalid argument".into())
    );
    // Other `w…` commands are not writes.
    assert!(matches!(parse_ex("wfoo"), Ex::Unknown(_)));
    assert!(matches!(parse_ex("wn"), Ex::Unknown(_)));
}

#[test]
fn saveas_wall_and_edit_parse() {
    assert_eq!(
        parse_ex("sav! n.txt"),
        Ex::SaveAs {
            bang: true,
            file: "n.txt".into(),
            opts: FileOpts::default()
        }
    );
    assert_eq!(
        parse_ex("saveas"),
        Ex::Fail("E471: Argument required".into())
    );
    assert_eq!(parse_ex("wa"), Ex::WriteAll { quit: false });
    assert_eq!(parse_ex("wall!"), Ex::WriteAll { quit: false });
    assert_eq!(parse_ex("wqa"), Ex::WriteAll { quit: true });
    assert_eq!(parse_ex("xall"), Ex::WriteAll { quit: true });
    assert_eq!(parse_ex("2wa"), Ex::Fail("E481: No range allowed".into()));
    assert_eq!(
        parse_ex(r"e! +/a\ b ++ff=unix f.txt"),
        Ex::Edit(EditSpec {
            bang: true,
            file: Some("f.txt".into()),
            cmd: Some("/a b".into()),
            opts: FileOpts {
                ff: Some(FileFormatOpt::Unix),
                bin: None,
            },
        })
    );
    let Ex::Edit(plus) = parse_ex("e + f.txt") else {
        panic!("not an edit")
    };
    assert_eq!(plus.cmd.as_deref(), Some(""));
    assert_eq!(
        split_bar("w !a | b"),
        ["w !a | b"],
        "`:w !cmd` owns the bar"
    );
    assert_eq!(split_bar("w! a|q"), ["w! a", "q"]);
}

/// #610: file-name modifiers, checked against nvim v0.12.6's `expand()`.
#[test]
fn filename_modifiers() {
    let names = FileNames {
        current: Some("sub/k.tar.gz"),
        alternate: Some(".bashrc"),
        buffers: &[],
    };
    let x = |arg: &str| expand_filename(arg, &names).unwrap();
    assert_eq!(x("%:h"), "sub");
    assert_eq!(x("%:t"), "k.tar.gz");
    assert_eq!(x("%:r"), "sub/k.tar");
    assert_eq!(x("%:e"), "gz");
    assert_eq!(x("%:t:r:r"), "k");
    assert_eq!(x("%:e:e"), "tar.gz");
    assert_eq!(x("%:r.bak"), "sub/k.tar.bak");
    assert_eq!(x("#:r"), ".bashrc", "a leading dot is not an extension");
    assert_eq!(x("#:h"), ".");
    assert_eq!(x("%:x"), "sub/k.tar.gz:x", "an unknown modifier stays text");
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(x("%:p"), cwd.join("sub/k.tar.gz").display().to_string());
    assert_eq!(x("%:p:h:h"), cwd.display().to_string());
    assert_eq!(x("%:p:."), "sub/k.tar.gz");
}
