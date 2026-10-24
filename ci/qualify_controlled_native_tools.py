#!/usr/bin/env python3
"""Assert the frozen repository reference corpus outside the source checkout."""
from __future__ import annotations

import argparse
import hashlib
import json
import platform
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path

if __package__:
    from .check_source_formatting import public_paths
    from .package_controlled_native_tools import check_cases, check_copies
    from .package_release import ROOT, VERSION, PLATFORMS
    from .qualify_data_processing import command, require, success, unpack
    from .qualify_docker_image_checks import measured, test_view
    from .qualify_native_preparation import qualify_preparation
    from .qualify_native_images import qualify_images
    from .qualify_native_evidence import qualify_evidence
else:
    from check_source_formatting import public_paths
    from package_controlled_native_tools import check_cases, check_copies
    from package_release import ROOT, VERSION, PLATFORMS
    from qualify_data_processing import command, require, success, unpack
    from qualify_docker_image_checks import measured, test_view
    from qualify_native_preparation import qualify_preparation
    from qualify_native_images import qualify_images
    from qualify_native_evidence import qualify_evidence

FIXTURES = "tests/golden/controlled-external-tools"
ERRORS = {"acquisition": "Checker acquisition failed.", "output": "Checker output is invalid.",
          "manifest": "Repository manifest is invalid.", "policy": "Repository policy input is invalid."}


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def payload(row, name):
    return bytes.fromhex(row[name + "_hex"]) if name + "_hex" in row else row[name].encode()


def corpus(inputs):
    reference = json.loads((inputs / "reference-cases.json").read_bytes())
    comparison = json.loads((inputs / "comparison-cases.json").read_bytes())
    require(reference["schema_version"] == comparison["schema_version"] == 1, "case schema differs")
    rows = reference["cases"]
    names = [row["name"] for row in rows]
    require(names and len(names) == len(set(names)) and names == comparison["reference_cases"],
            "reference case inventory differs")
    require(all(type(name) is str and re.fullmatch(r"[a-z][a-z0-9-]{0,63}", name) for name in names),
            "case name is not a safe evidence basename")
    require([row["name"] for row in rows if row.get("saved") is not None or "saved_hex" in row]
            == comparison["saved_cases"], "saved case inventory differs")
    require([row["name"] for row in rows if "code" in row] == comparison["live_cases"],
            "live case inventory differs")
    for row in rows:
        require(row["outcome"] in {"report", "saved", *ERRORS}, "unknown case outcome")
        require((row["outcome"] == "report") == bool(row["report"]), "case report contradicts outcome")
        if row["report"]:
            view = json.loads(row["report"])
            require(set(view) == {"passed", "manifest_ok", "findings"} and len(row["report"]) <= 256,
                    "expected report fields/bound differ")
            require(type(view["passed"]) is bool and type(view["manifest_ok"]) is bool
                    and type(view["findings"]) is int and 0 <= view["findings"] <= 256
                    and view["passed"] == (view["findings"] == 0 and view["manifest_ok"]),
                    "expected report values differ")
    return rows, comparison


def expected_exit(row):
    return int(not json.loads(row["report"])["passed"]) if row["report"] else 2


def assert_result(result, row, label, *, standalone=False, python=False, live=False):
    expected = expected_exit(row)
    require(result.returncode == (1 if standalone and expected == 2 else expected),
            f"{row['name']} {label}: exit differs: {result.returncode}")
    require(result.stdout == row["report"].encode(), f"{row['name']} {label}: report bytes differ")
    if expected == 2 and (python or standalone):
        message = ERRORS[row["outcome"]] if live else "Saved repository input failed."
        require(message.encode() in result.stderr, f"{row['name']} {label}: fixed error absent")
        if python:
            require(result.stderr == (message + "\n").encode(), f"{label}: Python diagnostic differs")
    require(b"SENSITIVE_" not in result.stderr + result.stdout
            and b"/home/private" not in result.stderr + result.stdout
            and b"\x1b[31m" not in result.stderr + result.stdout, f"{label}: raw payload leaked")


