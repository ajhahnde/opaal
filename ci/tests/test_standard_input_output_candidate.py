from __future__ import annotations

import tempfile
import unittest
import json
import os
import shutil
import subprocess
import sys
import stat
from types import SimpleNamespace
from unittest.mock import Mock, patch
from pathlib import Path

from ci.tests import test_data_processing_candidate as candidates
from ci.package_standard_input_output import ROOT, check_copies
from ci.qualify_standard_input_output import assessment
from ci.qualify_standard_io_failures import worker_has_pipe
from ci.qualify_data_processing import QualificationError


class StdioFixtureIdentityTests(candidates.CandidateIdentityTests):
    directory = "tests/golden/standard-input-output"
    bundle = "standard-input-output"
    kind = "stdio-fixtures"


class CiReferenceTests(unittest.TestCase):
    def test_linux_worker_pipe_requires_received_endpoint(self):
        with patch("ci.qualify_standard_io_failures.platform.system", return_value="Linux"), \
             patch("ci.qualify_standard_io_failures.Path") as path:
            descriptors = path.return_value.iterdir
            descriptors.return_value = [
                SimpleNamespace(name="0", stat=lambda: SimpleNamespace(st_mode=stat.S_IFIFO)),
                SimpleNamespace(name="8", stat=lambda: SimpleNamespace(st_mode=stat.S_IFSOCK)),
            ]
            self.assertFalse(worker_has_pipe(123))
            descriptors.return_value.append(
                SimpleNamespace(name="3", stat=lambda: SimpleNamespace(st_mode=stat.S_IFIFO)))
            self.assertTrue(worker_has_pipe(123))
            descriptors.side_effect = FileNotFoundError
            self.assertFalse(worker_has_pipe(123))

    def test_macos_worker_pipe_requires_received_endpoint(self):
        def fd_table(pid, flavor, argument, storage, size):
            self.assertEqual((pid, flavor, argument), (123, 1, 0))
            storage[0], storage[1] = 8, 2
            if received[0]:
                storage[2], storage[3] = 3, 6
                return 16
            return 8

        received = [False]
        library = Mock()
        library.proc_pidinfo.side_effect = fd_table
        with patch("ci.qualify_standard_io_failures.platform.system", return_value="Darwin"), \
             patch("ci.qualify_standard_io_failures.ctypes.CDLL", return_value=library):
            self.assertFalse(worker_has_pipe(123))
            received[0] = True
            self.assertTrue(worker_has_pipe(123))
            library.proc_pidinfo.side_effect = None
            library.proc_pidinfo.return_value = 3
            with self.assertRaisesRegex(QualificationError, "owned worker descriptors"):
                worker_has_pipe(123)

    def test_native_driver_preserves_exit_precedence_and_exclusive_acquisition(self):
        for selected, audit, expected in [(0, 0, 0), (1, 0, 1), (2, 0, 2),
                                           (130, 0, 130), (0, 1, 2), (1, 1, 2), (130, 1, 130)]:
            with self.subTest(selected=selected, audit=audit), tempfile.TemporaryDirectory() as temporary:
                work = Path(temporary)
                shutil.copyfile(ROOT / "tests/golden/standard-input-output/ci-job-check/project-ci.sh", work / "project-ci.sh")
                (work / "producer.py").write_text("print('input')\n")
                binary = work / "opaal"
                binary.write_text(f"#!{sys.executable}\n" +
                    "import os,sys\nfrom pathlib import Path\n"
                    "with Path('calls').open('a') as calls: calls.write(sys.argv[1]+'\\n')\n"
                    "if sys.argv[1] == 'plan': print('sha256:'+'1'*64)\n"
                    "if sys.argv[1] == 'execute': sys.exit(int(os.environ['SELECTED_EXIT']))\n"
                    "if sys.argv[1] == 'audit': sys.exit(int(os.environ['AUDIT_EXIT']))\n")
                binary.chmod(0o755)
                environment = {"PATH": str(work) + os.pathsep + str(Path(sys.executable).parent) + os.pathsep + os.defpath,
                               "SELECTED_EXIT": str(selected), "AUDIT_EXIT": str(audit)}
                result = subprocess.run(["/bin/sh", "project-ci.sh"], cwd=work, env=environment, capture_output=True, timeout=10)
                self.assertEqual(result.returncode, expected, result.stderr)
                self.assertEqual(result.stdout, b"")
                self.assertEqual((work / "calls").read_text().splitlines(), ["task", "task", "check", "plan", "execute", "audit"])
                (work / "calls").unlink()
                for state in ["existing", "dangling-symlink"]:
                    acquired = work / "acquired.json"
                    acquired.unlink(missing_ok=True)
                    if state == "existing": acquired.write_bytes(b"keep")
                    else: acquired.symlink_to(work / "untouched")
                    result = subprocess.run(["/bin/sh", "project-ci.sh"], cwd=work, env=environment, capture_output=True, timeout=10)
                    self.assertEqual(result.returncode, 2)
                    self.assertFalse((work / "calls").exists())
                    self.assertFalse((work / "untouched").exists())
                    if state == "existing": self.assertEqual(acquired.read_bytes(), b"keep")

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



if __name__ == "__main__":
    unittest.main()
