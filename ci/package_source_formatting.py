#!/usr/bin/env python3
"""Package the exact committed source-formatting fixtures for archive replay."""
from __future__ import annotations

import subprocess

if __package__:
    from .package_data_processing import ROOT, package_examples
else:
    from package_data_processing import ROOT, package_examples

FIXTURES = "tests/golden/source-formatting"


def main() -> None:
    # Every shipped input and qualification assertion belongs to the named head.
    if subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT):
        raise ValueError("fixture packaging requires a clean committed checkout")
    source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip()
    paths = subprocess.check_output(["git", "ls-files", "-z", "--", FIXTURES], cwd=ROOT)
    names = [path[len(FIXTURES) + 1:] for path in paths.decode().split("\0") if path]
    archive = package_examples(ROOT, names, source, directory=FIXTURES, name="source-formatting")
    print(archive.relative_to(ROOT))


if __name__ == "__main__":
    main()