def accepted(binary, work, row, *, live=False, suffix="", stdin=None):
    name = row["name"] + suffix
    task, environment = ("repository", "ci") if live else ("saved", "offline")
    bindings = (["--input-file", "script=fixtures/check.sh", "--input-file",
                 "manifest=fixtures/repository.toml"] if live else [])
    planned = command([binary, "plan", "--project", "opaal.toml", "--task", task,
                       "--environment", environment, *bindings, "--expires-in", "900s",
                       "--out", name + ".plan.json", "--format", "digest"], work, {})
    digest = success(planned, "fresh " + name + " plan").decode().strip()
    result = command([binary, "task", "run", "--plan", name + ".plan.json", "--accept", digest,
                      "--journal", name + ".run.jsonl", "--receipt-out", name + ".receipt.json",
                      "--audit-out", name + ".audit.json", "--require-true"], work, {}, stdin=stdin)
    explanation = json.loads(success(command([
        binary, "task", "explain", "--receipt", name + ".receipt.json",
        "--audit", name + ".audit.json", "--format", "json"], work, {}), "explain " + name))
    require(explanation["schema"] == "opaal.task-explanation.v1", "explanation schema differs")
    if row["report"]:
        require(explanation["assessment"]["value"] == json.loads(row["report"])["passed"],
                "explanation lost Bool assessment")
        require(explanation["conclusion"] == ("passed" if expected_exit(row) == 0 else "negative"),
                "explanation confused complete execution and policy success")
    else:
        require(explanation["assessment"] is None and explanation["execution"] == "error"
                and explanation["primary_code"] == "EXECUTE_LANGUAGE", "error assessment differs")
    if live:
        journal = [json.loads(line) for line in (work / (name + ".run.jsonl")).read_bytes().splitlines()]
        attempts = [event for event in journal if event["kind"] == "effect-before"
                    and event["payload"]["effect"] == "process.run"]
        require(len(attempts) == 1, "native reference probed or retried the checker")
        operation = attempts[0]["payload"]["operation"]
        require(operation["argv_count"] == 7 and operation["argv"] == []
                and operation["argv_digest"] is None, "native argument metadata differs")
        require(all(event["schema_version"] == 5 for event in journal), "native journal schema differs")
    for extension in ("plan.json", "run.jsonl", "receipt.json", "audit.json"):
        require(b"SENSITIVE_" not in (work / (name + "." + extension)).read_bytes(),
                "raw payload leaked into retained metadata")
    return result


def prepare(binary, reference, parent, name, tool=None):
    work = parent / name
    args = [binary, "project", "init", "--example", "repository-check", "--destination", work,
            "--environment", "ci" if tool else "offline"]
    if tool:
        args += ["--tool", tool]
    success(command(args, parent, {}), "fresh " + name + " setup")
    for path in work.rglob("*"):
        if path.is_file() and path.name not in {"opaal.toml", "offline-tools.toml", "live-tools.toml"}:
            require(path.read_bytes() == (reference / path.relative_to(work)).read_bytes(),
                    "embedded reference differs: " + path.name)
    shutil.copyfile(reference / "comparison.py", work / "comparison.py")
    return work


def python_run(work, *, stdin=None, tool=None):
    args = [sys.executable, "comparison.py"]
    if tool:
        args += ["--tool", tool, "--script", "fixtures/check.sh", "--manifest", "fixtures/repository.toml"]
    return command(args, work, {}, stdin=stdin)


