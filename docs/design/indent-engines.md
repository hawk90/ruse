---
doc: indent-engines
project: ruse
title: "ruse Indent Engines"
summary: >
  Which indenter places a line in the Vim profile, and how each one follows nvim v0.12.6: the C rules
  (a port of cindent, `indent_c.c`), the Lisp rules (`get_lisp_indent`), the Python script
  (`python#GetIndent`), and the tree-sitter node depth that stands in for the other languages' indent
  scripts. Lists what each one does not build, as documented divergences.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - editing-language.md
  - ../../spec/PRD.yaml
---

# Indent engines

## Which engine indents a line

nvim picks an indenter per buffer: `'lisp'` first, then `'indentexpr'` (an indent script), else the C
rules. Its `=` runs the C rules whenever `'indentexpr'` is empty, with or without `'cindent'`; the
opens and the typed keys run them only under `'cindent'`. ruse follows that order:

| buffer | `=` | `o`/`O`/`<CR>`, `cc`/`S`, typed keys | where |
|---|---|---|---|
| `'lisp'` set | Lisp rules | Lisp rules (under `'autoindent'`) | `crates/core/src/editor/lisp.rs` |
| C (`.c`, `.h`) | C rules | C rules, by `'cinkeys'` | `crates/core/src/editor/cindent.rs` |
| Python | Python script | Python script, by its `'indentkeys'` | `apps/tui/src/indent/python.rs` |
| Rust, JavaScript, Go, sh, CSS, JSON | node depth | node depth, by each file type's keys | `apps/tui/src/indent.rs` |
| no tree (or Markdown) | C rules | autoindent; C rules under `'cindent'` | core |

nvim's `indent/c.vim` and `indent/cpp.vim` only set `'cindent'`, so a C buffer is a cindent buffer.
Rust's and Java's scripts wrap cindent in an `'indentexpr'`, and JavaScript has its own script: ruse
indents those by node depth (Rust, JavaScript) and has no Java grammar.

## The C rules (#1163)

`cindent.rs` ports `get_c_indent` and every helper it calls (`cin_islabel`, `cin_isfuncdecl`,
`cin_is_cpp_baseclass`, `find_match`, …), plus the parts of `findmatchlimit` it runs: the backward search
for a `/*` comment start or a C++ raw string start, and the bracket search that skips strings, `'c'`
characters and escaped brackets (nvim's default `'cpoptions'`: no `%`, no `M`). It keeps nvim's
structure, a cursor the helpers move and 1-based line numbers, so a rule can be checked against the C
source line by line. The engine is a pure function of the buffer's lines, the options and a line
number; the core's `=` (`plan_reindent`) and the frontend's key path (`app/indent_keys.rs`) both call it.

- `'cinoptions'` is parsed as `parse_cino` does, into `CinOptions`. Every flag nvim knows is built:
  `> e n f { } ^ L : = l b g h N E p t i + c C / ( u U w W k m M j J ) * # P`, with `N`, `Ns`, `-N`,
  `N.Ms`.
- `'cinkeys'` is read as `in_cinkeys` does: `0`, `=word`, `=~word`, `^X`, `o`, `O`, `e`, `:`, `<name>`
  (one-byte names and `<:>`-style ones), `!^F`.
- `'cinwords'` and `'cinscopedecls'` are read where nvim reads them; `'comments'` steers the indent of
  a line inside a `/* */` comment.
- Opens follow `open_line` (after the autoindent, re-indent when `'cinkeys'` has `o`/`O`); a linewise
  `cc`/`S` follows `fix_indent` (no key check); a typed key follows `insert_do_cindent`, with nvim's
  `can_cindent` (a blank, `<Tab>` or `<BS>` in the indent, or `CTRL-T`/`CTRL-D`, stop the keys).
- Performance: a backward comment or raw-string search steps over a line without a `/*`, `*/` or
  `R"` in one go (each line's marks are worked out once per `=`). The searches stay bounded by
  `cino-)` and `cino-*` as in nvim.

### Not built (documented divergences)

- Keys with `*` in `'cinkeys'` (re-indent before the key goes in), and `!` keys other than `!^F`
  (re-indent instead of inserting): the frontend re-indents after a key only. nvim's default has none.
- `'indentkeys'` as an option: there is no `'indentexpr'`, so the cindent buffers read `'cinkeys'` and
  the other languages keep their built-in keys.
- `:set nocindent` in a C buffer still indents opens and keys by the C rules (nvim's filetype indent
  would leave plain autoindent). `'cindent'` is per view, and the C buffer's indenter comes from its
  file type, as the Python script's does.
- The comment-leader continuation `open_line` adds for a `//` comment after code under `'cindent'`
  with `'formatoptions'` `r`/`o` (nvim's default `'formatoptions'` has neither).

## The other engines

- Lisp (`lisp.rs`, #1212): `get_lisp_indent` and its Lisp bracket search. `'lispoptions'` only
  switches Lisp buffers to an `'indentexpr'`, which ruse has none of, so it is not built.
- Python (`indent/python.rs`, #1205): `python#GetIndent` with the `g:python_indent` defaults.
- Node depth (`indent.rs`): the distinct start rows of the multi-line nodes around a line, with the
  per-language rules of #1206/#1214. JavaScript's `GetJavascriptIndent` continuation after a line ending
  in an operator (`var o =` then `{` at 4) is not modelled (`ik_js_brace_object_value`).
