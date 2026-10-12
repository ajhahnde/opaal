#!/usr/bin/env python3
"""Package the self-contained Docker image check reference."""
from __future__ import annotations

import subprocess
from pathlib import Path

if __package__:
    from .package_data_processing import ROOT, package_examples
else:
    from package_data_processing import ROOT, package_examples

FIXTURES = "tests/golden/docker-image-checks"


def check_copies(root: Path) -> None:
    def files(directory):
        return {path.relative_to(directory).as_posix(): path.read_bytes()
                for path in directory.rglob("*") if path.is_file()}
    if files(root / FIXTURES) != files(root / "examples/image-check"):
        raise ValueError("image check examples differ from their maintained fixtures")


def main() -> None:
    if subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT):
        raise ValueError("fixture packaging requires a clean committed checkout")
    check_copies(ROOT)
    source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip()
    files = subprocess.check_output(["git", "ls-files", "-z", "--", FIXTURES], cwd=ROOT)
    names = [path[len(FIXTURES) + 1:] for path in files.decode().split("\0") if path]
    archive = package_examples(ROOT, names, source, directory=FIXTURES, name="docker-image-checks")
    print(archive.relative_to(ROOT))


if __name__ == "__main__":
    main()
