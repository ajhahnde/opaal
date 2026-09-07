#!/usr/bin/env python3
"""Verify transition claims owned by the standalone OPAAL repository."""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import tomllib
from dataclasses import dataclass, field
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
EVIDENCE_ROOT = ROOT / "transition-evidence" / "claims"

EXTRACTED_SOURCE_TIP = "6b010a66e9ef9df125ff599e6a242eaf8fce85fc"
EXTRACTED_SOURCE_TREE = "5cd3774ebc5500e3a1a1cea5b5706c1eab5c44e2"
FLASHOS_SOURCE_HEAD = "ce532330507f2b302bf603c0ffe27c11d7829d24"
FLASHOS_RETAINED_API_HEAD = "1ee171795d1c4c45c703bf90d4cc547b7bbaf2fd"
PUBLIC_SOURCE_PATH_COUNT = 427
PUBLIC_SOURCE_PATH_SHA256 = "60f985e7ebf9ec4d768cf6732a088f7a8ea092d18e1b92abb5a6ad30d5893747"
PUBLIC_INVENTORY_SHA256 = "05ba099856a4d6324ed95771d189705c2eda040349ecb77b2f6ed6e8cd52a1da"
PUBLIC_DISPOSITION_SHA256 = "3eb5ce4558effd89229a9a18bb203129e8f6bdec4bfdcce2d2112fa6499d77e6"
SOURCE_PATH_COMMIT_COUNT = 169
SOURCE_PATH_COMMIT_SHA256 = "3e5cd83fdee5d00fcaf55a9a0be1f394c842b398a93828725db9f7331a49c3e6"
MAPPED_SOURCE_COMMIT_SHA256 = "c31b353903c4068b1d98a752b14c4276f1a62e3400807c5a2116c089a1908989"
TOPOLOGY_ONLY_SOURCE_COMMITS = {
    "952dfaee5f429c6997ad4d925e02dc0afb735c13",
    "1e1ab57df01621c8135c60517d456331d42ccb7c",
}
FLASH_V1_RELEASE_SOURCE_COMMIT = "16609e7e71022eba8ba5bc884a47225c50e4ba7c"
FLASH_V1_RELEASE_DERIVED_COMMIT = "c3c603e8981e83b404d01b4c52c408f7c1d8c3b5"
FILTER_REPO_VERSION = "a40bce548d2c"

PUBLIC_REPLACEMENTS = {
    ".github/dependabot.yml",
    ".github/workflows/ci.yml",
    ".github/workflows/release.yml",
    ".github/workflows/security.yml",
    "CONTRIBUTING.md",
    "SECURITY.md",
    "benchmarks/evidence/host-darwin-arm64-opaal-v1.json",
    "ci/check_benchmarks.py",
    "ci/check_public_boundary.py",
    "ci/check_transition.py",
    "ci/tests/test_check_benchmarks.py",
    "examples/language-foundation.opaal",
}

WORKSPACE_PACKAGES = {
    "crates/opaal-syntax": "opaal-syntax",
    "crates/opaal-migrate": "opaal-migrate",
    "crates/opaal-runtime": "opaal-runtime",
    "crates/opaal-lsp": "opaal-lsp",
    "crates/opaal-platform": "opaal-platform",
    "crates/opaal-platform-posix": "opaal-platform-posix",
    "crates/opaal-cli": "opaal-cli",
}

PRIMARY_BINARIES = {
    "opaal",
    "opaal-language-server",
    "opaal-migrate-flash-v1",
}

FIXTURE_IDENTIFIERS = {
    "opaal-language-server-fixture",
    "opaal-repl-fixture",
    "opaal-benchmark-fixture",
    "opaal-e2e-hangup-observer-fixture",
    "opaal-e2e-process-observer-fixture",
    "opaal-e2e-status-fixture",
    "opaal-e2e-stream-fixture",
    "opaal-job-observer-fixture",
    "opaal-process-observer-fixture",
    "opaal-signal-guard-fixture",
    "opaal-status-fixture",
    "opaal-stream-fixture",
    "opaal-terminal-editor-fixture",
}

OPAAL_ENVIRONMENT_NAMES = {
    "OPAAL_GUARD_OBSERVER",
    "OPAAL_GUARD_REPORT",
    "OPAAL_GUARD_WORKSPACE",
    "OPAAL_LSP_EFFECT",
    "OPAAL_PROBE_FD",
    "OPAAL_PROBE_GROUP_REPORT",
    "OPAAL_PROBE_HOLD_UNTIL",
    "OPAAL_PROBE_RAISE",
    "OPAAL_PROBE_REPORT",
    "OPAAL_PROBE_VALUE",
    "OPAAL_TEST_CONTINUATION_PROMPT",
    "OPAAL_TEST_EXTERNAL_NOTICE",
    "OPAAL_TEST_HANGUP_IGNORE_CHILD",
    "OPAAL_TEST_PERSISTENT_HISTORY",
    "OPAAL_TEST_PROMPT",
    "OPAAL_TEST_SAFE_MODE",
    "OPAAL_TEST_TERMINAL_RESTORE",
}

OPAAL_DIAGNOSTIC_CODES = {
    "OP0001",
    "OP0002",
    "OP1000",
    "OP1001",
    "OP2001",
    "OP2002",
    "OP2003",
    "OP2004",
}

MIGRATION_DIAGNOSTIC_CODES = {
    "AUTH3001",
    "MIG1001",
    "MIG2004",
    "MIG3001",
    "MIG3002",
    "MIG3003",
    "MIG3005",
}

LEGACY_SYNTAX_FILES = {
    "crates/opaal-syntax/src/classification.rs",
    "crates/opaal-syntax/src/formatter.rs",
    "crates/opaal-syntax/src/lexer.rs",
    "crates/opaal-syntax/src/parser.rs",
}


