use super::*;
use proptest::prelude::*;

fn enc(name: &str) -> FileEncoding {
    FileEncoding::from_name(name).unwrap()
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// nvim v0.12.6's folding of `:set fenc={name}` (probed; RFC-0025 evidence 35 and the slice-1 probe).
#[test]
fn names_fold_as_nvim() {
    for (given, canonical) in [
        ("utf8", "utf-8"),
        ("UTF-8", "utf-8"),
        ("LATIN1", "latin1"),
        ("iso-8859-1", "latin1"),
        ("iso_8859_1", "latin1"),
        ("iso8859-1", "latin1"),
        ("latin-1", "latin1"),
        ("ansi", "latin1"),
        ("l1", "l1"),
        ("windows-1252", "windows-1252"),
        ("Windows_1252", "windows-1252"),
        ("cp1252", "cp1252"),
        ("utf16", "utf-16"),
        ("utf-16be", "utf-16"),
        ("UTF16LE", "utf-16le"),
        ("Utf_16", "utf-16"),
        ("ucs2", "ucs-2"),
        ("ucs-2be", "ucs-2"),
        ("unicode", "ucs-2"),
        ("UCS-2LE", "ucs-2le"),
        ("ucs4", "ucs-4"),
        ("utf-32", "ucs-4"),
        ("utf-32be", "ucs-4"),
        ("ucs-4be", "ucs-4"),
        ("utf-32le", "ucs-4le"),
        ("utf32le", "ucs-4le"),
        ("shift_jis", "sjis"),
        ("shift-jis", "sjis"),
        ("pck", "sjis"),
        ("cp932", "cp932"),
        ("932", "cp932"),
        ("eucjp", "euc-jp"),
        ("euc_jp", "euc-jp"),
        ("ujis", "euc-jp"),
        ("japan", "euc-jp"),
        ("iso-2022-jp", "iso-2022-jp"),
        ("euc-kr", "euc-kr"),
        ("EUC_KR", "euc-kr"),
        ("euckr", "euc-kr"),
        ("korea", "euc-kr"),
        ("5601", "euc-kr"),
        ("CP949", "cp949"),
        ("949", "cp949"),
        ("uhc", "uhc"),
        ("gbk", "cp936"),
        ("936", "cp936"),
        ("gb18030", "gb18030"),
        ("big5", "big5"),
        ("950", "cp950"),
        ("latin2", "iso-8859-2"),
        ("iso8859_2", "iso-8859-2"),
        ("ISO8859-5", "iso-8859-5"),
        ("cyrillic", "iso-8859-5"),
        ("greek", "iso-8859-7"),
        ("nordic", "iso-8859-10"),
        ("latin9", "iso-8859-15"),
        ("koi8_r", "koi8-r"),
        ("mac", "macroman"),
        ("mac_roman", "macroman"),
        ("macintosh", "macintosh"),
        ("ibm866", "ibm866"),
        ("cp866", "cp866"),
    ] {
        assert_eq!(enc(given).name(), canonical, "{given}");
    }
    assert_eq!(
        FileEncoding::from_name("").unwrap(),
        FileEncoding::default()
    );
}

/// A name outside the table is an error carrying the name as given (`E474` in slice 2), never a guess.
#[test]
fn an_unknown_name_is_refused() {
    for name in [
        "foo",
        "8bit-cp1252",
        "koi8r",
        "latin10",
        "cp437",
        "iso-8859-9",
        "iso-8859-11",
        "euc-cn",
        "utf-8-bom",
    ] {
        assert_eq!(
            FileEncoding::from_name(name),
            Err(UnknownEncoding(name.to_owned())),
            "{name}"
        );
    }
    assert_eq!(FileEncoding::from_canonical(b"UTF-8"), None);
    assert_eq!(FileEncoding::from_canonical(b"utf8"), None);
}

/// Every canonical name is one row and names itself; every alias names a row.
#[test]
fn the_table_is_consistent() {
    let names: Vec<_> = FileEncoding::names().collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        names.len(),
        "a canonical name is listed twice"
    );
    for n in &names {
        assert_eq!(enc(n).name(), *n);
        assert_eq!(
            FileEncoding::from_canonical(n.as_bytes()).unwrap().name(),
            *n
        );
        assert_eq!(fold(n), *n, "{n} is not in folded form");
        assert!(
            ALIASES.iter().all(|(a, _)| a != n),
            "{n} is a row and an alias"
        );
    }
    for (alias, target) in ALIASES {
        assert!(names.contains(&target), "{alias} -> {target}");
        assert_eq!(fold(alias), alias);
    }
    assert_eq!(FileEncoding::default().name(), "");
    assert!(!names.contains(&""));
}

