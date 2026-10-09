#!/usr/bin/env python3
"""Qualify bounded Random using exact programs and fixtures outside the checkout."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

if __package__:
    from .check_source_formatting import public_paths
    from .package_release import ROOT, VERSION, PLATFORMS
    from .package_random_values import FIXTURES
    from .qualify_data_processing import command, language_server, require, success, unpack
else:
    from check_source_formatting import public_paths
    from package_release import ROOT, VERSION, PLATFORMS
    from package_random_values import FIXTURES
    from qualify_data_processing import command, language_server, require, success, unpack


def domains(shard: str, fraction: str) -> None:
    require(re.fullmatch(r"[0-3]", shard) is not None, "shard outside [0,4)")
    number = float(fraction)
    require(0 <= number < 1 and (number * 2**53).is_integer(), "fraction outside 53-bit lattice")


def byte_count(rendered: str) -> int:
    count = offset = 0
    while offset < len(rendered):
        if rendered[offset] == "\\":
            escape = rendered[offset:offset + 4]
            if escape.startswith("\\x"):
                require(re.fullmatch(r"\\x[0-9a-fA-F]{2}", escape) is not None, "invalid byte escape")
                offset += 4
            else:
                require(rendered[offset:offset + 2] in ('\\\\', '\\"'), "invalid byte escape")
                offset += 2
        else:
            require(32 <= ord(rendered[offset]) <= 126, "invalid rendered byte")
            offset += 1
        count += 1
    return count


def qualify(archive: Path | None, fixtures: Path | None, binaries: Path | None,
            source: str, target: str) -> dict:
    require((platform.system(), platform.machine()) == {
        "linux-x86_64": ("Linux", "x86_64"), "macos-arm64": ("Darwin", "arm64"),
    }[target], "candidate target differs from running host")
    require(subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip() == source,
            "expected source differs from checkout head")
    require(bool(archive) == bool(fixtures), "archive mode requires exact fixture archive")
    paths = public_paths(ROOT)
    snapshot = {name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
                for name in paths if (ROOT / name).is_file()}
    dirty = bool(subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT))
    if archive:
        require(not dirty, "archive qualification requires a clean committed checkout")
    with tempfile.TemporaryDirectory(prefix="opaal-random-") as temporary:
        directory = Path(temporary)
        if archive:
            programs, program_manifest = unpack(archive, directory, kind="binaries", version=VERSION,
                                               source=source, platform=target)
            inputs, fixture_manifest = unpack(fixtures, directory, kind="random-fixtures", version=VERSION,
                                             source=source, platform="portable")
            expected = {name.removeprefix(FIXTURES + "/"): value for name, value in snapshot.items()
                        if name.startswith(FIXTURES + "/")}
            observed = {str(path.relative_to(inputs)): hashlib.sha256(path.read_bytes()).hexdigest()
                        for path in inputs.rglob("*") if path.is_file()}
            require(observed == expected, "archived fixture inventory/bytes differ from source")
        else:
            require(binaries is not None, "working mode requires binary directory")
            programs = directory / "bin"
            programs.mkdir()
            for name in ("opaal", "opaal-language-server"):
                shutil.copy2(binaries / name, programs / name)
            inputs = ROOT / FIXTURES
            program_manifest = fixture_manifest = {"sha256": None}
        work = directory / "sample"
        shutil.copytree(inputs, work)
        require('required_opaal = ">=1.3.0,<2.0.0"' in (work / "opaal.toml").read_text(),
                "Random reference must require its first delivering release")
        # Qualification of an unreleased binary changes only this isolated copy.
        if tuple(map(int, VERSION.split("."))) < (1, 3, 0):
            manifest = work / "opaal.toml"
            manifest.write_text(manifest.read_text().replace(">=1.3.0,<2.0.0", f">={VERSION},<2.0.0"))
        shutil.copyfile(work / ("tools-linux.toml" if target == "linux-x86_64" else "tools-macos.toml"), work / "tools.toml")
        environment = {"PATH": os.defpath}
        binary = programs / "opaal"
        def run(*arguments, stdin=None):
            return command([binary, *arguments], work, environment, stdin=stdin)
        require(success(run("--version"), "version") == f"opaal {VERSION}\n".encode(), "version differs")
        language_server(programs / "opaal-language-server", work, environment)
        for path in sorted(work.glob("*.opaal")):
            success(run("format", "--check", path.name), f"format {path.name}")
        success(run("check", "source.opaal"), "source check")
        shown = success(run("source.opaal"), "complete source")
        match = re.fullmatch(rb"shard=([0-3]) fraction=([^\n]+)\n", shown)
        require(match is not None, "source output differs")
        domains(match[1].decode(), match[2].decode())
        reference = json.loads(success(command([sys.executable, "reference.py"], work, environment), "reference workaround"))
        domains(str(reference["shard"]), str(reference["fraction"]))
        require(len(bytes.fromhex(reference["identifier_hex"])) == 16, "reference byte count differs")
        for name, code in [("interval", b"RANDOM001"), ("count", b"RANDOM001"),
                           ("excess", b"RANDOM001"), ("pure", b"refused[unsupported]"),
                           ("callback", b"refused[unsupported]")]:
            result = run(name + ".opaal")
            require(result.returncode != 0 and result.stdout == b"" and code in result.stderr,
                    f"{name}: failure/refusal differs")
        interactive = run(stdin=b"import std::random as random\nlet shard = random::int(0, 4)\nshard\nlet fraction = random::float()\nfraction\nlet identifier = random::bytes(16)\nidentifier\nrandom::int(4, 4)\nrandom::bytes(-1)\nrandom::int(7, 8)\n")
        require(interactive.returncode == 0 and interactive.stderr.count(b"RANDOM001") == 2,
                "interactive domain recovery differs")
        lines = interactive.stdout.decode().splitlines()
        prefixes = [">> >> >> ", ">> >> ", ">> >> ", ">> >> >> "]
        require(len(lines) == 5 and lines[4] == ">> " and all(line.startswith(prefix)
                for line, prefix in zip(lines, prefixes)), "interactive transcript differs")
        values = [line.removeprefix(prefix) for line, prefix in zip(lines, prefixes)]
        domains(values[0], values[1])
        require(byte_count(values[2]) == 16 and values[3] == "7", "retained bytes/no-draw recovery differs")
        help_result = success(run(stdin=b"import std::random as random\nhelp random::int\nhelp random::float\nhelp random::bytes\nexit 0\n"), "Random help")
        require(all(name in help_result for name in [b"int", b"float", b"bytes", b"entropy.system"]), "help identity differs")
        base = ("--project", "opaal.toml", "--task", "sample", "--environment", "ci")
        checked = json.loads(success(run("check", *base, "--format", "json"), "project check"))
        require(checked["schema"] == "opaal.check.v3", "check schema differs")
        receipt = success(run("plan", *base, "--expires-in", "900s", "--out", "sample.plan.json"), "plan")
        plan = json.loads((work / "sample.plan.json").read_bytes())
        require(plan["schema"] == "opaal.plan.v3" and plan["standard_host"] == checked["standard_host"] and
                receipt == f"plan {plan['digest']}\n".encode(), "plan identity/policy differs")
        inspected = success(run("plan", "inspect", "sample.plan.json"), "plan inspect")
        require(b"entropy.system evaluation" in inspected, "plan inspection lost entropy scope")
        require(not (work / "sample.run.jsonl").exists(), "static operations created execution evidence")
        execute = ("execute", "--plan", "sample.plan.json", "--accept", plan["digest"],
                   "--run-id", "0123456789abcdef0123456789abcdef")
        success(run(*execute, "--journal", "sample.run.jsonl"), "controlled execute")
        success(run("audit", "--project", "opaal.toml", "--journal", "sample.run.jsonl", "--out", "sample.audit.json"), "audit")
        success(run("audit", "inspect", "sample.audit.json"), "audit inspect")
        audit = json.loads((work / "sample.audit.json").read_bytes())
        require(audit["schema"] == "opaal.audit.v3" and audit["completeness"] == "complete" and
                audit["primary"]["class"] == "success", "audit outcome differs")
        records = [json.loads(line) for line in (work / "sample.run.jsonl").read_bytes().splitlines()]
        after = [row["payload"] for row in records if row.get("kind") == "effect-after"]
        require(len(after) == 3, "controlled entropy events differ")
        for event, count in zip(after, [8, 8, 16]):
            operation = event["operation"]
            require(operation["confirmed_bytes"] == operation["admitted_bytes"] == count and
                    operation["uncertain_bytes_upper_bound"] == 0, "entropy progress differs")
        for row in records:
            for field in ("primary", "outcome"):
                outcome = row.get("payload", {}).get(field)
                if outcome is not None:
                    require("value_digest" in outcome and outcome["value_digest"] is None and
                            outcome["message"] == "metadata-only outcome", "routine outcome leaked value/digest")
        with (work / "tasks.opaal").open("ab") as changed:
            changed.write(b"\n")
        stale = run(*execute, "--journal", "stale.run.jsonl")
        require(stale.returncode != 0 and not (work / "stale.run.jsonl").exists(), "stale source admitted")
        return {"schema_version": 1, "source": source, "committed_source": not dirty,
                "source_snapshot_sha256": hashlib.sha256(json.dumps(snapshot, sort_keys=True, separators=(",", ":")).encode()).hexdigest(),
                "version": VERSION, "platform": target,
                "archive_sha256": program_manifest["sha256"], "fixture_archive_sha256": fixture_manifest["sha256"],
                "binary_sha256": {name: hashlib.sha256((programs / name).read_bytes()).hexdigest() for name in ("opaal", "opaal-language-server")},
                "fixture_sha256": {name: value for name, value in snapshot.items() if name.startswith(FIXTURES + "/")},
                "assertions": ["program and fixture identity", "source check/format", "native scalar domains",
                               "independent reference", "domain and pure/callback refusal", "retained interactive cells and recovery",
                               "help and language server lifecycle", "project check/plan", "controlled progress and metadata-only audit", "stale source refusal"]}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    programs = parser.add_mutually_exclusive_group(required=True)
    programs.add_argument("--archive", type=Path)
    programs.add_argument("--binary-directory", type=Path)
    parser.add_argument("--fixtures", type=Path)
    parser.add_argument("--expected-source", required=True)
    parser.add_argument("--platform", required=True, choices=PLATFORMS)
    parser.add_argument("--report", required=True, type=Path)
    args = parser.parse_args()
    result = qualify(args.archive.resolve() if args.archive else None,
                     args.fixtures.resolve() if args.fixtures else None,
                     args.binary_directory.resolve() if args.binary_directory else None,
                     args.expected_source, args.platform)
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print("Random candidate qualification passed.")


if __name__ == "__main__":
    main()
