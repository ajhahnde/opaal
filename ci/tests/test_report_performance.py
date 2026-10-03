"""Exercise gain and regression boundaries without timing a test machine."""

import contextlib
import io
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import measure_report_performance as runner
import build_report_performance as builder


class IsolatedSourceGuard(unittest.TestCase):
    def test_checkout_metadata_or_unrelated_directory_refuses_before_writing(self):
        for kind in ["git-file", "git-directory", "unrelated-directory"]:
            with tempfile.TemporaryDirectory() as directory:
                source = Path(directory) / "source"
                source.mkdir()
                if kind == "git-file":
                    (source / ".git").write_text("gitdir: elsewhere\n")
                elif kind == "git-directory":
                    (source / ".git").mkdir()
                else:
                    (source / "unrelated").mkdir()
                output = Path(directory) / "artifacts"
                with patch.object(sys, "argv", ["build", "--source", str(source), "--output", str(output)]), contextlib.redirect_stderr(io.StringIO()):
                    with self.assertRaises(SystemExit) as raised:
                        builder.main()
                self.assertEqual(raised.exception.code, 2)
                self.assertFalse(output.exists())


class PerformanceGates(unittest.TestCase):
    def run_case(self, before, after, *, large=False, rss=10_000_000):
        indexes = {"baseline": 0, "candidate": 0}

        def measured(command, _cwd):
            engine = command[0]
            index = indexes[engine]
            indexes[engine] += 1
            value = (before if engine == "baseline" else after)[max(0, index - 3)]
            return {"wall_seconds": value, "user_seconds": value, "system_seconds": 0.0,
                    "peak_rss_bytes": 10_000_000 if engine == "baseline" else rss}, b"ok\n"

        with patch.object(runner, "measured", measured), contextlib.redirect_stdout(io.StringIO()):
            result = runner.row("case", {name: [name] for name in indexes}, Path.cwd(), b"ok\n", large=large)
        self.assertEqual(indexes, {"baseline": 10, "candidate": 10})
        self.assertEqual(result["pair_order"], [["baseline", "candidate"] if i % 2 == 0 else ["candidate", "baseline"] for i in range(7)])
        return result

    def test_large_reports_require_every_pair_to_beat_baseline_spread(self):
        self.assertTrue(self.run_case([1.0] * 7, [0.8] * 7, large=True)["pass"])
        self.assertFalse(self.run_case([1.0] * 7, [0.8] * 6 + [0.99], large=True)["pass"])
        self.assertFalse(self.run_case([1.0] * 6 + [1.3], [0.8] * 7, large=True)["pass"])

    def test_small_cases_keep_the_absolute_floor_and_relative_ceiling(self):
        self.assertTrue(self.run_case([0.001] * 7, [0.002] * 7)["pass"])
        self.assertFalse(self.run_case([0.001] * 7, [0.0031] * 7)["pass"])
        self.assertFalse(self.run_case([1.0] * 7, [1.051] * 7)["pass"])

    def test_report_gain_does_not_waive_peak_rss(self):
        self.assertFalse(self.run_case([1.0] * 7, [0.1] * 7, large=True, rss=11_048_577)["pass"])


if __name__ == "__main__":
    unittest.main()
