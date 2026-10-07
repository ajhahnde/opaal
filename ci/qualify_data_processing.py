#!/usr/bin/env python3
"""Qualify exact candidate archives and offline report examples outside a checkout."""

from __future__ import annotations

import argparse
import base64
import gzip
import io
import json
import os
import platform as host
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
from pathlib import Path, PurePosixPath

if __package__:
    from .package_data_processing import PLATFORMS, ROOT, SOURCE, digest
else:
    from package_data_processing import PLATFORMS, ROOT, SOURCE, digest

VERSION = re.compile(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)")
VALID = ("empty", "pending", "conclusions", "stable-prefix", "escaped", "producer-shaped")
INVALID = ("malformed", "duplicate", "null-jobs", "missing-jobs", "root-list",
           "unknown-status", "unknown-conclusion", "null-conclusion",
           "missing-completed-conclusion", "wrong-name", "wrong-job", "utf8")


class QualificationError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise QualificationError(message)


def unpack(archive: Path, destination: Path, *, kind: str, version: str,
           source: str, platform: str) -> tuple[Path, dict]:
    """Check identity, all hashes and modes before writing any archive member."""
    with archive.with_name(archive.name + ".manifest.json").open("rb") as handle:
        manifest_bytes = handle.read(1024 * 1024 + 1)
    require(len(manifest_bytes) <= 1024 * 1024, "archive manifest exceeds qualification bound")
    manifest = json.loads(manifest_bytes)
    require(kind in {"binaries", "examples", "formatting-fixtures"}, "unknown archive kind")
    suffix = {"examples": "data-processing", "formatting-fixtures": "source-formatting"}.get(kind, platform)
    prefix = f"opaal-v{version}-{suffix}"
    require(archive.name == prefix + ".tar.gz", "archive filename differs from candidate identity")
    expected = {"schema_version": 1, "kind": kind, "version": version,
                "source": source, "platform": platform, "archive": archive.name}
    for key, value in expected.items():
        require(manifest.get(key) == value, f"archive manifest identity differs: {key}")
    total_limit = 128 * 1024 * 1024 if kind == "binaries" else 8 * 1024 * 1024
    require(archive.stat().st_size <= total_limit, "compressed archive exceeds qualification bound")
    checksum = digest(archive.read_bytes())
    require(manifest.get("sha256") == checksum, "archive digest differs from manifest")
    with archive.with_name(archive.name + ".sha256").open("rb") as handle:
        require(handle.read(256) == f"{checksum}  {archive.name}\n".encode("ascii"),
                "archive checksum sidecar differs")
    entries = manifest.get("members")
    require(isinstance(entries, list) and 0 < len(entries) <= 256, "invalid member inventory")
    require(all(isinstance(entry, dict) and isinstance(entry.get("path"), str)
                for entry in entries), "invalid member identity")
    expected_members = {entry["path"]: entry for entry in entries}
    require(len(expected_members) == len(entries), "duplicate manifest members")
    if kind == "binaries":
        require(set(expected_members) == {f"{prefix}/{name}" for name in
                ("opaal", "opaal-language-server", "LICENSE", "README.md")},
                "binary archive inventory differs")
    # Bound metadata and padding too, before tarfile reads PAX/GNU headers.
    stream_limit = total_limit + 256 * 1024 + tarfile.RECORDSIZE
    with gzip.open(archive, "rb") as handle:
        expanded = handle.read(stream_limit + 1)
    require(len(expanded) <= stream_limit, "expanded archive stream exceeds qualification bound")
    contents = []
    with tarfile.open(fileobj=io.BytesIO(expanded), mode="r:") as tar:
        total = 0
        seen = set()
        for member in tar:
            require(len(seen) < len(entries), "archive member count differs")
            require(member.name in expected_members and member.name not in seen,
                    "archive member inventory differs")
            seen.add(member.name)
            name = PurePosixPath(member.name)
            require(not name.is_absolute() and len(name.parts) >= 2 and name.parts[0] == prefix
                    and ".." not in name.parts and name.as_posix() == member.name,
                    "unsafe archive member path")
            require(member.isfile() and not member.issym() and not member.islnk(),
                    "archive member is not a regular file")
            total += member.size
            require(0 <= member.size <= total_limit and total <= total_limit,
                    "expanded archive exceeds qualification bound")
            entry = expected_members[member.name]
            expected_mode = (0o755 if kind == "binaries" and name.name in
                             ("opaal", "opaal-language-server") else 0o644)
            require(member.mode == expected_mode and entry.get("mode") == expected_mode,
                    "archive member mode differs")
            content = tar.extractfile(member)
            require(content is not None, "archive member cannot be read")
            data = content.read()
            require(len(data) == member.size == entry.get("bytes") and
                    digest(data) == entry.get("sha256"), "archive member bytes differ")
            contents.append((name, data, expected_mode))
        require(seen == set(expected_members), "archive member inventory differs")
    for name, data, mode in contents:
        path = destination.joinpath(*name.parts)
        path.parent.mkdir(parents=True, exist_ok=True)
        require(not path.exists(), "archive destination already exists")
        path.write_bytes(data)
        path.chmod(mode)
    return destination / prefix, manifest


