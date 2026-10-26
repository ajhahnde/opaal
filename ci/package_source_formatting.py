#!/usr/bin/env python3
"""Package the exact committed source-formatting fixtures for archive replay."""
from __future__ import annotations

import subprocess

if __package__:
    from .package_data_processing import ROOT, package_examples
else:
    from package_data_processing import ROOT, package_examples

FIXTURES = "tests/golden/source-formatting"
MEMBER_IMPORT_FIXTURES = (
    "before.opaal", "after.opaal", "alternate.opaal", "model.opaal", "facade.opaal",
    "local-main.opaal", "io-allowed.opaal", "io-pure.opaal", "missing.opaal", "private.opaal",
    "duplicate.opaal", "incomplete.opaal", "noncanonical.opaal", "canonical.opaal",
    "pipeline.opaal", "project-tasks.opaal", "value.opaal", "policy-tests.opaal", "policy-negative.opaal",
)



def main() -> None:
    # Every shipped input and qualification assertion belongs to the named head.
    if subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT):
        raise ValueError("fixture packaging requires a clean committed checkout")
    source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip()
    paths = subprocess.check_output(["git", "ls-files", "-z", "--", FIXTURES], cwd=ROOT)
    names = [path[len(FIXTURES) + 1:] for path in paths.decode().split("\0") if path]
    members = {name.removeprefix("explicit-member-imports/") for name in names if name.startswith("explicit-member-imports/")}
    if members != set(MEMBER_IMPORT_FIXTURES):
        raise ValueError("member import fixture census differs from the committed source")
    archive = package_examples(ROOT, names, source, directory=FIXTURES, name="source-formatting")
    print(archive.relative_to(ROOT))


if __name__ == "__main__":
    main()
