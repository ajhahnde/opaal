from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path


sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import check_public_boundary as boundary  # noqa: E402


CHECKOUT = "3d3c42e5aac5ba805825da76410c181273ba90b1"


class PublicBoundaryTests(unittest.TestCase):
    def fixture(self) -> tempfile.TemporaryDirectory[str]:
        temporary = tempfile.TemporaryDirectory(prefix="opaal-public-boundary-")
        root = Path(temporary.name)
        for path in boundary.REQUIRED_POLICY_FILES:
            destination = root / path
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text("policy\n", encoding="utf-8")
        (root / ".github/workflows/ci.yml").write_text(
            "permissions:\n  contents: read\nsteps:\n"
            f"  - uses: actions/checkout@{CHECKOUT}\n",
            encoding="utf-8",
        )
        return temporary

    def test_accepts_pinned_read_only_standalone_policy(self) -> None:
        with self.fixture() as temporary:
            self.assertEqual(boundary.audit(Path(temporary)), [])

    def test_rejects_private_path_and_mutable_action(self) -> None:
        with self.fixture() as temporary:
            root = Path(temporary)
            (root / "README.md").write_text(
                "See ajhahnde/work/current.md.\n", encoding="utf-8"
            )
            (root / ".github/workflows/ci.yml").write_text(
                "steps:\n  - uses: actions/checkout@main\n", encoding="utf-8"
            )
            findings = "\n".join(boundary.audit(root))
            self.assertIn("private project path", findings)
            self.assertIn("not pinned to a full commit SHA", findings)

    def test_rejects_write_permission_and_publish_command(self) -> None:
        with self.fixture() as temporary:
            root = Path(temporary)
            (root / ".github/workflows/release.yml").write_text(
                "permissions:\n  contents: write\nsteps:\n"
                "  - run: cargo publish\n",
                encoding="utf-8",
            )
            findings = "\n".join(boundary.audit(root))
            self.assertIn("write permission is forbidden", findings)
            self.assertIn("publishing command is forbidden", findings)


if __name__ == "__main__":
    unittest.main()
