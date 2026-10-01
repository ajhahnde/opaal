from __future__ import annotations

import io
import json
import tarfile
import tempfile
import unittest
from pathlib import Path

from ci.package_data_processing import EXAMPLES, VERSION, digest, package_examples, write_manifest
from ci.qualify_data_processing import QualificationError, measure, unpack


SOURCE = "1" * 40


class CandidateIdentityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="opaal-candidate-test-")
        self.root = Path(self.temporary.name)
        fixture = self.root / EXAMPLES
        fixture.mkdir(parents=True)
        (fixture / "report.opaal").write_bytes(b"1 + 2\n")
        (fixture / "invalid").mkdir()
        (fixture / "invalid/utf8.json").write_bytes(b"\xff")
        self.names = ["report.opaal", "invalid/utf8.json"]
        self.archive = package_examples(self.root, self.names, SOURCE)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def extract(self, *, source: str = SOURCE) -> Path:
        result, _ = unpack(self.archive, self.root / "extracted", kind="examples",
                           version=VERSION, source=source, platform="portable")
        return result

    def replace_archive(self, members: list[tarfile.TarInfo], data: bytes = b"payload") -> None:
        with tarfile.open(self.archive, "w:gz") as tar:
            for member in members:
                tar.addfile(member, io.BytesIO(data) if member.isfile() else None)
        self.archive.with_name(self.archive.name + ".sha256").write_text(
            f"{digest(self.archive.read_bytes())}  {self.archive.name}\n")
        # Build manifests directly so forbidden archive members reach the
        # consumer even when the normal producer would refuse them.
        manifest = {
            "schema_version": 1, "kind": "examples", "version": VERSION,
            "source": SOURCE, "platform": "portable", "archive": self.archive.name,
            "sha256": digest(self.archive.read_bytes()),
            "members": [{"path": m.name, "bytes": m.size, "mode": m.mode,
                         "sha256": digest(data)} for m in members],
        }
        self.archive.with_name(self.archive.name + ".manifest.json").write_text(json.dumps(manifest))

    def test_bundle_is_deterministic_and_preserves_exact_fixture_bytes(self) -> None:
        original = self.archive.read_bytes()
        original_manifest = self.archive.with_name(self.archive.name + ".manifest.json").read_bytes()
        package_examples(self.root, list(reversed(self.names)), SOURCE)
        self.assertEqual(self.archive.read_bytes(), original)
        self.assertEqual(self.archive.with_name(self.archive.name + ".manifest.json").read_bytes(), original_manifest)
        installed = self.extract()
        for name in self.names:
            self.assertEqual((installed / name).read_bytes(), (self.root / EXAMPLES / name).read_bytes())
            self.assertEqual((installed / name).stat().st_mode & 0o777, 0o644)

    def test_wrong_source_and_tampered_archive_are_refused_before_extraction(self) -> None:
        with self.assertRaises(QualificationError):
            self.extract(source="2" * 40)
        self.assertFalse((self.root / "extracted").exists())
        self.archive.write_bytes(self.archive.read_bytes() + b"tampered")
        with self.assertRaises(QualificationError):
            self.extract()
        self.assertFalse((self.root / "extracted").exists())

    def test_member_hash_and_checksum_sidecar_are_each_required(self) -> None:
        manifest_path = self.archive.with_name(self.archive.name + ".manifest.json")
        original = manifest_path.read_text()
        manifest = json.loads(original)
        manifest["members"][0]["sha256"] = "0" * 64
        manifest_path.write_text(json.dumps(manifest))
        with self.assertRaises(QualificationError):
            self.extract()
        self.assertFalse((self.root / "extracted").exists())
        manifest_path.write_text(original)
        self.archive.with_name(self.archive.name + ".sha256").write_text("wrong\n")
        with self.assertRaises(QualificationError):
            self.extract()

    def test_traversal_links_duplicates_wrong_modes_and_oversize_members_are_refused(self) -> None:
        prefix = f"opaal-v{VERSION}-data-processing"
        for kind in ("traversal", "symlink", "duplicate", "mode", "size"):
            with self.subTest(kind=kind):
                member = tarfile.TarInfo(f"{prefix}/../../outside" if kind == "traversal" else f"{prefix}/report.opaal")
                member.size, member.mode = 7, 0o755 if kind == "mode" else 0o644
                if kind == "symlink":
                    member.type, member.linkname, member.size = tarfile.SYMTYPE, "../../outside", 0
                if kind == "size":
                    # A modest compressed zero stream exercises expanded-byte admission.
                    member.size = 8 * 1024 * 1024 + 1
                self.replace_archive([member, member] if kind == "duplicate" else [member],
                                     b"\0" * member.size if kind == "size" else b"payload")
                with self.assertRaises(QualificationError):
                    self.extract()
                self.assertFalse((self.root / "extracted").exists())
                self.assertFalse((self.root / "outside").exists())

    def test_producer_refuses_duplicate_traversal_and_symlink_inputs(self) -> None:
        for names in (["report.opaal", "report.opaal"], ["../report.opaal"]):
            with self.assertRaises(ValueError):
                package_examples(self.root, names, SOURCE)
        (self.root / EXAMPLES / "alias.opaal").symlink_to("report.opaal")
        with self.assertRaises(ValueError):
            package_examples(self.root, ["alias.opaal"], SOURCE)

    def test_manifest_requires_full_source_identity(self) -> None:
        with self.assertRaises(ValueError):
            write_manifest(self.archive, kind="examples", source="HEAD", platform="portable")

    def test_measurement_worker_preserves_bytes_and_rejects_failed_children(self) -> None:
        import sys
        output, measurement = measure([sys.executable, "-c", "print('report', end='')"], self.root, {})
        self.assertEqual(output, b"report")
        self.assertGreater(measurement["peak_rss_bytes"], 0)
        with self.assertRaises(QualificationError):
            measure([sys.executable, "-c", "raise SystemExit(7)"], self.root, {})
