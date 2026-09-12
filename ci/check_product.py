#!/usr/bin/env python3
"""Validate the current OPAAL product shape and unpublished release boundary."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tomllib
import urllib.error
import urllib.request
from pathlib import Path
from typing import Callable, Sequence


ROOT = Path(__file__).resolve().parents[1]
VERSION = "1.0.0-alpha.1"
REPOSITORY = "https://github.com/ajhahnde/opaal"
WORKSPACE_PACKAGES = {
    "crates/opaal-syntax": "opaal-syntax",
    "crates/opaal-runtime": "opaal-runtime",
    "crates/opaal-lsp": "opaal-lsp",
    "crates/opaal-platform": "opaal-platform",
    "crates/opaal-platform-posix": "opaal-platform-posix",
    "crates/opaal-cli": "opaal-cli",
}
PRIMARY_BINARIES = {"opaal", "opaal-language-server"}
FUZZ_TARGETS = {"lexer", "parser", "expander", "resources", "secret_sinks"}
SOURCE_DIRECTIVE = re.compile(
    r"^[ \t]*language[ \t]+[0-9]+(?=[ \t]*(?:[;#\r\n]|$))", re.MULTILINE
)
FORBIDDEN_PATHS = {
    "history",
    "transition-evidence",
    "crates/opaal-migrate",
    "tests/opaal-foundation/migration",
    "ci/check_transition.py",
    "fuzz/fuzz_targets/migration.rs",
}
REMOVED_IDENTIFIERS = {
    "ControlledVersionedParseOutcome",
    "ImportStatement",
    "LanguageDetection",
    "LanguageDirective",
    "LanguageIdentity",
    "ModuleNameImport",
    "OPAAL_V1",
    "OPAAL_LANGUAGE_NUMBER",
    "OpaalV1",
    "PureOpaalV1",
    "VersionedParseOutcome",
    "VersionedScript",
    "DEFAULT_OPAAL_V1_CALL_DEPTH",
    "DEFAULT_OPAAL_V1_COLLECTION_BYTES",
    "DEFAULT_OPAAL_V1_COLLECTION_ITEMS",
    "DEFAULT_OPAAL_V1_EVALUATION_STEPS",
    "language_major",
    "parse_opaal_submission",
}
REMOVED_LANGUAGE_IDENTIFIERS = {
    "BracedExpansion",
    "BracedExpansionStart",
    "CommandSubstitution",
    "CommandSubstitutionModifier",
    "CommandSubstitutionStart",
    "Expansion",
    "UnmatchedCommandSubstitution",
    "VariableReference",
}
CLASSIFIED_LEGACY_OPAAL = {
    "tests/opaal-foundation/language/lexical/invalid/removed-command-substitution.opaal",
}
OPAAL_FENCE = re.compile(r"```opaal[^\n]*\n(.*?)```", re.DOTALL)
TEXT_SUFFIXES = {
    "",
    ".json",
    ".md",
    ".opaal",
    ".py",
    ".rs",
    ".sh",
    ".toml",
    ".tsv",
    ".txt",
    ".yaml",
    ".yml",
}
Run = Callable[[Sequence[str], Path], subprocess.CompletedProcess[bytes]]
UrlStatus = Callable[[str], int]


def run_command(command: Sequence[str], root: Path) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(command, cwd=root, capture_output=True, check=False)


def repository_files(root: Path) -> list[Path]:
    if (root / ".git").exists():
        completed = subprocess.run(
            ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
            cwd=root,
            capture_output=True,
            check=True,
        )
        paths = [Path(os.fsdecode(item)) for item in completed.stdout.split(b"\0") if item]
    else:
        paths = [path.relative_to(root) for path in root.rglob("*")]
    return sorted(
        (
            path
            for path in paths
            if not any(part in {".git", "__pycache__", "target"} for part in path.parts)
            and (root / path).is_file()
        ),
        key=Path.as_posix,
    )


def load_toml(path: Path, problems: list[str]) -> dict[str, object]:
    try:
        with path.open("rb") as handle:
            value = tomllib.load(handle)
    except (OSError, tomllib.TOMLDecodeError) as error:
        problems.append(f"{path}: cannot read TOML: {error}")
        return {}
    return value


def command_json(
    label: str,
    command: Sequence[str],
    root: Path,
    run: Run,
    problems: list[str],
) -> object | None:
    completed = run(command, root)
    if completed.returncode != 0:
        detail = completed.stderr.decode(errors="replace").strip()
        problems.append(f"{label}: command failed ({completed.returncode}): {detail}")
        return None
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        problems.append(f"{label}: command returned invalid JSON: {error}")
        return None


def unquoted_dollar_lines(text: str) -> list[int]:
    lines: list[int] = []
    quote: str | None = None
    escaped = False
    for offset, character in enumerate(text):
        if quote == '"' and escaped:
            escaped = False
            continue
        if quote == '"' and character == "\\":
            escaped = True
            continue
        if quote is None and character in {"'", '"'}:
            quote = character
            continue
        if quote == character:
            quote = None
            continue
        if quote is None and character == "$":
            lines.append(text.count("\n", 0, offset) + 1)
    return lines


def removed_language_syntax_problems(path: Path, text: str) -> list[str]:
    relative = path.as_posix()
    if path.suffix == ".opaal" and relative not in CLASSIFIED_LEGACY_OPAAL:
        return [
            f"{relative}:{line}: unquoted dollar syntax remains"
            for line in unquoted_dollar_lines(text)
        ]
    if path.suffix == ".md" and relative != "docs/migration.md":
        problems: list[str] = []
        for fence in OPAAL_FENCE.finditer(text):
            fence_line = text.count("\n", 0, fence.start(1)) + 1
            problems.extend(
                f"{relative}:{fence_line + line - 1}: dollar syntax remains in an OPAAL example"
                for line in unquoted_dollar_lines(fence.group(1))
            )
        return problems
    return []


def source_problems(root: Path, *, run: Run = run_command) -> list[str]:
    problems: list[str] = []
    files = repository_files(root)
    observed_paths = {path.as_posix() for path in files}

    for forbidden in sorted(FORBIDDEN_PATHS):
        if forbidden in observed_paths or any(
            path.startswith(f"{forbidden}/") for path in observed_paths
        ):
            problems.append(f"forbidden current-tree path remains: {forbidden}")
    legacy_sources = sorted(path for path in observed_paths if path.endswith(".fsh"))
    if legacy_sources:
        problems.append(f"unsupported source files remain: {legacy_sources!r}")

    manifest = load_toml(root / "Cargo.toml", problems)
    workspace = manifest.get("workspace", {}) if isinstance(manifest, dict) else {}
    if not isinstance(workspace, dict):
        workspace = {}
    members = workspace.get("members")
    if members != list(WORKSPACE_PACKAGES):
        problems.append(f"workspace members differ: {members!r}")
    package_defaults = workspace.get("package", {})
    expected_defaults = {
        "version": VERSION,
        "license": "MPL-2.0",
        "repository": REPOSITORY,
        "publish": False,
    }
    observed_defaults = {
        key: package_defaults.get(key) if isinstance(package_defaults, dict) else None
        for key in expected_defaults
    }
    if observed_defaults != expected_defaults:
        problems.append(f"workspace package identity differs: {observed_defaults!r}")

    binary_names: set[str] = set()
    for crate_path, expected_name in WORKSPACE_PACKAGES.items():
        crate_manifest = load_toml(root / crate_path / "Cargo.toml", problems)
        package = crate_manifest.get("package", {}) if isinstance(crate_manifest, dict) else {}
        if not isinstance(package, dict) or package.get("name") != expected_name:
            problems.append(f"{crate_path}: package name must be {expected_name!r}")
        if not isinstance(package, dict) or package.get("publish") != {"workspace": True}:
            problems.append(f"{crate_path}: publish must inherit the workspace false value")
        bins = crate_manifest.get("bin", []) if isinstance(crate_manifest, dict) else []
        if isinstance(bins, list):
            binary_names.update(
                entry["name"]
                for entry in bins
                if isinstance(entry, dict) and isinstance(entry.get("name"), str)
            )
    if not PRIMARY_BINARIES <= binary_names:
        problems.append(
            f"primary binaries differ: missing={sorted(PRIMARY_BINARIES - binary_names)!r}"
        )

    fuzz_manifest = load_toml(root / "fuzz/Cargo.toml", problems)
    fuzz_package = fuzz_manifest.get("package", {}) if isinstance(fuzz_manifest, dict) else {}
    if not isinstance(fuzz_package, dict) or (
        fuzz_package.get("name"), fuzz_package.get("publish")
    ) != ("opaal-fuzz", False):
        problems.append("fuzz package must be named opaal-fuzz and remain unpublishable")
    fuzz_bins = fuzz_manifest.get("bin", []) if isinstance(fuzz_manifest, dict) else []
    observed_fuzz = {
        entry["name"]
        for entry in fuzz_bins
        if isinstance(entry, dict) and isinstance(entry.get("name"), str)
    } if isinstance(fuzz_bins, list) else set()
    if observed_fuzz != FUZZ_TARGETS:
        problems.append(f"fuzz targets differ: {sorted(observed_fuzz)!r}")

    metadata = command_json(
        "cargo metadata",
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"],
        root,
        run,
        problems,
    )
    if isinstance(metadata, dict):
        observed_packages = {
            package.get("name"): (package.get("version"), package.get("publish"))
            for package in metadata.get("packages", [])
            if isinstance(package, dict)
        }
        expected_packages = {
            name: (VERSION, []) for name in WORKSPACE_PACKAGES.values()
        }
        if observed_packages != expected_packages:
            problems.append(f"cargo metadata package identity differs: {observed_packages!r}")

    exempt = {
        "ci/check_benchmarks.py",
        "ci/check_product.py",
        "ci/check_public_boundary.py",
        "ci/tests/test_check_product.py",
        "ci/tests/test_check_public_boundary.py",
    }
    predecessor = re.compile(r"\bFlash(?:OS|Shell|V1| 1)?\b|flash-v1|opaal-migrate", re.IGNORECASE)
    for path in files:
        if path.as_posix() in exempt or path.suffix.lower() not in TEXT_SUFFIXES:
            continue
        try:
            text = (root / path).read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        if path.suffix == ".opaal" and SOURCE_DIRECTIVE.search(text):
            problems.append(f"{path.as_posix()}: source-generation directive remains")
        problems.extend(removed_language_syntax_problems(path, text))
        if predecessor.search(text):
            problems.append(f"{path.as_posix()}: predecessor product surface remains")
        if path.parts[:1] == ("crates",):
            for identifier in sorted(REMOVED_IDENTIFIERS):
                if re.search(rf"\b{re.escape(identifier)}\b", text):
                    problems.append(f"{path.as_posix()}: removed API remains: {identifier}")
        if len(path.parts) >= 3 and path.parts[0] == "crates" and path.parts[2] == "src":
            for identifier in sorted(REMOVED_LANGUAGE_IDENTIFIERS):
                if re.search(rf"\b{re.escape(identifier)}\b", text):
                    problems.append(
                        f"{path.as_posix()}: removed language handler remains: {identifier}"
                    )
        is_product_source = len(path.parts) >= 3 and path.parts[0] == "crates" and path.parts[2] == "src"
        if path.parts[:1] == ("docs",) or is_product_source or path.name in {
            "README.md",
            "SECURITY.md",
            "CHANGELOG.md",
        }:
            if re.search(r"\blanguage 1\b", text, re.IGNORECASE):
                problems.append(f"{path.as_posix()}: numeric source-language wording remains")
            if re.search(r"\b(?:language|source)[ -]generation\b", text, re.IGNORECASE):
                problems.append(f"{path.as_posix()}: source-generation wording remains")

    ci_path = root / ".github/workflows/ci.yml"
    release_path = root / ".github/workflows/release.yml"
    security_path = root / ".github/workflows/security.yml"
    try:
        ci = ci_path.read_text(encoding="utf-8")
        release = release_path.read_text(encoding="utf-8")
        security = security_path.read_text(encoding="utf-8")
    except OSError as error:
        problems.append(f"workflow inventory cannot be read: {error}")
    else:
        if "python3 ci/check_product.py source" not in ci:
            problems.append("CI does not run the product source validator")
        if "fuzz/run-smoke.sh" not in ci:
            problems.append("CI does not run the supported fuzz smoke")
        if not re.search(r"needs:\s*\[foundation, policy, fuzz\]", ci):
            problems.append("CI required aggregate does not require foundation, policy, and fuzz")
        if "python3 ci/check_product.py unpublished" not in release:
            problems.append("release workflow does not run the unpublished validator")
        if not re.search(
            r"needs:\s*\[dependency-review, cargo-policy, repository-policy\]",
            security,
        ):
            problems.append("security-required dependency inventory differs")
        if "name: security-required" not in security:
            problems.append("stable security-required aggregate is missing")
        if "check_transition" in ci + release + security:
            problems.append("a workflow still invokes the removed transition checker")

    benchmark = run(
        [sys.executable, "ci/check_benchmarks.py", "--contract-only"], root
    )
    if benchmark.returncode != 0:
        detail = benchmark.stderr.decode(errors="replace").strip()
        problems.append(f"benchmark contract validation failed: {detail}")

    readme = (root / "README.md").read_text(encoding="utf-8", errors="replace")
    changelog = (root / "CHANGELOG.md").read_text(encoding="utf-8", errors="replace")
    if VERSION not in readme or "[Unreleased]" not in changelog:
        problems.append("README and changelog must identify the unreleased development line")
    return sorted(set(problems))


def url_status(url: str) -> int:
    request = urllib.request.Request(
        url,
        headers={"User-Agent": f"opaal-product-validator/{VERSION} (+{REPOSITORY})"},
    )
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            return response.status
    except urllib.error.HTTPError as error:
        return error.code


def unpublished_problems(
    root: Path,
    *,
    run: Run = run_command,
    get_status: UrlStatus = url_status,
) -> list[str]:
    problems = source_problems(root, run=run)

    version = run(
        [
            "cargo",
            "run",
            "--quiet",
            "--locked",
            "-p",
            "opaal-cli",
            "--bin",
            "opaal",
            "--",
            "--version",
        ],
        root,
    )
    if version.returncode != 0 or version.stdout.strip() != f"opaal {VERSION}".encode():
        problems.append("CLI version does not match the unreleased workspace version")

    tags = run(["git", "tag", "--list"], root)
    if tags.returncode != 0 or tags.stdout.strip():
        problems.append("local release tags exist or could not be inspected")

    for label, endpoint in (
        ("tags", "repos/ajhahnde/opaal/tags"),
        ("releases", "repos/ajhahnde/opaal/releases"),
    ):
        value = command_json(
            f"remote {label}", ["gh", "api", endpoint], root, run, problems
        )
        if value != []:
            problems.append(f"remote {label} are present or unavailable")

    for package in [*WORKSPACE_PACKAGES.values(), "opaal-fuzz"]:
        url = f"https://crates.io/api/v1/crates/{package}/{VERSION}"
        try:
            status = get_status(url)
        except Exception as error:  # Network absence is evidence absence, not success.
            problems.append(f"crates.io {package}: unavailable: {error}")
            continue
        if status != 404:
            problems.append(f"crates.io {package}: expected 404, observed {status}")
    return sorted(set(problems))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=("source", "unpublished"))
    arguments = parser.parse_args()
    problems = (
        source_problems(ROOT)
        if arguments.mode == "source"
        else unpublished_problems(ROOT)
    )
    if problems:
        for problem in problems:
            print(problem, file=sys.stderr)
        print(f"product {arguments.mode}: failed ({len(problems)} findings)", file=sys.stderr)
        return 1
    print(f"product {arguments.mode}: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
