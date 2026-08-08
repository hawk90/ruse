#!/usr/bin/env python3
"""parity extract_emacs — the Emacs census, by runtime introspection (D-043).

WHY THIS LOOKS NOTHING LIKE extract_neovim.py
---------------------------------------------
Neovim publishes its own surfaces as machine-readable tables in the source tree (index.txt,
map.txt, options.lua, ex_cmds.lua), so its census is a PARSE. Emacs publishes nothing equivalent:
its surfaces exist only in a running image. So this extractor is a PROBE — `source_of_record: R`
for four of five surfaces — and that is not a weaker method, it is the correct one for this
upstream (upstreams.yaml#source_of_record already establishes that the source of record is per
item type, empirically, not a uniform rule).

THE DENOMINATOR, WHICH WAS THE BLOCKER
--------------------------------------
upstreams.yaml previously recorded the Emacs census as blocked: "`emacs -Q` + mapatoms / full
bundled load / autoload cookies are three different denominators, differing by multiples". That
was true, and it was the wrong question. Two corrections dissolve it:

  1. THE LOAD SET IS THE SCOPE LIST. The three candidate baselines were three guesses at "how much
     Emacs is Emacs". But census_scope.include already answers that — it names the libraries that
     are editor semantics. So the baseline is `emacs -Q --batch` plus `require` of EXACTLY those
     libraries. It is deterministic, and it is reviewable because changing it means editing the
     scope list, which the LLM rules already require a reason for.

  2. COMMANDS ARE DERIVED, NOT ENUMERATED. Asking "how many commands does Emacs have" yields 3,011
     (`-Q` preloaded) / 12,087 (`(interactive` in tree) / 9,371 (defcustom) — no defensible answer,
     because a command nobody can reach is not a surface. Asking "which commands are reachable from
     a bound key in an in-scope keymap" yields ONE number, and it is the number that matters for
     parity. Measured: 803 commands, collapsed from 1,971 command bindings.

WHAT THE NUMBERS SAY (the check that the scope cut is honest, not convenient)
----------------------------------------------------------------------------
Emacs surfaces land beside their Neovim counterparts rather than dwarfing them:

    core editing namespaces   1,106   vs  mode_key       708
    minibuffer family           233   vs  cmdline         59
    commands (derived)          803   vs  ex_command     557
    option                      434   vs  option         374
    hook                        106   vs  event          141

...and one number has NO counterpart at all: 613 keyboard bindings live in MAJOR-MODE maps
(dired-mode-map 138, bookmark-bmenu-mode-map 64, kmacro-menu-mode-map 44, ...). Vim has nowhere to
put those, because its eight namespaces are disjoint and selected by editor STATE, while Emacs's
are a stack selected by what the BUFFER is. That 613 is the layered-vs-disjoint difference made
quantitative, and it is the reason this census was worth running at all — see
concepts/irreconcilable.yaml#CONCEPT-KEYMAP-DISPATCH.

BINARY IDENTITY
---------------
The tree is not fetched: R-primary surfaces come from the installed binary, not the clone. That
opens a hole the pin discipline would otherwise close — an inventory could claim a revision it was
never generated from. So every document emitted here carries `derived_from: runtime-binary` plus
the probed `binary_version`, and `gov parity_discovery` FAILS if that version does not match the
pin's version_label. The build is still not byte-identical to the pinned commit; that is recorded
as `binary_identity: unverified-build` rather than hidden.

KEYMAP PARTITION
----------------
Named keymaps nest (ctl-x-map is reachable from global-map AND is itself a named map). Walking
naively double-counts. So a walk STOPS at any sub-keymap that is itself a named in-scope keymap and
records a prefix pointer instead — each binding belongs to exactly one namespace. This is the same
partition discipline surface_cover demands of families.yaml.

Regenerate:  python3 tools/parity/extract_emacs.py
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from collections import Counter

import yaml

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "rusekit"))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))

from rusekit import render, repo  # noqa: E402

EDITOR = "emacs"
OUT_DIR = "spec/parity/inventory/emacs"
UPSTREAMS = "spec/parity/upstreams.yaml"

# Preserved across regeneration — the human half of the census (mirrors extract_neovim.py).
HUMAN_FIELDS = ("status", "ruse", "note", "superseded_by", "surface_locked", "family", "secondary")

# The active-keymap precedence stack. This is the ONE surface here whose source of record is D, not
# R: the ordering is defined by the command loop (src/keyboard.c) and documented in
# (elisp) "Searching the Active Keymaps"; no runtime call returns it as an ordered list.
# `current-active-maps` returns the maps active RIGHT NOW, which in batch is a degenerate 2. So the
# order below is transcribed from the manual and each entry is corroborated at runtime by checking
# the symbol is bound — attestation [D, R], never R alone.
#
# This surface is the whole reason the Emacs census is worth running: Vim's eight namespaces are
# DISJOINT (exactly one is active), Emacs's are LAYERED (an ordered stack, all consulted). See
# concepts/irreconcilable.yaml#CONCEPT-KEYMAP-DISPATCH.
KEYMAP_TIERS = [
    ("overriding-terminal-local-map", "terminal-local override; consulted first, suppresses all below"),
    ("overriding-local-map", "buffer-local override; suppresses minor/local/global"),
    ("keymap", "text-property or overlay `keymap` property at point"),
    ("emulation-mode-map-alists", "emulation packages (evil-mode lives here); above minor modes"),
    ("minor-mode-overriding-map-alist", "major mode overriding a minor mode's binding"),
    ("minor-mode-map-alist", "active minor modes, in alist order"),
    ("local-map", "text-property or overlay `local-map` property at point"),
    ("current-local-map", "the major mode's keymap"),
    ("global-map", "the global fallback"),
]

ELISP = r"""
(require 'cl-lib)
(require 'json)
(setq load-prefer-newer t)

;; The load set IS the scope list — see the module docstring. Failures are reported, never silent:
;; a library that will not load changes the denominator and must show up in the manifest.
(defvar ruse-load-failures nil)
(dolist (lib '(%LIBS%))
  (condition-case e (require lib)
    (error (push (format "%s: %S" lib e) ruse-load-failures))))

;; Scope is a basename SET, not a regex: elisp regexp syntax treats `(` and `|` as literals, so a
;; PCRE-shaped pattern would match nothing and emit an empty census that looks like a real one.
(defvar ruse-scope (let ((h (make-hash-table :test 'equal)))
                     (dolist (n '(%SCOPE%)) (puthash n t h)) h))
(defun ruse-file (s) (ignore-errors (symbol-file s)))
(defun ruse-in-scope (s)
  (let ((f (ruse-file s))) (and f (gethash (file-name-base f) ruse-scope))))
(defun ruse-src (s)
  (let ((f (ruse-file s))) (if f (file-name-nondirectory f) "")))

;; ---- named keymap variables in scope -------------------------------------------------
(defvar ruse-named nil)
(mapatoms (lambda (s)
  (when (and (boundp s) (ruse-in-scope s)
             (keymapp (ignore-errors (symbol-value s))))
    (push s ruse-named))))
(setq ruse-named (sort ruse-named #'string<))
(defvar ruse-named-set (let ((h (make-hash-table :test 'eq)))
                         (dolist (s ruse-named) (puthash s t h)) h))

;; A keymap VALUE -> the named variable that owns it, so a nested map can be recognised as a
;; namespace boundary rather than recursed into (the partition rule).
(defvar ruse-owner (make-hash-table :test 'eq))
(dolist (s ruse-named) (puthash (symbol-value s) s ruse-owner))

(defvar ruse-bindings nil)
(defun ruse-walk (map owner pfx)
  (when (keymapp map)
    (map-keymap
     (lambda (k d)
       (let ((kd (condition-case nil
                     (if (consp k) (format "%s..%s" (key-description (vector (car k)))
                                           (key-description (vector (cdr k))))
                       (key-description (vector k)))
                   (error nil))))
         (when kd
           (let ((seq (if (string= pfx "") kd (concat pfx " " kd)))
                 (sub (and (keymapp d) d)))
             (cond
              ;; a nested map that is itself a named namespace -> prefix pointer, do not recurse
              ((and sub (gethash sub ruse-owner))
               (push (list :map (symbol-name owner) :key seq :kind "prefix"
                           :target (symbol-name (gethash sub ruse-owner))) ruse-bindings))
              (sub (ruse-walk sub owner seq))
              ((symbolp d)
               (when d
                 (push (list :map (symbol-name owner) :key seq
                             :kind (if (commandp d) "command" "binding")
                             :target (symbol-name d)) ruse-bindings))))))))
     map)))
(dolist (s ruse-named) (ruse-walk (symbol-value s) s ""))

;; ---- commands: DERIVED from the bindings above, never enumerated independently -------
(defvar ruse-cmds nil)
(dolist (b ruse-bindings)
  (when (string= (plist-get b :kind) "command")
    (cl-pushnew (intern (plist-get b :target)) ruse-cmds)))

;; ---- options / hooks, scoped ----------------------------------------------------------
(defvar ruse-opts nil) (defvar ruse-hooks nil)
(mapatoms (lambda (s)
  (when (ruse-in-scope s)
    (when (custom-variable-p s) (push s ruse-opts))
    (when (and (boundp s) (string-match-p "-\\(hook\\|functions\\)\\'" (symbol-name s)))
      (push s ruse-hooks)))))

(defun ruse-doc1 (s kind)
  (let ((d (ignore-errors (documentation-property s kind))))
    (when (and (stringp d) (> (length d) 0))
      (car (split-string d "\n")))))

;; Output is built as STRING-KEYED ALISTS wrapped in VECTORS, never as plists. `json-encode` decides
;; alist-vs-plist-vs-array by inspecting shape, and a list of plists satisfies `json-alist-p` (each
;; element is a cons whose car is an atom), so it silently encodes the whole collection as one
;; object keyed by the first plist key. Vectors are unambiguously arrays; string-keyed alists are
;; unambiguously objects.
(defun ruse-str (x) (if (stringp x) x (if x (format "%s" x) "")))
(princ (json-encode
  (list
   (cons "binary_version" emacs-version)
   (cons "load_failures" (vconcat (nreverse ruse-load-failures)))
   (cons "tiers"
         (vconcat (mapcar (lambda (n) (list (cons "name" n)
                                            (cons "bound" (if (boundp (intern n)) t :json-false))))
                          '(%TIERS%))))
   (cons "keymaps"
         (vconcat (mapcar (lambda (s)
                            (list (cons "name" (symbol-name s))
                                  (cons "src" (ruse-src s))
                                  (cons "doc" (ruse-str (ruse-doc1 s 'variable-documentation)))))
                          ruse-named)))
   (cons "bindings"
         (vconcat (mapcar (lambda (b)
                            (list (cons "map" (plist-get b :map))
                                  (cons "key" (plist-get b :key))
                                  (cons "kind" (plist-get b :kind))
                                  (cons "target" (plist-get b :target))))
                          (nreverse ruse-bindings))))
   (cons "commands"
         (vconcat (mapcar (lambda (s)
                            (list (cons "name" (symbol-name s))
                                  (cons "src" (ruse-src s))
                                  (cons "subr" (if (subrp (ignore-errors (symbol-function s)))
                                                   t :json-false))
                                  (cons "interactive_form"
                                        (let ((f (ignore-errors (cadr (interactive-form s)))))
                                          (if (stringp f) f "")))
                                  (cons "doc" (ruse-str
                                               (let ((d (ignore-errors (documentation s))))
                                                 (and (stringp d)
                                                      (car (split-string d "\n"))))))))
                          (sort ruse-cmds #'string<))))
   (cons "options"
         (vconcat (mapcar (lambda (s)
                            (list (cons "name" (symbol-name s))
                                  (cons "src" (ruse-src s))
                                  (cons "type" (format "%S" (or (get s 'custom-type) 'sexp)))
                                  (cons "default"
                                        (condition-case nil
                                            (let ((print-length 12) (print-level 3))
                                              (format "%S" (default-value s)))
                                          (error "")))
                                  (cons "doc" (ruse-str
                                               (ruse-doc1 s 'variable-documentation)))))
                          (sort ruse-opts #'string<))))
   (cons "hooks"
         (vconcat (mapcar (lambda (s)
                            (list (cons "name" (symbol-name s))
                                  (cons "src" (ruse-src s))
                                  (cons "doc" (ruse-str
                                               (ruse-doc1 s 'variable-documentation)))))
                          (sort ruse-hooks #'string<)))))))
"""


def load_upstreams() -> dict:
    with open(repo.path(UPSTREAMS), encoding="utf-8") as fh:
        return yaml.safe_load(fh) or {}


def probe(libs: list[str], scope_files: list[str]) -> dict:
    """Run the introspection program in a bare batch image.

    Batch is safe HERE and only here: every hazard recorded in upstreams.yaml#oracles.emacs is an
    EXECUTION hazard (`execute-kbd-macro` silently empties the buffer; `read-from-minibuffer` hangs
    forever). This program never executes a command — it reads symbol tables and keymap objects.
    Census by introspection and behaviour by oracle are different problems; only the second needs a
    pty, and it stays blocked.
    """
    src = (ELISP
           .replace("%LIBS%", " ".join(libs))
           .replace("%SCOPE%", " ".join(f'"{n}"' for n in scope_files))
           .replace("%TIERS%", " ".join(f'"{n}"' for n, _ in KEYMAP_TIERS)))
    out = subprocess.run(["emacs", "-Q", "--batch", "--eval", f"(progn {src})"],
                         capture_output=True, text=True, timeout=300, stdin=subprocess.DEVNULL)
    if out.returncode != 0 or not out.stdout.strip():
        raise SystemExit(f"emacs probe failed (rc={out.returncode}):\n{out.stderr[-2000:]}")
    return json.loads(out.stdout)


def _slug(s: str) -> str:
    s = re.sub(r"[^A-Za-z0-9]+", "-", s).strip("-").lower()
    return s or "x"


# Emacs keymaps bind PSEUDO-EVENTS alongside real keys: `<menu-bar>`, `<tool-bar>`, `<mode-line>`
# and mouse events all live in the same keymap objects that hold `C-x C-f`. Dropping them would be
# convenient and wrong — discovery is strict, and a menu descriptor IS a real upstream surface, just
# not a keyboard one. So they are enumerated and CLASSIFIED, which keeps the denominator honest
# while letting a family select only the subset it claims. Vim has no counterpart to this because
# its menu surface is a separate `:menu` command family, not entries in the same keymap.
EVENT_CLASS = (
    ("menu", ("<menu-bar>",)),
    ("tool-bar", ("<tool-bar>",)),
    ("chrome", ("<mode-line>", "<header-line>", "<tab-line>", "<vertical-scroll-bar>",
                "<horizontal-scroll-bar>", "<vertical-line>", "<left-fringe>", "<right-fringe>",
                "<left-margin>", "<right-margin>", "<tab-bar>")),
    ("mouse", ("mouse-", "wheel-")),
    ("remap", ("<remap>",)),
)


# Which Emacs keymaps are the counterpart of a Vim keymap namespace, and which have no counterpart
# at all. This split is the census's main structural finding, so it is a FIELD on every binding
# rather than a note: `core` compares to Vim's Normal/Operator/Visual namespaces, `minibuffer`
# compares to Cmdline, and `major-mode` compares to NOTHING — Vim selects a namespace by editor
# state, Emacs selects one by what the buffer is. A family claiming keyboard parity must say which
# of the three it means.
CORE_NAMESPACES = {
    "global-map", "esc-map", "ctl-x-map", "ctl-x-4-map", "ctl-x-5-map", "ctl-x-r-map",
    "ctl-x-x-map", "goto-map", "search-map", "narrow-map", "tab-prefix-map", "window-prefix-map",
    "abbrev-map", "mode-specific-map", "isearch-mode-map", "universal-argument-map",
}


def _namespace_group(keymap: str) -> str:
    if keymap in CORE_NAMESPACES:
        return "core"
    if keymap.startswith("minibuffer") or keymap.startswith("read"):
        return "minibuffer"
    return "major-mode"


def _event_class(key: str) -> str:
    for name, prefixes in EVENT_CLASS:
        if any(p in key for p in prefixes):
            return name
    return "key"


def _dedupe(items: list[dict]) -> list[dict]:
    """Ids must be unique; a keymap can bind the same slug twice (`C-x` vs `C-X`)."""
    seen: dict[str, int] = {}
    for it in items:
        base = it["id"]
        n = seen.get(base, 0)
        seen[base] = n + 1
        if n:
            it["id"] = f"{base}~{n + 1}"
    return items


def build(p: dict) -> list[tuple[str, str, str, list[dict]]]:
    tier_bound = {t["name"]: t["bound"] for t in p["tiers"]}
    tiers = [{
        "id": f"emacs.keymaptier.{i:02d}.{_slug(name)}",
        "surface": "keymap_tier",
        "rank": i,
        "name": name,
        "role": role,
        # `keymap` / `local-map` are text/overlay PROPERTIES, not variables — unbound is correct
        # for them and is not a miss. Recorded so the distinction survives regeneration.
        "runtime_bound": bool(tier_bound.get(name)),
        "kind": "property" if name in ("keymap", "local-map") else "variable",
        "attestation": ["D", "R"],
    } for i, (name, role) in enumerate(KEYMAP_TIERS, start=1)]

    keymaps = [{
        "id": f"emacs.keymap.{_slug(k['name'])}",
        "surface": "keymap",
        "name": k["name"],
        "defined_in": k.get("src") or "",
        "desc": (k.get("doc") or "").strip(),
        "attestation": ["R"],
    } for k in p["keymaps"]]

    bindings = _dedupe([{
        "id": f"emacs.key.{_slug(b['map'])}.{_slug(b['key'])}",
        "surface": "key_binding",
        "keymap": b["map"],
        "key": b["key"],
        "kind": b["kind"],
        "event_class": _event_class(b["key"]),
        "namespace_group": _namespace_group(b["map"]),
        "binds": b["target"],
        "attestation": ["R"],
    } for b in p["bindings"]])

    commands = [{
        "id": f"emacs.command.{_slug(c['name'])}",
        "surface": "command",
        "name": c["name"],
        "defined_in": c.get("src") or "",
        "builtin_c": bool(c.get("subr")),
        "interactive_spec": c.get("interactive_form") or "",
        "desc": (c.get("doc") or "").strip(),
        # The load-bearing field: this surface has no independent denominator. Every item is here
        # because a key in an in-scope keymap reaches it.
        "derived_from": "key_binding",
        "attestation": ["R"],
    } for c in p["commands"]]

    options = [{
        "id": f"emacs.option.{_slug(o['name'])}",
        "surface": "option",
        "name": o["name"],
        "defined_in": o.get("src") or "",
        "type": o.get("type") or "",
        "default": o.get("default") or "",
        "desc": (o.get("doc") or "").strip(),
        "attestation": ["R"],
    } for o in p["options"]]

    hooks = [{
        "id": f"emacs.hook.{_slug(h['name'])}",
        "surface": "hook",
        "name": h["name"],
        "defined_in": h.get("src") or "",
        "desc": (h.get("doc") or "").strip(),
        "attestation": ["R"],
    } for h in p["hooks"]]

    return [
        ("keymap_tier.yaml", "keymap_tier",
         "D: (elisp) Searching the Active Keymaps + src/keyboard.c (R: each symbol bound at the pin)",
         tiers),
        ("keymap.yaml", "keymap", "R: mapatoms + keymapp, scoped to census_scope.include", keymaps),
        ("key_binding.yaml", "key_binding", "R: map-keymap walk, partitioned at named-keymap boundaries",
         bindings),
        ("command.yaml", "command", "R: DERIVED from key_binding (no independent denominator)", commands),
        ("option.yaml", "option", "R: custom-variable-p, scoped to census_scope.include", options),
        ("hook.yaml", "hook", "R: -hook/-functions defvars, scoped to census_scope.include", hooks),
    ]


def load_existing(fname: str) -> dict[str, dict]:
    path = repo.path(OUT_DIR, fname)
    if not os.path.exists(path):
        return {}
    with open(path, encoding="utf-8") as fh:
        doc = yaml.safe_load(fh) or {}
    return {i["id"]: i for i in (doc.get("items") or []) if "id" in i}


def merge(fresh: list[dict], prior: dict[str, dict]) -> tuple[list[dict], int, int]:
    """Identical contract to extract_neovim.merge: machine fields from upstream, human fields carried
    forward, vanished ids RETAINED with `upstream_gone` so a bump never silently deletes a surface
    ruse already reasoned about."""
    out, fresh_ids = [], {i["id"] for i in fresh}
    for item in fresh:
        old = prior.get(item["id"], {})
        for f in HUMAN_FIELDS:
            if f in old:
                item[f] = old[f]
        item.setdefault("status", "unclassified")
        out.append(item)
    gone = 0
    for iid, old in sorted(prior.items()):
        if iid not in fresh_ids:
            old["upstream_gone"] = True
            out.append(old)
            gone += 1
    return out, len(fresh_ids - set(prior)), gone


def write_surface(fname: str, title: str, source: str, items: list[dict],
                  meta: dict, dry: bool) -> dict:
    prior = load_existing(fname)
    merged, added, gone = merge(items, prior)
    stats = {"total": len(merged), "added": added, "upstream_gone": gone,
             "unclassified": sum(1 for i in merged if i.get("status") == "unclassified")}
    if dry:
        return stats
    doc = {
        "version": 1,
        "generated": True,
        "generator": "tools/parity/extract_emacs.py",
        "upstream": EDITOR,
        "revision": meta["revision"],
        "version_label": meta["version_label"],
        # Checked by gov parity_discovery: an R-primary census has no tree to diff against, so the
        # binary it was probed from must be declared and must match the pin's label.
        "derived_from": "runtime-binary",
        "binary_version": meta["binary_version"],
        "binary_identity": "unverified-build",
        "baseline": meta["baseline"],
        "surface": title,
        "source_of_record": source,
        "items": merged,
    }
    os.makedirs(repo.path(OUT_DIR), exist_ok=True)
    header = (
        "# GENERATED — do not hand-edit item enumeration. Regenerate:\n"
        "#   python3 tools/parity/extract_emacs.py\n"
        f"# Human-owned fields ({', '.join(HUMAN_FIELDS)}) ARE preserved across regeneration; the\n"
        "# enumeration is not. `status: unclassified` is legitimate and expected — discovery is\n"
        "# strict, classification is lazy (and locked per surface by `ruse gov parity_discovery`).\n"
        "# This census is RUNTIME-DERIVED (no source tree): the baseline is `emacs -Q --batch` plus\n"
        "# `require` of exactly the census_scope.include libraries. Changing that load set changes\n"
        "# the denominator, so it lives in upstreams.yaml where it needs a reason.\n"
    )
    with open(repo.path(OUT_DIR, fname), "w", encoding="utf-8") as fh:
        fh.write(header)
        yaml.safe_dump(doc, fh, sort_keys=False, allow_unicode=True, width=120)
    return stats


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="parity extract_emacs")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args(argv if argv is not None else sys.argv[1:])

    ups = load_upstreams()
    cfg = (ups.get("upstreams") or {}).get(EDITOR) or {}
    label = cfg.get("version_label", "?")
    den = cfg.get("denominator") or {}
    libs = den.get("load_set") or []
    scope_files = den.get("scope_files") or []
    if not libs or not scope_files:
        render.fail(f"{UPSTREAMS} emacs.denominator must declare `load_set` and `scope_files` — "
                    f"the census baseline is not implicit")
        return 1

    render.heading(f"parity extract — {EDITOR} @ {label} (runtime introspection)")

    p = probe(libs, scope_files)
    binver = p.get("binary_version") or "?"
    want = label.replace("emacs-", "")
    if binver != want:
        render.fail(f"binary is emacs {binver} but the pin is {label} — an R-primary census from a "
                    f"different build is not answerable to its pin; install {label} or bump the pin")
        return 1
    render.field("Runtime probe", f"emacs {binver} — matches pin label {label}")
    render.field("Baseline", f"emacs -Q --batch + require of {len(libs)} in-scope libraries")

    if p.get("load_failures"):
        for f in p["load_failures"]:
            render.bullet(f"load failed, denominator is short by this library: {f}", mark="!")
        render.fail("the declared load set did not load — the census would understate the surface")
        return 1

    meta = {"revision": cfg.get("revision", "?"), "version_label": label,
            "binary_version": binver,
            "baseline": f"emacs -Q --batch + require({' '.join(libs)})"}

    total = 0
    for fname, title, source, items in build(p):
        st = write_surface(fname, title, source, items, meta, args.dry_run)
        total += st["total"]
        extra = f", +{st['added']} new" if st["added"] and load_existing(fname) else ""
        gone = f", {st['upstream_gone']} upstream-gone" if st["upstream_gone"] else ""
        render.bullet(f"{title:<12} {st['total']:>5} items ({st['unclassified']} unclassified{extra}{gone})")

    render.field("Cross-checks", "")
    ncmd = len(p["commands"])
    nbind = sum(1 for b in p["bindings"] if b["kind"] == "command")
    classes = Counter(_event_class(b["key"]) for b in p["bindings"])
    render.bullet("key_binding by event class: "
                  + ", ".join(f"{k} {v}" for k, v in classes.most_common())
                  + " — only `key` is a keyboard parity surface; the rest are enumerated (discovery "
                    "is strict) but a family that claims keyboard parity selects `key` alone")
    groups = Counter(_namespace_group(b["map"]) for b in p["bindings"]
                     if _event_class(b["key"]) == "key")
    render.bullet(f"keyboard bindings by namespace group: core {groups.get('core', 0)} "
                  f"(vs nvim mode_key 708) · minibuffer {groups.get('minibuffer', 0)} "
                  f"(vs nvim cmdline 59) · major-mode {groups.get('major-mode', 0)} (vs NOTHING)")
    render.bullet(f"the {groups.get('major-mode', 0)} major-mode bindings are the structural finding: "
                  f"Vim selects a namespace by editor STATE, Emacs by what the BUFFER is, so these "
                  f"have no Vim counterpart to compare against — CONCEPT-KEYMAP-DISPATCH")
    render.bullet(f"commands are DERIVED: {nbind} command bindings collapse to {ncmd} distinct "
                  f"commands — this surface has no independent denominator by construction")
    render.bullet(f"unbound-but-interactive commands are OUT of the census by design: a command no "
                  f"key reaches is not a parity surface (that is what made 3,011/12,087/9,371 "
                  f"unanswerable)")
    render.ok(f"{total} upstream items enumerated"
              + (" (dry run — nothing written)" if args.dry_run else f" → {OUT_DIR}/"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