def command(arguments: list[str | Path], cwd: Path, environment: dict[str, str],
            *, stdin: bytes | None = None) -> subprocess.CompletedProcess:
    return subprocess.run([str(arg) for arg in arguments], cwd=cwd, env=environment,
                          input=stdin, capture_output=True, timeout=120, check=False)


def success(result: subprocess.CompletedProcess, label: str) -> bytes:
    require(result.returncode == 0 and not result.stderr, f"{label}: expected successful quiet execution")
    return result.stdout


def invalid(result: subprocess.CompletedProcess, label: str) -> None:
    require(result.returncode != 0 and not result.stdout and bool(result.stderr),
            f"{label}: expected failure without a report")


def language_server(binary: Path, cwd: Path, environment: dict[str, str]) -> None:
    messages = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"capabilities": {}}},
        {"jsonrpc": "2.0", "method": "initialized", "params": {}},
        {"jsonrpc": "2.0", "id": 2, "method": "shutdown"},
        {"jsonrpc": "2.0", "method": "exit"},
    ]
    framed = b""
    for message in messages:
        body = json.dumps(message).encode()
        framed += f"Content-Length: {len(body)}\r\n\r\n".encode() + body
    output = success(command([binary], cwd, environment, stdin=framed), "language server lifecycle")
    responses = []
    while output:
        headers, separator, remaining = output.partition(b"\r\n\r\n")
        require(bool(separator), "invalid language server frame")
        length = re.search(rb"Content-Length: (\d+)", headers)
        require(length is not None, "missing language server frame length")
        count = int(length.group(1))
        require(len(remaining) >= count, "truncated language server response")
        responses.append(json.loads(remaining[:count]))
        output = remaining[count:]
    require(len(responses) == 2 and responses[0]["id"] == 1 and
            responses[0]["result"]["serverInfo"]["name"] == "OPAAL Language Server" and
            responses[1] == {"jsonrpc": "2.0", "id": 2, "result": None},
            "language server initialization/shutdown differs")


# One child per worker gives per-command RSS, rather than the cumulative
# RUSAGE_CHILDREN maximum across previously executed candidates/references.
MEASURE = """import base64,json,resource,subprocess,sys,time
started=time.perf_counter()
result=subprocess.run(json.loads(sys.argv[1]),capture_output=True,timeout=120)
usage=resource.getrusage(resource.RUSAGE_CHILDREN)
print(json.dumps({'returncode':result.returncode,
 'stdout':base64.b64encode(result.stdout).decode(),
 'stderr':base64.b64encode(result.stderr).decode(),
 'wall_seconds':time.perf_counter()-started,
 'peak_rss_bytes':usage.ru_maxrss*(1 if sys.platform=='darwin' else 1024),
 'user_seconds':usage.ru_utime,'system_seconds':usage.ru_stime}))
"""


def measure(arguments: list[str | Path], cwd: Path, environment: dict[str, str]) -> tuple[bytes, dict]:
    result = command([sys.executable, "-c", MEASURE,
                      json.dumps([str(arg) for arg in arguments])], cwd, environment)
    data = json.loads(success(result, "measurement worker"))
    require(data.pop("returncode") == 0, "10000-job measurement command failed")
    output = base64.b64decode(data.pop("stdout"), validate=True)
    require(not base64.b64decode(data.pop("stderr"), validate=True),
            "10000-job measurement command reported an error")
    require(data["wall_seconds"] > 0 and data["peak_rss_bytes"] > 0, "missing resource observation")
    return output, data


