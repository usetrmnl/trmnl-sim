"""Firmware code coverage (`--coverage`): lcov mid-run over the control API and at exit."""

import contextlib
import io
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

from support import ROOT, sim

sys.path.insert(0, str(ROOT / "scripts"))
import coverage  # noqa: E402  (scripts/coverage.py)

ENV = "trmnl"  # the PlatformIO environment these tests run (bin/spec trmnl)


def load(path: Path) -> dict:
    cov = {}
    coverage.parse(path, cov, [])
    return cov


class Coverage(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp(prefix="trmnl-cov-"))

    def tearDown(self):
        shutil.rmtree(self.dir, ignore_errors=True)

    def test_setup_boot_covers_bl_init_but_not_error_paths(self):
        final = self.dir / "final.info"
        with sim(erase=True, coverage=final, extra_args=("--offline",)) as s:
            s.wait(portal=True, timeout_s=90)
            mid = s.write_coverage(self.dir / "mid.info")
            self.assertGreater(mid["lines_hit"], 1000)
            self.assertLess(mid["lines_hit"], mid["lines_found"])

            cov = load(self.dir / "mid.info")
            # Paths of the firmware's own sources are relative to its checkout.
            bl, main = cov["src/bl.cpp"], cov["src/main.cpp"]
            self.assertGreater(bl.functions["bl_init()"][1], 0)
            self.assertGreater(main.functions["setup()"][1], 0)
            first_line = bl.functions["bl_init()"][0]
            self.assertGreater(bl.lines[first_line], 0)
            # Only reached when joining WiFi fails.
            self.assertEqual(bl.functions["wifiErrorDeepSleep()"][1], 0)
            line = bl.functions["wifiErrorDeepSleep()"][0]
            self.assertEqual(bl.lines[line], 0)

            # reset: start over; the portal loop keeps running, the boot code doesn't.
            s.write_coverage(self.dir / "before-reset.info", reset=True)
            again = s.write_coverage(self.dir / "after-reset.info")
            self.assertLess(again["lines_hit"], mid["lines_hit"] // 2)
            self.assertEqual(load(self.dir / "after-reset.info")["src/bl.cpp"].lines[first_line], 0)

        # Written on exit too: everything since the reset.
        self.assertTrue(final.exists())
        self.assertGreaterEqual(coverage.totals(load(final))[0], again["lines_hit"])

    def test_merge_tool_unions_runs(self):
        a, b = self.dir / "a.info", self.dir / "b.info"
        a.write_text("TN:\nSF:src/x.cpp\nFN:1,f()\nFNDA:1,f()\nDA:1,1\nDA:2,0\nend_of_record\n")
        b.write_text("TN:\nSF:src/x.cpp\nFN:1,f()\nFNDA:0,f()\nDA:1,1\nDA:2,1\nDA:3,0\nend_of_record\n"
                     "SF:/idf/y.c\nDA:5,1\nend_of_record\n")
        out = self.dir / "merged.info"
        args = [str(self.dir), "-o", str(out), "--include", "src/", "-q", "--html", str(self.dir / "html")]
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(coverage.main(args), 0)
        cov = load(out)
        self.assertEqual(list(cov), ["src/x.cpp"])
        self.assertEqual(cov["src/x.cpp"].lines, {1: 2, 2: 1, 3: 0})
        self.assertEqual(cov["src/x.cpp"].functions, {"f()": [1, 1]})
        self.assertTrue((self.dir / "html" / "index.html").exists())


if __name__ == "__main__":
    unittest.main()