@dataclass
class Verification:
    claim: str
    assertions: list[dict[str, object]] = field(default_factory=list)
    commands: list[dict[str, object]] = field(default_factory=list)

    def check(self, name: str, condition: bool, detail: str) -> None:
        self.assertions.append({"name": name, "success": condition, "detail": detail})

    @property
    def success(self) -> bool:
        return all(bool(item["success"]) for item in self.assertions)

    def run(
        self,
        name: str,
        command: list[str],
        *,
        cwd: Path = ROOT,
        expect: int = 0,
        public_command: list[str] | None = None,
        env: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess[bytes]:
        environment = None if env is None else {**os.environ, **env}
        completed = subprocess.run(
            command,
            cwd=cwd,
            capture_output=True,
            check=False,
            env=environment,
        )
        self.commands.append(
            {
                "name": name,
                "command": public_command or command,
                "exit_status": completed.returncode,
                "expected_exit_status": expect,
                "stdout_sha256": hashlib.sha256(completed.stdout).hexdigest(),
                "stderr_sha256": hashlib.sha256(completed.stderr).hexdigest(),
            }
        )
        self.check(
            name,
            completed.returncode == expect,
            f"exit {completed.returncode}; expected {expect}",
        )
        if completed.returncode != expect:
            sys.stderr.buffer.write(completed.stdout[-4_000:])
            sys.stderr.buffer.write(completed.stderr[-4_000:])
        return completed


def load_toml(path: Path) -> dict[str, object]:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def repository_files() -> list[Path]:
    completed = subprocess.run(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        cwd=ROOT,
        capture_output=True,
        check=True,
    )
    result = []
    for raw in completed.stdout.split(b"\0"):
        if not raw:
            continue
        path = Path(os.fsdecode(raw))
        if (ROOT / path).is_file() or (ROOT / path).is_symlink():
            result.append(path)
    return sorted(result, key=Path.as_posix)


def text_files() -> list[Path]:
    suffixes = {".md", ".py", ".rs", ".sh", ".toml", ".yml", ".yaml"}
    return [
        path
        for path in repository_files()
        if path.suffix in suffixes and not path.parts[:1] == ("history",)
    ]


def read_text(path: Path) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def git_output(*arguments: str) -> str:
    return subprocess.run(
        ["git", *arguments], cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout.strip()


def git_output_at(root: Path, *arguments: str) -> str:
    return subprocess.run(
        ["git", *arguments], cwd=root, capture_output=True, text=True, check=True
    ).stdout.strip()


def json_command(
    result: Verification,
    name: str,
    command: list[str],
) -> dict[str, object] | None:
    completed = result.run(name, command, env={"PYTHONDONTWRITEBYTECODE": "1"})
    if completed.returncode != 0:
        return None
    try:
        payload = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        result.check(f"{name}-json", False, f"invalid JSON output: {error}")
        return None
    result.check(f"{name}-json", isinstance(payload, dict), "command emitted a JSON object")
    return payload if isinstance(payload, dict) else None


def json_value_command(
    result: Verification,
    name: str,
    command: list[str],
    *,
    public_command: list[str] | None = None,
) -> object | None:
    completed = result.run(
        name,
        command,
        public_command=public_command,
        env={"PYTHONDONTWRITEBYTECODE": "1"},
    )
    if completed.returncode != 0:
        return None
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        result.check(f"{name}-json", False, f"invalid JSON output: {error}")
        return None


def public_inventory_rows(result: Verification) -> list[dict[str, object]]:
    inventory_path = ROOT / "ajhahnde/systems/opaal/plans/opaal-transition-public-inventory.toml"
    artifact_path = ROOT / "transition-evidence/public-path-disposition.json"
    try:
        artifact_bytes = artifact_path.read_bytes()
        artifact = json.loads(artifact_bytes)
    except (OSError, json.JSONDecodeError) as error:
        result.check("public-inventory-artifact", False, f"cannot load artifact: {error}")
        return []

    result.check(
        "public-inventory-artifact",
        hashlib.sha256(artifact_bytes).hexdigest() == PUBLIC_DISPOSITION_SHA256,
        f"sha256={hashlib.sha256(artifact_bytes).hexdigest()}",
    )
    source_tip = artifact.get("source_tip")
    source_tree = artifact.get("source_tree")
    rows = artifact.get("rows")
    if not isinstance(rows, list):
        result.check("public-inventory-artifact-rows", False, "artifact rows are missing")
        return []
    source_available = subprocess.run(
        ["git", "cat-file", "-e", f"{source_tip}^{{commit}}"],
        cwd=ROOT,
        capture_output=True,
        check=False,
    ).returncode == 0
    source_is_ancestor = source_available and subprocess.run(
        ["git", "merge-base", "--is-ancestor", str(source_tip), "HEAD"],
        cwd=ROOT,
        capture_output=True,
        check=False,
    ).returncode == 0
    result.check(
        "public-inventory-source-baseline",
        source_tip == EXTRACTED_SOURCE_TIP
        and source_tree == EXTRACTED_SOURCE_TREE
        and source_is_ancestor
        and git_output("rev-parse", f"{source_tip}^{{tree}}") == source_tree,
        f"source_tip={source_tip!r} source_tree={source_tree!r} ancestor={source_is_ancestor}",
    )

    raw_paths = subprocess.run(
        ["git", "ls-tree", "-r", "--name-only", "-z", str(source_tip)],
        cwd=ROOT,
        capture_output=True,
        check=True,
    ).stdout
    standalone_paths = [os.fsdecode(path) for path in raw_paths.split(b"\0") if path]
    source_paths = [f"components/flash/{path}" for path in standalone_paths]
    encoded = b"".join(path.encode("utf-8") + b"\0" for path in source_paths)
    observed_digest = hashlib.sha256(encoded).hexdigest()
    result.check(
        "public-inventory-baseline",
        len(source_paths) == PUBLIC_SOURCE_PATH_COUNT
        and observed_digest == PUBLIC_SOURCE_PATH_SHA256,
        f"paths={len(source_paths)} digest={observed_digest}",
    )
    artifact_sources = [row.get("source") for row in rows if isinstance(row, dict)]
    result.check(
        "public-inventory-classification",
        artifact_sources == source_paths
        and artifact.get("summary")
        == {
            "source_paths": PUBLIC_SOURCE_PATH_COUNT,
            "classified_paths": PUBLIC_SOURCE_PATH_COUNT,
            "standalone_replacements": len(PUBLIC_REPLACEMENTS),
        },
        f"classified={len(rows)} source_rows_match={artifact_sources == source_paths}",
    )

    if inventory_path.is_file():
        inventory_bytes = inventory_path.read_bytes()
        inventory = load_toml(inventory_path)
        result.check(
            "public-inventory-private-source",
            hashlib.sha256(inventory_bytes).hexdigest() == PUBLIC_INVENTORY_SHA256
            and artifact.get("inventory_sha256") == PUBLIC_INVENTORY_SHA256,
            f"inventory_sha256={hashlib.sha256(inventory_bytes).hexdigest()}",
        )
        rules = inventory.get("rules", [])
        expected_rows: list[dict[str, object]] = []
        unmatched: list[str] = []
        for source, standalone in zip(source_paths, standalone_paths, strict=True):
            matches = [
                (index, rule)
                for index, rule in enumerate(rules)
                if isinstance(rule, dict)
                and isinstance(rule.get("match"), str)
                and fnmatch.fnmatchcase(source, rule["match"])
            ]
            if not matches:
                unmatched.append(source)
                continue
            index, rule = matches[0]
            destination = rule.get("destination")
            pattern = rule["match"]
            if isinstance(destination, str) and pattern.endswith("/**"):
                prefix = pattern[:-3]
                suffix = source[len(prefix) :].lstrip("/")
                destination = f"{destination.rstrip('/')}/{suffix}"
            expected_rows.append(
                {
                    "source": source,
                    "standalone_source": standalone,
                    "disposition": rule.get("disposition"),
                    "destination": destination,
                    "owner": rule.get("owner"),
                    "rule_index": index,
                    "shadowed_rule_indices": [value for value, _ in matches[1:]],
                }
            )
        result.check(
            "public-inventory-private-expansion",
            not unmatched and expected_rows == rows,
            f"unmatched={unmatched[:8]!r} rows_match={expected_rows == rows}",
        )

    missing_replacements = sorted(
        path for path in PUBLIC_REPLACEMENTS if not (ROOT / path).is_file()
    )
    result.check(
        "standalone-replacements",
        not missing_replacements,
        f"missing={missing_replacements!r}",
    )
    return rows


def worktree_digest() -> str:
    digest = hashlib.sha256()
    for path in repository_files():
        if path.parts[:1] == ("transition-evidence",):
            continue
        absolute = ROOT / path
        digest.update(path.as_posix().encode("utf-8"))
        digest.update(b"\0")
        digest.update(oct(stat.S_IMODE(absolute.lstat().st_mode)).encode("ascii"))
        digest.update(b"\0")
        if absolute.is_symlink():
            digest.update(b"symlink\0")
            digest.update(os.readlink(absolute).encode("utf-8"))
        else:
            digest.update(b"file\0")
            digest.update(hashlib.sha256(absolute.read_bytes()).digest())
        digest.update(b"\0")
    return digest.hexdigest()


def materialize_public_candidate(destination: Path) -> int:
    copied = 0
    for relative in repository_files():
        source = ROOT / relative
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        if source.is_symlink():
            target.symlink_to(os.readlink(source))
        else:
            # Give the disposable copy fresh mtimes so Cargo cannot reuse test
            # binaries that embedded a deleted prior candidate's manifest path.
            shutil.copy(source, target, follow_symlinks=False)
        copied += 1
    return copied


def find_matches(pattern: re.Pattern[str], paths: list[Path]) -> list[str]:
    matches: list[str] = []
    for path in paths:
        for number, line in enumerate(read_text(path).splitlines(), 1):
            if pattern.search(line):
                matches.append(f"{path.as_posix()}:{number}")
    return matches


def verify_tr_001(result: Verification) -> None:
    source = flashos_root()
    observed: dict[str, object] = {}
    result.check(
        "source-baseline-available",
        source is not None,
        "the FlashOS source checkout is available read-only",
    )
    if source is not None:
        observed = {
            "head": git_output_at(source, "rev-parse", "HEAD"),
            "main": git_output_at(source, "rev-parse", "main"),
            "origin_main": git_output_at(source, "rev-parse", "origin/main"),
            "status": git_output_at(source, "status", "--porcelain=v1"),
            "component_tree": git_output_at(
                source, "rev-parse", "HEAD:components/flash"
            ),
            "retained_api_head": git_output_at(
                source, "rev-parse", "feat/flashos-system-api"
            ),
            "retained_api_origin": git_output_at(
                source, "rev-parse", "origin/feat/flashos-system-api"
            ),
        }
        result.check(
            "source-baseline-current",
            observed
            == {
                "head": FLASHOS_SOURCE_HEAD,
                "main": FLASHOS_SOURCE_HEAD,
                "origin_main": FLASHOS_SOURCE_HEAD,
                "status": "",
                "component_tree": EXTRACTED_SOURCE_TREE,
                "retained_api_head": FLASHOS_RETAINED_API_HEAD,
                "retained_api_origin": FLASHOS_RETAINED_API_HEAD,
            },
            f"observed={observed!r}",
        )

    repository = json_value_command(
        result,
        "target-repository",
        [
            "gh",
            "repo",
            "view",
            "ajhahnde/opaal",
            "--json",
            "nameWithOwner,visibility,isArchived,defaultBranchRef,mergeCommitAllowed,squashMergeAllowed,rebaseMergeAllowed,deleteBranchOnMerge,url",
        ],
    )
    branches = json_value_command(
        result,
        "target-branches",
        ["gh", "api", "repos/ajhahnde/opaal/branches"],
    )
    tags = json_value_command(
        result,
        "target-tags",
        ["gh", "api", "repos/ajhahnde/opaal/tags"],
    )
    releases = json_value_command(
        result,
        "target-releases",
        ["gh", "api", "repos/ajhahnde/opaal/releases"],
    )
    pull_request = json_value_command(
        result,
        "source-pr-91",
        [
            "gh",
            "pr",
            "view",
            "91",
            "--repo",
            "ajhahnde/FlashOS",
            "--json",
            "number,state,mergedAt,closedAt,headRefName,baseRefName,headRefOid,url",
        ],
    )
    default_branch = (
        repository.get("defaultBranchRef")
        if isinstance(repository, dict)
        else None
    )
    result.check(
        "target-public-empty",
        isinstance(repository, dict)
        and repository.get("nameWithOwner") == "ajhahnde/opaal"
        and repository.get("visibility") == "PUBLIC"
        and repository.get("isArchived") is False
        and isinstance(default_branch, dict)
        and default_branch.get("name") == ""
        and branches == []
        and tags == []
        and releases == [],
        "target is public, unarchived, and has no branch, tag, or release",
    )
    result.check(
        "source-pr-91-disposition",
        isinstance(pull_request, dict)
        and pull_request.get("number") == 91
        and pull_request.get("state") == "CLOSED"
        and pull_request.get("mergedAt") is None
        and pull_request.get("baseRefName") == "main",
        f"pull_request={pull_request!r}",
    )
    baseline = {
        "schema": 1,
        "source": observed,
        "target": {
            "repository": repository,
            "branches": branches,
            "tags": tags,
            "releases": releases,
        },
        "source_pr_91": pull_request,
    }
    destination = ROOT / "transition-evidence/repository-baseline.json"
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(
        json.dumps(baseline, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def sha256_lines(values: list[str]) -> str:
    payload = "".join(f"{value}\n" for value in sorted(values)).encode("utf-8")
    return hashlib.sha256(payload).hexdigest()


def commit_record(root: Path, commit: str) -> dict[str, object]:
    completed = subprocess.run(
        ["git", "show", "-s", "--format=%an%x00%ae%x00%aI%x00%s", commit],
        cwd=root,
        capture_output=True,
        check=True,
    )
    fields = completed.stdout.rstrip(b"\n").split(b"\0")
    if len(fields) != 4:
        raise RuntimeError(f"cannot decode commit metadata for {commit}")
    return {
        "commit": commit,
        "parents": git_output_at(root, "show", "-s", "--format=%P", commit).split(),
        "tree": git_output_at(root, "show", "-s", "--format=%T", commit),
        "author_name": os.fsdecode(fields[0]),
        "author_email": os.fsdecode(fields[1]),
        "author_date": os.fsdecode(fields[2]),
        "subject": os.fsdecode(fields[3]),
    }


def source_selected_trees(source: Path, commit: str) -> dict[str, str]:
    trees: dict[str, str] = {}
    for path in ("components/flashshell", "components/flash"):
        completed = subprocess.run(
            ["git", "rev-parse", f"{commit}:{path}"],
            cwd=source,
            capture_output=True,
            text=True,
            check=False,
        )
        if completed.returncode == 0:
            trees[path] = completed.stdout.strip()
    return trees


def generate_history_map(source: Path, destination: Path) -> dict[str, object]:
    commit_map = ROOT / ".git/filter-repo/commit-map"
    if not commit_map.is_file():
        raise RuntimeError("filter-repo commit map is unavailable")
    version = subprocess.run(
        ["git-filter-repo", "--version"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    if version != FILTER_REPO_VERSION:
        raise RuntimeError(f"git-filter-repo version {version!r} is not pinned")

    mapped: dict[str, str] = {}
    for line in commit_map.read_text(encoding="utf-8").splitlines()[1:]:
        old, new = line.split()
        if set(new) != {"0"}:
            mapped[old] = new

    path_commits = set(
        git_output_at(
            source,
            "log",
            "--format=%H",
            FLASHOS_SOURCE_HEAD,
            "--",
            "components/flash",
            "components/flashshell",
        ).splitlines()
    )
    expected_sources = path_commits | TOPOLOGY_ONLY_SOURCE_COMMITS
    if set(mapped) != expected_sources:
        added = sorted(set(mapped) - expected_sources)
        missing = sorted(expected_sources - set(mapped))
        raise RuntimeError(
            f"filter map differs from selected lineage: added={added[:8]} missing={missing[:8]}"
        )

    rows: list[dict[str, object]] = []
    for source_commit in sorted(mapped):
        derived_commit = mapped[source_commit]
        touched = git_output_at(
            source,
            "diff-tree",
            "--root",
            "--no-commit-id",
            "--name-only",
            "-r",
            source_commit,
            "--",
            "components/flash",
            "components/flashshell",
        ).splitlines()
        rows.append(
            {
                "source": {
                    **commit_record(source, source_commit),
                    "selected_paths": sorted(set(touched)),
                    "selected_trees": source_selected_trees(source, source_commit),
                },
                "derived": commit_record(ROOT, derived_commit),
                "mapping_reason": (
                    "path-touching"
                    if source_commit in path_commits
                    else "topology-only"
                ),
            }
        )

    retained_exclusive: set[str] = set()
    if subprocess.run(
        ["git", "show-ref", "--verify", "--quiet", "refs/heads/feat/flashos-system-api"],
        cwd=source,
        check=False,
    ).returncode == 0:
        retained_exclusive = set(
            git_output_at(
                source,
                "rev-list",
                "feat/flashos-system-api",
                "--not",
                "main",
            ).splitlines()
        )
    imported_exclusive = sorted(retained_exclusive & set(mapped))
    if imported_exclusive:
        raise RuntimeError(f"non-main retained branch commits were imported: {imported_exclusive}")

    payload: dict[str, object] = {
        "schema": 1,
        "filter_repo_version": version,
        "source_head": FLASHOS_SOURCE_HEAD,
        "source_component_tree": git_output_at(
            source, "rev-parse", f"{FLASHOS_SOURCE_HEAD}:components/flash"
        ),
        "derived_tip": EXTRACTED_SOURCE_TIP,
        "derived_tree": git_output("rev-parse", f"{EXTRACTED_SOURCE_TIP}^{{tree}}"),
        "source_path_touching_commit_count": len(path_commits),
        "source_path_touching_commit_sha256": sha256_lines(list(path_commits)),
        "mapped_source_commit_sha256": sha256_lines(list(mapped)),
        "topology_only_source_commits": sorted(TOPOLOGY_ONLY_SOURCE_COMMITS),
        "non_main_branch_imports": 0,
        "flash_v1_release": {
            "source_commit": FLASH_V1_RELEASE_SOURCE_COMMIT,
            "derived_commit": mapped.get(FLASH_V1_RELEASE_SOURCE_COMMIT),
            "derived_tree": git_output(
                "rev-parse", f"{mapped[FLASH_V1_RELEASE_SOURCE_COMMIT]}^{{tree}}"
            ),
        },
        "rows": rows,
    }
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(
        json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return payload


def verify_tr_002(result: Verification) -> None:
    destination = ROOT / "transition-evidence/history-map.json"
    source = flashos_root()
    if not destination.is_file() and source is not None:
        try:
            generate_history_map(source, destination)
        except (OSError, RuntimeError, subprocess.SubprocessError) as error:
            result.check("history-map-generation", False, str(error))
            return
    try:
        payload = json.loads(destination.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        result.check("history-map", False, f"cannot load history map: {error}")
        return

    rows = payload.get("rows")
    if not isinstance(rows, list):
        result.check("history-map-rows", False, "history map rows are missing")
        return
    source_commits = [row.get("source", {}).get("commit") for row in rows]
    derived_commits = [row.get("derived", {}).get("commit") for row in rows]
    reasons = [row.get("mapping_reason") for row in rows]
    result.check(
        "history-map-baseline",
        payload.get("schema") == 1
        and payload.get("filter_repo_version") == FILTER_REPO_VERSION
        and payload.get("source_head") == FLASHOS_SOURCE_HEAD
        and payload.get("source_component_tree") == EXTRACTED_SOURCE_TREE
        and payload.get("derived_tip") == EXTRACTED_SOURCE_TIP
        and payload.get("derived_tree") == EXTRACTED_SOURCE_TREE,
        f"source={payload.get('source_head')} derived={payload.get('derived_tip')}",
    )
    result.check(
        "history-map-source-inventory",
        len(rows) == SOURCE_PATH_COMMIT_COUNT + len(TOPOLOGY_ONLY_SOURCE_COMMITS)
        and len(set(source_commits)) == len(rows)
        and payload.get("source_path_touching_commit_count") == SOURCE_PATH_COMMIT_COUNT
        and payload.get("source_path_touching_commit_sha256") == SOURCE_PATH_COMMIT_SHA256
        and payload.get("mapped_source_commit_sha256") == MAPPED_SOURCE_COMMIT_SHA256
        and set(payload.get("topology_only_source_commits", []))
        == TOPOLOGY_ONLY_SOURCE_COMMITS
        and reasons.count("path-touching") == SOURCE_PATH_COMMIT_COUNT
        and reasons.count("topology-only") == len(TOPOLOGY_ONLY_SOURCE_COMMITS),
        f"rows={len(rows)} reasons={dict((reason, reasons.count(reason)) for reason in set(reasons))}",
    )
    derived_history = set(
        git_output("rev-list", EXTRACTED_SOURCE_TIP).splitlines()
    )
    result.check(
        "history-map-derived-lineage",
        len(set(derived_commits)) == len(rows)
        and set(derived_commits) == derived_history
        and git_output("rev-parse", f"{EXTRACTED_SOURCE_TIP}^{{tree}}")
        == EXTRACTED_SOURCE_TREE
        and subprocess.run(
            ["git", "merge-base", "--is-ancestor", EXTRACTED_SOURCE_TIP, "HEAD"],
            cwd=ROOT,
            capture_output=True,
            check=False,
        ).returncode
        == 0,
        f"mapped={len(set(derived_commits))} history={len(derived_history)}",
    )
    derived_mismatches: list[str] = []
    for row in rows:
        derived = row.get("derived")
        if not isinstance(derived, dict) or not isinstance(derived.get("commit"), str):
            derived_mismatches.append("<invalid-row>")
            continue
        if commit_record(ROOT, derived["commit"]) != derived:
            derived_mismatches.append(derived["commit"])
    result.check(
        "history-map-derived-metadata",
        not derived_mismatches,
        f"mismatches={derived_mismatches[:8]}",
    )
    release = payload.get("flash_v1_release", {})
    result.check(
        "history-map-release-tree",
        isinstance(release, dict)
        and release.get("source_commit") == FLASH_V1_RELEASE_SOURCE_COMMIT
        and release.get("derived_commit") == FLASH_V1_RELEASE_DERIVED_COMMIT
        and release.get("derived_tree")
        == git_output("rev-parse", f"{FLASH_V1_RELEASE_DERIVED_COMMIT}^{{tree}}"),
        f"release={release!r}",
    )
    result.check(
        "history-map-main-only",
        payload.get("non_main_branch_imports") == 0,
        f"non_main_branch_imports={payload.get('non_main_branch_imports')!r}",
    )

    if source is not None:
        source_path_commits = git_output_at(
            source,
            "log",
            "--format=%H",
            FLASHOS_SOURCE_HEAD,
            "--",
            "components/flash",
            "components/flashshell",
        ).splitlines()
        result.check(
            "history-map-live-source",
            git_output_at(source, "rev-parse", "HEAD") == FLASHOS_SOURCE_HEAD
            and git_output_at(source, "rev-parse", "main") == FLASHOS_SOURCE_HEAD
            and git_output_at(source, "rev-parse", "origin/main") == FLASHOS_SOURCE_HEAD
            and sha256_lines(source_path_commits) == SOURCE_PATH_COMMIT_SHA256,
            f"source_head={git_output_at(source, 'rev-parse', 'HEAD')}",
        )
        with tempfile.TemporaryDirectory(prefix="opaal-history-map-") as temporary:
            expected = generate_history_map(source, Path(temporary) / "history-map.json")
        result.check(
            "history-map-live-expansion",
            expected == payload,
            "the checked-in map exactly matches the re-derived source and derived histories",
        )

    result.check(
        "history-map-artifact",
        destination.is_file(),
        f"sha256={hashlib.sha256(destination.read_bytes()).hexdigest()}",
    )


def verify_tr_003(result: Verification) -> None:
    workflow_paths = [
        Path(".github/workflows/ci.yml"),
        Path(".github/workflows/security.yml"),
        Path(".github/workflows/release.yml"),
    ]
    missing = [path.as_posix() for path in workflow_paths if not (ROOT / path).is_file()]
    result.check("standalone-workflows", not missing, f"missing={missing!r}")
    if not missing:
        ci_workflow = read_text(workflow_paths[0])
        security_workflow = read_text(workflow_paths[1])
        release_workflow = read_text(workflow_paths[2])
        result.check(
            "required-aggregates",
            "name: required" in ci_workflow
            and "name: security-required" in security_workflow,
            "CI and security expose stable required aggregates",
        )
        result.check(
            "bootstrap-and-pr-events",
            all(event in ci_workflow for event in ("push:", "pull_request:", "workflow_dispatch:"))
            and all(
                event in security_workflow
                for event in ("push:", "pull_request:", "workflow_dispatch:")
            ),
            "CI and security run for initial bootstrap, pull requests, and manual checks",
        )
        result.check(
            "non-publishing-release-workflow",
            "workflow_dispatch:" in release_workflow
            and not re.search(r"^\s*[a-z-]+:\s*write\s*$", release_workflow, re.MULTILINE)
            and not re.search(
                r"\b(?:cargo\s+publish|gh\s+release\s+create|git\s+push)\b",
                release_workflow,
            ),
            "release policy is manual, read-only, and contains no publishing command",
        )

    standalone_files = [
        path
        for path in text_files()
        if path.parts[:1] in {(".github",), ("benchmarks",), ("ci",)}
        and path not in {Path("ci/check_transition.py"), Path("ci/check_public_boundary.py")}
    ]
    forbidden_dependency = find_matches(
        re.compile(
            r"components/flash|FLASH_AUTOMATION_RUNTIME|OPAAL_AUTOMATION_RUNTIME|"
            r"make flash-|(?:^|[\s'\"])(?:\.\./)+FlashOS"
        ),
        standalone_files,
    )
    result.check(
        "no-flashos-sibling-dependency",
        not forbidden_dependency,
        f"standalone dependency matches={forbidden_dependency!r}",
    )

    candidate_digest = worktree_digest()
    isolated = ROOT / "target" / "transition-public-candidates" / candidate_digest / "opaal"
    isolated.mkdir(parents=True, exist_ok=True)
    try:
        copied = materialize_public_candidate(isolated)
        forbidden_private = [
            path for path in ("ajhahnde", ".agents", ".codex", ".claude", ".private")
            if (isolated / path).exists() or (isolated / path).is_symlink()
        ]
        result.check(
            "fresh-public-candidate",
            copied > 0 and not forbidden_private,
            f"files={copied} forbidden_private={forbidden_private!r}",
        )
        cargo_environment = {"CARGO_TARGET_DIR": str(ROOT / "target")}
        python_environment = {
            "CARGO_TARGET_DIR": str(ROOT / "target"),
            "PYTHONDONTWRITEBYTECODE": "1",
        }
        result.run(
            "format", ["cargo", "fmt", "--all", "--", "--check"], cwd=isolated
        )
        result.run(
            "build",
            ["cargo", "build", "--workspace", "--locked"],
            cwd=isolated,
            env=cargo_environment,
        )
        result.run(
            "workspace-tests",
            ["cargo", "test", "--workspace", "--locked", "--no-fail-fast"],
            cwd=isolated,
            env=cargo_environment,
        )
        result.run(
            "clippy",
            [
                "cargo",
                "clippy",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
            cwd=isolated,
            env=cargo_environment,
        )
        result.run(
            "rustdoc",
            ["cargo", "doc", "--workspace", "--no-deps", "--locked"],
            cwd=isolated,
            env={**cargo_environment, "RUSTDOCFLAGS": "-D warnings"},
        )
        result.run(
            "dependency-policy",
            ["cargo", "deny", "check"],
            cwd=isolated,
            env=cargo_environment,
        )
        result.run(
            "public-boundary",
            [sys.executable, "ci/check_public_boundary.py"],
            cwd=isolated,
            env=python_environment,
        )
        result.run(
            "python-policy-tests",
            [
                sys.executable,
                "-m",
                "unittest",
                "discover",
                "-s",
                "ci/tests",
                "-p",
                "test_*.py",
            ],
            cwd=isolated,
            env=python_environment,
        )
        result.run(
            "benchmark-contract",
            [sys.executable, "ci/check_benchmarks.py", "--contract-only"],
            cwd=isolated,
            env=python_environment,
        )
    finally:
        # The digest-addressed source stays beside disposable Cargo artifacts.
        # Keeping it makes compile-time manifest paths valid for later claims.
        result.check(
            "digest-addressed-candidate-source",
            isolated.is_dir(),
            f"candidate_digest={candidate_digest}",
        )


def verify_tr_004(result: Verification) -> None:
    public_inventory_rows(result)
    audited_text_files = [
        path for path in text_files() if path != Path("ci/check_transition.py")
    ]
    root_manifest = load_toml(ROOT / "Cargo.toml")
    workspace = root_manifest.get("workspace", {})
    members = workspace.get("members", []) if isinstance(workspace, dict) else []
    result.check(
        "seven-member-workspace",
        members == list(WORKSPACE_PACKAGES),
        f"members={members!r}",
    )

    package_defaults = workspace.get("package", {}) if isinstance(workspace, dict) else {}
    expected_defaults = {
        "version": "1.0.0-alpha.1",
        "license": "MPL-2.0",
        "repository": "https://github.com/ajhahnde/opaal",
    }
    observed_defaults = {key: package_defaults.get(key) for key in expected_defaults}
    result.check(
        "workspace-identity",
        observed_defaults == expected_defaults,
        f"workspace package identity={observed_defaults!r}",
    )

    package_names: dict[str, str | None] = {}
    binary_names: set[str] = set()
    for crate_path, expected_name in WORKSPACE_PACKAGES.items():
        manifest = load_toml(ROOT / crate_path / "Cargo.toml")
        package = manifest.get("package", {})
        name = package.get("name") if isinstance(package, dict) else None
        package_names[crate_path] = name if isinstance(name, str) else None
        for binary in manifest.get("bin", []):
            if isinstance(binary, dict) and isinstance(binary.get("name"), str):
                binary_names.add(binary["name"])
        result.check(
            f"package-{expected_name}",
            name == expected_name,
            f"{crate_path} package={name!r}",
        )

    fuzz_manifest = load_toml(ROOT / "fuzz" / "Cargo.toml")
    fuzz_package = fuzz_manifest.get("package", {})
    result.check(
        "standalone-fuzz-package",
        isinstance(fuzz_package, dict)
        and fuzz_package.get("name") == "opaal-fuzz"
        and fuzz_package.get("publish") is False,
        f"fuzz package={fuzz_package!r}",
    )
    result.check(
        "primary-binaries",
        PRIMARY_BINARIES <= binary_names,
        f"missing={sorted(PRIMARY_BINARIES - binary_names)!r}",
    )

    current_text = "\n".join(read_text(path) for path in audited_text_files)
    missing_fixtures = sorted(name for name in FIXTURE_IDENTIFIERS if name not in current_text)
    result.check(
        "fixture-identifiers",
        not missing_fixtures,
        f"missing={missing_fixtures!r}",
    )
    observed_opaal_codes = set(re.findall(r'"(OP[0-9]{4})"', current_text))
    migration_text = "\n".join(
        path.read_text(encoding="utf-8")
        for path in sorted((ROOT / "crates/opaal-migrate").rglob("*.rs"))
    )
    observed_migration_codes = set(
        re.findall(r'"((?:MIG|AUTH)[0-9]{4})"', migration_text)
    )
    result.check(
        "diagnostic-code-inventory",
        observed_opaal_codes == OPAAL_DIAGNOSTIC_CODES
        and observed_migration_codes == MIGRATION_DIAGNOSTIC_CODES,
        f"opaal={sorted(observed_opaal_codes)!r} migration={sorted(observed_migration_codes)!r}",
    )
    result.check(
        "diagnostic-and-server-identity",
        "opaal:" in current_text
        and "OPAAL Language Server" in current_text
        and "Flash Language Server" not in current_text,
        "current diagnostics and LSP identify OPAAL",
    )

    paths = repository_files()
    invalid_source_paths = [
        path.as_posix()
        for path in paths
        if path.suffix == ".fsh"
        and path.parts[:1] != ("history",)
        and path.parts[:3] != ("tests", "opaal-foundation", "migration")
    ]
    result.check(
        "source-extension-boundary",
        not invalid_source_paths,
        f"unclassified .fsh paths={invalid_source_paths!r}",
    )

    old_environment = find_matches(
        re.compile(r"[\"']FLASH_[A-Z0-9_]+[\"']"), audited_text_files
    )
    result.check(
        "environment-cutover",
        not old_environment,
        f"current legacy environment names={old_environment!r}",
    )
    observed_opaal_environment = set(
        re.findall(r"[\"'](OPAAL_[A-Z0-9_]+)[\"']", current_text)
    )
    unknown_environment = sorted(observed_opaal_environment - OPAAL_ENVIRONMENT_NAMES)
    missing_environment = sorted(
        name for name in OPAAL_ENVIRONMENT_NAMES if name not in current_text
    )
    result.check(
        "environment-inventory",
        not unknown_environment and not missing_environment,
        f"unknown={unknown_environment!r} missing={missing_environment!r}",
    )

    manifests_and_current_code = [
        path
        for path in audited_text_files
        if path.name == "Cargo.toml"
        or path.parts[:2]
        in {
            ("crates", "opaal-cli"),
            ("crates", "opaal-lsp"),
            ("crates", "opaal-platform"),
            ("crates", "opaal-platform-posix"),
            ("crates", "opaal-runtime"),
        }
    ]
    stale_current_identity = []
    stale_pattern = re.compile(
        r"github\.com/ajhahnde/FlashOS|components/flash|\bflash[-_]|\bfsh:\s|"
        r"[\"']FLASH_[A-Z0-9_]+[\"']|\bLanguageMajor::(?:V1|V2)\b"
    )
    for path in manifests_and_current_code:
        for number, line in enumerate(read_text(path).splitlines(), 1):
            sanitized = line.replace("opaal-migrate-flash-v1", "").replace(
                "flash-v1-migration", ""
            )
            if stale_pattern.search(sanitized):
                stale_current_identity.append(f"{path.as_posix()}:{number}")
    result.check(
        "current-code-identity",
        not stale_current_identity,
        f"stale current identity={stale_current_identity!r}",
    )

    public_declaration_leaks: list[str] = []
    declaration = re.compile(r"^\s*pub(?:\([^)]*\))?\s+(?:const|enum|fn|mod|static|struct|trait|type|use)\b")
    legacy_identifier = re.compile(r"\b(?:FLASH_[A-Z0-9_]*|FlashV1|FlashV2|flash_[a-z0-9_]*)\b")
    for path in audited_text_files:
        if path.suffix != ".rs" or path.parts[:2] == ("crates", "opaal-migrate"):
            continue
        for number, line in enumerate(read_text(path).splitlines(), 1):
            if declaration.search(line) and legacy_identifier.search(line):
                if path.as_posix() in LEGACY_SYNTAX_FILES:
                    continue
                public_declaration_leaks.append(f"{path.as_posix()}:{number}")
    result.check(
        "exported-identity",
        not public_declaration_leaks,
        f"legacy public declarations={public_declaration_leaks!r}",
    )

    language_source = (ROOT / "crates/opaal-syntax/src/language.rs").read_text(
        encoding="utf-8"
    )
    identity_body = re.search(
        r"pub enum LanguageIdentity\s*\{(?P<body>.*?)\n\}", language_source, re.DOTALL
    )
    body = identity_body.group("body") if identity_body else ""
    result.check(
        "public-language-identity",
        "OpaalV1" in body and "FlashV1" not in body and "V2" not in body,
        "LanguageIdentity exposes only OpaalV1",
    )


def verify_tr_005(result: Verification) -> None:
    language_source = read_text(Path("crates/opaal-syntax/src/language.rs"))
    result.check(
        "language-one-constant",
        re.search(r"Self::OpaalV1\s*=>\s*1\s*,", language_source) is not None,
        "OPAAL language number is exactly 1",
    )
    result.check(
        "single-current-identity",
        "OpaalV1" in language_source
        and "LanguageIdentity::FlashV1" not in language_source
        and "LanguageIdentity::V2" not in language_source,
        "current syntax identity is OpaalV1 only",
    )

    result.run(
        "syntax-language-goldens",
        [
            "cargo",
            "test",
            "--locked",
            "-p",
            "opaal-syntax",
            "--test",
            "language_version",
            "--test",
            "input_classification",
            "--test",
            "formatter",
        ],
    )
    result.run(
        "runtime-language-goldens",
        [
            "cargo",
            "test",
            "--locked",
            "-p",
            "opaal-runtime",
            "--test",
            "language_version",
        ],
    )
    result.run(
        "cli-language-goldens",
        [
            "cargo",
            "test",
            "--locked",
            "-p",
            "opaal-cli",
            "--test",
            "language_version",
            "--test",
            "check_frontend",
            "--test",
            "format_frontend",
            "--test",
            "interactive_session",
        ],
    )
    result.run(
        "lsp-language-goldens",
        [
            "cargo",
            "test",
            "--locked",
            "-p",
            "opaal-lsp",
            "--test",
            "language_version",
            "--test",
            "server_e2e",
        ],
    )


def verify_tr_006(result: Verification) -> None:
    audited_text_files = [
        path for path in text_files() if path != Path("ci/check_transition.py")
    ]
    syntax_manifest = load_toml(ROOT / "crates/opaal-syntax/Cargo.toml")
    features = syntax_manifest.get("features", {})
    result.check(
        "migration-feature-default-off",
        isinstance(features, dict)
        and features.get("default") == []
        and features.get("flash-v1-migration") == [],
        f"opaal-syntax features={features!r}",
    )

    feature_enablers: list[str] = []
    for manifest_path in sorted(ROOT.glob("crates/*/Cargo.toml")):
        manifest = load_toml(manifest_path)
        dependencies = manifest.get("dependencies", {})
        if not isinstance(dependencies, dict):
            continue
        dependency = dependencies.get("opaal-syntax")
        if isinstance(dependency, dict) and "flash-v1-migration" in dependency.get(
            "features", []
        ):
            feature_enablers.append(manifest_path.parent.name)
    result.check(
        "migration-feature-reachability",
        feature_enablers == ["opaal-migrate"],
        f"feature enablers={feature_enablers!r}",
    )

    legacy_calls: list[str] = []
    pattern = re.compile(
        r"opaal_syntax::migration|SyntaxLanguage::FlashV1|"
        r"(?:parse|lex|classify|format)_flash_v1"
    )
    for path in audited_text_files:
        if path.suffix != ".rs" or path.parts[:1] == ("history",):
            continue
        if path.parts[:2] == ("crates", "opaal-migrate") or path.parts[:2] == (
            "crates",
            "opaal-syntax",
        ):
            continue
        for number, line in enumerate(read_text(path).splitlines(), 1):
            if pattern.search(line):
                legacy_calls.append(f"{path.as_posix()}:{number}")
    result.check(
        "flash-v1-source-reachability",
        not legacy_calls,
        f"legacy syntax reachable outside migration={legacy_calls!r}",
    )

    runtime_source = "\n".join(
        path.read_text(encoding="utf-8")
        for path in sorted((ROOT / "crates/opaal-runtime/src").glob("*.rs"))
    )
    forbidden_runtime = sorted(
        set(
            re.findall(
                r"\b(?:FLASH_V1_LANGUAGE_NUMBER|LanguageIdentity::FlashV1|"
                r"parse_flash_v1|lex_flash_v1|flash-v1-boundary)\b",
                runtime_source,
            )
        )
    )
    result.check(
        "runtime-personality",
        not forbidden_runtime,
        f"forbidden runtime routes={forbidden_runtime!r}",
    )
    script_source = read_text(Path("crates/opaal-runtime/src/script.rs"))
    retained_legacy_api = sorted(
        name
        for name in (
            "execute_script",
            "execute_chain_subshell",
            "execute_background_capsule",
        )
        if re.search(rf"\bpub fn {name}\b", script_source)
    )
    result.check(
        "legacy-script-api-removed",
        not retained_legacy_api,
        f"retained legacy adapters={retained_legacy_api!r}",
    )
    runtime_lib_source = read_text(Path("crates/opaal-runtime/src/lib.rs"))
    result.check(
        "legacy-capsule-surface-removed",
        "pub mod capsule;" not in runtime_lib_source
        and "FSHCAP" not in runtime_source
        and "FSHDONE" not in runtime_source,
        "the predecessor capsule API and wire identity are not public/current",
    )

    lib_source = (ROOT / "crates/opaal-syntax/src/lib.rs").read_text(encoding="utf-8")
    result.check(
        "migration-facade-gated",
        '#[cfg(feature = "flash-v1-migration")]\npub mod migration {' in lib_source,
        "migration facade requires the explicit feature",
    )

    result.run(
        "migration-crate-build",
        ["cargo", "check", "--locked", "-p", "opaal-migrate"],
    )
    result.run(
        "cli-fallback-negatives",
        [
            "cargo",
            "test",
            "--locked",
            "-p",
            "opaal-cli",
            "--test",
            "e2e",
            "--test",
            "checker_e2e",
            "--test",
            "formatter_e2e",
            "--test",
            "planner_e2e",
        ],
    )

    with tempfile.TemporaryDirectory(prefix="opaal-api-negative-") as temporary:
        consumer = Path(temporary)
        (consumer / "src").mkdir()
        syntax_path = (ROOT / "crates/opaal-syntax").as_posix()
        (consumer / "Cargo.toml").write_text(
            "[package]\nname = \"opaal-api-negative\"\nversion = \"0.0.0\"\n"
            "edition = \"2024\"\n\n[dependencies]\n"
            f"opaal-syntax = {{ path = {json.dumps(syntax_path)} }}\n",
            encoding="utf-8",
        )
        (consumer / "src/main.rs").write_text(
            "fn main() { let _ = opaal_syntax::migration::lex; }\n",
            encoding="utf-8",
        )
        result.run(
            "default-syntax-api-rejects-migration",
            ["cargo", "check", "--offline"],
            cwd=consumer,
            expect=101,
            public_command=[
                "cargo",
                "check",
                "--offline",
                "# default opaal-syntax consumer referencing migration",
            ],
        )
        runtime_path = (ROOT / "crates/opaal-runtime").as_posix()
        (consumer / "Cargo.toml").write_text(
            "[package]\nname = \"opaal-api-negative\"\nversion = \"0.0.0\"\n"
            "edition = \"2024\"\n\n[dependencies]\n"
            f"opaal-runtime = {{ path = {json.dumps(runtime_path)} }}\n",
            encoding="utf-8",
        )
        (consumer / "src/main.rs").write_text(
            "fn main() { let _ = opaal_runtime::script::execute_script; }\n",
            encoding="utf-8",
        )
        result.run(
            "runtime-api-rejects-unversioned-script-adapter",
            ["cargo", "check", "--offline"],
            cwd=consumer,
            expect=101,
            public_command=[
                "cargo",
                "check",
                "--offline",
                "# consumer referencing removed unversioned script adapter",
            ],
        )


def verify_tr_007(result: Verification) -> None:
    migration_source = read_text(Path("crates/opaal-migrate/src/lib.rs"))
    required_identity = {
        "SCHEMA_VERSION": "2",
        "SOURCE_PRODUCT": '"flash"',
        "SOURCE_LANGUAGE": "1",
        "TARGET_PRODUCT": '"opaal"',
        "TARGET_LANGUAGE": "1",
    }
    missing_identity = [
        name
        for name, value in required_identity.items()
        if re.search(rf"const\s+{name}\s*:[^=]+\s*=\s*{re.escape(value)}\s*;", migration_source)
        is None
    ]
    result.check(
        "schema-two-identity",
        not missing_identity,
        f"missing or changed constants={missing_identity!r}",
    )
    result.check(
        "no-apply-mode",
        "--apply" not in migration_source
        and "apply_edits" not in migration_source
        and "write_all" not in migration_source,
        "migration library exposes no source-writing path",
    )
    result.run(
        "migration-unit-and-acceptance",
        ["cargo", "test", "--locked", "-p", "opaal-migrate"],
    )

    nightly = subprocess.run(
        ["rustup", "which", "--toolchain", "nightly", "cargo"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    result.check(
        "nightly-fuzz-toolchain",
        nightly.returncode == 0 and Path(nightly.stdout.strip()).is_file(),
        "a nightly Cargo toolchain is available for the migration fuzz target",
    )
    if nightly.returncode == 0 and Path(nightly.stdout.strip()).is_file():
        nightly_path = str(Path(nightly.stdout.strip()).parent)
        result.run(
            "migration-fuzz-build",
            ["cargo", "fuzz", "check", "migration"],
            env={"PATH": nightly_path + os.pathsep + os.environ.get("PATH", "")},
        )


def verify_tr_008(result: Verification) -> None:
    contract = load_toml(ROOT / "benchmarks/contract-v1.toml")
    cases = contract.get("cases", [])
    observed_cases = {
        case.get("id") for case in cases if isinstance(case, dict) and isinstance(case.get("id"), str)
    }
    expected_cases = {
        "host-startup-cold",
        "host-startup-warm",
        "host-first-prompt-cold",
        "host-first-prompt-warm",
        "host-structured-stream-memory-warm",
        "host-completion-cold",
        "host-completion-warm",
    }
    result.check(
        "seven-host-benchmark-cases",
        observed_cases == expected_cases and len(cases) == 7,
        f"cases={sorted(observed_cases)!r}",
    )
    benchmark_text = read_text(Path("benchmarks/run.py"))
    forbidden_benchmark = sorted(
        token
        for token in (
            "host-command-overhead-warm",
            "host-pipeline-throughput-warm",
            "flashos-qemu-tcg",
            "AUTOMATION_RUNTIME",
        )
        if token in benchmark_text
    )
    result.check(
        "pure-benchmark-surfaces",
        not forbidden_benchmark,
        f"forbidden benchmark routes={forbidden_benchmark!r}",
    )
    result.run(
        "foundation-corpus",
        ["cargo", "test", "--workspace", "--locked", "--no-fail-fast"],
    )
    result.run(
        "benchmark-contract-and-evidence",
        [sys.executable, "ci/check_benchmarks.py", "--contract-only"],
    )
    with tempfile.TemporaryDirectory(prefix="opaal-transition-benchmark-") as temporary:
        smoke_result = Path(temporary) / "smoke.json"
        result.run(
            "benchmark-smoke",
            [
                sys.executable,
                "benchmarks/run.py",
                "--profile",
                "smoke",
                "--output",
                str(smoke_result),
            ],
            public_command=[
                sys.executable,
                "benchmarks/run.py",
                "--profile",
                "smoke",
                "--output",
                "<temporary>/smoke.json",
            ],
        )

    if sys.platform == "darwin" and os.uname().machine.lower() in {"arm64", "aarch64"}:
        result.run(
            "candidate-macos-qualification-evidence",
            [
                sys.executable,
                "ci/check_benchmarks.py",
                "--result",
                "benchmarks/evidence/host-darwin-arm64-opaal-v1.json",
                "--environment",
                "host-darwin-arm64",
            ],
        )
    else:
        result.check(
            "candidate-macos-qualification-evidence",
            True,
            "retained evidence is structurally validated; binary binding is host-specific",
        )


def verify_tr_009(result: Verification) -> None:
    rows = public_inventory_rows(result)
    preserved = [row for row in rows if row["disposition"] == "preserve-flash-history"]
    mismatched: list[str] = []
    for row in preserved:
        source = str(row["standalone_source"])
        destination = ROOT / str(row["destination"])
        original = subprocess.run(
            ["git", "show", f"HEAD:{source}"],
            cwd=ROOT,
            capture_output=True,
            check=False,
        )
        if (
            original.returncode != 0
            or not destination.is_file()
            or destination.read_bytes() != original.stdout
        ):
            mismatched.append(str(row["destination"]))
    result.check(
        "public-history-byte-preservation",
        bool(preserved) and not mismatched,
        f"preserved={len(preserved)} mismatched={mismatched[:8]!r}",
    )

    private_contract = ROOT / "ajhahnde/systems/opaal/checks/transition_contract.py"
    context = Path("ajhahnde/scripts/context.py")
    result.run(
        "private-history-and-retention",
        [sys.executable, str(private_contract), "inventory"],
        public_command=[sys.executable, "<private-transition-contract>", "inventory"],
        env={"PYTHONDONTWRITEBYTECODE": "1"},
    )
    default = json_command(
        result,
        "default-history-route",
        [sys.executable, str(context), "evidence", "--query", "Flash v1 requirements", "--json"],
    )
    archived = json_command(
        result,
        "explicit-history-route",
        [
            sys.executable,
            str(context),
            "evidence",
            "--query",
            "Flash v1 requirements",
            "--include-archive",
            "--json",
        ],
    )
    if default is not None and archived is not None:
        default_paths = [section["path"] for section in default.get("sections", [])]
        archived_paths = [section["path"] for section in archived.get("sections", [])]
        result.check(
            "archives-excluded-by-default",
            default.get("resolution", {}).get("archive_boundary") == "excluded"
            and not any("/archive/" in path or "/history/" in path for path in default_paths),
            f"default paths={default_paths!r}",
        )
        result.check(
            "explicit-archive-lookup",
            archived.get("resolution", {}).get("archive_boundary") == "included and labelled"
            and any("/archive/" in path or "/history/" in path for path in archived_paths),
            f"archive paths={archived_paths!r}",
        )


def verify_tr_010(result: Verification) -> None:
    result.run(
        "private-current-owner-inventory",
        [
            sys.executable,
            str(ROOT / "ajhahnde/systems/opaal/checks/transition_contract.py"),
            "inventory",
        ],
        public_command=[sys.executable, "<private-transition-contract>", "inventory"],
        env={"PYTHONDONTWRITEBYTECODE": "1"},
    )


def verify_tr_011(result: Verification) -> None:
    context = Path("ajhahnde/scripts/context.py")
    expected_facets = {
        "OPAAL": [
            "current-identity",
            "version-boundary",
            "flash2-lineage",
            "flash1-boundary",
            "transition-state",
        ],
        "Flash": ["current-identity", "transition-state"],
        "Flash 2": ["flash2-lineage"],
        "Flash 1": ["flash1-boundary"],
    }
    for query, facets in expected_facets.items():
        payload = json_command(
            result,
            f"route-{query.casefold().replace(' ', '-')}",
            [sys.executable, str(context), "evidence", "--query", query, "--json"],
        )
        if payload is None:
            continue
        resolution = payload.get("resolution", {})
        coverage = payload.get("coverage", [])
        result.check(
            f"route-{query.casefold().replace(' ', '-')}-contract",
            payload.get("status") == "complete"
            and resolution.get("primary_scopes") == ["system.opaal"]
            and resolution.get("facets") == facets
            and [row.get("facet_id") for row in coverage] == facets
            and all(row.get("status") == "covered" for row in coverage),
            f"scope={resolution.get('primary_scopes')!r} facets={resolution.get('facets')!r}",
        )


def verify_tr_012(result: Verification) -> None:
    context = Path("ajhahnde/scripts/context.py")
    for name, arguments in (
        ("knowledge-reconcile", ["reconcile"]),
        ("knowledge-validate", ["validate"]),
    ):
        result.run(
            name,
            [sys.executable, str(context), *arguments],
            public_command=[sys.executable, "<private-context>", *arguments],
            env={"PYTHONDONTWRITEBYTECODE": "1"},
        )
    evaluation = json_command(
        result,
        "knowledge-evaluate",
        [sys.executable, str(context), "evaluate", "--json"],
    )
    if evaluation is not None:
        summaries = {
            name: {"passed": value.get("passed"), "total": value.get("total")}
            for name, value in evaluation.items()
            if isinstance(value, dict) and "passed" in value and "total" in value
        }
        result.check(
            "knowledge-evaluation-complete",
            bool(summaries)
            and all(row["passed"] == row["total"] for row in summaries.values()),
            f"summaries={summaries!r}",
        )


def verify_tr_013(result: Verification) -> None:
    result.run(
        "provider-adapter-isolation",
        [
            sys.executable,
            str(ROOT / "ajhahnde/systems/opaal/checks/transition_contract.py"),
            "adapters",
        ],
        public_command=[sys.executable, "<private-transition-contract>", "adapters"],
        env={"PYTHONDONTWRITEBYTECODE": "1"},
    )


def flashos_root() -> Path | None:
    override = os.environ.get("OPAAL_FLASHOS_ROOT")
    candidates = [Path(override).resolve()] if override else []
    candidates.extend(ROOT.parents)
    for candidate in candidates:
        if (candidate / ".git").exists() and (candidate / "components/flash").is_dir():
            return candidate
    return None


def verify_tr_014(result: Verification) -> None:
    source = flashos_root()
    result.check(
        "flashos-source-available",
        source is not None,
        "the recorded FlashOS source checkout is available read-only",
    )
    if source is None:
        return
    private_artifact = ROOT / "ajhahnde/transition-evidence/private-content-disposition.json"
    try:
        recorded = json.loads(private_artifact.read_text(encoding="utf-8"))
        expected_head = recorded["source_head"]
    except (OSError, KeyError, json.JSONDecodeError) as error:
        result.check("flashos-recorded-baseline", False, f"invalid baseline artifact: {error}")
        return

    observed = {
        "head": git_output_at(source, "rev-parse", "HEAD"),
        "main": git_output_at(source, "rev-parse", "main"),
        "origin_main": git_output_at(source, "rev-parse", "origin/main"),
        "component_tree": git_output_at(source, "rev-parse", "HEAD:components/flash"),
        "candidate_source_tree": git_output("rev-parse", "HEAD^{tree}"),
    }
    tracked_status = subprocess.run(
        ["git", "status", "--porcelain", "--untracked-files=no"],
        cwd=source,
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    result.check(
        "flashos-head-and-tree-frozen",
        observed["head"] == observed["main"] == observed["origin_main"] == expected_head
        and observed["component_tree"] == observed["candidate_source_tree"],
        f"observed={observed!r} expected_head={expected_head}",
    )
    result.check(
        "flashos-no-tracked-transition-diff",
        tracked_status == "",
        f"tracked status={tracked_status!r}",
    )

    required_boundary = (
        "makes no claim that OPAAL is packaged by another operating system,\n"
        "available on Redox, or qualified on physical hardware."
    )
    result.check(
        "no-flashos-or-target-qualification-claim",
        required_boundary in read_text(Path("README.md"))
        and not (ROOT / "crates/opaal-platform-flashos").exists()
        and not (ROOT / "platforms").exists()
        and not list((ROOT / "benchmarks/evidence").glob("*flashos*")),
        "current source and evidence retain the explicit downstream qualification boundary",
    )
    evidence = {
        "schema": 1,
        "recorded_source_head": expected_head,
        "observed": observed,
        "tracked_status_clean": tracked_status == "",
    }
    destination = ROOT / "transition-evidence/flashos-freeze.json"
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(
        json.dumps(evidence, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def verify_tr_018(result: Verification) -> None:
    result.run(
        "private-recovery",
        [
            sys.executable,
            str(ROOT / "ajhahnde/systems/opaal/checks/transition_contract.py"),
            "recovery",
        ],
        public_command=[sys.executable, "<private-transition-contract>", "recovery"],
        env={"PYTHONDONTWRITEBYTECODE": "1"},
    )


def verify_tr_017(result: Verification) -> None:
    repository = json_value_command(
        result,
        "repository-settings",
        ["gh", "api", "repos/ajhahnde/opaal"],
    )
    workflow_permissions = json_value_command(
        result,
        "workflow-permissions",
        ["gh", "api", "repos/ajhahnde/opaal/actions/permissions/workflow"],
    )
    main_reference = json_value_command(
        result,
        "public-main-reference",
        ["gh", "api", "repos/ajhahnde/opaal/git/ref/heads/main"],
    )
    remote_sha = None
    if isinstance(main_reference, dict):
        object_row = main_reference.get("object")
        if isinstance(object_row, dict) and isinstance(object_row.get("sha"), str):
            remote_sha = object_row["sha"]
    remote_commit = (
        json_value_command(
            result,
            "public-main-commit",
            ["gh", "api", f"repos/ajhahnde/opaal/git/commits/{remote_sha}"],
        )
        if remote_sha is not None
        else None
    )
    check_runs = (
        json_value_command(
            result,
            "public-main-checks",
            [
                "gh",
                "api",
                "-H",
                "Accept: application/vnd.github+json",
                f"repos/ajhahnde/opaal/commits/{remote_sha}/check-runs?per_page=100",
            ],
        )
        if remote_sha is not None
        else None
    )
    ruleset_rows = json_value_command(
        result,
        "repository-rulesets",
        ["gh", "api", "repos/ajhahnde/opaal/rulesets"],
    )
    rulesets: list[dict[str, object]] = []
    if isinstance(ruleset_rows, list):
        for row in ruleset_rows:
            if not isinstance(row, dict) or not isinstance(row.get("id"), int):
                continue
            detail = json_value_command(
                result,
                f"repository-ruleset-{row['id']}",
                ["gh", "api", f"repos/ajhahnde/opaal/rulesets/{row['id']}"],
            )
            if isinstance(detail, dict):
                rulesets.append(detail)

    result.check(
        "final-repository-settings",
        isinstance(repository, dict)
        and repository.get("full_name") == "ajhahnde/opaal"
        and repository.get("visibility") == "public"
        and repository.get("archived") is False
        and repository.get("default_branch") == "main"
        and repository.get("allow_merge_commit") is False
        and repository.get("allow_squash_merge") is True
        and repository.get("allow_rebase_merge") is True
        and repository.get("delete_branch_on_merge") is True,
        "repository is public with the reviewed merge and branch-cleanup policy",
    )
    result.check(
        "read-only-workflow-default",
        isinstance(workflow_permissions, dict)
        and workflow_permissions.get("default_workflow_permissions") == "read"
        and workflow_permissions.get("can_approve_pull_request_reviews") is False,
        f"workflow_permissions={workflow_permissions!r}",
    )

    active_main_rulesets = []
    for ruleset in rulesets:
        conditions = ruleset.get("conditions", {})
        ref_name = conditions.get("ref_name", {}) if isinstance(conditions, dict) else {}
        includes = ref_name.get("include", []) if isinstance(ref_name, dict) else []
        if (
            ruleset.get("target") == "branch"
            and ruleset.get("enforcement") == "active"
            and any(value in {"~DEFAULT_BRANCH", "refs/heads/main"} for value in includes)
        ):
            active_main_rulesets.append(ruleset)
    combined_rules = [
        rule
        for ruleset in active_main_rulesets
        for rule in ruleset.get("rules", [])
        if isinstance(rule, dict)
    ]
    rule_types = {rule.get("type") for rule in combined_rules}
    required_checks: set[str] = set()
    strict_checks = False
    review_resolution = False
    for rule in combined_rules:
        parameters = rule.get("parameters", {})
        if not isinstance(parameters, dict):
            continue
        if rule.get("type") == "required_status_checks":
            strict_checks = parameters.get("strict_required_status_checks_policy") is True
            required_checks.update(
                check.get("context")
                for check in parameters.get("required_status_checks", [])
                if isinstance(check, dict) and isinstance(check.get("context"), str)
            )
        if rule.get("type") == "pull_request":
            review_resolution = parameters.get("required_review_thread_resolution") is True
    result.check(
        "protected-main-ruleset",
        len(active_main_rulesets) == 1
        and active_main_rulesets[0].get("bypass_actors") == []
        and {
            "deletion",
            "non_fast_forward",
            "pull_request",
            "required_linear_history",
            "required_status_checks",
        }.issubset(rule_types)
        and strict_checks
        and required_checks == {"required", "security-required"}
        and review_resolution,
        f"active={len(active_main_rulesets)} rules={sorted(str(value) for value in rule_types)} checks={sorted(required_checks)}",
    )

    security = repository.get("security_and_analysis", {}) if isinstance(repository, dict) else {}
    result.check(
        "repository-security-controls",
        isinstance(security, dict)
        and isinstance(security.get("secret_scanning"), dict)
        and security["secret_scanning"].get("status") == "enabled"
        and isinstance(security.get("secret_scanning_push_protection"), dict)
        and security["secret_scanning_push_protection"].get("status") == "enabled"
        and isinstance(security.get("dependabot_security_updates"), dict)
        and security["dependabot_security_updates"].get("status") == "enabled",
        f"security_and_analysis={security!r}",
    )
    result.run(
        "vulnerability-alerts-enabled",
        ["gh", "api", "--silent", "repos/ajhahnde/opaal/vulnerability-alerts"],
    )
    result.run(
        "dependabot-security-updates-enabled",
        ["gh", "api", "--silent", "repos/ajhahnde/opaal/automated-security-fixes"],
    )

    remote_tree = None
    if isinstance(remote_commit, dict):
        tree_row = remote_commit.get("tree")
        if isinstance(tree_row, dict):
            remote_tree = tree_row.get("sha")
    local_head = git_output("rev-parse", "HEAD")
    local_tree = git_output("rev-parse", "HEAD^{tree}")
    result.check(
        "exact-public-tip",
        remote_sha == local_head and remote_tree == local_tree,
        f"local_head={local_head} remote_head={remote_sha} local_tree={local_tree} remote_tree={remote_tree}",
    )
    completed_checks: dict[str, tuple[str | None, str | None]] = {}
    if isinstance(check_runs, dict):
        for row in check_runs.get("check_runs", []):
            if isinstance(row, dict) and row.get("name") in {"required", "security-required"}:
                completed_checks[str(row["name"])] = (
                    row.get("status"),
                    row.get("conclusion"),
                )
    result.check(
        "exact-tip-required-checks",
        completed_checks
        == {
            "required": ("completed", "success"),
            "security-required": ("completed", "success"),
        },
        f"checks={completed_checks!r}",
    )

    reviewed_claims = {
        f"TR-{number:03d}" for number in range(1, 16)
    } | {"TR-018"}
    reviewed_claims -= {"TR-016", "TR-017"}
    claim_digests: dict[str, str | None] = {}
    invalid_claims: list[str] = []
    for claim in sorted(reviewed_claims):
        path = EVIDENCE_ROOT / f"{claim}.json"
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            invalid_claims.append(claim)
            continue
        if payload.get("success") is not True:
            invalid_claims.append(claim)
        claim_digests[claim] = payload.get("candidate_worktree_sha256")
    current_digest = worktree_digest()
    result.check(
        "reviewed-claim-candidate",
        not invalid_claims
        and set(claim_digests.values()) == {current_digest},
        f"invalid={invalid_claims!r} digests={sorted(str(value) for value in set(claim_digests.values()))}",
    )

    evidence = {
        "schema": 1,
        "repository": {
            "name": repository.get("full_name") if isinstance(repository, dict) else None,
            "visibility": repository.get("visibility") if isinstance(repository, dict) else None,
            "default_branch": repository.get("default_branch") if isinstance(repository, dict) else None,
        },
        "local_head": local_head,
        "local_tree": local_tree,
        "public_head": remote_sha,
        "public_tree": remote_tree,
        "required_checks": completed_checks,
        "active_main_ruleset_ids": [row.get("id") for row in active_main_rulesets],
        "no_bypass": len(active_main_rulesets) == 1
        and active_main_rulesets[0].get("bypass_actors") == [],
        "workflow_permissions": workflow_permissions,
        "security_and_analysis": security,
    }
    destination = ROOT / "transition-evidence/repository-settings.json"
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(
        json.dumps(evidence, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def verify_tr_015(result: Verification) -> None:
    root_manifest = load_toml(ROOT / "Cargo.toml")
    workspace = root_manifest.get("workspace", {})
    package_defaults = workspace.get("package", {}) if isinstance(workspace, dict) else {}
    result.check(
        "development-version",
        package_defaults.get("version") == "1.0.0-alpha.1",
        f"workspace version={package_defaults.get('version')!r}",
    )
    result.check(
        "workspace-publication-disabled",
        package_defaults.get("publish") is False,
        f"workspace publish={package_defaults.get('publish')!r}",
    )

    publish_exceptions: list[str] = []
    for crate_path in WORKSPACE_PACKAGES:
        package = load_toml(ROOT / crate_path / "Cargo.toml").get("package", {})
        if not isinstance(package, dict) or package.get("publish") != {"workspace": True}:
            publish_exceptions.append(crate_path)
    fuzz_package = load_toml(ROOT / "fuzz/Cargo.toml").get("package", {})
    if not isinstance(fuzz_package, dict) or fuzz_package.get("publish") is not False:
        publish_exceptions.append("fuzz")
    result.check(
        "all-packages-unpublishable",
        not publish_exceptions,
        f"publishable or unbound packages={publish_exceptions!r}",
    )

    metadata = result.run(
        "cargo-metadata",
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"],
    )
    if metadata.returncode == 0:
        payload = json.loads(metadata.stdout)
        package_versions = {
            package["name"]: (package["version"], package["publish"])
            for package in payload.get("packages", [])
        }
        invalid_metadata = {
            name: values
            for name, values in package_versions.items()
            if values != ("1.0.0-alpha.1", [])
        }
        result.check(
            "metadata-version-and-publication",
            not invalid_metadata and set(package_versions) == set(WORKSPACE_PACKAGES.values()),
            f"invalid metadata={invalid_metadata!r}",
        )

    version = result.run(
        "cli-development-version",
        ["cargo", "run", "--quiet", "--locked", "-p", "opaal-cli", "--bin", "opaal", "--", "--version"],
    )
    result.check(
        "cli-version-output",
        version.returncode == 0 and version.stdout.strip() == b"opaal 1.0.0-alpha.1",
        f"stdout={version.stdout.decode(errors='replace').strip()!r}",
    )

    release_workflow = read_text(Path(".github/workflows/release.yml"))
    forbidden_release = re.findall(
        r"^\s*[a-z-]+:\s*write\s*$|\b(?:cargo\s+publish|gh\s+release\s+create|git\s+push)\b",
        release_workflow,
        re.MULTILINE,
    )
    result.check(
        "release-workflow-read-only",
        "workflow_dispatch:" in release_workflow and not forbidden_release,
        f"forbidden release entries={forbidden_release!r}",
    )
    result.check(
        "no-transition-tag",
        not git_output("tag", "--list"),
        f"local tags={git_output('tag', '--list')!r}",
    )
    target_tags = json_value_command(
        result,
        "target-tags",
        ["gh", "api", "repos/ajhahnde/opaal/tags"],
    )
    target_releases = json_value_command(
        result,
        "target-releases",
        ["gh", "api", "repos/ajhahnde/opaal/releases"],
    )
    result.check(
        "no-remote-tag-or-release",
        target_tags == [] and target_releases == [],
        f"tags={target_tags!r} releases={target_releases!r}",
    )
    published_versions: dict[str, str] = {}
    for package in [*WORKSPACE_PACKAGES.values(), "opaal-fuzz"]:
        response = result.run(
            f"unpublished-crate-{package}",
            [
                "curl",
                "--silent",
                "--show-error",
                "--user-agent",
                "opaal-transition-verifier/1.0 (+https://github.com/ajhahnde/opaal)",
                "--output",
                "/dev/null",
                "--write-out",
                "%{http_code}",
                f"https://crates.io/api/v1/crates/{package}/1.0.0-alpha.1",
            ],
        )
        published_versions[package] = response.stdout.decode(errors="replace")
    result.check(
        "no-published-transition-package",
        set(published_versions.values()) == {"404"},
        f"crates.io statuses={published_versions!r}",
    )
    current_docs = read_text(Path("README.md")) + read_text(Path("CHANGELOG.md"))
    result.check(
        "unreleased-public-claim",
        "1.0.0-alpha.1" in current_docs and "[Unreleased]" in current_docs,
        "README and changelog identify an unreleased development line",
    )


def write_evidence(result: Verification) -> Path:
    payload = {
        "schema": 1,
        "claim": result.claim,
        "success": result.success,
        "source_tip": git_output("rev-parse", "HEAD"),
        "candidate_worktree_sha256": worktree_digest(),
        "public_tip": None,
        "assertions": result.assertions,
        "commands": result.commands,
    }
    EVIDENCE_ROOT.mkdir(parents=True, exist_ok=True)
    destination = EVIDENCE_ROOT / f"{result.claim}.json"
    destination.write_text(
        json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return destination


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    verify = subparsers.add_parser("verify")
    verify.add_argument(
        "--claim",
        choices=(
            "TR-001",
            "TR-002",
            "TR-003",
            "TR-004",
            "TR-005",
            "TR-006",
            "TR-007",
            "TR-008",
            "TR-009",
            "TR-010",
            "TR-011",
            "TR-012",
            "TR-013",
            "TR-014",
            "TR-015",
            "TR-017",
            "TR-018",
        ),
        required=True,
    )
    return parser.parse_args()


def main() -> int:
    arguments = parse_args()
    result = Verification(arguments.claim)
    if arguments.claim == "TR-001":
        verify_tr_001(result)
    elif arguments.claim == "TR-002":
        verify_tr_002(result)
    elif arguments.claim == "TR-003":
        verify_tr_003(result)
    elif arguments.claim == "TR-004":
        verify_tr_004(result)
    elif arguments.claim == "TR-005":
        verify_tr_005(result)
    elif arguments.claim == "TR-006":
        verify_tr_006(result)
    elif arguments.claim == "TR-007":
        verify_tr_007(result)
    elif arguments.claim == "TR-008":
        verify_tr_008(result)
    elif arguments.claim == "TR-009":
        verify_tr_009(result)
    elif arguments.claim == "TR-010":
        verify_tr_010(result)
    elif arguments.claim == "TR-011":
        verify_tr_011(result)
    elif arguments.claim == "TR-012":
        verify_tr_012(result)
    elif arguments.claim == "TR-013":
        verify_tr_013(result)
    elif arguments.claim == "TR-014":
        verify_tr_014(result)
    elif arguments.claim == "TR-015":
        verify_tr_015(result)
    elif arguments.claim == "TR-017":
        verify_tr_017(result)
    elif arguments.claim == "TR-018":
        verify_tr_018(result)
    destination = write_evidence(result)
    if result.success:
        print(f"{result.claim}: verified ({destination.relative_to(ROOT)})")
        return 0
    for assertion in result.assertions:
        if not assertion["success"]:
            print(
                f"{result.claim}: {assertion['name']}: {assertion['detail']}",
                file=sys.stderr,
            )
    print(f"{result.claim}: failed ({destination.relative_to(ROOT)})", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
