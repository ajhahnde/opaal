from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from ci import check_docs as checker


class RepositoryDocumentationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="opaal-repository-docs-test-")
        self.root = Path(self.temporary.name) / "repo"
        self.root.mkdir()
        for name in checker.ROOT_GUIDES:
            (self.root / name).write_text(f"# {name}\n", encoding="utf-8")
        (self.root / "README.md").write_text(
            f"# OPAAL\n\n[Product documentation]({checker.WEBSITE})\n"
            "[Development](DEVELOPMENT.md)\n",
            encoding="utf-8",
        )

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def test_accepts_root_guides_and_local_links(self) -> None:
        self.assertEqual(checker.check_guides(self.root), [])
        self.assertEqual(checker.check_links(self.root), [])

    def test_fences_cover_tildes_indentation_long_closers_and_unclosed_blocks(self) -> None:
        text = "  ~~~ opaal\n1\n  ~~~~\n````text\n```opaal\nignored\n```\n````\n```opaal\n2\n"
        sources = [source for _, source in checker.opaal_fences(text)]
        self.assertEqual(sources, ["1\n", "2\n"])

    def test_rejects_missing_guide_and_old_product_tree(self) -> None:
        (self.root / "RELEASING.md").unlink()
        (self.root / "docs").mkdir()
        findings = checker.check_guides(self.root)
        self.assertIn("RELEASING.md: required repository guide is missing", findings)
        self.assertIn("docs/: former repository product documentation remains", findings)

    def test_rejects_missing_website_entry(self) -> None:
        (self.root / "README.md").write_text("# OPAAL\n", encoding="utf-8")
        self.assertIn("README.md: canonical website documentation link is missing",
                      checker.check_guides(self.root))

    def test_rejects_stale_or_escaping_links(self) -> None:
        (self.root / "README.md").write_text(
            f"[Docs]({checker.WEBSITE})\n"
            "[Old reference](docs/reference/language/README.md)\n"
            "[Outside](../private.md)\n",
            encoding="utf-8",
        )
        findings = "\n".join(checker.check_links(self.root))
        self.assertIn("missing link: docs/reference/language/README.md", findings)
        self.assertIn("link escapes repository: ../private.md", findings)


if __name__ == "__main__":
    unittest.main()
