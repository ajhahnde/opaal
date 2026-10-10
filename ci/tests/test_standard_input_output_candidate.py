from __future__ import annotations

import tempfile
import unittest
import io
import json
import runpy
import subprocess
from pathlib import Path
from unittest.mock import Mock, patch

from ci.tests import test_data_processing_candidate as candidates
from ci.package_standard_input_output import ROOT, check_copies
from ci.qualify_standard_input_output import assessment


class StdioFixtureIdentityTests(candidates.CandidateIdentityTests):
    directory = "tests/golden/standard-input-output"
    bundle = "standard-input-output"
    kind = "stdio-fixtures"


class CiReferenceTests(unittest.TestCase):
    def test_policy_only_passes_without_failed_or_pending_jobs(self):
        for failed, pending, passed in [(0, 0, True), (1, 0, False), (0, 1, False)]:
            report = {"failed": failed, "pending": pending, "total": 1, "failures": []}
            result = assessment(report)
            self.assertFalse(result.endswith(b"\n"))
            self.assertEqual(json.loads(result), {"report": report, "passed": passed})

    def test_report_core_copy_must_match_maintained_owner(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            owner = root / "tests/golden/data-processing"
            owner.mkdir(parents=True)
            (owner / "report.opaal").write_bytes(b"maintained")
            copy = root / "reference/ci-job-check/report.opaal"
            copy.parent.mkdir(parents=True)
            copy.write_bytes(b"maintained")
            check_copies(root, root / "reference")
            copy.write_bytes(b"changed")
            with self.assertRaisesRegex(ValueError, "maintained owner"):
                check_copies(root, root / "reference")

    def test_project_ci_never_accepts_a_bool_without_successful_matching_evidence(self):
        driver = ROOT / "tests/golden/standard-input-output/ci-job-check/project-ci.py"
        receipt = {"schema": "opaal.execution-receipt.v1", "schema_version": 1,
                   "run_id": "0123456789abcdef0123456789abcdef", "plan_digest": "accepted",
                   "primary": {"class": "success", "value_digest": None}, "secondary": [],
                   "journal_state": "complete", "omitted_secondary_count": 0}
        cases = [(0, b"", receipt, True, 0), (0, b"", receipt, False, 1),
                 (1, b"", receipt, True, 1), (0, b"unexpected", receipt, True, 1),
                 (0, b"", receipt, "true", 1)]
        for key, value in [("run_id", "stale"), ("plan_digest", "stale"),
                           ("journal_state", "unavailable"), ("omitted_secondary_count", 1),
                           ("secondary", [{"category": "receipt", "code": "JOURNAL005"}]),
                           ("primary", {"class": "error", "value_digest": None})]:
            cases.append((0, b"", dict(receipt, **{key: value}), True, 1))
        for code, stderr, evidence, passed, expected in cases:
            with self.subTest(code=code, evidence=evidence, passed=passed):
                output = json.dumps({"passed": passed, "report": {}}).encode()
                results = [subprocess.CompletedProcess([], 0, b"input", b""),
                           subprocess.CompletedProcess([], 0), subprocess.CompletedProcess([], 0),
                           subprocess.CompletedProcess([], code, output, stderr)]
                def read(path):
                    return json.dumps({"digest": "accepted"} if path.name == "ci.plan.json" else evidence).encode()
                stdout = Mock(buffer=io.BytesIO())
                with patch("subprocess.run", side_effect=results), patch.object(Path, "read_bytes", read), \
                        patch("sys.stdout", stdout), patch("sys.stderr", io.StringIO()), \
                        self.assertRaises(SystemExit) as exit_code:
                    runpy.run_path(str(driver), run_name="__main__")
                self.assertEqual(exit_code.exception.code, expected)
                accepted = code == 0 and not stderr and evidence == receipt and type(passed) is bool
                self.assertEqual(stdout.buffer.getvalue(), output if accepted else b"")


if __name__ == "__main__":
    unittest.main()