def controlled(binary: Path, work: Path, environment: dict[str, str], expected: bytes,
               *, invalid_input: bool = False) -> None:
    for name in ("report.plan.json", "report.run.jsonl", "report.audit.json", "report.json"):
        (work / name).unlink(missing_ok=True)
    base = ["--project", "opaal.toml", "--task", "summarize", "--environment", "ci",
            "--input-file", "input=jobs.json"]
    success(command([binary, "check", *base], work, environment), "project check")
    output = success(command([binary, "plan", *base, "--expires-in", "900s", "--out",
                              "report.plan.json"], work, environment), "project plan")
    require(re.fullmatch(rb"plan sha256:[0-9a-f]{64}\n", output) is not None, "invalid plan identity")
    accepted = output.decode().strip().split()[1]
    plan = json.loads((work / "report.plan.json").read_bytes())
    require(plan["tools"] == [] and plan["secrets"] == [] and
            len(plan["authority"]["requests"]) == 2, "project requests unexpected authority")
    result = command([binary, "execute", "--plan", "report.plan.json", "--accept", accepted,
                      "--journal", "report.run.jsonl"], work, environment)
    if invalid_input:
        require(result.returncode != 0 and not (work / "report.json").exists(),
                "invalid controlled input produced a report")
    else:
        success(result, "controlled execution")
        require((work / "report.json").read_bytes() == expected, "controlled report differs")
    success(command([binary, "audit", "--project", "opaal.toml", "--journal",
                     "report.run.jsonl", "--out", "report.audit.json"], work, environment), "audit")
    audit = json.loads((work / "report.audit.json").read_bytes())
    require(audit["primary"]["class"] == ("error" if invalid_input else "success") and
            audit["primary"]["partial"] == invalid_input, "audit primary differs")
    effects = [event["payload"]["effect"] for event in audit["events"]
               if event["kind"] == "effect-before"]
    require(effects == (["filesystem.read"] if invalid_input else
                        ["filesystem.read", "filesystem.write"]), "audit effects differ")


