//! Command-line completion through the real key path (#613): file, directory and buffer names, the
//! wildmenu keys, and `c_CTRL-D`. Expected lines are nvim v0.12.6's (probed headless with `-u NONE`
//! over the same tree); command and option completion are pinned by the `wild_*` oracle fixtures.

use ruse_tui::headless::Headless;
use std::path::PathBuf;

/// A temp tree like the probe's: `dir/{fo o.md,foo.txt,foobar.rs,bar.c,food/}` and `sub/{a1,a2}`.
struct Tree(PathBuf);

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = std::env::temp_dir().join(format!("ruse-wild-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["dir/food", "sub"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        for f in [
            "dir/fo o.md",
            "dir/foo.txt",
            "dir/foobar.rs",
            "dir/bar.c",
            "sub/a1",
            "sub/a2",
        ] {
            std::fs::write(root.join(f), b"x\n").unwrap();
        }
        Tree(root)
    }

    /// The tree's root as typed on the command line, with its trailing `/`.
    fn at(&self) -> String {
        format!("{}/", self.0.display())
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The open command line's text, or `None` once it closed.
fn line(h: &Headless) -> Option<String> {
    h.engine().cmdline().map(|(_, t, _)| t.to_string())
}

fn wild_open(h: &Headless) -> bool {
    h.engine().cmdline_wild().is_some()
}

#[test]
fn file_names_cycle_through_the_wildmenu() {
    let t = Tree::new("cycle");
    let r = t.at();
    let mut h = Headless::vim("abc");
    h.keys(&format!(":e {r}dir/fo<Tab>"));
    // nvim: `dir/fo\ o.md`, `dir/foo.txt`, `dir/foobar.rs`, `dir/food/`, then back to `dir/fo`.
    assert_eq!(line(&h).unwrap(), format!("e {r}dir/fo\\ o.md"));
    let (entries, sel, _) = h.engine().cmdline_wild().unwrap();
    assert_eq!(entries, ["fo\\ o.md", "foo.txt", "foobar.rs", "food/"]);
    assert_eq!(sel, Some(0));
    for want in ["foo.txt", "foobar.rs", "food/", "fo"] {
        h.keys("<Tab>");
        assert_eq!(line(&h).unwrap(), format!("e {r}dir/{want}"));
    }
    h.keys("<S-Tab>");
    assert_eq!(line(&h).unwrap(), format!("e {r}dir/food/"));
    h.keys("<C-e>");
    assert_eq!(line(&h).unwrap(), format!("e {r}dir/fo"));
    assert!(!wild_open(&h));
    // `<Esc>` on an open wildmenu abandons the line.
    h.keys("<Tab><Esc>");
    assert_eq!(line(&h), None);
}

#[test]
fn a_single_match_completes_without_a_menu() {
    let t = Tree::new("single");
    let r = t.at();
    let mut h = Headless::vim("abc");
    h.keys(&format!(":e {r}su<Tab>"));
    assert_eq!(line(&h).unwrap(), format!("e {r}sub/"));
    assert!(!wild_open(&h));
    // No match: the line stays and no `<Tab>` is typed.
    h.keys("zz<Tab>");
    assert_eq!(line(&h).unwrap(), format!("e {r}sub/zz"));
}

#[test]
fn down_enters_a_directory_and_up_leaves_it() {
    let t = Tree::new("updown");
    let r = t.at();
    let mut h = Headless::vim("abc");
    // `food/` is last; `<Down>` on it completes inside it (empty: the line stays at `food/`).
    h.keys(&format!(":e {r}dir/fo<S-Tab><Down>"));
    assert_eq!(line(&h).unwrap(), format!("e {r}dir/food/"));
    assert!(!wild_open(&h));
    // `<Down>` on a file keeps it.
    h.keys(&format!("<Esc>:e {r}dir/fo<Tab><Down>"));
    assert_eq!(line(&h).unwrap(), format!("e {r}dir/fo\\ o.md"));
    // `<Up>` from `sub/a1` completes the directory above `sub/`, the tree's root.
    h.keys(&format!("<Esc>:e {r}sub/a<Tab><Up>"));
    assert_eq!(line(&h).unwrap(), format!("e {r}dir/"));
    assert!(wild_open(&h));
}

#[test]
fn ctrl_d_lists_tails_and_ctrl_l_ctrl_a_insert() {
    let t = Tree::new("list");
    let r = t.at();
    let mut h = Headless::vim("abc");
    h.keys(&format!(":e {r}dir/fo<C-d>"));
    assert_eq!(line(&h).unwrap(), format!("e {r}dir/fo"));
    let listed: Vec<&str> = h
        .engine()
        .cmdline_listing()
        .unwrap()
        .iter()
        .map(|i| i.list.as_str())
        .collect();
    assert_eq!(listed, ["fo o.md", "foo.txt", "foobar.rs", "food/"]);
    h.keys("<Esc>");
    h.keys(&format!(":e {r}dir/foob<C-l>"));
    assert_eq!(line(&h).unwrap(), format!("e {r}dir/foobar.rs"));
    h.keys(&format!("<Esc>:e {r}sub/<C-a>"));
    assert_eq!(line(&h).unwrap(), format!("e {r}sub/a1 {r}sub/a2"));
}

#[test]
fn buffer_names_and_percent() {
    let t = Tree::new("bufs");
    let r = t.at();
    let mut h = Headless::vim("abc");
    h.keys(&format!(":e {r}dir/foo.txt<CR>:e {r}sub/a1<CR>"));
    h.keys("<Esc>:b foo<Tab>");
    assert_eq!(line(&h).unwrap(), format!("b {r}dir/foo.txt"));
    h.keys("<Esc>:e %<Tab>");
    assert_eq!(line(&h).unwrap(), format!("e {r}sub/a1"));
    h.keys("<Esc>:e #<Tab>");
    assert_eq!(line(&h).unwrap(), format!("e {r}dir/foo.txt"));
}

#[test]
fn search_and_expression_lines_do_not_complete() {
    let mut h = Headless::vim("abc");
    h.keys("/ab<Tab>");
    assert_eq!(line(&h).unwrap(), "ab");
    assert!(!wild_open(&h));
}