def policy_change(binary, work, row):
    policy, tests, comparison = [work / name for name in ("policy.opaal", "policy-tests.opaal", "comparison.py")]
    orchestration = {name: sha(work / name) for name in ("tasks.opaal", "saved-tasks.opaal", "source.opaal")}
    original = {path: path.read_bytes() for path in (policy, tests, comparison)}
    try:
        policy.write_bytes(original[policy].replace(b'license == "MIT"',
                                                   b'(license == "MIT" || license == "Apache-2.0")'))
        extra = ('\ndef test_apache() -> Bool\n{\n'
                 '    return policy::checked(0, { comments: [] }, '
                 '{ repository: { license: "Apache-2.0", private: false } }).passed\n}\n')
        tests.write_bytes(original[tests].replace(b"export { test_pass,", b"export { test_apache, test_pass,")
                          + extra.encode())
        comparison.write_bytes(original[comparison].replace(b'repository["license"] == "MIT"',
                                b'repository["license"] in ("MIT", "Apache-2.0")'))
        require(sha(policy) != hashlib.sha256(original[policy]).hexdigest(), "policy edit did not apply")
        view = test_view(success(command([binary, "test", "policy-tests.opaal", "--format", "json"],
                                         work, {}), "changed shared policy tests"))
        require(view["counts"]["passed"] == 7, "changed policy cases differ")
        changed = {**row, "name": "changed-apache-policy", "report":
                   '{"findings":0,"manifest_ok":true,"passed":true}'}
        source = payload(row, "saved")
        for label, result in (("Python", python_run(work, stdin=source)),
                              ("standalone", command([binary, "source.opaal"], work, {}, stdin=source)),
                              ("accepted", accepted(binary, work, changed, stdin=source))):
            assert_result(result, changed, "policy change " + label)
        require(orchestration == {name: sha(work / name) for name in orchestration},
                "policy change modified task/evidence orchestration")
        return {"pure_cases_passed": 7, "orchestration_sha256": orchestration,
                "policy_sha256": sha(policy), "tests_sha256": sha(tests),
                "comparison_sha256": sha(comparison)}
    finally:
        for path, content in original.items():
            path.write_bytes(content)


def repeated(binary, work, row, *, tool=None):
    samples = {"python": [], "opaal_plan_run_explain": []}
    source = None if tool else payload(row, "saved")
    for index in range(6):
        for name, run in (("python", lambda: python_run(work, stdin=source, tool=tool)),
                          ("opaal_plan_run_explain", lambda: accepted(
                              binary, work, row, live=bool(tool), suffix=f"-repeat{index}", stdin=source))):
            result, timing = measured(run)
            assert_result(result, row, name + " repeat", live=bool(tool))
            if index:
                samples[name].append(timing)
    return {name: {"runs": runs, "median": {key: statistics.median(run[key] for run in runs)
                                           for key in runs[0]}} for name, runs in samples.items()}


