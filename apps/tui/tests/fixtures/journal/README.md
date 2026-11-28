# RJ01 journals written by earlier builds (#1318)

Real recovery journals, one per RJ01 schema version, replayed by
`apps/tui/src/persist/rj01/compat_tests.rs` to check the compatibility promise of RFC-0023 §2
(INV-ADDITIVE): a newer build replays every older minor version of the same major.

| File      | `schema_ver` | Written by the `ruse` binary built at                                   |
|-----------|--------------|--------------------------------------------------------------------------|
| `v1_0.rj` | 1.0          | `cd7141b9` feat(persist): per-transaction RJ01 journal (#1200)           |
| `v1_1.rj` | 1.1          | `e7f21edf` the last 1.1 commit, parent of `1743eb16` (#1667, 1.2)        |
| `v1_2.rj` | 1.2          | `d3b61329` main when the fixtures were made (1.2 since #1667)            |

SHA-256: `v1_0.rj` 057001c3…ec4e2f, `v1_1.rj` cf5f2dbd…58657a2, `v1_2.rj` f8ff6825…f0736c.

## How they were produced

1. `git worktree add --detach <tmp> <commit>`, then `cargo build -p ruse-tui --bin ruse` there.
2. A Python `pty.fork` driver ran that binary on a 24x80 PTY in `/tmp/ruse-rj-fixture`, with `HOME`,
   `XDG_STATE_HOME` and `XDG_CONFIG_HOME` set to `/tmp/ruse-rj-fixture/home`, `TERM=xterm-256color`,
   `LANG=C.UTF-8`, on `doc.txt` holding `alpha\r\nbeta\r\n` (so the file opens as dos).
3. Keys, each followed by 0.4 s: `Ihello <Esc>`, `:w<CR>`, `jA again<Esc>`, `oworld<Esc>`, `u`,
   `:set ff=unix<CR>`. Then 6 s idle (past the 4 s idle sync, so a Marker is written) and SIGKILL, which
   runs no handler: the journal is what the writer had synced, with no crash Checkpoint.
4. The one `*.rj` left in `home/ruse/journal/` was copied out.
5. Scrub: the Header record's `host` (the machine's host name) was replaced with `fixture-host`, and that
   record's `payload_len`, `header_crc` and `payload_crc` recomputed. Every other byte is as the binary
   wrote it. This also keeps the owner from ever looking like a live session on the machine running the
   tests.

Each journal holds: Header (dos format), a Checkpoint of the saved text `hello alpha\nbeta\n` (the `:w`
restarts the journal), 12 Fresh edit records (` again`, then the line `world`), 6 Navigate records (the `u`
undoing the `o` insert node by node), from 1.1 on a Meta file-format record (unix; 1.0 did not journal a
`:set ff`), and a Marker. They replay to `hello alpha\nbeta again\n`.

Not covered by a real session: a non-default 1.2 encoding name (no command sets `'fileencoding'` yet, so
the field is empty), a 1.1-only origin such as a reload, marks (not journaled). Those are covered by the
encoder round-trip tests in `rj01.rs`.

To add a fixture for a new schema version, repeat the steps with that version's binary and add a
`check_session` call for it.