def qualify(archive: Path, examples: Path, version: str, source: str, platform: str) -> dict:
    require(VERSION.fullmatch(version) is not None and SOURCE.fullmatch(source) is not None,
            "expected version/source identity is not canonical")
    observed = {("Linux", "x86_64"): "linux-x86_64", ("Darwin", "arm64"): "macos-arm64"}
    require(observed.get((host.system(), host.machine())) == platform, "qualification host differs")
    started = time.perf_counter()
    with tempfile.TemporaryDirectory(prefix="opaal-data-processing-") as temporary:
        root = Path(temporary).resolve()
        require(not root.is_relative_to(ROOT), "first-use directory must be outside the checkout")
        installed, binaries_manifest = unpack(archive, root, kind="binaries", version=version,
                                              source=source, platform=platform)
        work, examples_manifest = unpack(examples, root, kind="examples", version=version,
                                        source=source, platform="portable")
        binary = installed / "opaal"
        bin_dir = root / "bin"
        bin_dir.mkdir()
        (bin_dir / "cat").symlink_to("/bin/cat")
        environment = {"PATH": str(bin_dir)}
        require(success(command([binary, "--version"], root, environment), "binary version") ==
                f"opaal {version}\n".encode(), "binary version differs")
        language_server(installed / "opaal-language-server", root, environment)
        original_manifest = (work / "opaal.toml").read_bytes()
        floor = b'>=1.2.0,<2.0.0'
        require(original_manifest.count(floor) == 1, "unexpected example version requirement")
        adapted = original_manifest if tuple(map(int, version.split("."))) >= (1, 2, 0) else original_manifest.replace(
            floor, f">={version},<2.0.0".encode())
        (work / "opaal.toml").write_bytes(adapted)
        tool = "tools-macos.toml" if platform == "macos-arm64" else "tools-linux.toml"
        shutil.copyfile(work / tool, work / "tools.toml")
        sources = ["report.opaal", "json-report.opaal", "text-report.opaal",
                   "policy-operations.opaal", "tasks.opaal"]
        for name in sources[:-1]:
            success(command([binary, "check", name], work, environment), f"source check {name}")
        success(command([binary, "format", "--check", *sources], work, environment), "format")
        default = (work / "jobs.json").read_bytes()
        comparisons = []
        for name in ("jobs", *VALID):
            raw = default if name == "jobs" else (work / "valid" / f"{name}.json").read_bytes()
            (work / "jobs.json").write_bytes(raw)
            for extension, script in (("json", "json-report.opaal"), ("txt", "text-report.opaal")):
                expected = (work / (f"expected-report.{extension}" if name == "jobs" else
                                   f"valid/{name}.expected.{extension}")).read_bytes()
                actual = success(command([binary, script], work, environment), f"ordinary {name}")
                reference = [sys.executable, work / "reference.py", work / "jobs.json"]
                if extension == "txt":
                    reference.append("--text")
                baseline = success(command(reference, work, environment), f"reference {name}")
                require(actual == baseline == expected, f"{name}: report/reference bytes differ")
                comparisons.append({"case": name, "format": extension, "sha256": digest(actual)})
            expected = (work / ("expected-report.json" if name == "jobs" else
                               f"valid/{name}.expected.json")).read_bytes()
            cell = ("import './report.opaal' as report\nimport std::data as data\n"
                    "^cat jobs.json | from json document | each {|document| "
                    "data::json_encode(report::build(document))} | encode bytes\nexit 0\n")
            interactive = success(command([binary], work, environment, stdin=cell.encode()),
                                  f"interactive {name}")
            require(expected in interactive, f"{name}: interactive output lacks exact report bytes")
            # Controlled execution must work with no ambient executable available.
            controlled(binary, work, {"PATH": str(root / "unavailable-tools")}, expected)
        for name in INVALID:
            (work / "jobs.json").write_bytes((work / "invalid" / f"{name}.json").read_bytes())
            for script in ("json-report.opaal", "text-report.opaal"):
                invalid(command([binary, script], work, environment), f"invalid {name}")
            reference = command([sys.executable, work / "reference.py", work / "jobs.json"],
                                work, environment)
            invalid(reference, f"invalid reference {name}")
            require(reference.stderr == b"reference: invalid input\n", "reference failure class differs")
        controlled(binary, work, {"PATH": str(root / "unavailable-tools")}, b"", invalid_input=True)
        (work / "jobs.json").unlink()
        invalid(command([binary, "json-report.opaal"], work, environment), "unavailable input")
        require(command([sys.executable, work / "reference.py", work / "jobs.json"], work,
                        environment).stderr == b"reference: unavailable input\n", "reference unavailable class differs")
        (work / "jobs.json").write_bytes(default)
        invalid(command([binary, "json-report.opaal"], work,
                        {"PATH": str(root / "unavailable-tools")}), "unavailable cat")
        jobs = [{"name": f"job/{index % 101:03}/{index:05}", "status": "completed",
                 "conclusion": "failure" if index % 3 == 0 else "success"} for index in range(10000)]
        (work / "jobs.json").write_text(json.dumps({"jobs": jobs}, separators=(",", ":")))
        actual, opaal_measurement = measure([binary, "json-report.opaal"], work, environment)
        baseline, python_measurement = measure([sys.executable, work / "reference.py", work / "jobs.json"], work, environment)
        require(actual == baseline, "10000-job report differs from reference")
        # Bind the unchanged bundled files; only the project floor may be adapted.
        for entry in examples_manifest["members"]:
            relative = PurePosixPath(entry["path"]).relative_to(work.name).as_posix()
            if relative == "jobs.json":
                (work / relative).write_bytes(default)
            expected_digest = digest(adapted) if relative == "opaal.toml" else entry["sha256"]
            require(digest((work / relative).read_bytes()) == expected_digest, "bundled source changed during qualification")
        return {
            "schema_version": 1, "source": source, "version": version, "platform": platform,
            "archive_sha256": binaries_manifest["sha256"], "examples_sha256": examples_manifest["sha256"],
            "project_manifest": {"original_sha256": digest(original_manifest), "qualified_sha256": digest(adapted),
                                 "version_floor_adapted": original_manifest != adapted},
            "comparisons": comparisons, "invalid_cases": list(INVALID),
            "controlled_cases": 7, "interactive_cases": 7, "outside_checkout": True,
            "resource_observations": {"jobs": 10000, "report_sha256": digest(actual),
                                      "opaal": opaal_measurement, "python": python_measurement},
            "remaining_tools": ["cat for ordinary acquisition", "Python for qualification comparison"],
            "adoption_evidence": "No user adoption or superiority claim; same-input engineering comparison only.",
            "wall_seconds": time.perf_counter() - started,
        }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--examples", type=Path, required=True)
    parser.add_argument("--expected-version", required=True)
    parser.add_argument("--expected-source", required=True)
    parser.add_argument("--platform", choices=PLATFORMS, required=True)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    try:
        report = qualify(args.archive.resolve(), args.examples.resolve(), args.expected_version,
                         args.expected_source, args.platform)
        encoded = json.dumps(report, sort_keys=True, indent=2) + "\n"
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(encoded)
        print(encoded, end="")
        return 0
    except (QualificationError, OSError, ValueError, KeyError, TypeError, tarfile.TarError,
            subprocess.TimeoutExpired) as error:
        print(f"Data-processing qualification failed: {type(error).__name__}: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
