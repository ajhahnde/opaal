#!/usr/bin/env python3
"""Validate repository guides after product documentation moved to the website."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tempfile
from pathlib import Path
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
ROOT_GUIDES = (
    "README.md", "SECURITY.md", "CONTRIBUTING.md", "CHANGELOG.md",
    "DEVELOPMENT.md", "RELEASING.md", "LICENSE",
)
WEBSITE = "https://opaal-lang.org/docs/"
LINK = re.compile(r"(?<!!)\[[^]]*\]\(([^)]+)\)")
FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})(.*)$")


def opaal_fences(text: str) -> list[tuple[int, str]]:
    """Return source offsets/text for fenced OPAAL blocks, including unclosed blocks."""
    result = []
    opening = None
    offset = start = 0
    language = ""
    for line in text.splitlines(keepends=True):
        match = FENCE.match(line.rstrip("\r\n"))
        if match:
            marker, info = match.groups()
            if opening is None:
                opening = marker
                language = info.strip().split()[0] if info.strip() else ""
                start = offset + len(line)
            elif (marker[0] == opening[0] and len(marker) >= len(opening)
                  and not info.strip()):
                if language == "opaal":
                    result.append((start, text[start:offset]))
                opening = None
        offset += len(line)
    if opening is not None and language == "opaal":
        result.append((start, text[start:]))
    return result


def markdown_pages(root: Path) -> list[Path]:
    if (root / ".git").exists():
        names = subprocess.check_output(
            ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", "*.md"],
            cwd=root,
        ).decode().split("\0")
        return sorted({root / name for name in names if name and (root / name).is_file()})
    pages = [root / name for name in ROOT_GUIDES if name.endswith(".md")]
    for folder in ("benchmarks", "fuzz", "tests"):
        pages.extend((root / folder).rglob("*.md"))
    return sorted({page for page in pages if page.is_file()})


def prose(path: Path) -> list[str]:
    lines = []
    fenced = False
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.lstrip().startswith(("```", "~~~")):
            fenced = not fenced
        elif not fenced:
            lines.append(line)
    return lines


def check_guides(root: Path) -> list[str]:
    errors = [f"{name}: required repository guide is missing"
              for name in ROOT_GUIDES if not (root / name).is_file()]
    if (root / "docs").exists():
        errors.append("docs/: former repository product documentation remains")
    readme = root / "README.md"
    if readme.is_file() and WEBSITE not in readme.read_text(encoding="utf-8"):
        errors.append("README.md: canonical website documentation link is missing")
    return errors


def check_links(root: Path) -> list[str]:
    errors = []
    root = root.resolve()
    for page in markdown_pages(root):
        for line in prose(page):
            for raw in LINK.findall(line):
                parsed = urlsplit(raw.strip().strip("<>").split(" ", 1)[0])
                if parsed.scheme or parsed.netloc or not parsed.path:
                    continue
                target = (page.parent / unquote(parsed.path)).resolve()
                if not target.is_relative_to(root):
                    errors.append(f"{page.relative_to(root)}: link escapes repository: {raw}")
                elif not target.exists():
                    errors.append(f"{page.relative_to(root)}: missing link: {raw}")
    return errors


def check_examples(root: Path, binary: Path) -> list[str]:
    errors = []
    example = root / "examples/language-foundation.opaal"
    for args in (("format", "--check", str(example)), ("check", str(example)),
                 (str(example),)):
        result = subprocess.run([str(binary), *args], cwd=root, capture_output=True, text=True, timeout=30)
        if result.returncode:
            detail = (result.stderr or result.stdout).strip().splitlines()
            errors.append(f"{example.relative_to(root)}: {' '.join(args)} failed: "
                          f"{detail[0] if detail else result.returncode}")
    with tempfile.TemporaryDirectory(prefix="opaal-repository-docs-") as directory:
        for page in markdown_pages(root):
            content = page.read_text(encoding="utf-8")
            for index, (offset, source) in enumerate(opaal_fences(content)):
                path = Path(directory) / f"example-{index}.opaal"
                path.write_text(source, encoding="utf-8")
                for args in (("format", "--check", str(path)), ("check", str(path)), (str(path),)):
                    result = subprocess.run([str(binary), *args], cwd=root,
                                            capture_output=True, text=True, timeout=30)
                    if result.returncode:
                        line = content.count("\n", 0, offset) + 1
                        errors.append(f"{page.relative_to(root)}:{line}: OPAAL example {args[0]} failed")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cli", type=Path, help="built opaal binary for example checks")
    args = parser.parse_args()
    errors = check_guides(ROOT) + check_links(ROOT)
    if args.cli:
        errors += check_examples(ROOT, args.cli.resolve())
    for error in errors:
        print(error, file=sys.stderr)
    if errors:
        return 1
    print("Repository guides, local links, website entry, and examples passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