/// `latin1` is ISO-8859-1, not windows-1252: every byte is its own code point, both ways, so every file
/// decodes and writes back (RFC-0025 §2, the maintainer's decision on #1292).
#[test]
fn latin1_is_iso_8859_1() {
    let l = enc("latin1");
    let all: Vec<u8> = (0..=255).collect();
    let d = l.decode(&all);
    assert_eq!(d.malformed, None);
    let text = std::str::from_utf8(&d.text).unwrap();
    assert!(text.chars().map(u32::from).eq(0..=255));
    assert_eq!(l.encode(&d.text).unwrap(), all);
    // Evidence 1 and 2: `café` and 0x80 as U+0080.
    assert_eq!(
        l.text(&hex("636166e90a6e61ef76650a")),
        "café\nnaïve\n".as_bytes()
    );
    assert_eq!(l.text(b"\x80"), "\u{80}".as_bytes());
    let e = l.encode("a\nb\n한".as_bytes()).unwrap_err();
    assert_eq!(
        e,
        Unencodable {
            ch: '한',
            offset: 4,
            line: 3
        }
    );
    // cp1252 is the separate row that reads 0x80 as the euro sign (evidence 3).
    assert_eq!(
        enc("cp1252").text(&hex("7072696365208020350a")),
        "price € 5\n".as_bytes()
    );
    assert_eq!(
        enc("cp1252").encode("\u{80}".as_bytes()).unwrap_err().ch,
        '\u{80}'
    );
}

/// The files Python's codecs make (RFC-0025 evidence), each read and written back.
#[test]
fn evidence_files_decode_and_encode_as_pythons_codecs() {
    for (name, file, text) in [
        ("latin1", "636166e90a6e61ef76650a", "café\nnaïve\n"),
        ("utf-16le", "630061006600e90020005cd50a00", "café 한\n"),
        ("utf-16", "00630061006600e90020d55c000a", "café 한\n"),
        ("utf-16le", "3dd800de", "😀"),
        ("ucs-4le", "6100000000f601000a000000", "a😀\n"),
        ("ucs-4", "000000610001f6000000000a", "a😀\n"),
        ("sjis", "93fa967b8cea0a", "日本語\n"),
        ("euc-kr", "c7d1b1b9beee0a", "한국어\n"),
        ("cp949", "81418c6320c7d1b1b9beee0a", "갂똠 한국어\n"),
        ("cp936", "d6d0cec40a", "中文\n"),
        ("big5", "a4a4a4e50a", "中文\n"),
        ("euc-jp", "c6fccbdcb8ec0a", "日本語\n"),
        ("iso-2022-jp", "611b2442467c1b2842620a", "a日b\n"),
    ] {
        let e = enc(name);
        let raw = hex(file);
        assert_eq!(
            e.decode_exact(&raw).as_deref(),
            Some(text.as_bytes()),
            "{name}"
        );
        assert_eq!(e.encode(text.as_bytes()).unwrap(), raw, "{name}");
    }
}

/// UCS-2 has no surrogate pairs: a character above U+FFFF cannot be written, and a pair is not read.
#[test]
fn ucs_2_refuses_what_utf_16_pairs() {
    assert_eq!(
        enc("ucs-2le").encode("a😀".as_bytes()).unwrap_err().offset,
        1
    );
    assert_eq!(enc("ucs-2le").decode(&hex("3dd800de")).malformed, Some(0));
    assert_eq!(
        enc("utf-16le").encode("😀".as_bytes()).unwrap(),
        hex("3dd800de")
    );
    // A lone surrogate, an odd final byte and a UTF-32 value past U+10FFFF are kept and reported.
    let raw = hex("610000d84100");
    let d = enc("utf-16le").decode(&raw);
    assert_eq!((d.malformed, &d.text[..]), (Some(2), &b"a\x00\xd8A"[..]));
    let raw = hex("610041");
    let d = enc("utf-16le").decode(&raw);
    assert_eq!((d.malformed, &d.text[..]), (Some(2), &b"aA"[..]));
    let raw = hex("0011000000000061ff");
    let d = enc("ucs-4").decode(&raw);
    assert_eq!(d.malformed, Some(0));
    assert_eq!(&d.text[..], b"\x00\x11\x00\x00a\xff");
}

