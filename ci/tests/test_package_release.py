from __future__ import annotations

import hashlib
import tarfile
import tempfile
import unittest
from pathlib import Path

from ci.package_release import FILES, package


class ReleasePackageTests(unittest.TestCase):
    def test_archive_contains_exact_programs_and_checksum(self) -> None:
        with tempfile.TemporaryDirectory(prefix="opaal-release-package-") as temporary:
            root = Path(temporary)
            for source_name, _, mode in FILES:
                source = root / source_name
                source.parent.mkdir(parents=True, exist_ok=True)
                source.write_bytes(source_name.encode())
                source.chmod(mode)

            archive, checksum = package(root, "linux-x86_64")
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            self.assertEqual(checksum.read_text(), f"{digest}  {archive.name}\n")
            with tarfile.open(archive, "r:gz") as tar:
                self.assertEqual(len(tar.getmembers()), len(FILES))
                for source_name, member_name, mode in FILES:
                    member = tar.getmember(f"opaal-v1.0.0-linux-x86_64/{member_name}")
                    self.assertEqual(member.mode, mode)
                    self.assertEqual(tar.extractfile(member).read(), source_name.encode())

            repeat, _ = package(root, "linux-x86_64")
            self.assertEqual(hashlib.sha256(repeat.read_bytes()).hexdigest(), digest)

    def test_rejects_unknown_platform(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaises(ValueError):
                package(Path(temporary), "linux-arm64")
