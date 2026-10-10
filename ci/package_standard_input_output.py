#!/usr/bin/env python3
"""Package the self-contained standard-stream and CI check references."""
from __future__ import annotations

import subprocess
from pathlib import Path

if __package__:
    from .package_data_processing import ROOT, package_examples
else:
    from package_data_processing import ROOT, package_examples

FIXTURES = "tests/golden/standard-input-output"


def check_copies(root: Path, fixtures: Path) -> None:
    owner = root / "tests/golden/data-processing"
    for path in [owner / "report.opaal", *sorted((owner / "valid").glob("*")),
                 *sorted((owner / "invalid").glob("*"))]:
        copy = fixtures / "ci-job-check" / path.relative_to(owner)
        if copy.read_bytes() != path.read_bytes():
            raise ValueError(f"CI reference copy differs from its maintained owner: {path.name}")


def main() -> None:
    if subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT):
        raise ValueError("fixture packaging requires a clean committed checkout")
    check_copies(ROOT, ROOT / FIXTURES)
    source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip()
    files = subprocess.check_output(["git", "ls-files", "-z", "--", FIXTURES], cwd=ROOT)
    names = [path[len(FIXTURES) + 1:] for path in files.decode().split("\0") if path]
    archive = package_examples(ROOT, names, source, directory=FIXTURES, name="standard-input-output")
    print(archive.relative_to(ROOT))


if __name__ == "__main__":
    main()