/// The multi-byte WHATWG decoders accept sequences their encoders do not give back. Such a file is not
/// read exactly, so a `'fileencodings'` entry does not accept it and an unchanged buffer never writes other
/// bytes (F-008). Probed exhaustively over all byte pairs with encoding_rs 0.8.44: Shift_JIS 2278 pairs,
/// Big5 3941, gb18030 130, EUC-JP 10 (and every JIS X 0212 triple), GBK 2; EUC-KR and the single-byte sets 0.
#[test]
fn an_inexact_decode_is_not_accepted() {
    let sjis = enc("sjis");
    assert_eq!(sjis.text(&[0x87, 0x90]), "≒".as_bytes());
    assert_eq!(&sjis.encode("≒".as_bytes()).unwrap()[..], [0x81, 0xE0]);
    assert_eq!(sjis.decode_exact(&[0x87, 0x90]), None);
    assert_eq!(enc("big5").decode_exact(&[0x88, 0x62]), None);
    assert_eq!(enc("cp936").decode_exact(&[0xA2, 0xE3]), None);
    assert_eq!(enc("euc-jp").decode_exact(&[0x8F, 0xB0, 0xA1]), None);
    assert_eq!(enc("iso-2022-jp").decode_exact(b"\x1b(Jab\x1b(B"), None);
    // EUC-KR (windows-949) is exact: every pair it decodes comes back.
    let kr = enc("euc-kr");
    for a in 0x81..=0xFEu8 {
        for b in 0x41..=0xFEu8 {
            if let Some(t) = kr.decode_exact(&[a, b]) {
                assert_eq!(&kr.encode(&t).unwrap()[..], [a, b]);
            }
        }
    }
}

/// A byte sequence a WHATWG decoder rejects is kept as it is, in order, and reported at its offset; the
/// kept bytes write back as they are (RFC-0025 §3.1 `++bad=keep`).
#[test]
fn malformed_bytes_are_kept_in_place() {
    // Evidence 18: a Shift_JIS lead byte before a space.
    let raw = b"ok \x81\x20 x";
    let d = enc("sjis").decode(raw);
    assert_eq!(d.malformed, Some(3));
    assert_eq!(&d.text[..], raw);
    assert_eq!(enc("sjis").encode(&d.text).unwrap(), &raw[..]);
    // gb18030 re-reads the bytes after a broken four-byte sequence: they stay in their place.
    let raw = b"a\x81\x30\x81\xFFb";
    let d = enc("gb18030").decode(raw);
    assert_eq!((d.malformed, &d.text[..]), (Some(1), &raw[..]));
    let d = enc("euc-kr").decode(b"a\xFF\xFFb\xB0");
    assert_eq!(
        (d.malformed, &d.text[..]),
        (Some(1), &b"a\xFF\xFFb\xB0"[..])
    );
    // A kept byte in the text is written as it is in every encoding.
    for name in FileEncoding::names() {
        let e = enc(name);
        let Ok(out) = e.encode(b"\xFF") else {
            panic!("{name}")
        };
        assert_eq!(&out[..], [0xFF], "{name}");
    }
}

