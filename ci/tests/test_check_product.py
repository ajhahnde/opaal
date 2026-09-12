from __future__ import annotations

import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from typing import Sequence

from ci import check_product as checker


class ProductCheckerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="opaal-product-check-")
        self.root = Path(self.temporary.name)
        members = "\n".join(f'    "{path}",' for path in checker.WORKSPACE_PACKAGES)
        self.write(
            "Cargo.toml",
            f"""[workspace]
resolver = "3"
members = [
{members}
]

[workspace.package]
version = "{checker.VERSION}"
license = "MPL-2.0"
repository = "{checker.REPOSITORY}"
publish = false
""",
        )
        for path, name in checker.WORKSPACE_PACKAGES.items():
            binary = ""
            if name == "opaal-cli":
                binary = '\n[[bin]]\nname = "opaal"\npath = "src/main.rs"\n'
            elif name == "opaal-lsp":
                binary = (
                    '\n[[bin]]\nname = "opaal-language-server"\npath = "src/main.rs"\n'
                )
            self.write(
                f"{path}/Cargo.toml",
                f"""[package]
name = "{name}"
version.workspace = true
license.workspace = true
repository.workspace = true
publish.workspace = true
{binary}""",
            )
        fuzz_bins = "\n".join(
            f'[[bin]]\nname = "{name}"\npath = "fuzz_targets/{name}.rs"'
            for name in sorted(checker.FUZZ_TARGETS)
        )
        self.write(
            "fuzz/Cargo.toml",
            f"""[package]
name = "opaal-fuzz"
version = "0.0.0"
publish = false
{fuzz_bins}
""",
        )
        self.write(
            ".github/workflows/ci.yml",
            "python3 ci/check_product.py source\n"
            "fuzz/run-smoke.sh\n"
            "name: required\n"
            "needs: [foundation, policy, fuzz]\n",
        )
        self.write(
            ".github/workflows/release.yml",
            "python3 ci/check_product.py unpublished\n",
        )
        self.write(
            ".github/workflows/security.yml",
            "name: security-required\n"
            "needs: [dependency-review, cargo-policy, repository-policy]\n",
        )
        self.write("ci/check_benchmarks.py", "# fixture\n")
        self.write("README.md", f"OPAAL {checker.VERSION} is unreleased.\n")
        self.write("CHANGELOG.md", "# Changelog\n\n## [Unreleased]\n")
        self.write("SECURITY.md", "# Security\n")

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write(self, relative: str, text: str) -> None:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def runner(
        self, command: Sequence[str], root: Path
    ) -> subprocess.CompletedProcess[bytes]:
        del root
        if list(command[:2]) == ["cargo", "metadata"]:
            payload = {
                "packages": [
                    {"name": name, "version": checker.VERSION, "publish": []}
                    for name in checker.WORKSPACE_PACKAGES.values()
                ]
            }
            return subprocess.CompletedProcess(command, 0, json.dumps(payload).encode(), b"")
        if list(command[:2]) == ["cargo", "run"]:
            return subprocess.CompletedProcess(
                command, 0, f"opaal {checker.VERSION}\n".encode(), b""
            )
        if list(command[:3]) == ["git", "tag", "--list"]:
            return subprocess.CompletedProcess(command, 0, b"", b"")
        if list(command[:2]) == ["gh", "api"]:
            return subprocess.CompletedProcess(command, 0, b"[]\n", b"")
        if command[-2:] == ["ci/check_benchmarks.py", "--contract-only"]:
            return subprocess.CompletedProcess(command, 0, b"benchmark: ok\n", b"")
        return subprocess.CompletedProcess(command, 1, b"", b"unexpected command")

    def test_accepts_exact_current_product_and_unpublished_boundaries(self) -> None:
        self.assertEqual(checker.source_problems(self.root, run=self.runner), [])
        self.assertEqual(
            checker.unpublished_problems(
                self.root, run=self.runner, get_status=lambda _url: 404
            ),
            [],
        )

    def test_rejects_workspace_fuzz_and_removed_api_drift(self) -> None:
        self.write("history/predecessor.txt", "retained\n")
        self.write("fuzz/fuzz_targets/migration.rs", "fn main() {}\n")
        self.write(
            "crates/opaal-runtime/src/lib.rs",
            "/// One language generation.\n"
            "pub struct LanguageIdentity;\n"
            "pub const OPAAL_V1: u16 = 1;\n",
        )
        findings = "\n".join(checker.source_problems(self.root, run=self.runner))
        self.assertIn("forbidden current-tree path remains: history", findings)
        self.assertIn("fuzz/fuzz_targets/migration.rs", findings)
        self.assertIn("removed API remains: LanguageIdentity", findings)
        self.assertIn("removed API remains: OPAAL_V1", findings)
        self.assertIn("source-generation wording remains", findings)

    def test_rejects_source_directives(self) -> None:
        self.write("examples/stale.opaal", "## note\r\nlanguage 1; let answer = 42\r\n")
        findings = "\n".join(checker.source_problems(self.root, run=self.runner))
        self.assertIn("source-generation directive remains", findings)

    def test_rejects_removed_language_syntax_and_handlers(self) -> None:
        self.write("examples/stale.opaal", "echo $name\n")
        self.write("docs/stale.md", "```opaal\necho ${name}\n```\n")
        self.write(
            "crates/opaal-cli/src/highlight.rs",
            "enum TokenHandler { CommandSubstitutionStart }\n",
        )
        findings = "\n".join(checker.source_problems(self.root, run=self.runner))
        self.assertIn("examples/stale.opaal:1: unquoted dollar syntax remains", findings)
        self.assertIn("docs/stale.md:2: dollar syntax remains in an OPAAL example", findings)
        self.assertIn(
            "removed language handler remains: CommandSubstitutionStart", findings
        )

    def test_allows_literal_dollars_and_the_classified_negative_fixture(self) -> None:
        self.write("examples/literal.opaal", "echo \"$name\" '$(literal)'\n")
        self.write(
            "tests/opaal-foundation/language/lexical/invalid/removed-command-substitution.opaal",
            "echo $(removed)\n",
        )
        self.assertEqual(checker.source_problems(self.root, run=self.runner), [])

    def test_rejects_workflow_dependency_and_invocation_drift(self) -> None:
        self.write(
            ".github/workflows/ci.yml",
            "python3 ci/check_transition.py verify\nneeds: [foundation, policy]\n",
        )
        findings = "\n".join(checker.source_problems(self.root, run=self.runner))
        self.assertIn("does not run the product source validator", findings)
        self.assertIn("does not run the supported fuzz smoke", findings)
        self.assertIn("required aggregate", findings)
        self.assertIn("removed transition checker", findings)

    def test_unpublished_mode_fails_closed_on_network_or_positive_state(self) -> None:
        def unavailable(_url: str) -> int:
            raise OSError("offline")

        findings = "\n".join(
            checker.unpublished_problems(
                self.root, run=self.runner, get_status=unavailable
            )
        )
        self.assertIn("unavailable: offline", findings)

        findings = "\n".join(
            checker.unpublished_problems(
                self.root, run=self.runner, get_status=lambda _url: 200
            )
        )
        self.assertIn("expected 404, observed 200", findings)


if __name__ == "__main__":
    unittest.main()
