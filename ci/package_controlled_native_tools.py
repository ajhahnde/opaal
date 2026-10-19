#!/usr/bin/env python3
"""Package the fixed repository reference and independent native-tool cases."""
from __future__ import annotations

import hashlib
import json
import subprocess
from pathlib import Path

if __package__:
    from .package_data_processing import ROOT, package_examples
else:
    from package_data_processing import ROOT, package_examples

FIXTURES = "tests/golden/controlled-external-tools"
CASE_MANIFESTS = {
    "image": (115, "20379050cdda8c7e4ad1a9bfdac3d1261fcd4d15f46b371f6764ad46b205eed4"),
    "prepare": (49, "18e0080599832f73e99534dddac612b905f3a2ab14bf2c2bdf069ec6a0cf9e32"),
    "evidence": (130, "04e86885c16d4a3c80758529d0622f83bc06d6c5efe7766006bb4c9bcb1bc10f"),
}


def check_cases(inputs: Path) -> dict:
    inventories = {}
    for name, (count, digest) in CASE_MANIFESTS.items():
        data = (inputs / (name + "-cases.json")).read_bytes()
        if hashlib.sha256(data).hexdigest() != digest:
            raise ValueError("native case inventory differs: " + name)
        document = json.loads(data)
        cases = document["cases"]
        if document["schema_version"] != 1 or len(cases) != count or len({case["name"] for case in cases}) != count:
            raise ValueError("native case inventory is invalid: " + name)
        inventories[name] = cases
    return inventories


def check_copies(root: Path) -> None:
    def files(directory):
        return {path.relative_to(directory).as_posix(): path.read_bytes()
                for path in directory.rglob("*") if path.is_file() and "__pycache__" not in path.parts}
    if files(root / FIXTURES / "reference") != files(root / "examples/repository-check"):
        raise ValueError("repository check examples differ from their frozen reference")
    check_cases(root / FIXTURES)


def main() -> None:
    if subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT):
        raise ValueError("fixture packaging requires a clean committed checkout")
    check_copies(ROOT)
    source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip()
    files = subprocess.check_output(["git", "ls-files", "-z", "--", FIXTURES], cwd=ROOT)
    names = [path[len(FIXTURES) + 1:] for path in files.decode().split("\0") if path]
    archive = package_examples(ROOT, names, source, directory=FIXTURES, name="controlled-native-tools")
    print(archive.relative_to(ROOT))


if __name__ == "__main__":
    main()
