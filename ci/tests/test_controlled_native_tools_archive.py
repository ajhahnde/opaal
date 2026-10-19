from __future__ import annotations

import json
import shutil
import tempfile
import unittest
from contextlib import ExitStack
from pathlib import Path
from unittest.mock import patch

from ci import qualify_controlled_native_tools as native
from ci.package_data_processing import package_examples, write_manifest
from ci.package_release import ROOT, VERSION, package
from ci.qualify_data_processing import QualificationError


class NativeArchiveTests(unittest.TestCase):
    def setUp(self):
        self.stack = ExitStack()
        self.addCleanup(self.stack.close)
        self.root = Path(self.stack.enter_context(tempfile.TemporaryDirectory())).resolve()
        self.source = "1" * 40
        self.target = "macos-arm64"
        self.output = self.root / "replay"
        self.checkout = self.root / "checkout"
        shutil.copytree(ROOT / native.FIXTURES, self.checkout / native.FIXTURES,
                        ignore=shutil.ignore_patterns("__pycache__"))
        shutil.copytree(ROOT / "examples/repository-check", self.checkout / "examples/repository-check",
                        ignore=shutil.ignore_patterns("__pycache__"))
        for name in ("LICENSE", "README.md"):
            (self.checkout / name).write_bytes((ROOT / name).read_bytes())
        for name in ("opaal", "opaal-language-server"):
            path = self.checkout / "target/release" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(f"#!/bin/sh\nprintf 'opaal {VERSION}\\n'\n")
            path.chmod(0o755)
        self.names = sorted(path.relative_to(self.checkout / native.FIXTURES).as_posix()
                            for path in (self.checkout / native.FIXTURES).rglob("*") if path.is_file())
        self.archive, _ = package(self.checkout, self.target)
        write_manifest(self.archive, kind="binaries", source=self.source, platform=self.target)
        self.fixtures = package_examples(self.checkout, self.names, self.source,
                                        directory=native.FIXTURES, name="controlled-native-tools")
        self.stack.enter_context(patch.object(native, "ROOT", self.checkout))
        self.stack.enter_context(patch.object(native, "public_paths", return_value=[
            native.FIXTURES + "/" + name for name in self.names]))
        self.git = self.stack.enter_context(patch.object(native.subprocess, "check_output", side_effect=lambda args, **kw:
            (self.source + "\n").encode() if args[1] == "rev-parse" else b""))
        self.stack.enter_context(patch.object(native.platform, "system", return_value="Darwin"))
        self.stack.enter_context(patch.object(native.platform, "machine", return_value="arm64"))
        self.replay = self.stack.enter_context(patch.object(native, "qualify", side_effect=self.observe_replay))

    def observe_replay(self, binary, inputs, output, **options):
        self.assertFalse(binary.is_relative_to(self.checkout))
        self.assertFalse(inputs.is_relative_to(self.checkout))
        self.assertEqual(binary.read_bytes(), (self.checkout / "target/release/opaal").read_bytes())
        self.assertEqual({path.relative_to(inputs).as_posix() for path in inputs.rglob("*") if path.is_file()},
                         set(self.names))
        output.mkdir()
        return {"omitted": ["archive and full candidate qualification", "native faults"]}

    def run_archive(self, **changes):
        options = dict(archive=self.archive, fixtures=self.fixtures, output=self.output,
                       source=self.source, target=self.target)
        options.update(changes)
        return native.qualify_archive(**options)

    def test_exact_pair_replays_extracted_bytes_and_records_both_identities(self):
        result = self.run_archive(checker=Path("/explicit/tool"))
        self.assertEqual(result["archive_sha256"], native.sha(self.archive))
        self.assertEqual(result["fixture_archive_sha256"], native.sha(self.fixtures))
        self.assertEqual(result["source"], self.source)
        self.assertTrue(result["committed_source"])
        self.assertEqual(result["omitted"], ["native faults", "full candidate qualification"])
        self.assertEqual(self.replay.call_args.kwargs, {"checker": Path("/explicit/tool")})
        self.assertEqual(json.loads((self.output / "native-tools-comparison.json").read_bytes()), result)

    def test_linux_pair_uses_linux_identity(self):
        self.target = "linux-x86_64"
        self.archive, _ = package(self.checkout, self.target)
        write_manifest(self.archive, kind="binaries", source=self.source, platform=self.target)
        with patch.object(native.platform, "system", return_value="Linux"), \
                patch.object(native.platform, "machine", return_value="x86_64"):
            self.assertEqual(self.run_archive()["platform"], self.target)

    def test_missing_pair_source_host_and_dirty_checkout_refuse_before_replay(self):
        for changes in ({"fixtures": None}, {"source": None}, {"target": None},
                        {"source": "2" * 40}, {"target": "linux-x86_64"}, {"output": self.checkout / "replay"}):
            with self.subTest(changes=changes), self.assertRaises(QualificationError):
                self.run_archive(**changes)
        self.git.side_effect = lambda args, **kw: ((self.source + "\n").encode()
                                                  if args[1] == "rev-parse" else b" M README.md\n")
        with self.assertRaisesRegex(QualificationError, "clean committed"):
            self.run_archive()
        self.replay.assert_not_called()
        self.assertFalse(self.output.exists())

    def test_each_archive_rejects_wrong_identity_checksum_and_member_hash(self):
        for archive in (self.archive, self.fixtures):
            manifest = archive.with_name(archive.name + ".manifest.json")
            original = manifest.read_bytes()
            for fault in ("source", "platform", "sha256", "member"):
                data = json.loads(original)
                if fault == "member":
                    data["members"][0]["sha256"] = "0" * 64
                else:
                    data[fault] = "0" * 64
                manifest.write_text(json.dumps(data))
                with self.subTest(archive=archive.name, fault=fault), self.assertRaises(QualificationError):
                    self.run_archive()
                manifest.write_bytes(original)
        self.replay.assert_not_called()
        self.assertFalse(self.output.exists())

    def test_self_consistent_reference_archive_cannot_change_source_inventory_or_bytes(self):
        repack = self.root / "repack"
        shutil.copytree(self.checkout / native.FIXTURES, repack / native.FIXTURES)
        for fault in ("missing", "extra", "changed"):
            names = self.names.copy()
            if fault == "missing":
                names.pop()
            elif fault == "extra":
                (repack / native.FIXTURES / "extra.txt").write_bytes(b"extra")
                names.append("extra.txt")
            else:
                (repack / native.FIXTURES / names[0]).write_bytes(b"changed")
            fixtures = package_examples(repack, names, self.source, directory=native.FIXTURES,
                                        name="controlled-native-tools")
            with self.subTest(fault=fault), self.assertRaisesRegex(QualificationError, "inventory/bytes"):
                self.run_archive(fixtures=fixtures)
        self.replay.assert_not_called()
        self.assertFalse(self.output.exists())

    def test_wrong_binary_version_and_existing_output_refuse_before_replay(self):
        with patch.object(native, "command") as command:
            command.return_value.returncode = 0
            command.return_value.stderr = b""
            command.return_value.stdout = b"opaal 0.0.0\n"
            with self.assertRaisesRegex(QualificationError, "binary version"):
                self.run_archive()
        self.output.mkdir()
        (self.output / "keep").write_bytes(b"original")
        with self.assertRaisesRegex(QualificationError, "exclusive"):
            self.run_archive()
        self.assertEqual((self.output / "keep").read_bytes(), b"original")
        self.replay.assert_not_called()


if __name__ == "__main__":
    unittest.main()
