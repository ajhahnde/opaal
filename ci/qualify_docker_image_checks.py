#!/usr/bin/env python3
"""Assert the packaged saved-image or explicitly provisioned Linux reference."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import runpy
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

if __package__:
    from .check_source_formatting import public_paths
    from .package_docker_image_checks import FIXTURES, check_copies
    from .package_release import ROOT, VERSION, PLATFORMS
    from .qualify_data_processing import command, require, success, unpack
else:
    from check_source_formatting import public_paths
    from package_docker_image_checks import FIXTURES, check_copies
    from package_release import ROOT, VERSION, PLATFORMS
    from qualify_data_processing import command, require, success, unpack


def decoder_payload(document: dict, case: dict) -> bytes:
    if "hex" in case:
        return bytes.fromhex(case["hex"])
    value = json.loads(json.dumps(document))
    def assign(path, selected):
        parent = value
        for key in path[:-1]:
            parent = parent[key]
        parent[path[-1]] = selected
    for change in case.get("changes", []):
        assign(change["path"], change["value"])
    for path in case.get("remove", []):
        parent = value
        for key in path[:-1]:
            parent = parent[key]
        del parent[path[-1]]
    if "repeat" in case:
        repeat = case["repeat"]
        selected = ([repeat["value"]] * repeat["count"] if repeat.get("array") else
                    repeat["value"] * repeat["count"] + repeat.get("tail", ""))
        assign(repeat["path"], selected)
    if "depth" in case:
        nested = 0
        for _ in range(2, case["depth"]):
            nested = [nested]
        value["unknown"] = nested
    encode = lambda: json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()
    if "padding_bytes" in case:
        value["padding"] = ""
        value["padding"] = "a" * (case["padding_bytes"] - len(encode()))
    return encode()


def cases(document: dict) -> list[tuple[str, bytes, tuple[str, str, str], int, dict | None]]:
    """Named input/report cases; every result is asserted by native task execution."""
    expected = (document["Id"], "linux/amd64", "abc123")
    result = []
    def add(name, value, expectations=expected, error=False, input_bytes=None):
        payload = input_bytes if input_bytes is not None else (
            value if isinstance(value, bytes) else json.dumps(value, ensure_ascii=False).encode())
        report = None
        if not error:
            revision = (value.get("Config") or {}).get("Labels") or {}
            revision = revision.get("org.opencontainers.image.revision")
            actual = (value["Id"], value["Os"] + "/" + value["Architecture"] +
                      ("/" + value["Variant"] if value.get("Variant") else ""), revision or "")
            checks = [{"name": field, "passed": observed == wanted and (index != 2 or observed != ""),
                       "expected": wanted, "actual": observed}
                      for index, (field, observed, wanted) in enumerate(zip(
                          ("image-id", "platform", "revision"), actual, expectations))]
            checks[2]["missing"] = revision is None
            report = {"passed": all(check["passed"] for check in checks), "checks": checks}
        result.append((name, payload, expectations, 2 if error else int(not report["passed"]), report))
    add("single-oci", document)
    docker = json.loads(json.dumps(document))
    docker["Descriptor"]["mediaType"] = "application/vnd.docker.distribution.manifest.v2+json"
    add("single-docker", docker)
    for index, name in enumerate(("negative-id", "negative-platform", "negative-revision")):
        changed = list(expected)
        changed[index] = "wrong"
        add(name, document, tuple(changed))
        changed[index] = ""
        add("empty-expectation-" + name.removeprefix("negative-"), document, tuple(changed), True)
    for name, config in (("missing-revision", None), ("null-revision", {"Labels": {
            "org.opencontainers.image.revision": None}}), ("empty-revision", {"Labels": {
            "org.opencontainers.image.revision": ""}}), ("unicode-revision", {"Labels": {
            "org.opencontainers.image.revision": "café\n\u001b[31m"}})):
        changed = json.loads(json.dumps(document))
        changed["Config"] = config
        add(name, changed)
    changed = json.loads(json.dumps(document))
    changed["Variant"] = "v8"
    add("variant", changed, (expected[0], "linux/amd64/v8", expected[2]))
    changed = json.loads(json.dumps(document))
    changed["ignored"] = {"diagnostic": "raw-metadata-canary"}
    changed["Config"]["Labels"]["unknown"] = "unknown-label-canary"
    add("ignored-canaries", changed)
    for name, descriptor in (("descriptorless", None), ("null-descriptor", None),
                             ("index", {"mediaType": "application/vnd.oci.image.index.v1+json", "digest": expected[0]}),
                             ("manifest-list", {"mediaType": "application/vnd.docker.distribution.manifest.list.v2+json", "digest": expected[0]}),
                             ("descriptor-mismatch", {"mediaType": document["Descriptor"]["mediaType"], "digest": "sha256:" + "b" * 64})):
        changed = json.loads(json.dumps(document))
        if name == "descriptorless":
            del changed["Descriptor"]
        else:
            changed["Descriptor"] = descriptor
        add(name, changed, error=True)
    for name, payload in (("invalid-selected", b'{"Id":false}'), ("duplicate", b'{"Id":1,"Id":2}'),
                          ("invalid-utf8", b'{"label":"\xff"}'), ("root-array", b'[]'),
                          ("multiple-objects", b'{}{}'), ("malformed", b'{')):
        add(name, payload, error=True)
    corpus = json.loads((ROOT / FIXTURES / "decode/cases.json").read_bytes())
    require(corpus["schema_version"] == 1 and corpus["cases"], "decoder corpus schema/cases differ")
    for case in corpus["cases"]:
        require(case["expect"] in {"image", "error"}, "unknown decoder expectation")
        payload = decoder_payload(document, case)
        add("decode-" + case["name"], payload if case["expect"] == "error" else json.loads(payload),
            error=case["expect"] == "error", input_bytes=payload)
    return result


def accepted_run(binary, work, environment, task, selected_environment, expected, *, image=None, stdin=None):
    bindings = []
    for name, value in zip(("expected_id", "expected_platform", "expected_revision"), expected):
        bindings += ["--input", name + "=" + value]
    if image is not None:
        bindings += ["--input", "image=" + image]
    planned = command([binary, "plan", "--project", "opaal.toml", "--task", task,
                       "--environment", selected_environment, *bindings, "--expires-in", "900s",
                       "--out", task + ".plan.json", "--format", "digest"], work, environment)
    if planned.returncode:
        return planned
    accepted = planned.stdout.decode().strip()
    return command([binary, "task", "run", "--plan", task + ".plan.json", "--accept", accepted,
                    "--journal", task + ".run.jsonl", "--receipt-out", task + ".receipt.json",
                    "--audit-out", task + ".audit.json", "--require-true"], work, environment, stdin=stdin)


def references(binary: Path, inputs: Path, directory: Path, target: str) -> list[dict]:
    template = directory / "reference"
    environment = {"PATH": str(binary.parent) + os.pathsep + os.defpath}
    success(command([binary, "project", "init", "--example", "image-check", "--destination",
                     template, "--environment", "offline"], directory, environment), "native offline setup")
    require('required_opaal = ">=1.5.0,<2.0.0"' in (template / "opaal.toml").read_text(),
            "reference version floor differs")
    for path in template.rglob("*"):
        if path.is_file() and path.name not in {"opaal.toml", "offline-tools.toml"}:
            require(path.read_bytes() == (inputs / path.relative_to(template)).read_bytes(),
                    "embedded asset differs from packaged input")
    # Independent lifecycle/comparison drivers remain qualification inputs.
    for name in ("comparison.py", "run-comparison.sh"):
        shutil.copyfile(inputs / name, template / name)
    selected_lock = "offline-tools-linux.toml" if target == "linux-x86_64" else "offline-tools-macos.toml"
    require((template / "offline-tools.toml").read_bytes() == (inputs / selected_lock).read_bytes(),
            "embedded offline host lock differs")
    require((template / "opaal.toml").read_bytes() == (inputs / "offline.toml").read_bytes(),
            "embedded manifest differs")
    diagnosed = command([binary, "project", "doctor", "--project", "opaal.toml", "--task", "saved",
                         "--environment", "offline", "--input", "expected_id=x", "--input",
                         "expected_platform=x", "--input", "expected_revision=x", "--format", "json"],
                        template, environment)
    view = json.loads(success(diagnosed, "native offline diagnosis"))
    require(view["schema"] == "opaal.project-doctor.v1" and view["schema_version"] == 1
            and view["local_readiness"] == "ready" and view["live_readiness"] == "not-applicable"
            and not view["findings"], "offline diagnosis differs")
    for path in sorted(template.glob("*.opaal")):
        success(command([binary, "format", "--check", path], template, environment), "canonical " + path.name)
        if path.name not in {"tasks.opaal", "saved-tasks.opaal"}:
            success(command([binary, "check", path], template, environment), "check " + path.name)
    discovered = json.loads(success(command([binary, "task", "list", "--project", "opaal.toml",
                                           "--format", "json"], template, environment), "task list"))
    require(discovered["schema"] == "opaal.project-inspection.v1", "discovery schema differs")
    inspected = success(command([binary, "task", "inspect", "--project", "opaal.toml", "saved",
                                 "--format", "json"], template, environment), "task inspect")
    require(b'check_saved' in inspected and b'Bool' in inspected and b'stdin.read' in inspected,
            "typed task metadata differs")
    document = json.loads((template / "fixtures/image.json").read_bytes())
    interactive = command([binary], template, environment, stdin=(
        "import './policy.opaal' as policy\nimport std::docker as docker\n"
        "let image = docker::decode_image(" + json.dumps(document) + ")\n"
        "policy::assess(image, '" + document["Id"] + "', 'linux/amd64', 'abc123')\n"
        "docker::decode_image({})\n"
        "policy::assess(image, '" + document["Id"] + "', 'linux/amd64', 'wrong')\nexit\n"
    ).encode())
    (directory / "interactive.stdout.txt").write_bytes(interactive.stdout)
    (directory / "interactive.stderr.txt").write_bytes(interactive.stderr)
    records = [json.loads(line.lstrip(b"> ")) for line in interactive.stdout.splitlines()
               if line.lstrip(b"> ").startswith(b'{')]
    reports = {name: report for name, _, _, _, report in cases(document)}
    require(interactive.returncode == 0 and records == [reports["single-oci"], reports["negative-revision"]]
            and interactive.stderr.count(b"error[") == 1,
            "interactive policy and recoverable decoder error differ")
    rows = []
    for name, payload, expected, exit_code, report in cases(document):
        work = directory / name
        shutil.copytree(template, work)
        (work / "input.json").write_bytes(payload)
        before = directory / (name + "-legacy")
        shutil.copytree(template, before)
        legacy_driver = ROOT / "tests/fixtures/image-check-legacy/run-saved.sh"
        legacy = command(["sh", legacy_driver, *expected], before, environment, stdin=payload)
        output = accepted_run(binary, work, environment, "saved", "offline", expected, stdin=payload)
        require((legacy.returncode, legacy.stdout) == (output.returncode, output.stdout),
                name + ": original driver and native run differ")
        (work / "stdout.json").write_bytes(output.stdout)
        (work / "stderr.txt").write_bytes(output.stderr)
        require(output.returncode == exit_code, f"{name}: selected exit differs: {output.stderr!r}")
        if report is None:
            require(not output.stdout, name + ": failed input produced a report")
        else:
            require(json.loads(output.stdout) == report, name + ": report differs")
            if not report["passed"]:
                require(b"Image check failed:" in output.stderr, name + ": negative diagnostic missing")
            require(b"\x1b" not in output.stdout + output.stderr, name + ": unescaped terminal control")
        for canary in (b"raw-metadata-canary", b"unknown-label-canary", b"credential-canary"):
            require(canary not in output.stdout + output.stderr, name + ": ignored metadata leaked")
        comparison = command(["sh", "run-comparison.sh", *expected], work, environment, stdin=payload)
        (work / "comparison.stdout.json").write_bytes(comparison.stdout)
        (work / "comparison.stderr.txt").write_bytes(comparison.stderr)
        require(comparison.returncode == exit_code
                and (json.loads(comparison.stdout) if comparison.stdout else None) == report,
                name + ": independent Python/shell report or exit differs")
        require(all(canary not in comparison.stdout + comparison.stderr for canary in
                    (b"raw-metadata-canary", b"unknown-label-canary", b"credential-canary")),
                name + ": comparison leaked ignored metadata")
        receipt = json.loads((work / "saved.receipt.json").read_bytes())
        audit = json.loads((work / "saved.audit.json").read_bytes())
        plan = json.loads((work / "saved.plan.json").read_bytes())
        explained = command([binary, "task", "explain", "--receipt", "saved.receipt.json",
                             "--audit", "saved.audit.json", "--format", "json"], work, environment)
        view = json.loads(success(explained, name + ": native explanation"))
        require(view["schema"] == "opaal.task-explanation.v1" and view["schema_version"] == 1
                and view["run_id"] == receipt["run_id"] and view["plan_digest"] == plan["digest"]
                and view["conclusion"] == ("failed" if report is None else "passed" if report["passed"] else "negative")
                and view["partial"] == receipt["primary"]["partial"] and view["progress"] == receipt["progress"],
                name + ": explanation differs")
        (work / "explanation.json").write_bytes(explained.stdout)
        candidate = None if report is None else {"kind": "bool", "value": report["passed"]}
        require(receipt["schema"] == "opaal.execution-receipt.v2" and audit["schema"] == "opaal.audit.v4"
                and plan["schema"] == "opaal.plan.v4", name + ": artifact schema differs")
        require(receipt["plan_digest"] == plan["digest"] and receipt["run_id"] == audit["run_id"]
                and receipt["primary"] == audit["primary"] and receipt["assessment"] == audit["assessment"] == candidate,
                name + ": outcome bindings differ")
        require(receipt["primary"]["class"] == ("error" if report is None else "success")
                and receipt["secondary"] == [] and receipt["omitted_secondary_count"] == 0
                and receipt["journal_state"] == audit["completeness"] == "complete",
                name + ": completion differs")
        require(receipt["progress"]["stdout.write"]["confirmed_bytes"] == len(output.stdout),
                name + ": stdout counts differ")
        evidence = b"".join((work / path).read_bytes() for path in
                            ("saved.run.jsonl", "saved.receipt.json", "saved.audit.json"))
        require(all(canary not in evidence for canary in
                    (b"raw-metadata-canary", b"unknown-label-canary", b"credential-canary")),
                name + ": raw metadata in evidence")
        started = time.monotonic()
        ordinary = command([binary, "source.opaal"], work, environment, stdin=payload)
        elapsed = time.monotonic() - started
        (work / "standalone.stdout.json").write_bytes(ordinary.stdout)
        (work / "standalone.stderr.txt").write_bytes(ordinary.stderr)
        (work / "standalone.result.json").write_text(json.dumps({
            "exit": ordinary.returncode, "elapsed_seconds": elapsed}, sort_keys=True) + "\n")
        if report is not None:
            source_report = json.loads(json.dumps(report))
            for index, wanted in enumerate((document["Id"], "linux/amd64", "abc123")):
                check = source_report["checks"][index]
                check["expected"] = wanted
                check["passed"] = check["actual"] == wanted and (index != 2 or check["actual"] != "")
            source_report["passed"] = all(check["passed"] for check in source_report["checks"])
            require(ordinary.returncode == int(not source_report["passed"])
                    and json.loads(ordinary.stdout) == source_report and not ordinary.stderr,
                    name + ": standalone report differs")
        elif name.startswith("empty-expectation-"):
            require(ordinary.returncode == 0 and json.loads(ordinary.stdout) == cases(document)[0][4]
                    and not ordinary.stderr, name + ": fixed standalone expectations differ")
        else:
            require(ordinary.returncode == 1 and not ordinary.stdout, name + ": standalone error differs")
        rows.append({"case": name, "exit": exit_code, "assessment": candidate,
                     "comparison_exit": comparison.returncode,
                     "artifacts": {path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                                   for path in work.iterdir() if path.suffix in {".json", ".jsonl", ".txt"}}})
    modified = directory / "modified-policy"
    shutil.copytree(template, modified)
    policy = modified / "policy.opaal"
    policy.write_text(policy.read_text().replace("let revision_ok = (revision !=", "let revision_ok = (revision =="))
    result = accepted_run(binary, modified, environment, "saved", "offline",
                          [document["Id"], "linux/amd64", "abc123"], stdin=json.dumps(document).encode())
    require(result.returncode == 1 and json.loads(result.stdout)["passed"] is False,
            "independent policy modification did not change assessment")
    comparison_policy = modified / "comparison.py"
    comparison_policy.write_text(comparison_policy.read_text().replace(
        'index != 2 or actual != ""', 'index != 2 or actual == ""'))
    comparison = command(["sh", "run-comparison.sh", document["Id"], "linux/amd64", "abc123"],
                         modified, environment, stdin=json.dumps(document).encode())
    require(comparison.returncode == 1 and json.loads(comparison.stdout) == json.loads(result.stdout),
            "equivalent independent policy modification differs")
    (modified / "comparison.stdout.json").write_bytes(comparison.stdout)
    (modified / "comparison.stderr.txt").write_bytes(comparison.stderr)
    stale = accepted_run(binary, modified, environment, "saved", "offline",
                         [document["Id"], "linux/amd64", "abc123"], stdin=json.dumps(document).encode())
    require(stale.returncode != 0, "existing plan was overwritten")
    return rows


def live_report_comparison(work: Path, live: dict, reference: str, expected: list[str],
                           environment: dict[str, str]) -> tuple[subprocess.CompletedProcess, dict]:
    """Independently acquire trusted fixture data; this is not a lifetime proof."""
    decoder = runpy.run_path(str(work / "comparison.py"))
    child_environment = {"LC_ALL": "C", "TZ": "UTC", "DOCKER_API_VERSION": "1.48",
                         "PATH": str(work / "empty-path"), "HOME": str(work / "sandbox-home"),
                         "TMPDIR": str(work / "sandbox-tmp")}
    prefix = [live["docker"], "--config", work / "docker-empty", "--host", "unix://" + str(live["socket"])]
    version = ["version", "--format", "{{json .}}"]
    vectors = [["--version"], version,
               ["image", "inspect", "--format", "{{json .}}", "--", reference], version]
    started = time.monotonic()
    stages = []
    before = None
    image = None
    # shortcut: capture is checked after exit for the reviewed CLI, use streaming for hostile-child resource comparisons.
    try:
        for stage, vector in enumerate(vectors):
            remaining = 60 - (time.monotonic() - started)
            if remaining <= 0:
                raise ValueError("acquisition deadline")
            observation = {"stage": stage, "exit": None, "stdout_bytes": None, "stderr_bytes": None}
            stages.append(observation)
            result = subprocess.run([str(arg) for arg in prefix + vector], cwd=work,
                                    env=child_environment, stdin=subprocess.DEVNULL,
                                    capture_output=True, timeout=min(30, remaining), check=False)
            observation.update(exit=result.returncode, stdout_bytes=len(result.stdout),
                               stderr_bytes=len(result.stderr))
            if result.returncode or len(result.stdout) > 1_048_576 or len(result.stderr) > 65_536:
                raise ValueError("failed child")
            if time.monotonic() - started >= 60:
                raise ValueError("acquisition deadline")
            if stage == 0:
                if not re.fullmatch(rb"Docker version 29\.9\.0, build [!-~]{1,512}\n", result.stdout):
                    raise ValueError("client profile")
            else:
                document = json.loads(result.stdout.decode("utf-8"),
                                      object_pairs_hook=decoder["unique_object"],
                                      parse_int=decoder["integer"], parse_float=decoder["number"],
                                      parse_constant=decoder["number"])
                decoder["depth"](document)
                if not isinstance(document, dict) or len(json.dumps(document, ensure_ascii=False,
                        separators=(",", ":"), allow_nan=False).encode("utf-8")) > 1_048_576:
                    raise ValueError("JSON profile")
                if stage == 2:
                    image = result.stdout
                    continue
                selected = [document[side][key] for side, keys in (
                    ("Client", ("Version", "ApiVersion", "Os", "Arch")),
                    ("Server", ("Version", "ApiVersion", "MinAPIVersion", "Os", "Arch")))
                            for key in keys]
                if selected[:5] != ["29.9.0", "1.48", "linux", "amd64", "29.9.0"] or selected[7:] != ["linux", "amd64"]:
                    raise ValueError("server profile")
                def api(text):
                    if not isinstance(text, str) or not re.fullmatch(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", text):
                        raise ValueError("API profile")
                    parts = tuple(map(int, text.split(".")))
                    if any(part > 4_294_967_295 for part in parts):
                        raise ValueError("API profile")
                    return parts
                if not api(selected[6]) <= (1, 48) <= api(selected[5]) or (before is not None and before != selected):
                    raise ValueError("API profile changed")
                before = selected
        result = command(["sh", "run-comparison.sh", *expected], work, environment, stdin=image)
    except KeyboardInterrupt:
        result = subprocess.CompletedProcess([], 130, b"", b"Image check cancelled.\n")
    except (ValueError, TypeError, KeyError, RecursionError, OSError, subprocess.TimeoutExpired):
        result = subprocess.CompletedProcess([], 2, b"", b"Image check failed: acquisition failure.\n")
    return result, {"stages": stages, "wall_seconds": time.monotonic() - started,
                    "limits": "trusted fixture comparison; no target binding, bounded capture or group-lifetime proof"}


def live_references(binary: Path, inputs: Path, directory: Path, live: dict) -> list[dict]:
    require((platform.system(), platform.machine()) == ("Linux", "x86_64"),
            "live qualification requires Linux x86_64")
    expected = (live["id"], "linux/amd64", live["revision"])
    require(all(expected), "live expectations require nonempty fields")
    report = {"passed": True, "checks": [
        {"name": name, "passed": True, "expected": value, "actual": value}
        for name, value in zip(("image-id", "platform", "revision"), expected)]}
    report["checks"][2]["missing"] = False
    environment = {"PATH": str(binary.parent) + os.pathsep + os.defpath}
    inventory = json.loads((inputs / "live/cases.json").read_bytes())
    require(inventory["schema_version"] == 1 and inventory["cases"], "live case inventory differs")
    rows = []
    names = set()
    for case in inventory["cases"]:
        name = case["name"]
        require(name not in names, "duplicate live case")
        names.add(name)
        work = directory / name
        setup = command([binary, "project", "init", "--example", "image-check", "--destination", work,
                         "--environment", "ci", "--docker", live["docker"], "--socket", live["socket"],
                         "--qualified-version", "29.9.0"], directory, environment)
        success(setup, name + ": fresh native setup")
        for driver in ("comparison.py", "run-comparison.sh"):
            shutil.copyfile(inputs / driver, work / driver)
        (work / "setup.stdout.txt").write_bytes(setup.stdout)
        (work / "setup.stderr.txt").write_bytes(setup.stderr)
        wanted = list(expected)
        reference = live["image"]
        selected = json.loads(json.dumps(report))
        if "mismatch" in case:
            index = case["mismatch"]
            wanted[index] = "wrong"
            selected["checks"][index]["expected"] = "wrong"
            selected["checks"][index]["passed"] = selected["passed"] = False
        if "ref" in case:
            reference = live["index"] if case["ref"] == "index" else case["ref"]
            selected = None
        for filename, value in zip(("expected-image-ref.txt", "expected-image-id.txt",
                                    "expected-platform.txt", "expected-revision.txt"),
                                   (reference, *wanted)):
            (work / filename).write_text(value + "\n")
        output = accepted_run(binary, work, environment, "image", "ci", wanted, image=reference)
        (work / "stdout.json").write_bytes(output.stdout)
        (work / "stderr.txt").write_bytes(output.stderr)
        require(output.returncode == case["exit"], name + ": selected exit differs")
        require((json.loads(output.stdout) if output.stdout else None) == selected,
                name + ": report differs")
        comparison, comparison_acquisition = live_report_comparison(work, live, reference, wanted, environment)
        (work / "comparison.stdout.json").write_bytes(comparison.stdout)
        (work / "comparison.stderr.txt").write_bytes(comparison.stderr)
        require(comparison.returncode == case["exit"]
                and (json.loads(comparison.stdout) if comparison.stdout else None) == selected,
                name + ": independent live acquisition report or exit differs")
        receipt = json.loads((work / "image.receipt.json").read_bytes())
        audit = json.loads((work / "image.audit.json").read_bytes())
        plan = json.loads((work / "image.plan.json").read_bytes())
        assessment = None if selected is None else {"kind": "bool", "value": selected["passed"]}
        require(receipt["schema"] == "opaal.execution-receipt.v2"
                and audit["schema"] == "opaal.audit.v4" and plan["schema"] == "opaal.plan.v4",
                name + ": artifact schema differs")
        require(receipt["assessment"] == audit["assessment"] == assessment
                and receipt["primary"] == audit["primary"]
                and receipt["plan_digest"] == plan["digest"]
                and receipt["run_id"] == audit["run_id"], name + ": outcome binding differs")
        require(receipt["primary"]["class"] == ("error" if selected is None else "success")
                and receipt["secondary"] == [] and receipt["omitted_secondary_count"] == 0
                and receipt["journal_state"] == audit["completeness"] == "complete",
                name + ": completion differs")
        for path in ("docker-empty", "empty-path", "sandbox-home", "sandbox-tmp"):
            require(not any((work / path).iterdir()), name + ": safe directory changed")
        observed = output.stdout + output.stderr + comparison.stdout + comparison.stderr + b"".join((work / path).read_bytes() for path in
                    ("image.run.jsonl", "image.receipt.json", "image.audit.json"))
        require(all(canary not in observed for canary in (b"OPAAL_UNKNOWN_LABEL_CANARY",
                b"credential-canary", b"raw-metadata-canary")), name + ": raw metadata leaked")
        rows.append({"case": name, "exit": output.returncode, "assessment": assessment,
                     "comparison_exit": comparison.returncode,
                     "comparison_acquisition": comparison_acquisition,
                     "artifacts": {path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                                   for path in work.iterdir() if path.suffix in {".json", ".jsonl", ".txt"}}})
    return rows


def qualify(archive, fixtures, binaries, source, target, artifacts, live=None):
    require((platform.system(), platform.machine()) == {
        "linux-x86_64": ("Linux", "x86_64"), "macos-arm64": ("Darwin", "arm64")}[target],
        "candidate target differs from running host")
    require(subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip() == source,
            "expected source differs from checkout head")
    require(bool(archive) == bool(fixtures), "archive mode requires exact fixture archive")
    require(not artifacts.exists(), "artifact directory must be exclusive")
    snapshot = {name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
                for name in public_paths(ROOT) if (ROOT / name).is_file()}
    dirty = bool(subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT))
    check_copies(ROOT)
    with tempfile.TemporaryDirectory(prefix="opaal-image-") as temporary:
        directory = Path(temporary).resolve()
        if archive:
            require(not dirty, "archive qualification requires a clean committed checkout")
            programs, pm = unpack(archive, directory, kind="binaries", version=VERSION, source=source, platform=target)
            inputs, fm = unpack(fixtures, directory, kind="docker-fixtures", version=VERSION, source=source, platform="portable")
            expected = {name.removeprefix(FIXTURES + "/"): value for name, value in snapshot.items()
                        if name.startswith(FIXTURES + "/")}
            require({str(path.relative_to(inputs)): hashlib.sha256(path.read_bytes()).hexdigest()
                     for path in inputs.rglob("*") if path.is_file()} == expected, "archived fixtures differ")
        else:
            require(binaries is not None, "working mode requires binary directory")
            programs = directory / "bin"
            programs.mkdir()
            shutil.copy2(binaries / "opaal", programs / "opaal")
            inputs = ROOT / FIXTURES
            pm = fm = {"sha256": None}
        binary = programs / "opaal"
        require(success(command([binary, "--version"], directory, {"PATH": os.defpath}), "version")
                == f"opaal {VERSION}\n".encode(), "binary version differs")
        try:
            rows = (live_references(binary, inputs, directory, live) if live else
                    references(binary, inputs, directory, target))
        finally:
            shutil.copytree(directory, artifacts)
        return {"schema_version": 1, "source": source, "committed_source": not dirty,
                "source_snapshot_sha256": hashlib.sha256(json.dumps(snapshot, sort_keys=True).encode()).hexdigest(),
                "version": VERSION, "platform": target, "mode": "live" if live else "offline",
                "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                "archive_sha256": pm["sha256"], "fixture_archive_sha256": fm["sha256"],
                "fixture_sha256": {name: value for name, value in snapshot.items() if name.startswith(FIXTURES + "/")},
                "live_target": ({"docker_sha256": hashlib.sha256(live["docker"].read_bytes()).hexdigest(),
                                 "profile_sha256": hashlib.sha256((inputs / "docker-cli-profile.json").read_bytes()).hexdigest(),
                                 "image_id": live["id"]} if live else None),
                "cases": rows, "assertions": (["outside-checkout live first use", "exact report/exit/evidence",
                                               "safe directories remain empty", "ignored metadata redacted",
                                               "independent trusted-CLI report and exit parity"] if live else
                                             ["outside-checkout first use", "independent policy modification",
                                              "exclusive evidence paths", "standalone/task report parity",
                                              "independent Python/shell report and exit parity",
                                              "equivalent independent policy modification",
                                              "interactive policy and decoder error recovery"])}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    programs = parser.add_mutually_exclusive_group(required=True)
    programs.add_argument("--archive", type=Path)
    programs.add_argument("--binary-directory", type=Path)
    parser.add_argument("--fixtures", type=Path)
    modes = parser.add_mutually_exclusive_group(required=True)
    modes.add_argument("--offline", action="store_true")
    modes.add_argument("--live", action="store_true")
    parser.add_argument("--docker", type=Path)
    parser.add_argument("--socket", type=Path)
    parser.add_argument("--image-ref")
    parser.add_argument("--image-id")
    parser.add_argument("--image-revision")
    parser.add_argument("--index-ref")
    parser.add_argument("--expected-source", required=True)
    parser.add_argument("--platform", required=True, choices=PLATFORMS)
    parser.add_argument("--report", required=True, type=Path)
    args = parser.parse_args()
    live = None
    if args.live:
        require(all((args.docker, args.socket, args.image_ref, args.image_id, args.image_revision, args.index_ref)),
                "live qualification requires explicit tool/socket/single-image expectations/index ref")
        require(args.docker.is_absolute() and args.socket.is_absolute(), "live paths must be absolute")
        live = {"docker": args.docker, "socket": args.socket, "image": args.image_ref,
                "id": args.image_id, "revision": args.image_revision, "index": args.index_ref}
    args.report.parent.mkdir(parents=True, exist_ok=True)
    require(not args.report.exists(), "report must be exclusive")
    result = qualify(args.archive.resolve() if args.archive else None,
                     args.fixtures.resolve() if args.fixtures else None,
                     args.binary_directory.resolve() if args.binary_directory else None,
                     args.expected_source, args.platform, args.report.with_suffix(".artifacts"), live)
    args.report.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print("Docker image qualification passed.")


if __name__ == "__main__":
    main()
