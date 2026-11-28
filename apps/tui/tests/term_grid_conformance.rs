//! VT conformance fixtures for the terminal grid (#913). Each fixture feeds one byte stream to a fresh
//! [`Grid`] and states the screen text and cursor that xterm documents for it (ctlseqs.ms, "XTerm Control
//! Sequences"; VT100/VT220 manuals for the DEC behaviour xterm follows). Style fixtures check the SGR pen.
//! A property then proves chunk-split equivalence: feeding a stream whole or split at arbitrary positions,
//! inside escape sequences and inside UTF-8 sequences, yields the same grid (a PTY read can end anywhere).
//!
//! Screen rows are rendered one char per column: a blank cell is a space, the right half of a wide glyph is
//! omitted (the wide glyph covers it), and a cell keeps its combining marks.

#![cfg(unix)]

use crossterm::style::Color;
use proptest::prelude::*;
use ruse_tui::screen::CellStyle;
use ruse_tui::term_grid::{ChunkParser, Grid};

fn feed(rows: u16, cols: u16, bytes: &[u8]) -> Grid {
    let mut g = Grid::new(rows, cols);
    ChunkParser::new().advance(&mut g, bytes);
    g
}

fn screen(g: &Grid) -> Vec<String> {
    let (rows, cols) = g.size();
    (0..rows)
        .map(|r| {
            (0..cols)
                .filter_map(|c| {
                    g.cell(r, c)
                        .map(|(t, _)| if t.is_empty() { " " } else { t })
                })
                .collect()
        })
        .collect()
}

fn check(rows: u16, cols: u16, input: &[u8], want: &[&str], cursor: (u16, u16)) {
    let g = feed(rows, cols, input);
    let got = screen(&g);
    assert_eq!(got, want, "screen for {:?}", String::from_utf8_lossy(input));
    assert_eq!(
        g.cursor(),
        cursor,
        "cursor (row, col) for {:?}",
        String::from_utf8_lossy(input)
    );
}

