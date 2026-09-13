from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from ci import check_docs as checker


class DocumentationCheckerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="opaal-docs-check-")
        self.root = Path(self.temporary.name) / "repo"
        self.root.mkdir()

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write(self, relative: str, text: str) -> None:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def populate_navigation(self) -> None:
        for relative in checker.PRODUCT_PAGES:
            self.write(f"docs/{relative}", f"# {Path(relative).stem}\n")
        links = "\n".join(
            f"- [{relative}]({relative})"
            for relative in checker.PRODUCT_PAGES
            if relative != "README.md"
        )
        self.write("docs/README.md", f"# Documentation\n\n{links}\n")
        for name in ("README.md", "SECURITY.md", "CONTRIBUTING.md", "CHANGELOG.md"):
            self.write(name, f"# {name}\n")

    def populate_limits(self) -> None:
        self.write(
            "docs/reference/limits.md",
            """# Limits

| Area | Ceiling |
| --- | --- |
| Source closure | 8 MiB; 256; 64; 1,000,000 |
| Analysis | 64; 100,000; 100,000; 1,024; 5,000,000 |
| Evaluation | 1,000,000; 256; 1,000,000; 16 MiB |
| Project control | 1 MiB; 16; 256; 64 |
| Journal | 16 MiB; 100,000 |
""",
        )
        self.write(
            "crates/opaal-runtime/src/module.rs",
            """pub const OPAAL: Self = Self {
    limits: [Some(8 * 1024 * 1024), Some(256), Some(64), Some(1_000_000),
             Some(64), Some(100_000), Some(100_000), Some(1_024), Some(5_000_000)]
};
""",
        )
        self.write(
            "crates/opaal-runtime/src/eval.rs",
            """const DEFAULT_OPAAL_EVALUATION_STEPS: usize = 1_000_000;
const DEFAULT_OPAAL_CALL_DEPTH: usize = 256;
const DEFAULT_OPAAL_COLLECTION_ITEMS: usize = 1_000_000;
const DEFAULT_OPAAL_COLLECTION_BYTES: usize = 16 * 1024 * 1024;
""",
        )
        self.write(
            "crates/opaal-runtime/src/project.rs",
            """const MAX_PROJECT_DOCUMENT_BYTES: usize = 1024 * 1024;
const MAX_PROJECT_ENTRIES: usize = 256;
const MAX_PROJECT_DOCUMENT_DEPTH: usize = 16;
const MAX_PROJECT_INPUTS: usize = 64;
""",
        )
        self.write(
            "crates/opaal-runtime/src/workflow.rs",
            """const MAX_JOURNAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_JOURNAL_LINES: usize = 100_000;
""",
        )

    def populate_catalogs(self) -> None:
        self.write(
            "crates/opaal-runtime/src/builtin.rs",
            'documented(\n'
            '    CommandSignature::new("sample", [Carrier::Empty], Carrier::Value),\n'
            '    "sample [VALUE]",\n'
            '    "Test command.",\n'
            ');\n',
        )
        self.write(
            "docs/reference/language/core-commands.md",
            "| Command | Invocation | Purpose |\n"
            "| --- | --- | --- |\n"
            "| `sample` | `sample [VALUE]` | Test |\n",
        )
        self.write(
            "crates/opaal-runtime/src/module.rs",
            """const STANDARD_OUTCOME_MODULE: &str = r#"export { Result, Option }
"#;
fn is_standard_module(namespace: &str, module: &str) -> bool {
    matches!((namespace, module), ("std", "value") | ("std", "outcome"))
}
fn parse_opaal_source() {}
""",
        )
        self.write(
            "docs/reference/std/README.md",
            "[`std::value`](value.md)\n[`std::outcome`](outcome.md)\n",
        )
        self.write("docs/reference/std/value.md", "`length`\n")
        self.write("docs/reference/std/outcome.md", "`Result` and `Option`\n")
        schemas = {
            "CHECK": ("opaal.check.v2", "check-artifact.md"),
            "PLAN": ("opaal.plan.v2", "plan-artifact.md"),
            "JOURNAL": ("opaal.journal.v2", "journal-artifact.md"),
            "AUDIT": ("opaal.audit.v2", "audit-artifact.md"),
        }
        source = ""
        index = ""
        for kind, (schema, filename) in schemas.items():
            source += f'const {kind}_SCHEMA: &str = "{schema}";\n'
            index += f"`{schema}`\n"
            self.write(f"docs/reference/formats/{filename}", f"`{schema}`\n")
        self.write("crates/opaal-runtime/src/workflow.rs", source)
        self.write("docs/reference/formats/README.md", index)

    def test_navigation_accepts_complete_topology(self) -> None:
        self.populate_navigation()
        self.assertEqual(checker.navigation(self.root), [])

    def test_navigation_rejects_missing_required_page(self) -> None:
        self.populate_navigation()
        (self.root / "docs/how-to/audit-a-run.md").unlink()
        findings = "\n".join(checker.navigation(self.root))
        self.assertIn("docs/how-to/audit-a-run.md: required product page is missing", findings)

    def test_navigation_rejects_link_outside_repository(self) -> None:
        self.populate_navigation()
        (self.root.parent / "private.md").write_text("private\n", encoding="utf-8")
        entry = self.root / "docs/README.md"
        entry.write_text(
            entry.read_text(encoding="utf-8") + "[outside](../../private.md)\n",
            encoding="utf-8",
        )
        findings = "\n".join(checker.navigation(self.root))
        self.assertIn("docs/README.md: link escapes repository", findings)

    def test_limits_require_every_repeated_source_value(self) -> None:
        self.populate_limits()
        self.assertEqual(checker.limits(self.root), [])
        path = self.root / "docs/reference/limits.md"
        path.write_text(
            path.read_text(encoding="utf-8").replace(
                "64; 100,000; 100,000; 1,024", "64; 100,000; 1,024"
            ),
            encoding="utf-8",
        )
        findings = "\n".join(checker.limits(self.root))
        self.assertIn("Analysis: source limit 100,000 occurs 2 times, documented 1", findings)

    def test_catalogs_accept_exact_owners_and_report_export_drift(self) -> None:
        self.populate_catalogs()
        self.assertEqual(checker.catalogs(self.root), [])
        self.write("docs/reference/std/outcome.md", "`Result`\n")
        self.assertIn(
            "std::outcome: undocumented Option", checker.catalogs(self.root)
        )

    def test_catalogs_report_core_invocation_drift(self) -> None:
        self.populate_catalogs()
        path = self.root / "docs/reference/language/core-commands.md"
        path.write_text(
            path.read_text(encoding="utf-8").replace(
                "sample [VALUE]", "sample VALUE"
            ),
            encoding="utf-8",
        )
        self.assertIn(
            "core command sample: source invocation 'sample [VALUE]', "
            "documented 'sample VALUE'",
            checker.catalogs(self.root),
        )

    def test_spelling_ignores_fences_and_reports_prose(self) -> None:
        self.populate_navigation()
        self.write(
            "docs/how-to/check-source.md",
            "# Check\n\n```text\nteh\n```\n\n`--task TASK` is exact.\n\n"
            "The the prose says recieve.\n",
        )
        findings = "\n".join(checker.spelling(self.root))
        self.assertNotIn("teh", findings)
        self.assertIn("The the", findings)
        self.assertIn("recieve", findings)

    def test_cli_compares_the_complete_usage_block(self) -> None:
        self.write(
            "docs/reference/tooling/cli.md",
            "# CLI\n\nopaal\nopaal check [--] SOURCE\n",
        )
        completed = subprocess.CompletedProcess(
            ["opaal", "--help"],
            0,
            "OPAAL\n\nUsage:\n  opaal\n  opaal check [--] SOURCE\n\nArguments:\n",
            "",
        )
        with mock.patch.object(checker.subprocess, "run", return_value=completed):
            self.assertEqual(checker.cli(self.root, Path("opaal")), [])
            self.write(
                "docs/reference/tooling/cli.md",
                "# CLI\n\nopaal\nopaal check [--] SOURCE\nopaal stale\n",
            )
            self.assertIn("opaal stale", "\n".join(checker.cli(self.root, Path("opaal"))))

    def test_examples_parse_data_and_check_standalone_opaal(self) -> None:
        self.write(
            "docs/examples.md",
            """# Examples

```toml
schema_version = 1
```

```json
{"enabled": true}
```

```opaal
let answer = 42
answer
```
""",
        )
        self.write("examples/language-foundation.opaal", "42\n")
        commands: list[list[str]] = []

        def runner(command: list[str], **_kwargs: object) -> subprocess.CompletedProcess[str]:
            commands.append(command)
            return subprocess.CompletedProcess(command, 0, "", "")

        self.assertEqual(checker.data_examples(self.root), [])
        self.assertEqual(checker.opaal_examples(self.root, Path("opaal"), run=runner), [])
        self.assertTrue(any(command[1:3] == ["format", "--check"] for command in commands))
        self.assertTrue(any(command[1] == "check" for command in commands))
        self.assertIn(["opaal", str(self.root / "examples/language-foundation.opaal")], commands)

    def test_examples_report_invalid_data_and_unclosed_fence(self) -> None:
        self.write(
            "docs/examples.md",
            "# Examples\n\n```json\n{invalid}\n```\n\n```toml\nopen = true\n",
        )
        findings = "\n".join(checker.data_examples(self.root))
        self.assertIn("invalid json", findings)
        self.assertIn("unclosed fenced code block", findings)


if __name__ == "__main__":
    unittest.main()
