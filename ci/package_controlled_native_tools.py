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
    "image": (115, "03bb3cc4f8ea719206db79f4475dd12d530b5df572e2a1179d77f0aabc1c5686"),
    "prepare": (49, "c3e1d0bfb4011f460ea11195f0ee5a9f89c02169ff37212b75fc2cf2ffd4b834"),
    "evidence": (130, "8ed76a51dd06c9986a7a815ca815bca30c5ae8ca5ddf5e92f9a49e380886ea38"),
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
