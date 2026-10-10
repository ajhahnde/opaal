#!/usr/bin/env python3
"""Check every public source role, documentation fence, embedded owner and fuzz seed."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path

if __package__:
    from . import check_docs
else:
    import check_docs

ROOT = Path(__file__).resolve().parents[1]
INVENTORY = "tests/golden/source-formatting/inventory.json"
MARKERS = re.compile(r"SourceFile::|\.opaal\b|\b(?:def|action)\s+\w+\s*\(")
FUNCTION = re.compile(r"(?m)^\s*(?:(?:pub(?:\([^)]*\))?|async)\s+)*(?:fn|def)\s+(\w+)")
ROLES = {"positive", "semantic-negative", "runtime-negative", "refused", "syntax-invalid",
         "syntax-incomplete", "lexical-only", "noncanonical"}


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def public_paths(root: Path) -> list[str]:
    result = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=root,
    ).decode().split("\0")
    return sorted({name for name in result if name and
                   ((root / name).exists() or (root / name).is_symlink())})


def fences(root: Path, paths: list[str]) -> list[dict]:
    result = []
    for name in paths:
        if name.endswith(".md"):
            for index, (_, source) in enumerate(check_docs.opaal_fences((root / name).read_text())):
                result.append({"path": name, "ordinal": index,
                               "sha256": digest(source.encode())})
    return result


def embedded_owners(root: Path, paths: list[str]) -> list[dict]:
    """A conservative owner census, including constructors, generators and file readers.

    Owner hashes force reclassification when snippets or their assertions change;
    mixed test owners retain legacy, invalid and incomplete buffers deliberately.
    """
    result = []
    for name in paths:
        if Path(name).suffix not in {".rs", ".py", ".sh"}:
            continue
        text = (root / name).read_text()
        markers = list(MARKERS.finditer(text))
        # Helper-based test buffers need no SourceFile or declaration marker.
        if not markers and not ("tests" in Path(name).parts
                                or Path(name).parts[0] in {"benchmarks", "ci", "fuzz"}):
            continue
        functions = list(FUNCTION.finditer(text))
        owners = {function.group(1) for function in functions} if not markers else set()
        for marker in markers:
            preceding = [function for function in functions if function.start() <= marker.start()]
            owners.add(preceding[-1].group(1) if preceding else "module constants/imports")
        result.append({"path": name, "sha256": digest(text.encode()),
                       "source_markers": len(markers), "constructors_or_assertions": sorted(owners)})
    return result


def seed_census(root: Path, paths: list[str]) -> list[dict]:
    script = (root / "fuzz/run-smoke.sh").read_text()
    roots = re.findall(r'"\$repository_root/(tests/opaal-foundation/[^"\n]+)"', script)
    result = []
    common = ["lexer", "parser", "expander", "resources", "secret_sinks"]
    for name in paths:
        targets = []
        if any(name.startswith(folder + "/") for folder in roots):
            targets = common
        elif name.startswith("fuzz/seeds/"):
            targets = [Path(name).parts[2]]
        elif name.startswith("tests/golden/source-formatting/") and name.endswith(".opaal"):
            targets = ["parser"]
        if targets:
            result.append({"path": name, "targets": targets, "sha256": digest((root / name).read_bytes())})
    return result


def compare(label: str, actual: list[dict], expected: list[dict], keys: tuple[str, ...]) -> list[str]:
    identity = lambda row: tuple(row[key] for key in keys)
    observed = {identity(row): row for row in actual}
    recorded = {identity(row): row for row in expected}
    errors = []
    if len(recorded) != len(expected):
        errors.append(f"{label}: duplicate classification")
    for key in sorted(observed.keys() - recorded.keys()):
        errors.append(f"{label}: unclassified {key}")
    for key in sorted(recorded.keys() - observed.keys()):
        errors.append(f"{label}: missing {key}")
    for key in sorted(observed.keys() & recorded.keys()):
        if observed[key] != recorded[key]:
            errors.append(f"{label}: changed identity/content {key}; review its classification")
    return errors


def check(root: Path, binary: Path, inventory: dict) -> list[str]:
    paths = public_paths(root)
    errors = []
    files = inventory["files"]
    errors += compare("source", [{"path": name} for name in paths if name.endswith(".opaal")],
                      [{"path": row["path"]} for row in files], ("path",))
    errors += compare("Markdown", [{"path": name} for name in paths if name.endswith(".md")],
                      inventory["markdown"], ("path",))
    errors += compare("fence", fences(root, paths), inventory["fences"], ("path", "ordinal"))
    errors += compare("embedded owner", embedded_owners(root, paths),
                      [{key: value for key, value in row.items() if key not in {"role", "reason", "assertion"}}
                       for row in inventory["embedded_owners"]], ("path",))
    errors += compare("fuzz seed", seed_census(root, paths), inventory["seeds"], ("path",))
    for row in inventory["embedded_owners"]:
        if (row.get("role") not in {"mixed-test-buffers", "positive-generator", "source-infrastructure"}
                or not row.get("reason") or not row.get("assertion")):
            errors.append(f"{row['path']}: embedded role, reason and owning assertion are required")
    for row in files:
        name, role = row["path"], row["role"]
        path = root / name
        if role not in ROLES or not row.get("reason") or row.get("assertion") not in paths:
            errors.append(f"{name}: role, reason and owning assertion are required")
            continue
        if name not in paths:
            continue
        if not path.is_file() or path.is_symlink():
            errors.append(f"{name}: source must be a regular file")
            continue
        before = path.read_bytes()
        if row.get("sha256") and digest(before) != row["sha256"]:
            errors.append(f"{name}: exact fixture bytes changed")
        result = subprocess.run([str(binary), "format", "--check", "--", str(path)],
                                cwd=root, capture_output=True, timeout=30)
        code = row.get("format_diagnostic")
        if ((role == "positive" and code) or
                (role == "syntax-incomplete" and code != "SYN002") or
                (role == "syntax-invalid" and code in {None, "FMT001", "SYN002"}) or
                (role == "noncanonical" and code != "FMT001") or
                (role != "positive" and not row.get("sha256"))):
            errors.append(f"{name}: role conflicts with its format or exact-byte expectation")
        if code:
            expected = f"error[{code}]: {row['format_message']}".encode()
            good = result.returncode == 1 and not result.stdout and expected in result.stderr
        else:
            good = result.returncode == 0 and not result.stdout and not result.stderr
        if not good:
            errors.append(f"{name}: unexpected format result for {role}: {result.stderr.decode(errors='replace').strip()}")
        if path.read_bytes() != before:
            errors.append(f"{name}: format check changed source bytes")
    # The docs checker owns extraction and canonical checks for positive fences.
    errors += check_docs.check_examples(root, binary)
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, required=True)
    args = parser.parse_args()
    try:
        inventory = json.loads((ROOT / INVENTORY).read_text())
        errors = check(ROOT, args.cli.resolve(), inventory)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        errors = [str(error)]
    for error in errors:
        print(error, file=sys.stderr)
    if errors:
        return 1
    print("All source roles, Markdown fences, embedded owners and fuzz seeds passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