/// `ucs-bom`: UTF-32's BOM is checked before UTF-16's (evidence 10), then UTF-8's.
#[test]
fn the_bom_names_its_encoding() {
    for (raw, name) in [
        (&b"\xFF\xFE\x00\x00a"[..], Some("ucs-4le")),
        (b"\x00\x00\xFE\xFFa", Some("ucs-4")),
        (b"\xFE\xFF\x00a", Some("utf-16")),
        (b"\xFF\xFEa\x00", Some("utf-16le")),
        (b"\xEF\xBB\xBFa", Some("utf-8")),
        (b"a", None),
        (b"", None),
    ] {
        let got = FileEncoding::sniff_bom(raw);
        assert_eq!(got.map(FileEncoding::name), name, "{raw:?}");
        if let Some(e) = got {
            assert!(raw.starts_with(e.bom().unwrap()));
        }
    }
    assert_eq!(FileEncoding::default().bom(), Some(&b"\xEF\xBB\xBF"[..]));
    assert_eq!(enc("latin1").bom(), None);
    assert_eq!(enc("sjis").bom(), None);
}

/// The default and `utf-8` copy nothing either way.
#[test]
fn utf8_is_the_bytes() {
    for e in [FileEncoding::default(), enc("utf-8")] {
        assert!(e.is_utf8());
        let raw = b"caf\xE9 \xC3\xA9\n";
        assert!(matches!(e.text(raw), Cow::Borrowed(_)));
        assert!(matches!(e.encode(raw), Ok(Cow::Borrowed(_))));
        assert_eq!(e.decode(raw).malformed, Some(3));
    }
}

fn unicode_text() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            any::<char>(),
            Just('\n'),
            Just('é'),
            Just('한'),
            Just('😀'),
            Just('\u{FEFF}'),
            Just('\u{FFFF}'),
            Just('\0'),
        ],
        0..24,
    )
    .prop_map(|v| v.into_iter().collect())
}

proptest! {
    #![proptest_config(crate::test_cases(256))]

    /// Every byte string reads in latin1 and writes back as it was.
    #[test]
    fn latin1_round_trips_every_file(raw in proptest::collection::vec(any::<u8>(), 0..64)) {
        let l = enc("latin1");
        let t = l.decode_exact(&raw).unwrap();
        let back = l.encode(&t).unwrap();
        prop_assert_eq!(&back[..], &raw[..]);
    }

    /// Any text through UTF-16 and UTF-32, both byte orders, with and without the BOM: the encoded file
    /// decodes to the text, and with the BOM `ucs-bom` finds the encoding again.
    #[test]
    fn unicode_encodings_round_trip_any_text(text in unicode_text(), bom in any::<bool>()) {
        for name in ["utf-16", "utf-16le", "ucs-4", "ucs-4le", "ucs-2", "ucs-2le"] {
            let e = enc(name);
            let Ok(body) = e.encode(text.as_bytes()) else {
                prop_assert!(name.starts_with("ucs-2") && text.chars().any(|c| u32::from(c) > 0xFFFF));
                continue;
            };
            let bom_bytes = e.bom().unwrap();
            let file = if bom { [bom_bytes, &body].concat() } else { body.to_vec() };
            let body = if bom {
                // `ucs-bom` finds a BOM that starts the file: this one, except that a UTF-16LE text opening
                // with U+0000 starts like UTF-32LE's BOM.
                let found = FileEncoding::sniff_bom(&file).unwrap();
                prop_assert!(file.starts_with(found.bom().unwrap()));
                if !name.ends_with("le") || name == "ucs-4le" {
                    prop_assert_eq!(found.bom(), e.bom());
                }
                &file[bom_bytes.len()..]
            } else {
                &file[..]
            };
            let got = e.decode_exact(body);
            prop_assert_eq!(got.as_deref(), Some(text.as_bytes()), "{}", name);
        }
    }

    /// F-008 for every row: whatever bytes an encoding reads exactly, it writes back unchanged; and whatever
    /// it reads with kept bytes and no error, likewise.
    #[test]
    fn an_exact_read_writes_back_its_bytes(
        raw in proptest::collection::vec(
            prop_oneof![any::<u8>(), Just(b'\n'), Just(0x00), Just(0x81), Just(0xA4), Just(0x1B), Just(b'$'), Just(b'B')],
            0..16),
    ) {
        for name in FileEncoding::names() {
            let e = enc(name);
            if let Some(t) = e.decode_exact(&raw) {
                let back = e.encode(&t).ok().map(Cow::into_owned);
                prop_assert_eq!(back.as_deref(), Some(&raw[..]), "{}", name);
            }
        }
    }
}
