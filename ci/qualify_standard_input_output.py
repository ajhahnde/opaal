#!/usr/bin/env python3
"""Replay exact native processor and CI job-check references outside the checkout."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

if __package__:
    from .check_source_formatting import public_paths
    from .package_release import ROOT, VERSION, PLATFORMS
    from .package_standard_input_output import FIXTURES, check_copies
    from .qualify_data_processing import INVALID, VALID, command, language_server, require, success, unpack
else:
    from check_source_formatting import public_paths
    from package_release import ROOT, VERSION, PLATFORMS
    from package_standard_input_output import FIXTURES, check_copies
    from qualify_data_processing import INVALID, VALID, command, language_server, require, success, unpack


def assessment(report: dict) -> bytes:
    return json.dumps({"report": report, "passed": report["failed"] == report["pending"] == 0},
                      sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def references(binary: Path, inputs: Path, directory: Path, target: str) -> list[str]:
    """Exercise the same shipped files used by CLI tests and archive qualification."""
    work = directory / "sample"
    shutil.copytree(inputs, work)
    environment = {"PATH": str(binary.parent) + os.pathsep + os.defpath}
    for sample in [work, work / "ci-job-check"]:
        manifest = sample / "opaal.toml"
        require('required_opaal = ">=1.3.0,<2.0.0"' in manifest.read_text(), "reference version floor differs")
        if tuple(map(int, VERSION.split("."))) < (1, 3, 0):
            manifest.write_text(manifest.read_text().replace(">=1.3.0,<2.0.0", f">={VERSION},<2.0.0"))
        shutil.copyfile(sample / ("tools-linux.toml" if target == "linux-x86_64" else "tools-macos.toml"),
                        sample / "tools.toml")
    def run(sample, *arguments, stdin=None):
        return command([binary, *arguments], sample, environment, stdin=stdin)
    for path in sorted(work.rglob("*.opaal")):
        success(run(work, "format", "--check", path), f"format {path.name}")
        if path.name != "tasks.opaal":
            success(run(work, "check", path), f"check {path.name}")
    for payload in [b"", "Grüße\n".encode(), b"a" * 4096]:
        result = run(work, "source.opaal", stdin=payload)
        require(result.returncode == 0 and result.stdout == payload and result.stderr == b"processed\n",
                "processor exact bytes differ")
    for payload, diagnostic in [(b"\xff", b"invalid UTF8"), (b"a" * 4097, b"IO002")]:
        result = run(work, "source.opaal", stdin=payload)
        require(result.returncode == 1 and not result.stdout and diagnostic in result.stderr,
                "processor invalid/excess input differs")
    reader, writer = os.pipe()
    os.close(reader)
    try:
        child = subprocess.Popen([binary, "source.opaal"], cwd=work, env=environment,
                                 stdin=subprocess.PIPE, stdout=writer, stderr=subprocess.PIPE)
    finally:
        os.close(writer)
    _, diagnostics = child.communicate(b"hello\n", timeout=35)
    require(child.returncode == 1 and b"IO003" in diagnostics, "processor Broken Pipe differs")

    ci = work / "ci-job-check"
    interactive = run(ci, stdin=b"import './assess.opaal' as assess\n"
                      b"assess::evaluate({ jobs: [] })\n"
                      b"assess::evaluate({ jobs: [{ name: 'build', status: 'queued' }] })\n"
                      b"assess::evaluate({ jobs: null })\n"
                      b"assess::evaluate({ jobs: [] })\nexit\n")
    records = [json.loads(line.lstrip(b"> ")) for line in interactive.stdout.splitlines()
               if line.lstrip(b"> ").startswith(b"{")]
    empty_result = json.loads(assessment({"failed": 0, "failures": [], "pending": 0, "total": 0}))
    pending_result = json.loads(assessment({"failed": 0, "failures": [], "pending": 1, "total": 1}))
    require(interactive.returncode == 0 and records == [empty_result, pending_result, empty_result]
            and interactive.stderr.count(b"error[") == 1 and b"error[RUN001]" in interactive.stderr,
            "retained interactive assessment/error recovery differs")
    occupied = run(work, stdin=b"import std::io as io\nio::read_stdin(4096)\n"
                   b"io::print('later-cell')\nexit\n")
    require(occupied.returncode == 0 and occupied.stderr.count(b"refused[") == 1
            and occupied.stdout == b">> >> >> later-cell>> ",
            "nonterminal stdin refusal consumed a later source cell")
    for name in VALID:
        report = json.loads((ci / "valid" / f"{name}.expected.json").read_bytes())
        expected = assessment(report)
        result = run(ci, "source.opaal", stdin=(ci / "valid" / f"{name}.json").read_bytes())
        require(result.returncode == (0 if json.loads(expected)["passed"] else 1)
                and result.stdout == expected and not result.stderr, f"CI assessment differs: {name}")
    for name in INVALID:
        result = run(ci, "source.opaal", stdin=(ci / "invalid" / f"{name}.json").read_bytes())
        require(result.returncode == 1 and not result.stdout and bool(result.stderr), f"invalid CI input: {name}")
    empty = assessment({"failed": 0, "failures": [], "pending": 0, "total": 0})
    exact_cap = b'{"jobs":[]}' + b' ' * (65536 - len(b'{"jobs":[]}'))
    many = json.dumps({"jobs": [{"name": name, "status": "completed", "conclusion": name}
                                for name in ("success", "skipped", "neutral")]}).encode()
    for payload, expected, exit_code in [(exact_cap, empty, 0), (exact_cap + b' ', b"", 1),
                                         (many, assessment({"failed": 0, "failures": [], "pending": 0, "total": 3}), 0)]:
        result = run(ci, "source.opaal", stdin=payload)
        require(result.returncode == exit_code and result.stdout == expected
                and (not result.stderr if exit_code == 0 else b"IO002" in result.stderr),
                "CI exact cap/first excess/positive many differs")
    first_report = {"failed": 0, "failures": [], "pending": 0, "total": 1}
    first = command([sys.executable, "ci.py"], ci, environment)
    require(first.returncode == 0 and first.stdout == assessment(first_report) and not first.stderr,
            "documented first use differs")
    producer = ci / "producer.py"
    producer.write_text(producer.read_text().replace("sys.exit(0)", "sys.exit(7)"))
    sentinel = directory / "sentinel"
    sentinel.mkdir()
    (sentinel / "opaal").write_text("#!/bin/sh\nprintf launched > processor-launched\nexit 0\n")
    (sentinel / "opaal").chmod(0o755)
    failed = command([sys.executable, "ci.py"], ci, {"PATH": str(sentinel)})
    require(failed.returncode == 7 and not failed.stdout and failed.stderr == b"acquisition failed\n"
            and not (ci / "processor-launched").exists(), "failed acquisition launched processor or reused output")

    for sample, cases in [(work, [("processor", "Grüße\n".encode(), "Grüße\n".encode(), "success"),
                                ("invalid", b"\xff", b"", "error"),
                                ("excess", b"a" * 4097, b"", "error")]),
                          (ci, [("positive", b'{"jobs":[]}', assessment({"failed": 0, "failures": [], "pending": 0, "total": 0}), "success"),
                                ("negative", b'{"jobs":[{"name":"build","status":"queued"}]}', assessment({"failed": 0, "failures": [], "pending": 1, "total": 1}), "success"),
                                ("exact-cap", exact_cap, empty, "success"),
                                ("excess", exact_cap + b' ', b"", "error"),
                                ("invalid", b'{"jobs":null}', b"", "error")])]:
        base = ("--project", "opaal.toml", "--task", "sample", "--environment", "ci")
        checked = json.loads(success(run(sample, "check", *base, "--format", "json"), "project check"))
        require(checked["schema"] == "opaal.check.v3", "project check schema differs")
        success(run(sample, "plan", *base, "--expires-in", "900s", "--out", "plan.json"), "project plan")
        plan = json.loads((sample / "plan.json").read_bytes())
        require(plan["schema"] == "opaal.plan.v3", "plan schema differs")
        require(not list(sample.glob("*.jsonl")), "static operations created a journal")
        for index, (name, payload, expected, primary) in enumerate(cases, 1):
            run_id = str(index) * 32
            result = run(sample, "execute", "--plan", "plan.json", "--accept", plan["digest"],
                         "--run-id", run_id, "--journal", name + ".jsonl", "--receipt-out", name + ".receipt.json", stdin=payload)
            receipt = json.loads((sample / (name + ".receipt.json")).read_bytes())
            require(receipt["schema"] == "opaal.execution-receipt.v1" and receipt["schema_version"] == 1
                    and receipt["run_id"] == run_id and receipt["plan_digest"] == plan["digest"], "receipt identity differs")
            require(result.returncode == (0 if primary == "success" else 1) and result.stdout == expected
                    and result.stderr == (b"processed\n" if name == "processor" else b""), "controlled streams/exit differ")
            require(receipt["primary"]["class"] == primary and receipt["primary"]["value_digest"] is None
                    and receipt["secondary"] == [] and receipt["omitted_secondary_count"] == 0
                    and receipt["journal_state"] == "complete", "controlled outcome differs")
            success(run(sample, "audit", *base[:2], "--journal", name + ".jsonl", "--out", name + ".audit.json"), "audit")
            audit = json.loads((sample / (name + ".audit.json")).read_bytes())
            require(audit["schema"] == "opaal.audit.v3" and audit["completeness"] == "complete"
                    and audit["primary"] == receipt["primary"], "audit and receipt differ")
            for role, count in [("stdin.read", min(len(payload), 4097) if sample == work else len(payload)),
                                ("stdout.write", len(expected)), ("stderr.write", 10 if name == "processor" else 0)]:
                require(receipt["progress"][role]["confirmed_bytes"] == count
                        and receipt["progress"][role]["uncertain_bytes_upper_bound"] == 0, "controlled counts differ")
            if sample == ci and primary == "success":
                passed = json.loads(result.stdout)["passed"]
                require(type(passed) is bool and passed == json.loads(expected)["passed"], "domain policy differs")
    producer.write_text(producer.read_text().replace("sys.exit(7)", "sys.exit(0)"))
    for name, payload, exit_code, expected in [
        ("positive", b'{"jobs":[]}', 0, assessment({"failed": 0, "failures": [], "pending": 0, "total": 0})),
        ("negative", b'{"jobs":[{"name":"build","status":"queued"}]}', 1,
         assessment({"failed": 0, "failures": [], "pending": 1, "total": 1})),
        ("invalid", b'{"jobs":null}', 1, b""),
    ]:
        driver = directory / ("driver-" + name)
        shutil.copytree(ci, driver)
        (driver / "jobs.json").write_bytes(payload)
        result = command([sys.executable, "project-ci.py"], driver, environment)
        require(result.returncode == exit_code and result.stdout == expected
                and result.stderr == (b"project check failed; inspect the journal and receipt\n" if name == "invalid" else b""),
                "documented controlled CI exit mapping differs")
    return ["canonical source check/format", "exact processor UTF8/empty/cap/Broken Pipe",
            "all report policy and invalid input fixtures", "documented CI first use",
            "full bytes plus producer exit 7: zero processor launches",
            "controlled positive/negative/invalid receipts and audit", "exact role counts and exit mapping",
            "documented project CI success/negative/invalid first use",
            "retained interactive assessment/error recovery outside checkout",
            "nonterminal stdin ownership refusal and later source cell outside checkout"]


def qualify(archive: Path | None, fixtures: Path | None, binaries: Path | None, source: str, target: str) -> dict:
    require((platform.system(), platform.machine()) == {
        "linux-x86_64": ("Linux", "x86_64"), "macos-arm64": ("Darwin", "arm64"),
    }[target], "candidate target differs from running host")
    require(subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip() == source,
            "expected source differs from checkout head")
    require(bool(archive) == bool(fixtures), "archive mode requires exact fixture archive")
    snapshot = {name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
                for name in public_paths(ROOT) if (ROOT / name).is_file()}
    dirty = bool(subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT))
    if archive:
        require(not dirty, "archive qualification requires a clean committed checkout")
    check_copies(ROOT, ROOT / FIXTURES)
    with tempfile.TemporaryDirectory(prefix="opaal-stdio-") as temporary:
        directory = Path(temporary)
        if archive:
            programs, program_manifest = unpack(archive, directory, kind="binaries", version=VERSION, source=source, platform=target)
            inputs, fixture_manifest = unpack(fixtures, directory, kind="stdio-fixtures", version=VERSION, source=source, platform="portable")
            expected = {name.removeprefix(FIXTURES + "/"): value for name, value in snapshot.items() if name.startswith(FIXTURES + "/")}
            observed = {str(path.relative_to(inputs)): hashlib.sha256(path.read_bytes()).hexdigest() for path in inputs.rglob("*") if path.is_file()}
            require(observed == expected, "archived fixture inventory/bytes differ from source")
        else:
            require(binaries is not None, "working mode requires binary directory")
            programs = directory / "bin"
            programs.mkdir()
            for name in ("opaal", "opaal-language-server"):
                shutil.copy2(binaries / name, programs / name)
            inputs = ROOT / FIXTURES
            program_manifest = fixture_manifest = {"sha256": None}
        require(success(command([programs / "opaal", "--version"], directory, {"PATH": os.defpath}), "version")
                == f"opaal {VERSION}\n".encode(), "version differs")
        language_server(programs / "opaal-language-server", directory, {"PATH": os.defpath})
        assertions = references(programs / "opaal", inputs, directory, target)
        return {"schema_version": 1, "source": source, "committed_source": not dirty,
                "source_snapshot_sha256": hashlib.sha256(json.dumps(snapshot, sort_keys=True, separators=(",", ":")).encode()).hexdigest(),
                "version": VERSION, "platform": target, "archive_sha256": program_manifest["sha256"],
                "fixture_archive_sha256": fixture_manifest["sha256"],
                "binary_sha256": {name: hashlib.sha256((programs / name).read_bytes()).hexdigest() for name in ("opaal", "opaal-language-server")},
                "fixture_sha256": {name: value for name, value in snapshot.items() if name.startswith(FIXTURES + "/")},
                "assertions": assertions}


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
    result = qualify(args.archive.resolve() if args.archive else None, args.fixtures.resolve() if args.fixtures else None,
                     args.binary_directory.resolve() if args.binary_directory else None, args.expected_source, args.platform)
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print("Standard input/output candidate qualification passed.")


if __name__ == "__main__":
    main()
