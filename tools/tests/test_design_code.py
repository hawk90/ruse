import unittest

from . import _support  # noqa: F401  (sys.path side effect)
from rusekit.gov import design_code as dc  # noqa: E402


class TestParseRustTypes(unittest.TestCase):
    def test_struct_fields_and_pub_and_comments(self):
        src = """
        /// doc comment
        pub struct Foo {
            pub a: u32,
            pub(crate) b: Vec<Bar>,
            c: Option<Baz>,  // trailing
        }
        """
        t = dc.parse_rust_types(src)
        self.assertEqual(t["Foo"]["kind"], "struct")
        self.assertEqual(t["Foo"]["members"], {"a", "b", "c"})

    def test_enum_variants(self):
        src = "enum Origin { UserInput, Macro, Plugin(Detail), Lsp { id: u32 } }"
        t = dc.parse_rust_types(src)
        self.assertEqual(t["Origin"]["kind"], "enum")
        self.assertEqual(t["Origin"]["members"], {"UserInput", "Macro", "Plugin", "Lsp"})

    def test_tuple_struct_has_no_named_members(self):
        t = dc.parse_rust_types("pub struct Revision(pub u64);")
        self.assertEqual(t["Revision"]["members"], set())

    def test_generics_are_not_members(self):
        t = dc.parse_rust_types("struct Stamped<T> { payload: T, revision: Revision }")
        self.assertEqual(t["Stamped"]["members"], {"payload", "revision"})


class TestDiffTypes(unittest.TestCase):
    def _mk(self, kind, members):
        return {"kind": kind, "members": set(members), "docs": {"docs/design/x.md"}}

    def test_in_sync_is_not_flagged(self):
        doc = {"T": self._mk("struct", {"a", "b"})}
        code = {"T": {"kind": "struct", "members": {"a", "b"}}}
        self.assertEqual(dc.diff_types(doc, code), [])

    def test_code_superset_is_in_sync(self):
        # the illustration is a strict simplification (code has extra fields) → not a divergence.
        doc = {"T": self._mk("struct", {"a"})}
        code = {"T": {"kind": "struct", "members": {"a", "b", "c"}}}
        self.assertEqual(dc.diff_types(doc, code), [])

    def test_doc_only_member_is_flagged(self):
        doc = {"T": self._mk("struct", {"a", "b"})}
        code = {"T": {"kind": "struct", "members": {"a"}}}
        f = dc.diff_types(doc, code)
        self.assertEqual(len(f), 1)
        self.assertEqual(f[0]["doc_only"], ["b"])
        self.assertFalse(f[0]["acked"])

    def test_name_collision_without_overlap_is_ignored(self):
        # a register `Slot` and an anchor `Slot` share a name but no members → not the same type.
        doc = {"Slot": self._mk("struct", {"content", "origin"})}
        code = {"Slot": {"kind": "struct", "members": {"gen", "anchor"}}}
        self.assertEqual(dc.diff_types(doc, code), [])

    def test_kind_mismatch_is_flagged_when_overlapping(self):
        doc = {"T": self._mk("struct", {"a"})}
        code = {"T": {"kind": "enum", "members": {"a"}}}
        f = dc.diff_types(doc, code)
        self.assertEqual(len(f), 1)
        self.assertTrue(f[0]["kind_mismatch"])

    def test_ack_marks_finding_acknowledged(self):
        doc = {"T": self._mk("struct", {"a", "b"})}
        code = {"T": {"kind": "struct", "members": {"a"}}}
        f = dc.diff_types(doc, code, acked={"T"})
        self.assertEqual(len(f), 1)
        self.assertTrue(f[0]["acked"], "acked type is still reported but flagged acknowledged")


class TestRepoState(unittest.TestCase):
    def test_checker_passes_on_repo_warn_only(self):
        # The live checker is warn-only, so it must return 0 on the repo regardless of divergence.
        self.assertEqual(dc.main([]), 0)

    def test_repo_has_no_unacknowledged_divergence_in_strict(self):
        # After the slice's two divergences were acknowledged, --strict must pass. This guards that a NEW
        # unacknowledged doc<->code drift makes the strict gate fail.
        self.assertEqual(dc.main(["--strict"]), 0)


if __name__ == "__main__":
    unittest.main()
