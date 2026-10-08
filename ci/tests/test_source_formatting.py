from __future__ import annotations

import subprocess
import tempfile
import unittest
from unittest.mock import patch
from pathlib import Path

from ci import check_source_formatting as checker
from ci import package_source_formatting
from ci.qualify_source_formatting import normalized_artifact


class SourceInventoryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="opaal-source-inventory-")
        self.root = Path(self.temporary.name) / "repo"
        self.root.mkdir()
        subprocess.run(["git", "init", "--quiet", str(self.root)], check=True)
        self.write("examples/language-foundation.opaal", "1\n")
        self.write("README.md", "# Repository\n")
        self.write("fuzz/run-smoke.sh", "# corpus roots\n")
        self.binary = Path(self.temporary.name) / "opaal"
        self.binary.write_text("#!/bin/sh\nexit 0\n")
        self.binary.chmod(0o755)
        self.inventory = self.census()

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write(self, name: str, text: str) -> None:
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def census(self) -> dict:
        paths = checker.public_paths(self.root)
        return {
            "files": [{"path": "examples/language-foundation.opaal", "role": "positive",
                       "reason": "example", "assertion": "README.md"}],
            "markdown": [{"path": name} for name in paths if name.endswith(".md")],
            "fences": checker.fences(self.root, paths),
            "embedded_owners": [dict(row, role="mixed-test-buffers", reason="test owner",
                                    assertion=row["path"])
                                for row in checker.embedded_owners(self.root, paths)],
            "seeds": checker.seed_census(self.root, paths),
        }

    def test_added_missing_and_duplicate_sources_fail(self) -> None:
        self.assertEqual(checker.check(self.root, self.binary, self.inventory), [])
        self.write("new/location/unclassified.opaal", "2\n")
        self.assertIn("source: unclassified", "\n".join(checker.check(self.root, self.binary, self.inventory)))
        (self.root / "new/location/unclassified.opaal").unlink()
        self.inventory["files"].append(dict(self.inventory["files"][0]))
        self.assertIn("source: duplicate classification", checker.check(self.root, self.binary, self.inventory))
        (self.root / "examples/language-foundation.opaal").unlink()
        self.assertIn("source: missing", "\n".join(checker.check(self.root, self.binary, self.inventory)))

    def test_new_markdown_directory_and_changed_fence_fail(self) -> None:
        self.write("new/guide.md", "  ~~~opaal\n1\n  ~~~\n")
        errors = "\n".join(checker.check(self.root, self.binary, self.inventory))
        self.assertIn("Markdown: unclassified", errors)
        self.assertIn("fence: unclassified", errors)
        self.inventory = self.census()
        self.write("new/guide.md", "  ~~~opaal\n2\n  ~~~\n")
        self.assertIn("fence: changed identity/content", "\n".join(checker.check(self.root, self.binary, self.inventory)))

    def test_embedded_sources_and_non_opaal_seeds_are_discovered(self) -> None:
        self.write("crates/sample.rs", 'fn oracle() { SourceFile::new(0, "test", "1"); }\n')
        self.write("crates/tests/helper.rs", 'fn helper() { value("1 + 2"); }\n')
        self.write("fuzz/seeds/parser/plain.txt", "not a complete script")
        errors = "\n".join(checker.check(self.root, self.binary, self.inventory))
        self.assertIn("embedded owner: unclassified", errors)
        self.assertIn("fuzz seed: unclassified", errors)
        owners = checker.embedded_owners(self.root, checker.public_paths(self.root))
        self.assertEqual(owners[0]["constructors_or_assertions"], ["oracle"])
        self.assertEqual(owners[1]["constructors_or_assertions"], ["helper"])
        self.write("crates/sample.rs", 'fn oracle() { SourceFile::new(0, "test", "2"); }\n')
        self.assertIn("changed identity/content", "\n".join(checker.compare(
            "owner", checker.embedded_owners(self.root, checker.public_paths(self.root)), owners, ("path",))))

    def test_fixture_bytes_roles_and_symlinks_fail_closed(self) -> None:
        row = self.inventory["files"][0]
        row.update(role="semantic-negative", sha256=checker.digest(b"previous\n"))
        self.assertIn("exact fixture bytes changed", "\n".join(checker.check(self.root, self.binary, self.inventory)))
        row["role"] = "unknown"
        self.assertIn("role, reason", "\n".join(checker.check(self.root, self.binary, self.inventory)))
        row["role"] = "positive"
        path = self.root / row["path"]
        path.unlink()
        path.symlink_to(self.binary)
        self.assertIn("source must be a regular file", "\n".join(checker.check(self.root, self.binary, self.inventory)))


class QualificationComparisonTests(unittest.TestCase):
    def test_source_receipts_change_but_authority_and_results_still_compare(self) -> None:
        legacy = {"schema": "opaal.plan.v2", "digest": "old", "created_at": "old", "expires_at": "old",
                  "task": {"contract_digest": "old", "action_id": "readiness"},
                  "sources": [{"module": "tasks.opaal", "digest": "old", "size": 1}],
                  "observations": [{"kind": "source", "digest": "old", "size": 1},
                                   {"kind": "wall-clock", "observed_at": "old"}],
                  "actions": [{"id": "old#000000", "contract_digest": "old", "requests": []}],
                  "authority": {"requests": []}, "outcome": {"class": "success"}}
        import copy
        canonical = copy.deepcopy(legacy)
        canonical.update(digest="new", created_at="new", expires_at="new")
        canonical["task"]["contract_digest"] = "new"
        canonical["sources"][0].update(digest="new", size=2)
        canonical["observations"][0].update(digest="new", size=2)
        canonical["observations"][1]["observed_at"] = "new"
        canonical["actions"][0].update(id="new#000000", contract_digest="new")
        self.assertEqual(normalized_artifact(legacy), normalized_artifact(canonical))
        for field, changed in (("authority", {"requests": ["clock.wall"]}), ("outcome", {"class": "refused"})):
            candidate = copy.deepcopy(canonical)
            candidate[field] = changed
            self.assertNotEqual(normalized_artifact(legacy), normalized_artifact(candidate))
        self.assertEqual(legacy["digest"], "old")

    def test_packaging_refuses_dirty_source_before_writing(self) -> None:
        with patch("ci.package_source_formatting.subprocess.check_output", return_value=b" M tasks.opaal\n"):
            with self.assertRaisesRegex(ValueError, "clean committed checkout"):
                package_source_formatting.main()


if __name__ == "__main__":
    unittest.main()
