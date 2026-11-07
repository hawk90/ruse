"""The oracle binaries the scheduled recapture installs (#904): `tools/parity/install_pin.py` reads them from
spec/parity/upstreams.yaml and refuses a pin whose URL does not name the editor's version_label."""
import importlib.util
import os
import unittest

from . import _support  # noqa: F401

PATH = os.path.join(_support.TOOLS, "parity", "install_pin.py")
SPEC = importlib.util.spec_from_file_location("install_pin", PATH)
install_pin = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(install_pin)


class InstallPinTest(unittest.TestCase):
    def test_both_oracles_are_pinned_to_their_version(self):
        nvim = install_pin.pin("neovim")
        self.assertEqual(nvim["ORACLE_VERSION"], "v0.12.6")
        self.assertIn("/v0.12.6/", nvim["ORACLE_URL"])
        emacs = install_pin.pin("emacs")
        self.assertEqual(emacs["ORACLE_VERSION"], "31.1")
        self.assertTrue(emacs["ORACLE_URL"].endswith("emacs-31.1.tar.xz"))
        for p in (nvim, emacs):
            self.assertRegex(p["ORACLE_SHA256"], r"^[0-9a-f]{64}$")

    def test_a_url_for_another_version_is_refused(self):
        lines = [
            "upstreams:",
            "  neovim:",
            "    version_label: v0.12.7",
            "oracles:",
            "  neovim:",
            "    install_url: https://example.invalid/v0.12.6/nvim.tar.gz",
            "    install_sha256: " + "0" * 64,
        ]
        real = install_pin.UPSTREAMS
        try:
            install_pin.UPSTREAMS = _Fake("\n".join(lines))
            with self.assertRaises(SystemExit):
                install_pin.pin("neovim")
        finally:
            install_pin.UPSTREAMS = real


class _Fake:
    def __init__(self, text: str):
        self.text = text

    def read_text(self, encoding: str = "utf-8") -> str:
        return self.text


if __name__ == "__main__":
    unittest.main()
