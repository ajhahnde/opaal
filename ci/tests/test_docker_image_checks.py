from __future__ import annotations

import json
import os
import runpy
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from pathlib import Path

from ci.package_data_processing import package_examples
from ci.package_docker_image_checks import FIXTURES, ROOT, check_copies
from ci.package_release import VERSION
from ci.qualify_data_processing import unpack
from ci.qualify_docker_image_checks import cases, decoder_payload, live_report_comparison


class DockerImageChecksTests(unittest.TestCase):
    def test_live_comparison_uses_four_fixed_children_and_stops_redacted_failures(self):
        version = {"Client": {"Version": "29.9.0", "ApiVersion": "1.48", "Os": "linux", "Arch": "amd64"},
                   "Server": {"Version": "29.9.0", "ApiVersion": "1.56", "MinAPIVersion": "1.40",
                              "Os": "linux", "Arch": "amd64"}}
        image = (ROOT / FIXTURES / "fixtures/image.json").read_bytes()
        expected = ["sha256:" + "a" * 64, "linux/amd64", "abc123"]
        original_run = subprocess.run
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary)
            for name in ("comparison.py", "run-comparison.sh"):
                shutil.copyfile(ROOT / FIXTURES / name, work / name)
            for name in ("docker-empty", "empty-path", "sandbox-home", "sandbox-tmp"):
                (work / name).mkdir()
            live = {"docker": work / "docker", "socket": work / "daemon.sock"}
            for stage, fault, expected_exit, process_count in (
                (None, None, 0, 4), (None, "negative", 1, 4),
                (0, "status", 2, 1), (1, "status", 2, 2), (2, "status", 2, 3), (3, "status", 2, 4),
                (0, "probe", 2, 1), (1, "profile", 2, 2), (3, "profile", 2, 4),
                (1, "api", 2, 2), (3, "changed-api", 2, 4),
                (1, "duplicate", 2, 2), (2, "duplicate", 2, 3), (3, "duplicate", 2, 4),
                (2, "descriptorless", 2, 4), (2, "stdout-limit", 2, 3),
                (2, "stderr-limit", 2, 3), (2, "timeout", 2, 3),
            ):
                with self.subTest(stage=stage, fault=fault):
                    calls = []
                    payloads = [b"Docker version 29.9.0, build abc\n", json.dumps(version).encode(),
                                image, json.dumps(version).encode()]
                    if fault == "probe":
                        payloads[stage] = b"credential-canary"
                    elif fault in ("profile", "api", "changed-api"):
                        changed = json.loads(payloads[stage])
                        if fault == "profile":
                            changed["Server"]["Version"] = "29.8.2"
                        else:
                            changed["Server"]["ApiVersion"] = "01.56" if fault == "api" else "1.55"
                        payloads[stage] = json.dumps(changed).encode()
                    elif fault == "duplicate":
                        payloads[stage] = b'{"credential-canary":1,"credential-canary":2}'
                    elif fault == "descriptorless":
                        changed = json.loads(image)
                        del changed["Descriptor"]
                        payloads[stage] = json.dumps(changed).encode()
                    elif fault == "stdout-limit":
                        payloads[stage] = b"x" * 1_048_577
                    def run(argv, **kwargs):
                        if argv[0] != str(live["docker"]):
                            return original_run(argv, **kwargs)
                        current = len(calls)
                        self.assertEqual(kwargs["env"], {
                            "LC_ALL": "C", "TZ": "UTC", "DOCKER_API_VERSION": "1.48",
                            "PATH": str(work / "empty-path"), "HOME": str(work / "sandbox-home"),
                            "TMPDIR": str(work / "sandbox-tmp")})
                        self.assertEqual(kwargs["stdin"], subprocess.DEVNULL)
                        self.assertGreater(kwargs["timeout"], 0)
                        self.assertLessEqual(kwargs["timeout"], 30)
                        calls.append(argv)
                        if current == stage and fault == "timeout":
                            raise subprocess.TimeoutExpired(argv, kwargs["timeout"], stderr=b"credential-canary")
                        return subprocess.CompletedProcess(argv, 7 if current == stage and fault == "status" else 0,
                            payloads[current], b"x" * 65_537 if current == stage and fault == "stderr-limit" else b"credential-canary")
                    wanted = expected if fault != "negative" else [*expected[:2], "wrong"]
                    with patch("ci.qualify_docker_image_checks.subprocess.run", side_effect=run):
                        result, acquisition = live_report_comparison(work, live, "example:test", wanted,
                                                                    {"PATH": os.defpath})
                    self.assertEqual(result.returncode, expected_exit)
                    self.assertEqual(len(acquisition["stages"]), process_count)
                    self.assertNotIn(b"credential-canary", result.stdout + result.stderr)
                    prefix = [str(live["docker"]), "--config", str(work / "docker-empty"),
                              "--host", "unix://" + str(live["socket"])]
                    vectors = [["--version"], ["version", "--format", "{{json .}}"],
                               ["image", "inspect", "--format", "{{json .}}", "--", "example:test"],
                               ["version", "--format", "{{json .}}"]]
                    self.assertEqual(calls, [prefix + vector for vector in vectors[:process_count]])
                    if expected_exit in (0, 1):
                        self.assertEqual(json.loads(result.stdout)["passed"], expected_exit == 0)
                    else:
                        self.assertFalse(result.stdout)

    def test_comparison_shell_redacts_parser_errors_and_refuses_output_failure(self):
        directory = ROOT / FIXTURES
        expected = ["sha256:" + "a" * 64, "linux/amd64", "abc123"]
        malformed = subprocess.run(["sh", "run-comparison.sh", *expected], cwd=directory,
                                   input=b'{"credential-canary":1,"credential-canary":2}',
                                   capture_output=True, timeout=10)
        self.assertEqual(malformed.returncode, 2)
        self.assertFalse(malformed.stdout)
        self.assertNotIn(b"credential-canary", malformed.stderr)
        self.assertNotIn(b"Traceback", malformed.stderr)
        with open(os.devnull, "rb") as unwritable:
            failed = subprocess.run(["sh", "run-comparison.sh", *expected], cwd=directory,
                                    input=(directory / "fixtures/image.json").read_bytes(),
                                    stdout=unwritable, stderr=subprocess.PIPE, timeout=10)
        self.assertEqual(failed.returncode, 2)
        self.assertNotIn(b"Traceback", failed.stderr)

    def test_independent_python_comparison_matches_every_named_report_and_exit(self):
        assess = runpy.run_path(str(ROOT / FIXTURES / "comparison.py"))["assess"]
        document = json.loads((ROOT / FIXTURES / "fixtures/image.json").read_bytes())
        for name, payload, expected, code, report in cases(document):
            with self.subTest(case=name):
                try:
                    observed = assess(payload, expected)
                    selected = int(not observed["passed"])
                except (ValueError, TypeError, RecursionError):
                    observed, selected = None, 2
                self.assertEqual((observed, selected), (report, code))

    def test_decoder_corpus_materializes_exact_byte_utf8_and_depth_boundaries(self):
        document = json.loads((ROOT / FIXTURES / "fixtures/image.json").read_bytes())
        corpus = json.loads((ROOT / FIXTURES / "decode/cases.json").read_bytes())
        self.assertEqual(corpus["schema_version"], 1)
        by_name = {row["name"]: row for row in corpus["cases"]}
        self.assertEqual(len(by_name), len(corpus["cases"]))
        for size in (1_048_576, 1_048_577):
            self.assertEqual(len(decoder_payload(document, by_name[f"document-bytes-{size}"])), size)
        for size in (4096, 4097):
            value = json.loads(decoder_payload(document, by_name[f"revision-utf8-bytes-{size}"]))
            self.assertEqual(len(value["Config"]["Labels"]["org.opencontainers.image.revision"].encode()), size)
        for depth in (64, 65):
            value = json.loads(decoder_payload(document, by_name[f"document-depth-{depth}"]))["unknown"]
            observed = 2
            while isinstance(value, list):
                value = value[0]
                observed += 1
            self.assertEqual(observed, depth)
        for row in corpus["cases"]:
            self.assertIn(row["expect"], {"image", "error"})
            self.assertTrue(decoder_payload(document, row))

    def test_named_cases_distinguish_missing_empty_and_acquisition_errors(self):
        document = json.loads((ROOT / FIXTURES / "fixtures/image.json").read_bytes())
        rows = cases(document)
        self.assertEqual(len({row[0] for row in rows}), len(rows))
        by_name = {name: (exit_code, report) for name, _, _, exit_code, report in rows}
        self.assertEqual(by_name["single-oci"][0], 0)
        self.assertEqual(by_name["single-docker"][0], 0)
        for name in ("negative-id", "negative-platform", "negative-revision", "empty-revision", "missing-revision"):
            self.assertEqual(by_name[name][0], 1)
        self.assertFalse(by_name["empty-revision"][1]["checks"][2]["missing"])
        self.assertTrue(by_name["missing-revision"][1]["checks"][2]["missing"])
        for name in ("descriptorless", "index", "manifest-list", "descriptor-mismatch", "duplicate", "invalid-utf8"):
            self.assertEqual(by_name[name], (2, None))

    def test_reference_copies_and_package_roundtrip_keep_exact_bytes(self):
        check_copies(ROOT)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            shutil.copytree(ROOT / FIXTURES, root / FIXTURES)
            shutil.copytree(ROOT / "examples/image-check", root / "examples/image-check")
            check_copies(root)
            paths = sorted(path.relative_to(root / FIXTURES).as_posix()
                           for path in (root / FIXTURES).rglob("*") if path.is_file())
            archive = package_examples(root, paths, "a" * 40, directory=FIXTURES, name="docker-image-checks")
            original = archive.read_bytes()
            package_examples(root, paths, "a" * 40, directory=FIXTURES, name="docker-image-checks")
            self.assertEqual(archive.read_bytes(), original)
            output = root / "unpacked"
            output.mkdir()
            copied, manifest = unpack(archive, output, kind="docker-fixtures", version=VERSION,
                                      source="a" * 40, platform="portable")
            self.assertEqual(manifest["kind"], "docker-fixtures")
            for name in paths:
                self.assertEqual((copied / name).read_bytes(), (root / FIXTURES / name).read_bytes())
            (root / "examples/image-check/policy.opaal").write_text("changed")
            with self.assertRaisesRegex(ValueError, "differ"):
                check_copies(root)

    def test_setup_rejects_unqualified_inputs_without_creating_configuration(self):
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary)
            shutil.copyfile(ROOT / FIXTURES / "docker-cli-profile.json", work / "docker-cli-profile.json")
            (work / "qualified-docker-version.txt").write_text("unqualified\n")
            result = subprocess.run([sys.executable, ROOT / "tests/fixtures/image-check-legacy/setup-live.py"],
                                    cwd=work, capture_output=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(sorted(path.name for path in work.iterdir()),
                             ["docker-cli-profile.json", "qualified-docker-version.txt"])


if __name__ == "__main__":
    unittest.main()
