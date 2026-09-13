from __future__ import annotations

import base64
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path
from unittest import mock


sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import qualify_operational_core as qualifier  # noqa: E402


class OperationalCoreQualifierTests(unittest.TestCase):
    def test_parses_only_canonical_maintained_tool_versions(self) -> None:
        self.assertEqual(
            qualifier.parse_version(b"git version 2.55.0\n", "git version "),
            "2.55.0",
        )
        self.assertEqual(
            qualifier.parse_version(
                b"cargo 1.97.1 (5a8a81884 2026-02-10)\nrelease: 1.97.1\n",
                "cargo ",
            ),
            "1.97.1",
        )
        with self.assertRaises(qualifier.QualificationError):
            qualifier.parse_version(b"git version 2.55\n", "git version ")
        with self.assertRaises(qualifier.QualificationError):
            qualifier.parse_version(b"cargo nightly\n", "cargo ")
        with self.assertRaises(qualifier.QualificationError):
            qualifier.parse_version(b"git version 0\n", "git version ")

    def test_detects_raw_and_encoded_canaries(self) -> None:
        self.assertTrue(qualifier.contains_canary(qualifier.CANARY))
        self.assertTrue(
            qualifier.contains_canary(base64.b64encode(qualifier.CANARY).rstrip(b"="))
        )
        self.assertTrue(qualifier.contains_canary(qualifier.CANARY.hex().encode("ascii")))
        percent_encoded = b"".join(
            f"%{byte:02X}".encode("ascii") for byte in qualifier.CANARY
        )
        self.assertTrue(qualifier.contains_canary(percent_encoded))
        self.assertFalse(qualifier.contains_canary(b"ordinary qualification output"))

    def test_requires_the_exact_successful_execution_acknowledgement(self) -> None:
        expected = qualifier.CommandResult(
            ("execute",),
            0,
            f"run {qualifier.RUN_ID_READINESS} success\n".encode(),
            b"",
        )
        qualifier.assert_accepted_execution(expected, qualifier.RUN_ID_READINESS)
        for stdout, stderr in (
            (b"", b""),
            (f"run {qualifier.RUN_ID_READINESS} failed\n".encode(), b""),
            (expected.stdout, b"unexpected\n"),
        ):
            with self.subTest(stdout=stdout, stderr=stderr):
                with self.assertRaises(qualifier.QualificationError):
                    qualifier.assert_accepted_execution(
                        qualifier.CommandResult(("execute",), 0, stdout, stderr),
                        qualifier.RUN_ID_READINESS,
                    )

    def test_qualification_commands_use_only_the_closed_environment(self) -> None:
        environment = {"HOME": "/fixture/home", "PATH": "/fixture/bin"}
        qualification = qualifier.Qualification(
            Path("/fixture/opaal"),
            Path("/fixture/project"),
            "aarch64-apple-darwin",
            environment,
            (),
        )
        completed = subprocess.CompletedProcess(["opaal", "--version"], 0, b"ok\n", b"")
        with mock.patch.object(qualifier, "run_raw", return_value=completed) as run:
            qualification.cli("--version")
        self.assertEqual(run.call_args.kwargs["environment"], environment)

    def test_only_apple_silicon_is_a_claimed_macos_host(self) -> None:
        qualification = qualifier.Qualification(
            Path("/fixture/opaal"), Path("/fixture/project"),
            "aarch64-apple-darwin", {}, (),
        )
        self.assertTrue(qualification.is_macos)
        intel = qualifier.Qualification(
            Path("/fixture/opaal"), Path("/fixture/project"),
            "x86_64-apple-darwin", {}, (),
        )
        self.assertFalse(intel.is_macos)

    def test_generates_a_closed_native_tool_lock(self) -> None:
        with tempfile.TemporaryDirectory(prefix="opaal-tool-lock-test-") as temporary:
            root = Path(temporary)
            environment = {
                "HOME": str(root / "home"),
                "TMPDIR": str(root / "tmp"),
                "PATH": "/usr/bin:/bin",
                "CARGO_HOME": str(root / "cargo"),
                "RUSTC": str(root / "rustc"),
                "RUSTDOC": str(root / "rustdoc"),
                "LC_ALL": "C",
                "TZ": "UTC",
                "CARGO_NET_OFFLINE": "true",
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_CONFIG_GLOBAL": str(root / "gitconfig"),
            }
            tools = (
                qualifier.Tool("git", "git", Path("/usr/bin/git"), "2.55.0", "sha256:" + "1" * 64),
                qualifier.Tool("cargo", "cargo", Path("/opt/bin/cargo"), "1.97.1", "sha256:" + "2" * 64),
            )
            document = tomllib.loads(
                qualifier.tool_lock_text("aarch64-apple-darwin", environment, tools)
            )
            self.assertEqual(document["schema_version"], 1)
            self.assertEqual(document["platform"], "aarch64-apple-darwin")
            self.assertEqual(document["child_environment"]["inherit"], [])
            variables = document["child_environment"]["variables"]
            self.assertEqual(len(variables), 11)
            decoded = {
                row["name"]: base64.urlsafe_b64decode(
                    row["value"]["value"] + "=" * (-len(row["value"]["value"]) % 4)
                ).decode()
                for row in variables
            }
            self.assertEqual(decoded, environment)
            self.assertEqual([row["id"] for row in document["tools"]], ["git", "cargo"])

    def test_requires_the_exact_artifact_schema(self) -> None:
        with tempfile.TemporaryDirectory(prefix="opaal-artifact-test-") as temporary:
            path = Path(temporary) / "artifact.json"
            path.write_text('{"schema":"opaal.plan.v2"}\n', encoding="utf-8")
            self.assertEqual(
                qualifier.exact_schema(path, "opaal.plan.v2")["schema"],
                "opaal.plan.v2",
            )
            with self.assertRaises(qualifier.QualificationError):
                qualifier.exact_schema(path, "opaal.audit.v2")

    def test_fixture_is_complete_and_non_publishing(self) -> None:
        expected = {
            "Cargo.lock",
            "Cargo.toml",
            "authority-ci.toml",
            "ca.pem",
            "opaal.toml",
            "readiness-probe/Cargo.toml",
            "readiness-probe/src/lib.rs",
            "release-readiness.opaal",
            "tasks.opaal",
        }
        observed = {
            path.relative_to(qualifier.TEMPLATE).as_posix()
            for path in qualifier.TEMPLATE.rglob("*")
            if path.is_file()
        }
        self.assertEqual(observed, expected)
        text = "\n".join(
            path.read_text(encoding="utf-8")
            for path in qualifier.TEMPLATE.rglob("*")
            if path.is_file() and path.suffix != ".pem"
        )
        for command in ("cargo publish", "git push", "gh release", "cargo package"):
            self.assertNotIn(command, text)


if __name__ == "__main__":
    unittest.main()
