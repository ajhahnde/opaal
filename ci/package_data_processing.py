#!/usr/bin/env python3
"""Package candidate programs and the exact offline data-processing examples."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import re
import subprocess
import tarfile
from pathlib import Path

if __package__:
    from .package_release import PLATFORMS, ROOT, VERSION, package
else:
    from package_release import PLATFORMS, ROOT, VERSION, package

EXAMPLES = "tests/golden/data-processing"
SOURCE = re.compile(r"[0-9a-f]{40}")


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def write_manifest(archive: Path, *, kind: str, source: str, platform: str) -> Path:
    if SOURCE.fullmatch(source) is None:
        raise ValueError("source must be one full Git commit identity")
    members = []
    with tarfile.open(archive, "r:gz") as tar:
        for member in tar.getmembers():
            content = tar.extractfile(member) if member.isfile() else None
            if content is None:
                raise ValueError("candidate archive must contain only regular files")
            data = content.read()
            members.append({"path": member.name, "bytes": len(data),
                            "mode": member.mode, "sha256": digest(data)})
    manifest = {
        "schema_version": 1, "kind": kind, "source": source,
        "version": VERSION, "platform": platform,
        "archive": archive.name, "sha256": digest(archive.read_bytes()),
        "members": members,
    }
    path = archive.with_name(archive.name + ".manifest.json")
    path.write_text(json.dumps(manifest, sort_keys=True, indent=2) + "\n")
    return path


def package_examples(root: Path, paths: list[str], source: str, *,
                     directory: str = EXAMPLES, name: str = "data-processing") -> Path:
    root = root.resolve()
    if SOURCE.fullmatch(source) is None or (directory, name) not in {
        (EXAMPLES, "data-processing"), ("tests/golden/source-formatting", "source-formatting"),
        ("tests/golden/random-values", "random-values"),
    }:
        raise ValueError("invalid fixture source or bundle name")
    prefix = f"opaal-v{VERSION}-{name}"
    output = root / "dist"
    output.mkdir(exist_ok=True)
    archive = output / f"{prefix}.tar.gz"
    names = sorted(paths)
    if not names or len(names) != len(set(names)):
        raise ValueError("example inventory must be nonempty and unique")
    with archive.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as zipped:
            with tarfile.open(fileobj=zipped, mode="w", format=tarfile.USTAR_FORMAT) as tar:
                for relative in names:
                    name = Path(relative)
                    if name.is_absolute() or ".." in name.parts or name.as_posix() != relative:
                        raise ValueError("example path must be canonical and relative")
                    source_file = root / directory / name
                    if any(parent.is_symlink() for parent in (source_file, *source_file.parents)):
                        raise ValueError(f"example input has a symlink component: {relative}")
                    if not source_file.is_file() or source_file.is_symlink():
                        raise ValueError(f"example input must be a regular file: {relative}")
                    data = source_file.read_bytes()
                    info = tarfile.TarInfo(f"{prefix}/{relative}")
                    info.size, info.mode, info.mtime = len(data), 0o644, 0
                    with source_file.open("rb") as handle:
                        tar.addfile(info, handle)
    archive.with_name(archive.name + ".sha256").write_text(
        f"{digest(archive.read_bytes())}  {archive.name}\n", encoding="ascii"
    )
    kind = {EXAMPLES: "examples", "tests/golden/source-formatting": "formatting-fixtures",
            "tests/golden/random-values": "random-fixtures"}[directory]
    write_manifest(archive, kind=kind,
                   source=source, platform="portable")
    return archive


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", choices=PLATFORMS, required=True)
    args = parser.parse_args()
    source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip()
    # The local harness may be uncommitted, but the runtime and shipped fixture
    # inputs must still match the commit named in the artifact manifests.
    subprocess.run([
        "git", "diff", "--exit-code", "HEAD", "--", "crates", "Cargo.toml",
        "Cargo.lock", "rust-toolchain.toml", EXAMPLES, "LICENSE", "README.md",
    ], cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
    files = subprocess.check_output(["git", "ls-files", "-z", "--", EXAMPLES], cwd=ROOT)
    names = [Path(path).relative_to(EXAMPLES).as_posix()
             for path in files.decode().split("\0") if path]
    archive, _ = package(ROOT, args.platform)
    write_manifest(archive, kind="binaries", source=source, platform=args.platform)
    examples = package_examples(ROOT, names, source)
    print(f"{archive.relative_to(ROOT)}\n{examples.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
