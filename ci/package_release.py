#!/usr/bin/env python3
"""Package the two built OPAAL programs for one supported release host."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import os
import tarfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
VERSION = "1.0.0"
PLATFORMS = ("linux-x86_64", "macos-arm64")
FILES = (
    ("target/release/opaal", "opaal", 0o755),
    ("target/release/opaal-language-server", "opaal-language-server", 0o755),
    ("LICENSE", "LICENSE", 0o644),
    ("README.md", "README.md", 0o644),
)


def package(root: Path, platform: str) -> tuple[Path, Path]:
    if platform not in PLATFORMS:
        raise ValueError(f"unsupported release platform: {platform}")

    prefix = f"opaal-v{VERSION}-{platform}"
    output = root / "dist"
    output.mkdir(exist_ok=True)
    archive = output / f"{prefix}.tar.gz"
    expected: dict[str, tuple[bytes, int]] = {}

    with archive.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as zipped:
            with tarfile.open(fileobj=zipped, mode="w", format=tarfile.USTAR_FORMAT) as tar:
                for source_name, member_name, mode in FILES:
                    source = root / source_name
                    if not source.is_file() or source.is_symlink():
                        raise ValueError(
                            "release input is missing or not a regular file: "
                            f"{source_name}"
                        )
                    if mode == 0o755 and not os.access(source, os.X_OK):
                        raise ValueError(f"release program is not executable: {source_name}")
                    data = source.read_bytes()
                    if not data:
                        raise ValueError(f"release input is empty: {source_name}")
                    archive_name = f"{prefix}/{member_name}"
                    info = tarfile.TarInfo(archive_name)
                    info.size = len(data)
                    info.mode = mode
                    info.mtime = 0
                    with source.open("rb") as input_file:
                        tar.addfile(info, input_file)
                    expected[archive_name] = (data, mode)

    with tarfile.open(archive, "r:gz") as tar:
        members = tar.getmembers()
        if {member.name for member in members} != set(expected):
            raise ValueError("release archive has unexpected members")
        for member in members:
            content = tar.extractfile(member)
            if not member.isfile() or content is None:
                raise ValueError(f"release member is not a file: {member.name}")
            data, mode = expected[member.name]
            if content.read() != data or member.mode != mode:
                raise ValueError(f"release member differs from its input: {member.name}")

    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    checksum = output / f"{archive.name}.sha256"
    checksum.write_text(f"{digest}  {archive.name}\n", encoding="ascii")
    return archive, checksum


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--platform", choices=PLATFORMS, required=True)
    args = parser.parse_args()
    archive, checksum = package(ROOT, args.platform)
    print(f"{archive.relative_to(ROOT)}\n{checksum.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