def qualify(binary, inputs, output, *, checker=None, fault_checker=None, preparation_test_binary=None,
            image_test_binary=None, evidence_test_binary=None, process_test_binary=None, cli_test_binary=None,
            posix_test_binary=None, lsp_test_binary=None):
    rows, inventory = corpus(inputs)
    pending_cases = check_cases(inputs)
    output.mkdir()
    # The worker must execute outside the candidate's build filesystem too.
    shutil.copyfile(binary, output / "opaal")
    binary = output / "opaal"
    binary.chmod(0o700)
    preparation = qualify_preparation(binary, inputs, output / "preparation", pending_cases["prepare"],
                                      test_binary=preparation_test_binary)
    images = qualify_images(binary, inputs, output / "images", pending_cases["image"],
                            test_binary=image_test_binary, process_test_binary=process_test_binary,
                            posix_test_binary=posix_test_binary, frontend_test_binary=preparation_test_binary)
    evidence = qualify_evidence(binary, inputs, output / "evidence", pending_cases["evidence"],
                                test_binary=evidence_test_binary, cli_test_binary=cli_test_binary,
                                finish_test_binary=preparation_test_binary, journal_test_binary=image_test_binary,
                                lsp_test_binary=lsp_test_binary)
    reference = inputs / "reference"
    offline, setup_timing = measured(lambda: prepare(binary, reference, output, "offline"))
    for path in sorted(offline.glob("*.opaal")):
        success(command([binary, "format", "--check", path], offline, {}), "format " + path.name)
        original = path.read_bytes()
        success(command([binary, "format", "--write", path], offline, {}), "format roundtrip " + path.name)
        require(original == path.read_bytes(), "canonical reference bytes changed")
    for name in ("policy.opaal", "policy-tests.opaal", "source.opaal"):
        success(command([binary, "check", name], offline, {}), "check " + name)
    interactive = command([binary], offline, {}, stdin=(
        "import './policy.opaal' as policy\n"
        'policy::checked(0, { comments: [] }, { repository: { license: "MIT", private: false } })\n'
        'policy::checked(1, { comments: [{ code: 2086, level: "info" }] }, { repository: { license: "MIT", private: false } })\n'
        'try { policy::checked(2, { comments: [] }, { repository: { license: "MIT", private: false } }) } catch error { error.message }\n'
        "exit\n").encode())
    interactive_bytes = success(interactive, "interactive pure policy")
    reports = [json.loads(line.lstrip(b"> ")) for line in interactive_bytes.splitlines()
               if line.lstrip(b"> ").startswith(b"{")]
    require(reports == [json.loads(rows[0]["report"]), json.loads(rows[1]["report"])]
            and b"Checker acquisition failed." in interactive_bytes, "interactive policy differs")
    (output / "interactive.stdout.txt").write_bytes(interactive_bytes)
    view = test_view(success(command([binary, "test", "policy-tests.opaal", "--format", "json"],
                                     offline, {}), "pure policy tests"))
    require(view["counts"]["passed"] == 6, "pure policy inventory differs")
    observed = {"saved": [], "synthetic_live": [], "real_checker": []}
    for row in rows:
        if row["name"] not in inventory["saved_cases"]:
            continue
        source = payload(row, "saved")
        for label, result in (("Python", python_run(offline, stdin=source)),
                              ("standalone", command([binary, "source.opaal"], offline, {}, stdin=source)),
                              ("accepted", accepted(binary, offline, row, stdin=source))):
            assert_result(result, row, label, standalone=label == "standalone", python=label == "Python")
        observed["saved"].append(row["name"])
    changed = policy_change(binary, offline, next(row for row in rows if row["name"] == "apache-license"))
    timings = {"offline_setup": setup_timing, "saved": repeated(binary, offline, rows[0])}
    if fault_checker or checker:
        require(platform.system() == "Linux" and platform.machine() in {"x86_64", "aarch64"}, "live host unsupported")
    if fault_checker:
        work = prepare(binary, reference, output, "synthetic", fault_checker)
        for row in rows:
            if row["name"] not in inventory["live_cases"]:
                continue
            (work / "checker.stdout").write_bytes(payload(row, "stdout"))
            (work / "checker.code").write_text(str(row["code"]))
            (work / "fixtures/repository.toml").write_bytes(payload(row, "manifest"))
            assert_result(python_run(work, tool=fault_checker), row, "Python live", python=True, live=True)
            assert_result(accepted(binary, work, row, live=True), row, "accepted live", live=True)
            observed["synthetic_live"].append(row["name"])
        require((work / "checker.launches").read_bytes() == b"launch\n" * (2 * len(observed["synthetic_live"])),
                "synthetic child attempt count differs")
    if checker:
        work, timings["live_setup"] = measured(lambda: prepare(binary, reference, output, "real-checker", checker))
        diagnosis, timings["live_diagnosis"] = measured(lambda: command([
            binary, "project", "doctor", "--project", "opaal.toml", "--task", "repository",
            "--environment", "ci", "--input-file", "script=fixtures/check.sh", "--input-file",
            "manifest=fixtures/repository.toml", "--format", "json"], work, {}))
        diagnosed = json.loads(diagnosis.stdout)
        require(diagnosis.returncode == 1 and not diagnosis.stderr
                and diagnosed["local_readiness"] == diagnosed["live_readiness"] == "unknown"
                and [finding["code"] for finding in diagnosed["findings"]] == ["DOCTOR017"],
                "native diagnosis differs")
        for name in inventory["real_checker_cases"]:
            row = next(row for row in rows if row["name"] == name)
            (work / "fixtures/repository.toml").write_bytes(payload(row, "manifest"))
            (work / "fixtures/check.sh").write_bytes(
                b"#!/bin/sh\nprintf '%s\\n' $1\n" if name == "findings"
                else (reference / "fixtures/check.sh").read_bytes())
            assert_result(python_run(work, tool=checker), row, "real Python", python=True, live=True)
            assert_result(accepted(binary, work, row, live=True), row, "real accepted", live=True)
            observed["real_checker"].append(name)
        (work / "fixtures/repository.toml").write_bytes((reference / "fixtures/repository.toml").read_bytes())
        timings["live"] = repeated(binary, work, rows[0], tool=checker)
    require(observed["saved"] == inventory["saved_cases"], "saved producer omitted cases")
    if fault_checker:
        require(observed["synthetic_live"] == inventory["live_cases"], "live producer omitted cases")
    if checker:
        require(observed["real_checker"] == inventory["real_checker_cases"], "real checker omitted cases")
    result = {"schema_version": 1, "qualification_scope": "reference-corpus",
              "qualifier_sha256": sha(Path(__file__)),
              "host": {"system": platform.system(), "machine": platform.machine(), "python": platform.python_version()},
              "cli_sha256": sha(binary), "inputs_sha256": {path.relative_to(inputs).as_posix(): sha(path)
                  for path in sorted(inputs.rglob("*")) if path.is_file()},
              "checker_sha256": sha(checker) if checker else None,
              "synthetic_checker_sha256": sha(fault_checker) if fault_checker else None,
              "case_inputs_sha256": {row["name"]: {
                  key: hashlib.sha256(payload(row, key)).hexdigest()
                  for key in ("saved", "stdout", "manifest")
                  if row.get(key) is not None or key + "_hex" in row} for row in rows},
              "cases": observed, "preparation_cases": preparation, "image_cases": images,
              "evidence_cases": evidence,
              "policy_change": changed, "measurements": timings,
              "workflow_costs": {
                  "setup": "Python needs the reference program and explicit tool/input paths; OPAAL additionally initializes manifest, lock and authority.",
                  "learning": "No independent user learning time measured; no adopter feedback claimed.",
                  "acceptance": "Every OPAAL run creates a fresh plan, requires review of its digest and fresh evidence paths. Measurements include command execution, not human review time.",
                  "policy_change": "Apache-2.0 is added in one shared policy and one pure test; all three callers retain identical orchestration bytes. Python changes its independent policy separately.",
                  "diagnosis": "Negative report separates findings from manifest policy. Errors have no report; accepted explanations retain safe codes, so inspect selected source/input separately.",
                  "remaining_glue": "Supply and qualify the executable, review pins/grants, choose fresh paths and reconcile unenforced child effects."},
              "measurement_limits": "RSS is the cumulative child high-water mark. OPAAL timing includes fresh plan, run, and explanation; Python has no acceptance/evidence system.",
              "pending_qualification_cases": {"prepare": preparation["pending"], "image": images["pending"],
                                              "evidence": evidence["pending"]},
              "omitted": ["remaining native image/lifetime fault execution", "remaining evidence fault execution",
                          "archive and full candidate qualification"] + (
                              ["injected preparation fault and asset-bound execution"] if preparation["pending"] else [])}
    (output / "native-tools-comparison.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    return result


def qualify_archive(archive, fixtures, output, source, target, **options):
    require(fixtures is not None and source is not None and target in PLATFORMS,
            "archive mode requires --fixtures, --expected-source and --platform")
    require((platform.system(), platform.machine()) == {
        "linux-x86_64": ("Linux", "x86_64"), "linux-aarch64": ("Linux", "aarch64"),
        "macos-arm64": ("Darwin", "arm64")}[target],
        "candidate target differs from running host")
    require(subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip() == source,
            "expected source differs from checkout head")
    require(not subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT),
            "archive qualification requires a clean committed checkout")
    require(not output.is_relative_to(ROOT), "reference execution must be outside the checkout")
    require(not output.exists(), "artifact directory must be exclusive")
    check_copies(ROOT)
    snapshot = {name: sha(ROOT / name) for name in public_paths(ROOT) if (ROOT / name).is_file()}
    expected = {name.removeprefix(FIXTURES + "/"): value for name, value in snapshot.items()
                if name.startswith(FIXTURES + "/")}
    with tempfile.TemporaryDirectory(prefix="opaal-native-archives-") as temporary:
        directory = Path(temporary).resolve()
        require(not directory.is_relative_to(ROOT), "archive extraction must be outside the checkout")
        programs, pm = unpack(archive, directory, kind="binaries", version=VERSION, source=source, platform=target)
        inputs, fm = unpack(fixtures, directory, kind="native-fixtures", version=VERSION,
                            source=source, platform="portable")
        require({path.relative_to(inputs).as_posix(): sha(path)
                 for path in inputs.rglob("*") if path.is_file()} == expected,
                "archived fixture inventory/bytes differ from source")
        binary = programs / "opaal"
        require(success(command([binary, "--version"], directory, {}), "version")
                == f"opaal {VERSION}\n".encode(), "binary version differs")
        result = qualify(binary, inputs, output, **options)
        result.update({"qualification_scope": "archive-reference-corpus", "source": source,
                       "committed_source": True, "version": VERSION, "platform": target,
                       "source_snapshot_sha256": hashlib.sha256(
                           json.dumps(snapshot, sort_keys=True).encode()).hexdigest(),
                       "archive_sha256": pm["sha256"], "fixture_archive_sha256": fm["sha256"],
                       "binary_sha256": {name: sha(programs / name)
                                         for name in ("opaal", "opaal-language-server")}})
        result["omitted"] = [item for item in result["omitted"] if item != "archive and full candidate qualification"]
        result["omitted"].append("full candidate qualification")
        (output / "native-tools-comparison.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    programs = parser.add_mutually_exclusive_group(required=True)
    programs.add_argument("--cli", type=Path)
    programs.add_argument("--archive", type=Path)
    parser.add_argument("--fixtures", type=Path, help="fixture directory with --cli, exact archive with --archive")
    parser.add_argument("--expected-source")
    parser.add_argument("--platform", choices=PLATFORMS)
    parser.add_argument("--output", type=Path, required=True, help="absent directory outside the checkout")
    parser.add_argument("--checker", type=Path, help="explicit independently qualified ShellCheck image")
    parser.add_argument("--fault-checker", type=Path, help="compiled checker.c synthetic observer")
    parser.add_argument("--preparation-test-binary", type=Path,
                        help="candidate opaal-cli library test executable for preparation faults and bounded image-source admission")
    parser.add_argument("--image-test-binary", type=Path,
                        help="candidate opaal-runtime library test executable for structural image API observations")
    parser.add_argument("--evidence-test-binary", type=Path,
                        help="candidate opaal-runtime workflow_artifacts test executable for artifact API observations")
    parser.add_argument("--process-test-binary", type=Path,
                        help="candidate opaal-runtime controlled_native_tools test executable for request/budget/deadline API observations")
    parser.add_argument("--cli-test-binary", type=Path,
                        help="candidate opaal-cli controlled_native_tools test executable for accepted redaction/input observations")
    parser.add_argument("--posix-test-binary", type=Path,
                        help="candidate opaal-platform-posix controlled_native_tools test executable; Linux requires strace")
    parser.add_argument("--lsp-test-binary", type=Path,
                        help="candidate opaal-lsp server_e2e test executable for passive native project requests")
    args = parser.parse_args()
    require(not args.output.resolve().is_relative_to(ROOT), "reference execution must be outside the checkout")
    options = dict(
        checker=args.checker.resolve() if args.checker else None,
        fault_checker=args.fault_checker.resolve() if args.fault_checker else None,
        preparation_test_binary=args.preparation_test_binary.resolve() if args.preparation_test_binary else None,
        image_test_binary=args.image_test_binary.resolve() if args.image_test_binary else None,
        evidence_test_binary=args.evidence_test_binary.resolve() if args.evidence_test_binary else None,
        process_test_binary=args.process_test_binary.resolve() if args.process_test_binary else None,
        cli_test_binary=args.cli_test_binary.resolve() if args.cli_test_binary else None,
        posix_test_binary=args.posix_test_binary.resolve() if args.posix_test_binary else None,
        lsp_test_binary=args.lsp_test_binary.resolve() if args.lsp_test_binary else None)
    if args.archive:
        qualify_archive(args.archive.resolve(), args.fixtures.resolve() if args.fixtures else None,
                        args.output.resolve(), args.expected_source, args.platform, **options)
    else:
        require(args.expected_source is None and args.platform is None, "source/platform require archive mode")
        qualify(args.cli.resolve(), (args.fixtures or ROOT / FIXTURES).resolve(), args.output.resolve(), **options)
    print("Repository reference cases and independent comparison passed.")


if __name__ == "__main__":
    main()
