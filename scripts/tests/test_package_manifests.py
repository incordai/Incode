"""Tests for scripts/package_manifests.py.

    python -m unittest discover -s scripts/tests

The real check is .github/workflows/package-check.yml, which installs with
what the script writes; these pin what it writes and what it refuses.
"""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import package_manifests as pm  # noqa: E402  (the path is set just above)

# One distinct hash per archive, so a manifest that names the wrong one fails.
HASHES = {name: f"{index:x}" * 64 for index, name in enumerate(pm.ARCHIVES, start=1)}
HASHES = {name: digest[:64] for name, digest in HASHES.items()}
SUMS = "".join(f"{digest}  {name}\n" for name, digest in HASHES.items())


class Render(unittest.TestCase):
    def setUp(self) -> None:
        self.files = pm.render("v1.2.3", SUMS)

    def test_every_manifest_is_written(self) -> None:
        self.assertEqual(
            sorted(self.files),
            [
                "homebrew/incode.rb",
                "scoop/incode.json",
                "winget/IncordAI.Incode.installer.yaml",
                "winget/IncordAI.Incode.locale.en-US.yaml",
                "winget/IncordAI.Incode.yaml",
            ],
        )

    def test_winget_installs_both_programs_from_the_windows_zip(self) -> None:
        installer = self.files["winget/IncordAI.Incode.installer.yaml"]
        self.assertIn(
            "InstallerUrl: https://github.com/incordai/Incode/releases/download/v1.2.3/"
            "incode-x86_64-pc-windows-msvc.zip\n",
            installer,
        )
        self.assertIn(f"InstallerSha256: {HASHES[pm.WINDOWS_X64].upper()}\n", installer)
        self.assertIn("PortableCommandAlias: incode\n", installer)
        self.assertIn("PortableCommandAlias: sbx\n", installer)
        for name in self.files:
            if name.startswith("winget/"):
                self.assertIn("PackageVersion: 1.2.3\n", self.files[name], name)

    def test_scoop_names_the_zip_its_hash_and_both_programs(self) -> None:
        manifest = json.loads(self.files["scoop/incode.json"])
        self.assertEqual(manifest["version"], "1.2.3")
        self.assertEqual(manifest["architecture"]["64bit"]["hash"], HASHES[pm.WINDOWS_X64])
        self.assertTrue(manifest["architecture"]["64bit"]["url"].endswith(pm.WINDOWS_X64))
        self.assertEqual(manifest["bin"], ["incode.exe", "sbx.exe"])

    def test_homebrew_gives_each_platform_its_own_archive_and_hash(self) -> None:
        formula = self.files["homebrew/incode.rb"]
        for archive in (pm.MACOS_ARM, pm.MACOS_INTEL, pm.LINUX_ARM, pm.LINUX_INTEL):
            block = f'url "{pm.url("v1.2.3", archive)}"\n      sha256 "{HASHES[archive]}"\n'
            self.assertIn(block, formula, archive)
        self.assertNotIn(pm.WINDOWS_X64, formula)
        self.assertIn('bin.install "incode", "sbx"', formula)

    def test_the_summary_fits_homebrews_rules(self) -> None:
        self.assertLess(len(pm.SUMMARY), 80)
        self.assertNotIn("incode", pm.SUMMARY.lower())
        self.assertFalse(pm.SUMMARY.lower().startswith(("a ", "an ", "the ")))

    def test_files_are_written_with_lf_line_ends(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            sums = Path(scratch) / "SHA256SUMS"
            sums.write_text(SUMS, encoding="utf-8")
            out = Path(scratch) / "out"
            argv = ["--tag", "v1.2.3", "--sums", str(sums), "--out", str(out)]
            self.assertEqual(pm.main(argv), 0)
            written = (out / "homebrew" / "incode.rb").read_bytes()
        self.assertNotIn(b"\r\n", written)
        self.assertEqual(written.decode("utf-8"), self.files["homebrew/incode.rb"])


class Refusals(unittest.TestCase):
    def test_a_pre_release_or_odd_tag_is_refused(self) -> None:
        for tag in ("v1.2.3-rc.1", "1.2.3", "v1.2", "latest"):
            with self.assertRaises(pm.ManifestError, msg=tag):
                pm.render(tag, SUMS)

    def test_a_missing_archive_is_named(self) -> None:
        partial = "".join(line + "\n" for line in SUMS.splitlines() if pm.LINUX_ARM not in line)
        with self.assertRaisesRegex(pm.ManifestError, pm.LINUX_ARM):
            pm.render("v1.2.3", partial)

    def test_a_line_that_is_not_a_sum_is_an_error(self) -> None:
        with self.assertRaisesRegex(pm.ManifestError, "line 6"):
            pm.render("v1.2.3", SUMS + "not a checksum line\n")

    def test_main_reports_a_refusal_and_exits_non_zero(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            sums = Path(scratch) / "SHA256SUMS"
            sums.write_text(SUMS, encoding="utf-8")
            out = Path(scratch) / "out"
            argv = ["--tag", "v1.2.3-rc.1", "--sums", str(sums), "--out", str(out)]
            self.assertEqual(pm.main(argv), 1)
            self.assertFalse(out.exists())


if __name__ == "__main__":
    unittest.main()