/// `name: (rows, cols, input) => [screen rows] @ (cursor row, cursor col);` — one `#[test]` per row, plus
/// the [`FIXTURES`] table the chunk-split property replays. A fixture may carry attributes (`#[ignore]`).
macro_rules! fixtures {
    ($( $(#[$attr:meta])* $name:ident: ($rows:expr, $cols:expr, $input:expr) => [$($line:expr),* $(,)?] @ $cursor:expr; )*) => {
        $(
            #[test]
            $(#[$attr])*
            fn $name() {
                check($rows, $cols, $input, &[$($line),*], $cursor);
            }
        )*
        /// Every fixture's `(name, rows, cols, input)`.
        const FIXTURES: &[(&str, u16, u16, &[u8])] = &[$((stringify!($name), $rows, $cols, $input)),*];
    };
}

fixtures! {
    // --- printing, wrap, C0 controls ---
    plain_text_wraps_at_the_right_margin: (2, 4, b"abcde") => ["abcd", "e   "] @ (1, 1);
    // xterm/VT100 pending wrap: the cursor stays on the last column until the NEXT printable.
    writing_exactly_cols_chars_leaves_a_pending_wrap: (2, 4, b"abcd") => ["abcd", "    "] @ (0, 3);
    carriage_return_cancels_the_pending_wrap: (2, 4, b"abcd\rx") => ["xbcd", "    "] @ (0, 1);
    cub_cancels_the_pending_wrap: (2, 4, b"abcd\x1b[Dx") => ["abxd", "    "] @ (0, 3);
    cha_cancels_the_pending_wrap: (2, 4, b"abcd\x1b[2Gx") => ["axcd", "    "] @ (0, 2);
    cup_cancels_the_pending_wrap: (2, 4, b"abcd\x1b[1;1Hx") => ["xbcd", "    "] @ (0, 1);
    backspace_cancels_the_pending_wrap: (2, 4, b"abcd\x08x") => ["abxd", "    "] @ (0, 3);
    wrap_at_the_bottom_scrolls: (2, 3, b"abcdefg") => ["def", "g  "] @ (1, 1);
    // LF is a pure index (no LNM): the column is kept.
    line_feed_keeps_the_column: (3, 4, b"ab\ncd") => ["ab  ", "  cd", "    "] @ (1, 3);
    vertical_tab_and_form_feed_are_line_feeds: (3, 3, b"a\x0bb\x0cc") => ["a  ", " b ", "  c"] @ (2, 2);
    line_feed_at_the_bottom_scrolls: (2, 2, b"a\r\nb\r\nc") => ["b ", "c "] @ (1, 1);
    backspace_moves_left_one: (1, 4, b"ab\x08c") => ["ac  "] @ (0, 2);
    backspace_stops_at_column_zero: (1, 4, b"\x08\x08a") => ["a   "] @ (0, 1);
    tab_stops_every_eight_columns: (1, 12, b"a\tb") => ["a       b   "] @ (0, 9);
    tab_stops_at_the_right_margin: (1, 10, b"\t\tx") => ["         x"] @ (0, 9);
    bel_and_other_c0_are_ignored: (1, 4, b"a\x07\x00\x01b") => ["ab  "] @ (0, 2);

    // --- cursor addressing ---
    cup_is_one_based: (3, 4, b"\x1b[2;3Hx") => ["    ", "  x ", "    "] @ (1, 3);
    cup_without_params_homes: (2, 3, b"ab\x1b[Hx") => ["xb ", "   "] @ (0, 1);
    cup_clamps_to_the_screen: (2, 3, b"\x1b[99;99Hx") => ["   ", "  x"] @ (1, 2);
    hvp_is_cup: (2, 3, b"\x1b[2;2fx") => ["   ", " x "] @ (1, 2);
    cuu_cud_cuf_cub_move_and_clamp:
        (3, 4, b"\x1b[3;1H\x1b[9Aa\x1b[9Bb\x1b[9Cc\x1b[9Dd") => ["a   ", "    ", "db c"] @ (2, 1);
    cuf_zero_means_one: (1, 4, b"\x1b[0Cx") => [" x  "] @ (0, 2);
    vpa_sets_the_row: (3, 3, b"a\x1b[3dx") => ["a  ", "   ", " x "] @ (2, 2);
    decsc_decrc_restore_the_position: (2, 4, b"a\x1b7\x1b[2;4Hb\x1b8c") => ["ac  ", "   b"] @ (0, 2);
    scosc_scorc_restore_the_position: (2, 4, b"a\x1b[s\x1b[2;4Hb\x1b[uc") => ["ac  ", "   b"] @ (0, 2);

    // --- erase ---
    el0_erases_to_the_end_of_line: (1, 5, b"abcde\x1b[3G\x1b[K") => ["ab   "] @ (0, 2);
    el1_erases_to_the_cursor_inclusive: (1, 5, b"abcde\x1b[3G\x1b[1K") => ["   de"] @ (0, 2);
    el2_erases_the_whole_line: (2, 5, b"abcde\r\nfg\x1b[1A\x1b[2K") => ["     ", "fg   "] @ (0, 2);
    el_at_the_pending_wrap_erases_the_last_column: (1, 4, b"abcd\x1b[K") => ["abc "] @ (0, 3);
    ed0_erases_to_the_end_of_screen: (3, 3, b"abc\r\ndef\r\nghi\x1b[2;2H\x1b[J") => ["abc", "d  ", "   "] @ (1, 1);
    ed1_erases_to_the_cursor_inclusive: (3, 3, b"abc\r\ndef\r\nghi\x1b[2;2H\x1b[1J") => ["   ", "  f", "ghi"] @ (1, 1);
    ed2_erases_the_screen_and_keeps_the_cursor: (2, 3, b"abc\r\nde\x1b[2J") => ["   ", "   "] @ (1, 2);
    // xterm "erase saved lines": CSI 3 J clears the scrollback only (#781).
    ed3_keeps_the_screen: (2, 3, b"abc\r\nde\x1b[3J") => ["abc", "de "] @ (1, 2);
    ech_erases_in_place: (1, 5, b"abcde\x1b[2G\x1b[2X") => ["a  de"] @ (0, 1);
    ech_clamps_at_the_right_margin: (1, 4, b"abcd\x1b[2G\x1b[99X") => ["a   "] @ (0, 1);

    // --- insert / delete ---
    ich_shifts_right_and_drops_at_the_margin: (1, 5, b"abcde\x1b[2G\x1b[2@") => ["a  bc"] @ (0, 1);
    ich_clamps_its_count: (1, 4, b"abcd\x1b[2G\x1b[99@") => ["a   "] @ (0, 1);
    dch_shifts_left_and_blanks_the_margin: (1, 5, b"abcde\x1b[2G\x1b[2P") => ["ade  "] @ (0, 1);
    dch_clamps_its_count: (1, 4, b"abcd\x1b[2G\x1b[99P") => ["a   "] @ (0, 1);
    il_pushes_rows_down: (3, 2, b"a\r\nb\r\nc\x1b[2;1H\x1b[L") => ["a ", "  ", "b "] @ (1, 0);
    dl_pulls_rows_up: (3, 2, b"a\r\nb\r\nc\x1b[1;1H\x1b[M") => ["b ", "c ", "  "] @ (0, 0);
    il_outside_the_region_is_a_no_op: (3, 2, b"a\r\nb\r\nc\x1b[1;2r\x1b[3;1H\x1b[L") => ["a ", "b ", "c "] @ (2, 0);

    // --- scroll region, index, reverse index ---
    decstbm_homes_the_cursor: (3, 3, b"\x1b[2;2H\x1b[2;3rx") => ["x  ", "   ", "   "] @ (0, 1);
    lf_at_the_bottom_margin_scrolls_only_the_region:
        (4, 2, b"a\r\nb\r\nc\r\nd\x1b[2;3r\x1b[3;1H\nx") => ["a ", "c ", "x ", "d "] @ (2, 1);
    ri_moves_up: (2, 2, b"\x1b[2;1H\x1bMx") => ["x ", "  "] @ (0, 1);
    ri_at_the_top_scrolls_down: (3, 2, b"a\r\nb\r\nc\x1b[1;1H\x1bM") => ["  ", "a ", "b "] @ (0, 0);
    ri_at_the_top_margin_scrolls_only_the_region:
        (4, 2, b"a\r\nb\r\nc\r\nd\x1b[2;3r\x1b[2;1H\x1bM") => ["a ", "  ", "b ", "d "] @ (1, 0);

    // --- modes and reset ---
    alt_screen_is_blank_then_restores_the_main: (2, 3, b"ab\x1b[?1049hzz\x1b[?1049l") => ["ab ", "   "] @ (0, 2);
    alt_screen_starts_blank: (2, 3, b"ab\x1b[?1049hz") => ["z  ", "   "] @ (0, 1);
    ris_clears_and_homes: (2, 3, b"abc\r\nd\x1bcx") => ["x  ", "   "] @ (0, 1);
    ris_resets_the_scroll_region: (3, 2, b"\x1b[1;2r\x1bc\x1b[3;1Ha\nb") => ["  ", "a ", " b"] @ (2, 1);

    // --- private markers and intermediates (#776) ---
    xtmodkeys_is_not_sgr: (1, 3, b"\x1b[>4;2mx") => ["x  "] @ (0, 1);
    kitty_keyboard_push_is_not_restore_cursor: (1, 4, b"a\x1b[s\x1b[3G\x1b[>1ux") => ["a x "] @ (0, 3);
    decscusr_is_not_save_cursor: (1, 4, b"a\x1b[2 qb") => ["ab  "] @ (0, 2);
    da2_query_does_not_print: (1, 3, b"\x1b[>cx") => ["x  "] @ (0, 1);

    // --- wide glyphs and combining marks (#780, #782) ---
    wide_glyph_takes_two_columns: (1, 4, "中a".as_bytes()) => ["中a "] @ (0, 3);
    wide_glyph_at_the_last_column_wraps_first: (2, 3, "ab中".as_bytes()) => ["ab ", "中 "] @ (1, 2);
    overwriting_the_left_half_blanks_the_right: (1, 3, "中\rx".as_bytes()) => ["x  "] @ (0, 1);
    overwriting_the_right_half_blanks_the_left: (1, 3, "中\x1b[2Gx".as_bytes()) => [" x "] @ (0, 2);
    combining_mark_joins_the_previous_glyph: (1, 3, "e\u{301}x".as_bytes()) => ["e\u{301}x "] @ (0, 2);
    combining_mark_joins_the_glyph_at_the_pending_wrap: (1, 2, "ab\u{301}".as_bytes()) => ["ab\u{301}"] @ (0, 1);
    combining_mark_joins_a_wide_glyph: (1, 3, "中\u{301}".as_bytes()) => ["中\u{301} "] @ (0, 2);
    combining_mark_at_column_zero_is_dropped: (1, 2, "\u{301}a".as_bytes()) => ["a "] @ (0, 1);

    // --- invalid UTF-8 (vte decodes; xterm shows U+FFFD for a malformed sequence) ---
    invalid_byte_prints_a_replacement: (1, 4, b"a\xffb") => ["a\u{fffd}b "] @ (0, 3);
    truncated_sequence_then_ascii_prints_a_replacement: (1, 4, b"a\xe4\xb8b") => ["a\u{fffd}b "] @ (0, 3);
    // vte executes a lone 0x80..=0x9F byte as an 8-bit C1 control and the grid ignores it, so the byte
    // vanishes; xterm in UTF-8 mode does not take 8-bit C1 and shows U+FFFD for the malformed byte.
    // (the lone 0x80..=0x9F case is the quarantined `lone_continuation_byte_prints_a_replacement` below)

    // --- OSC, DCS, and other strings are consumed, never printed ---
    osc_title_terminated_by_bel: (1, 4, b"\x1b]0;title\x07ab") => ["ab  "] @ (0, 2);
    osc_title_terminated_by_st: (1, 4, b"\x1b]2;title\x1b\\ab") => ["ab  "] @ (0, 2);
    osc8_hyperlink_prints_only_the_link_text:
        (1, 6, b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\") => ["link  "] @ (0, 4);
    unknown_osc_is_ignored: (1, 4, b"\x1b]777;notify;a;b\x07ok") => ["ok  "] @ (0, 2);
    osc_with_utf8_payload_is_ignored: (1, 4, "\x1b]0;中文\x07ok".as_bytes()) => ["ok  "] @ (0, 2);
    dcs_is_consumed: (1, 4, b"\x1bP1$r0m\x1b\\ok") => ["ok  "] @ (0, 2);
    can_aborts_a_csi: (1, 4, b"\x1b[2\x18J") => ["J   "] @ (0, 1);
    c0_inside_a_csi_executes: (2, 4, b"ab\x1b[1\rC") => ["ab  ", "    "] @ (0, 1);
}

#[test]
#[ignore = "#1750: a lone 0x80..=0x9F byte is dropped as C1, not shown as U+FFFD"]
fn lone_continuation_byte_prints_a_replacement() {
    check(1, 4, b"\x80a", &["\u{fffd}a  "], (0, 2));
}

// --- chunk-split regressions (#1765), found by `chunk_split_is_equivalent_to_one_feed` ---
//
// vte 0.15 completes a UTF-8 sequence cut across `advance` calls from a lookahead, and skips the bytes
// after the char that the lookahead held. `ChunkParser` keeps the cut sequence back instead.

fn assert_split_matches_one_feed(rows: u16, cols: u16, bytes: &[u8], cuts: &[usize]) {
    assert_eq!(
        snapshot(&feed_split(rows, cols, bytes, cuts)),
        snapshot(&feed(rows, cols, bytes)),
        "{:?} cut at {cuts:?}",
        String::from_utf8_lossy(bytes)
    );
}

/// `é` cut between its two bytes lost the LF after it, so `中` overwrote the row that holds `é`.
#[test]
fn split_utf8_keeps_the_linefeed_after_it() {
    let bytes = "xyz\x1bM\x1b[H\x1b[?38:5:200H\n\x1b]0;t\x07xyz\u{e9}\n\u{4e2d}\x1b[?H".as_bytes();
    let cut = bytes.iter().position(|&b| b == 0xc3).expect("é") + 1;
    assert_split_matches_one_feed(1, 3, bytes, &[cut]);
}

/// NBSP cut between its two bytes lost the CR after it, so the U+FFFD for the lone 0xA0 landed one cell right.
#[test]
fn split_utf8_keeps_the_carriage_return_after_it() {
    let bytes = b"\x1b[?38:5:200;H\xc2\xa0\r\xa0\x00\x00a";
    assert_eq!(screen(&feed(1, 2, bytes)), ["\u{fffd}a"]);
    assert_split_matches_one_feed(1, 2, bytes, &[14]);
}

/// `é` cut between its two bytes lost the space after it (then a lone 0x80 follows).
#[test]
fn split_utf8_then_lone_c1_matches_one_feed() {
    assert_split_matches_one_feed(1, 2, b"\xc3\xa9 \x80", &[1]);
}

/// A U+0085 (NEL) cut between its two bytes was printed; one feed executes it as a C1 control.
#[test]
fn split_utf8_c1_control_is_executed_as_in_one_feed() {
    assert_split_matches_one_feed(1, 4, b"a\xc2\x85b", &[2]);
}

// --- SGR pen fixtures ---

fn pen(input: &[u8]) -> CellStyle {
    let g = feed(1, 2, input);
    *g.cell(0, 0).expect("cell (0,0)").1
}

#[test]
fn sgr_basic_attributes_set_and_reset() {
    let s = pen(b"\x1b[1;3;4;7mx");
    assert!(s.bold && s.italic && s.underline && s.reverse);
    let s = pen(b"\x1b[1;3;4;7m\x1b[22;23;24;27mx");
    assert!(!s.bold && !s.italic && !s.underline && !s.reverse);
    assert_eq!(
        pen(b"\x1b[1;31m\x1b[mx"),
        CellStyle::default(),
        "CSI m is SGR 0"
    );
}

#[test]
fn sgr_16_colors() {
    let s = pen(b"\x1b[31;42mx");
    assert_eq!((s.fg, s.bg), (Color::AnsiValue(1), Color::AnsiValue(2)));
    let s = pen(b"\x1b[91;102mx");
    assert_eq!((s.fg, s.bg), (Color::AnsiValue(9), Color::AnsiValue(10)));
    let s = pen(b"\x1b[31;42m\x1b[39;49mx");
    assert_eq!((s.fg, s.bg), (Color::Reset, Color::Reset));
}

#[test]
fn sgr_extended_colors_semicolon_form() {
    assert_eq!(pen(b"\x1b[38;5;196mx").fg, Color::AnsiValue(196));
    let s = pen(b"\x1b[48;2;1;2;3;1mx");
    assert_eq!(s.bg, Color::Rgb { r: 1, g: 2, b: 3 });
    assert!(s.bold, "the parameter after the RGB triple is its own SGR");
}

/// ITU T.416 colon subparameters stay in their group (#779).
#[test]
fn sgr_extended_colors_colon_form() {
    assert_eq!(
        pen(b"\x1b[38:2::10:20:30mx").fg,
        Color::Rgb {
            r: 10,
            g: 20,
            b: 30
        }
    );
    assert_eq!(
        pen(b"\x1b[38:2:10:20:30mx").fg,
        Color::Rgb {
            r: 10,
            g: 20,
            b: 30
        }
    );
    assert_eq!(pen(b"\x1b[48:5:9mx").bg, Color::AnsiValue(9));
    let s = pen(b"\x1b[38:5:1mx");
    assert!(!s.bold, "subparameter 1 is not SGR bold");
    let s = pen(b"\x1b[4m\x1b[4:0mx");
    assert!(!s.underline, "4:0 is no underline");
    assert!(
        pen(b"\x1b[4:3mx").underline,
        "4:3 (curly) draws as underline"
    );
}

#[test]
fn erase_uses_the_current_background() {
    let g = feed(1, 3, b"abc\x1b[44m\x1b[2K");
    assert_eq!(g.cell(0, 1).unwrap().1.bg, Color::AnsiValue(4));
}

#[test]
fn ris_resets_the_pen_and_modes() {
    let g = feed(1, 2, b"\x1b[31m\x1b[?25l\x1b[?2004h\x1bcx");
    assert_eq!(*g.cell(0, 0).unwrap().1, CellStyle::default());
    assert!(g.cursor_visible());
    assert!(!g.bracketed_paste());
}

#[test]
fn dectcem_and_bracketed_paste_modes() {
    let g = feed(1, 1, b"\x1b[?25l\x1b[?1049;2004h");
    assert!(!g.cursor_visible());
    assert!(g.bracketed_paste());
    let g = feed(1, 1, b"\x1b[?25l\x1b[?2004h\x1b[?25h\x1b[?2004l");
    assert!(g.cursor_visible());
    assert!(!g.bracketed_paste());
}

#[test]
fn fixture_table_is_nonempty_and_named_uniquely() {
    let mut names: Vec<&str> = FIXTURES.iter().map(|f| f.0).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), FIXTURES.len());
}

// --- chunk-split equivalence ---

/// Everything observable on a grid: every cell (None = wide continuation), cursor, and modes.
#[derive(Debug, PartialEq)]
struct Snapshot {
    cells: Vec<Option<(String, CellStyle)>>,
    cursor: (u16, u16),
    cursor_visible: bool,
    bracketed_paste: bool,
}

fn snapshot(g: &Grid) -> Snapshot {
    let (rows, cols) = g.size();
    let cells = (0..rows)
        .flat_map(|r| (0..cols).map(move |c| (r, c)))
        .map(|(r, c)| g.cell(r, c).map(|(t, s)| (t.to_string(), *s)))
        .collect();
    Snapshot {
        cells,
        cursor: g.cursor(),
        cursor_visible: g.cursor_visible(),
        bracketed_paste: g.bracketed_paste(),
    }
}

/// Feed `bytes` split at the (sorted, deduplicated, clamped) `cuts`, through one parser.
fn feed_split(rows: u16, cols: u16, bytes: &[u8], cuts: &[usize]) -> Grid {
    let mut cuts: Vec<usize> = cuts.iter().map(|&c| c % (bytes.len() + 1)).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut g = Grid::new(rows, cols);
    let mut parser = ChunkParser::new();
    let mut start = 0;
    for cut in cuts.into_iter().chain(std::iter::once(bytes.len())) {
        parser.advance(&mut g, &bytes[start..cut]);
        start = cut;
    }
    g
}

/// `n` cases, scaled by `RUSE_PROPTEST_SCALE` for the scheduled deep tier (#920).
fn cases(n: u32) -> ProptestConfig {
    let scale = std::env::var("RUSE_PROPTEST_SCALE")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1)
        .max(1);
    ProptestConfig {
        cases: n.saturating_mul(scale),
        ..ProptestConfig::default()
    }
}

/// One piece of terminal output, weighted toward multi-byte units a read boundary can cut: CSI (with `;`
/// and `:` parameters and private markers), OSC (BEL or ST), DCS, multi-byte and invalid UTF-8, C0, raw bytes.
fn piece() -> impl Strategy<Value = Vec<u8>> {
    let param = prop_oneof![
        Just(String::new()),
        (0u32..12).prop_map(|n| n.to_string()),
        Just("38:2::1:2:3".to_string()),
        Just("38:5:200".to_string()),
        Just("4:3".to_string()),
        Just("1049".to_string()),
        Just("2004".to_string()),
    ];
    let csi = (
        prop_oneof![Just(""), Just("?"), Just(">")],
        proptest::collection::vec(param, 0..5),
        proptest::sample::select(b"HfABCDGdJKLM@PXrmsuhl".to_vec()),
    )
        .prop_map(|(lead, ps, act)| {
            let mut b = format!("\x1b[{lead}{}", ps.join(";")).into_bytes();
            b.push(act);
            b
        });
    let osc = ("[ -~]{0,12}", prop_oneof![Just("\x07"), Just("\x1b\\")])
        .prop_map(|(body, end)| format!("\x1b]0;{body}{end}").into_bytes());
    prop_oneof![
        3 => csi,
        1 => osc,
        1 => Just(b"\x1bP1$r0m\x1b\\".to_vec()),
        1 => proptest::sample::select(b"M78c".to_vec()).prop_map(|c| vec![0x1b, c]),
        3 => proptest::sample::select(vec![
            "a", "xyz", "\u{4e2d}", "\u{e9}", "\u{301}", "\u{1f600}", "\t", "\r\n", "\n", "\x08",
        ])
        .prop_map(|s| s.as_bytes().to_vec()),
        1 => proptest::sample::select(vec![
            b"\xff".to_vec(), b"\xe4\xb8".to_vec(), b"\x80".to_vec(), b"\xf0\x9f\x98".to_vec(),
        ]),
        1 => proptest::collection::vec(any::<u8>(), 0..12),
    ]
}

proptest! {
    #![proptest_config(cases(256))]

    /// A byte stream fed in one call and the same stream fed in pieces cut anywhere give the same grid.
    #[test]
    fn chunk_split_is_equivalent_to_one_feed(
        size in (1u16..8, 1u16..10),
        pieces in proptest::collection::vec(piece(), 0..24),
        cuts in proptest::collection::vec(any::<usize>(), 0..12),
    ) {
        let bytes = pieces.concat();
        let whole = snapshot(&feed(size.0, size.1, &bytes));
        let split = snapshot(&feed_split(size.0, size.1, &bytes, &cuts));
        prop_assert_eq!(whole, split);
    }

    /// Every fixture stream, cut anywhere, still gives its one-feed grid.
    #[test]
    fn fixtures_survive_any_split(
        idx in 0..FIXTURES.len(),
        cuts in proptest::collection::vec(any::<usize>(), 1..8),
    ) {
        let (name, rows, cols, input) = FIXTURES[idx];
        let whole = snapshot(&feed(rows, cols, input));
        let split = snapshot(&feed_split(rows, cols, input, &cuts));
        prop_assert_eq!(whole, split, "fixture {}", name);
    }
}

/// Every split point of every fixture, exhaustively (fixtures are short).
#[test]
fn fixtures_survive_every_single_split() {
    for &(name, rows, cols, input) in FIXTURES {
        let whole = snapshot(&feed(rows, cols, input));
        for cut in 0..=input.len() {
            let split = snapshot(&feed_split(rows, cols, input, &[cut]));
            assert_eq!(whole, split, "fixture {name} cut at byte {cut}");
        }
    }
}

/// Byte-at-a-time is the worst split: every fixture fed one byte per call.
#[test]
fn fixtures_survive_byte_at_a_time() {
    for &(name, rows, cols, input) in FIXTURES {
        let whole = snapshot(&feed(rows, cols, input));
        let cuts: Vec<usize> = (1..input.len()).collect();
        let split = snapshot(&feed_split(rows, cols, input, &cuts));
        assert_eq!(whole, split, "fixture {name} fed byte at a time");
    }
}
