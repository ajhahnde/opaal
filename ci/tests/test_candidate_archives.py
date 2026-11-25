from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "qualify_candidate_archives.sh"
SOURCE = "a" * 40


class CandidateArchiveTests(unittest.TestCase):
    def run_replay(self, platform: str, *, fail: str = "") -> tuple[int, list[list[str]]]:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            shim = root / "python3"
            shim.write_text(
                f"#!{sys.executable}\n"
                "import json, os, sys\n"
                "with open(os.environ['COMMAND_LOG'], 'a') as log:\n"
                "    print(json.dumps(sys.argv[1:]), file=log)\n"
                "if sys.argv[1] == '-c':\n"
                "    print('1.2.3')\n"
                "elif sys.argv[1] == os.environ['FAILED_COMMAND']:\n"
                "    sys.exit(42)\n"
            )
            shim.chmod(0o755)
            git = root / "git"
            git.write_text(f"#!/bin/sh\ntest \"$*\" = 'rev-parse HEAD' && printf '%s\\n' '{SOURCE}'\n")
            git.chmod(0o755)
            commands = root / "commands.jsonl"
            environment = dict(os.environ, PATH=f"{root}{os.pathsep}{os.environ['PATH']}",
                               COMMAND_LOG=str(commands), FAILED_COMMAND=fail)
            result = subprocess.run(["bash", str(SCRIPT), platform, "report directory"],
                                    cwd=root, env=environment, capture_output=True, check=False)
            lines = commands.read_text().splitlines() if commands.exists() else []
            return result.returncode, [json.loads(line) for line in lines]

    def test_replays_bind_the_same_archive_and_source_on_each_platform(self) -> None:
        for platform, host, docker in (
            ("linux-x86_64", "linux", "docker-image-checks-linux"),
            ("macos-arm64", "macos", "docker-image-checks-macos"),
            ("linux-arm64", "linux-arm64", "docker-image-checks-offline-linux-arm64"),
        ):
            with self.subTest(platform=platform):
                code, calls = self.run_replay(platform)
                self.assertEqual(code, 0)
                self.assertEqual(calls[1:7], [
                    ["ci/package_data_processing.py", "--platform", platform],
                    ["ci/package_source_formatting.py"],
                    ["ci/package_random_values.py"],
                    ["ci/package_standard_input_output.py"],
                    ["ci/package_docker_image_checks.py"],
                    ["ci/package_controlled_native_tools.py"],
                ])
                common = ["--archive", f"dist/opaal-v1.2.3-{platform}.tar.gz",
                          "--expected-source", SOURCE, "--platform", platform]
                self.assertEqual(calls[7:], [
                    ["ci/qualify_source_formatting.py", *common,
                     "--fixtures", "dist/opaal-v1.2.3-source-formatting.tar.gz",
                     "--report", f"report directory/source-formatting-{host}.json"],
                    ["ci/qualify_data_processing.py", *common, "--expected-version", "1.2.3",
                     "--examples", "dist/opaal-v1.2.3-data-processing.tar.gz",
                     "--report", f"report directory/data-processing-{host}.json"],
                    ["ci/qualify_numeric_operations.py", *common,
                     "--report", f"report directory/numeric-operations-{host}.json"],
                    ["ci/qualify_random_values.py", *common,
                     "--fixtures", "dist/opaal-v1.2.3-random-values.tar.gz",
                     "--report", f"report directory/random-values-{host}.json"],
                    ["ci/qualify_standard_input_output.py", *common,
                     "--fixtures", "dist/opaal-v1.2.3-standard-input-output.tar.gz",
                     "--report", f"report directory/standard-input-output-{host}.json"],
                    ["ci/qualify_docker_image_checks.py", "--offline", *common,
                     "--fixtures", "dist/opaal-v1.2.3-docker-image-checks.tar.gz",
                     "--report", f"report directory/{docker}.json"],
                ])

    def test_stops_on_the_first_failed_qualifier(self) -> None:
        failed = "ci/qualify_data_processing.py"
        code, calls = self.run_replay("linux-x86_64", fail=failed)
        self.assertEqual(code, 42)
        self.assertEqual(calls[-1][0], failed)

    def test_rejects_an_unknown_platform_before_packaging(self) -> None:
        self.assertEqual(self.run_replay("linux-aarch64"), (2, []))
