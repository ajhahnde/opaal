#!/usr/bin/env python3
"""Replay source-formatting CLI, framed LSP and project fixtures outside a checkout."""
from __future__ import annotations

import argparse
import copy
import json
import os
import platform
import pty
import re
import select
import shutil
import signal
import subprocess
import tempfile
import time
from pathlib import Path

if __package__:
    from .check_source_formatting import public_paths, seed_census
    from .package_data_processing import SOURCE, digest
    from .package_release import PLATFORMS, ROOT, VERSION
    from .package_source_formatting import FIXTURES, MEMBER_IMPORT_FIXTURES, COMPOUND_ASSIGNMENT_FIXTURES, LIST_LITERAL_SPREAD_FIXTURES, RECORD_SHORTHAND_FIXTURES, GROUPED_EXPORT_FIXTURES
    from .qualify_data_processing import require, success, unpack
else:
    from check_source_formatting import public_paths, seed_census
    from package_data_processing import SOURCE, digest
    from package_release import PLATFORMS, ROOT, VERSION
    from package_source_formatting import FIXTURES, MEMBER_IMPORT_FIXTURES, COMPOUND_ASSIGNMENT_FIXTURES, LIST_LITERAL_SPREAD_FIXTURES, RECORD_SHORTHAND_FIXTURES, GROUPED_EXPORT_FIXTURES
    from qualify_data_processing import require, success, unpack

PAIRS = (("legacy.opaal", "canonical.opaal"), ("legacy-effects.opaal", "effects.opaal"),
         ("legacy-annotated.opaal", "annotated.opaal"),
         ("legacy-function-comments.opaal", "function-comments.opaal"))


def normalized_artifact(value: dict) -> dict:
    """Ignore only receipt/time and source-content fields, preserving every other field."""
    value = copy.deepcopy(value)
    value.pop("digest")
    value["task"].pop("contract_digest")
    for source in value["sources"]:
        source.pop("digest")
        source.pop("size")
    if value["schema"] in ("opaal.plan.v2", "opaal.plan.v3", "opaal.plan.v4"):
        value.pop("created_at")
        value.pop("expires_at")
        for action in value["actions"]:
            action.pop("contract_digest")
            action["id"] = action["id"].split("#", 1)[1]
        for observation in value["observations"]:
            if observation["kind"] == "source":
                observation.pop("digest")
                observation.pop("size")
            elif observation["kind"] == "wall-clock":
                observation["observed_at"] = None
    return value


def project_workflow(run, work: Path, fixtures: Path, target: str, *, member_imports: bool = False,
                     compound_assignments: bool = False, list_spreads: bool = False, record_shorthands: bool = False, grouped_exports: bool = False) -> dict:
    shutil.copytree(fixtures / "project", work)
    lock = "tools-macos.toml" if target == "macos-arm64" else "tools-linux-aarch64.toml" if target == "linux-aarch64" else "tools-linux.toml"
    shutil.copyfile(work / lock, work / "tools.toml")
    base = ("--project", "opaal.toml", "--task", "verify", "--environment", "ci")
    original = {path.name: path.read_bytes() for path in work.glob("*.toml")}

    def verify_contract(value: dict, name: str, effects: bytes = b"") -> None:
        path = work / "tasks.opaal"
        expected_paths = [path] + ([work / "value.opaal"] if member_imports and name == "readiness" else [])
        expected_sources = {str(source): {"digest": "sha256:" + digest(source.read_bytes()), "size": source.stat().st_size} for source in expected_paths}
        require({source["module"]: {"digest": source["digest"], "size": source["size"]} for source in value["sources"]} == expected_sources, "project source census/digests differ")
        graph_bytes = b"".join(os.fsencode(source) + b"\0" + source.read_bytes() + b"\xff" for source in sorted(expected_paths))
        if len(expected_paths) > 1:
            graph_bytes += b"std::math\0\xff"
        graph = digest(graph_bytes)
        expected = "sha256:" + digest(os.fsencode(path) + b"\0" + name.encode() + b"->Int" +
                                      effects + b"\0" + graph.encode())
        require(value["task"]["contract_digest"] == expected, "source-derived action contract differs")
        for action in value.get("actions", []):
            require(action["contract_digest"] == expected and action["id"] == expected + "#000000" and
                    action["dependencies"] == [], "source-derived plan node differs")

    def plan(label: str, *, refused: bool = False) -> dict:
        result = run("plan", *base, "--expires-in", "900s", "--out", f"{label}.plan.json")
        require(result.returncode == (1 if refused else 0) and
                (result.stderr == b"opaal: CHECK005: request `clock.wall` `evaluation` has no exact grant\n"
                 if refused else not result.stderr),
                f"{label}: plan status differs")
        receipt = re.fullmatch(rb"plan (sha256:[0-9a-f]{64})( refused)?\n", result.stdout)
        require(receipt is not None and bool(receipt[2]) == refused, "plan receipt differs")
        value = json.loads((work / f"{label}.plan.json").read_bytes())
        verify_contract(value, "annotated" if refused else "readiness", b"\0clock.wall()" if refused else b"")
        require(receipt[1].decode() == value["digest"], "receipt does not name the sealed plan")
        success(run("plan", "inspect", f"{label}.plan.json"), "plan inspection")
        return value

    def execute(label: str, value: dict):
        return run("execute", "--plan", f"{label}.plan.json", "--accept", value["digest"],
                   "--journal", f"{label}.run.jsonl")

    def audit(label: str, value: dict) -> dict:
        success(execute(label, value), f"{label}: execution")
        success(run("audit", "--project", "opaal.toml", "--journal", f"{label}.run.jsonl",
                    "--out", f"{label}.audit.json"), "audit")
        success(run("audit", "inspect", f"{label}.audit.json"), "audit inspection")
        result = json.loads((work / f"{label}.audit.json").read_bytes())
        require(result["completeness"] == "complete" and result["cleanup"] == [] and
                result["primary"]["class"] == "success" and result["primary"]["partial"] is False and
                result["primary"]["value_digest"] == "sha256:" + digest(b"5"),
                "project did not return Int 5 successfully")
        require(result["plan_digest"] == result["accepted_plan_digest"] == value["digest"],
                "audit does not bind the accepted receipt")
        require([event["kind"] for event in result["events"]] ==
                ["action-start", "action-end", "terminal"], "no-effect task produced effect events")
        for event in result["events"][:2]:
            require(event["payload"]["action_node_id"] == value["task"]["contract_digest"] + "#000000",
                    "audit action node differs from plan")
            event["payload"].pop("action_node_id")
        require(result["events"][0]["payload"].pop("contract_digest") == value["task"]["contract_digest"],
                "audit contract differs from plan")
        return {"primary": result["primary"], "cleanup": result["cleanup"],
                "events": [{"kind": event["kind"], "payload": event["payload"]}
                           for event in result["events"] if event["kind"] != "terminal"]}

    source = work / "tasks.opaal"
    canonical = source.read_bytes()
    if grouped_exports:
        canonical = (fixtures / "grouped-export-prefixes/project-tasks.opaal").read_bytes()
        source.write_bytes(canonical.replace(b"read*", b"read *", 1))
        refused_check = run("check", "tasks.opaal")
        refused_test = run("test", "tasks.opaal", "--format", "json")
        require(refused_check.returncode == 1 and b"PROJECT001" in refused_check.stderr and
                refused_test.returncode == 2 and json.loads(refused_test.stdout)["failure"] == "TEST_SOURCE",
                "prefix task standalone admission changed")
    elif member_imports:
        canonical = (fixtures / "explicit-member-imports/project-tasks.opaal").read_bytes()
        source.write_bytes(canonical.replace(b"import './value.opaal'::{result}", b"import './value.opaal'::{ result }", 1))
        for name in ("value.opaal", "policy-tests.opaal", "policy-negative.opaal"):
            shutil.copyfile(fixtures / "explicit-member-imports" / name, work / name)
        refused_check = run("check", "tasks.opaal")
        refused_test = run("test", "tasks.opaal", "--format", "json")
        require(refused_check.returncode == 1 and b"PROJECT001" in refused_check.stderr and refused_test.returncode == 2 and json.loads(refused_test.stdout)["failure"] == "TEST_SOURCE", "standalone task admission changed")
        success(run("check", "policy-tests.opaal"), "member standalone policy check")
        for case, code, passed, failed in (("test_value", 0, 1, 0), ("test_negative", 1, 0, 1)):
            result = run("test", "policy-tests.opaal" if passed else "policy-negative.opaal", "--case", case, "--format", "json")
            value = json.loads(result.stdout)
            require(result.returncode == code and value["counts"]["selected"] == 1 and value["counts"]["passed"] == passed and value["counts"]["failed"] == failed and value["completeness"] == "complete", "member project named test differs")
    elif compound_assignments or list_spreads or record_shorthands:
        feature = "record-shorthand" if record_shorthands else "list-literal-spread" if list_spreads else "compound-assignment"
        canonical = (fixtures / feature / "project-tasks.opaal").read_bytes()
        source.write_bytes(canonical.replace(b"def result() -> Int\n{", b"def result() -> Int {", 1) if record_shorthands else
                           canonical.replace(b"...defaults", b"... defaults", 1) if list_spreads else
                           canonical.replace(b"total += 5", b"total+=5", 1))
        for name in ("policy-tests.opaal", "policy-negative.opaal"):
            shutil.copyfile(fixtures / feature / name, work / name)
        success(run("check", "policy-tests.opaal"), "standalone policy check")
        for case, name, code, passed, failed in (
            ("test_value", "policy-tests.opaal", 0, 1, 0),
            ("test_negative", "policy-negative.opaal", 1, 0, 1),
        ):
            result = run("test", name, "--case", case, "--format", "json")
            value = json.loads(result.stdout)
            require(result.returncode == code and value["counts"]["selected"] == 1 and
                    value["counts"]["passed"] == passed and value["counts"]["failed"] == failed and
                    value["counts"]["error"] == value["counts"]["unrun"] == 0 and
                    value["completeness"] == "complete", "named policy test differs")
    else:
        source.write_bytes((work / "legacy-tasks.opaal").read_bytes())
    legacy_inspection = success(run("task", "inspect", "--project", "opaal.toml", "verify"), "inspect legacy")
    require(b"signature readiness() -> Int\n" in legacy_inspection and
            b"effects\ntools\n" in legacy_inspection, "task result/effects differ")
    success(run("check", *base), "legacy project check")
    legacy_check = json.loads(success(run("check", *base, "--format", "json"), "legacy JSON check"))
    verify_contract(legacy_check, "readiness")
    require(legacy_check["findings"] == [] and legacy_check["authority"]["requests"] == [],
            "no-effect project check acquired findings or requests")
    legacy = plan("legacy")
    require(legacy["tools"] == legacy["secrets"] == legacy["authority"]["requests"] == [],
            "no-effect project acquired authority")
    legacy_audit = audit("legacy", legacy)
    stale = plan("stale")
    require(not (work / "stale.run.jsonl").exists(), "stale journal already exists")
    success(run("format", "--write", "--", "tasks.opaal"), "project migration")
    require(source.read_bytes() == canonical, "project migration bytes differ")
    refused = execute("stale", stale)
    require(refused.returncode == 1 and not refused.stdout and b"EXECUTE_STALE" in refused.stderr and
            not (work / "stale.run.jsonl").exists(), "rewritten source accepted a stale plan")
    success(run("format", "--check", "--", "tasks.opaal"), "project canonical check")
    inspected = success(run("task", "inspect", "--project", "opaal.toml", "verify"), "inspect canonical")
    require(re.sub(rb"contract sha256:[0-9a-f]{64}", b"contract", inspected) ==
            re.sub(rb"contract sha256:[0-9a-f]{64}", b"contract", legacy_inspection),
            "task signature, effects or documentation changed with layout")
    success(run("check", *base), "canonical project check")
    checked = json.loads(success(run("check", *base, "--format", "json"), "canonical JSON check"))
    canonical_check_digest = checked["digest"]
    verify_contract(checked, "readiness")
    fresh = plan("fresh")
    require(legacy["sources"][0]["digest"] != fresh["sources"][0]["digest"] and
            stale["digest"] != fresh["digest"], "source/receipt digests did not change")
    require(normalized_artifact(legacy_check) == normalized_artifact(checked) and
            normalized_artifact(legacy) == normalized_artifact(fresh), "project semantics changed with layout")
    require(legacy_audit == audit("fresh", fresh), "execution/audit semantics changed with layout")

    if grouped_exports:
        expanded = plan("expanded-stale")
        source.write_bytes(canonical + b"let read_added = 1\n")
        success(run("check", *base), "prefix addition project check")
        # Remove only the project task alias to inspect the same declarations as a file API.
        probe = work / "export-probe.opaal"
        probe.write_bytes(source.read_bytes().replace(b"task verify = readiness\n", b""))
        observed = run(stdin=b"import './export-probe.opaal' as probe\nprobe::read_added\n")
        require(observed.returncode == 0 and not observed.stderr and b"1\n" in observed.stdout,
                "new prefix match absent from concrete API")
        refused = execute("expanded-stale", expanded)
        require(refused.returncode == 1 and b"EXECUTE_STALE" in refused.stderr and
                not (work / "expanded-stale.run.jsonl").exists(), "prefix addition accepted a stale plan")
        source.write_bytes(canonical)

    # Declaring an effect is sufficient to require authority even when its body is pure.
    if compound_assignments or list_spreads or record_shorthands or grouped_exports:
        source.write_bytes(canonical.replace(b"readiness", b"annotated").replace(b"read*", b"annotated*").replace(
            b"effects {\n}", b"effects {\n clock.wall();\n}"))
    else:
        source.write_bytes((fixtures / "legacy-annotated.opaal").read_bytes() + b"task verify = annotated\n")
    denied_checks, denied_plans = [], []
    for label in ("denied-legacy", "denied-canonical"):
        result = run("check", *base, "--format", "json")
        require(result.returncode == 1 and not result.stderr, "declared effect was not refused")
        checked = json.loads(result.stdout)
        verify_contract(checked, "annotated", b"\0clock.wall()")
        require(checked["outcome"]["class"] == "refused" and
                [row["effect"] for row in checked["authority"]["requests"]] == ["clock.wall"] and
                [row["verdict"] for row in checked["authority"]["requests"]] == ["denied"],
                "declared effect acquired authority")
        denied_checks.append(normalized_artifact(checked))
        value = plan(label, refused=True)
        denied_plans.append(normalized_artifact(value))
        result = execute(label, value)
        require(result.returncode == 1 and not (work / f"{label}.run.jsonl").exists(),
                "refused effect plan executed or created a journal")
        success(run("format", "--write", "--", "tasks.opaal"), "effect declaration migration")
    require(denied_checks[0] == denied_checks[1] and denied_plans[0] == denied_plans[1],
            "effect refusal semantics changed with layout")
    require(all((work / name).read_bytes() == data for name, data in original.items()),
            "formatting changed a declarative project file")
    return {"legacy_check": legacy_check["digest"], "canonical_check": canonical_check_digest,
            "legacy_plan": legacy["digest"], "stale_plan": stale["digest"], "fresh_plan": fresh["digest"],
            "task_value_digest": legacy_audit["primary"]["value_digest"],
            "assertions": ["same task signature/effects with verified source-derived contract regeneration",
                           "same check/plan fields except source-derived digests/sizes and receipt times",
                           "Int 5 and identical primary/action audit payloads", "empty tools/secrets/effects",
                           "stale receipt refuses without journal", "fresh receipt succeeds",
                           "clock.wall declaration and refusal preserved", "TOML bytes preserved"]}


def concise_imports(run, work: Path, fixtures: Path) -> dict:
    destination = work / "concise-imports"
    shutil.copytree(fixtures / "concise-imports", destination)

    def invoke(*arguments):
        return run(*arguments, cwd=destination)

    cases = {}
    for name in ("before.opaal", "after.opaal"):
        success(invoke("check", name), "import check")
        success(invoke("format", "--check", name, "helpers.opaal"), "import canonical bytes")
        listed = json.loads(success(invoke("test", "--list", name, "--format", "json"), "import list"))
        tested = json.loads(success(invoke("test", name, "--format", "json"), "import tests"))
        require(listed["counts"]["discovered"] == 1 and listed["cases"][0]["name"] == "test_imports" and
                tested["counts"]["passed"] == 1 and tested["completeness"] == "complete", "import pure results differ")
        require(success(invoke(name), "import script") == b"", "pure imports printed implicitly")
        cases[name] = {"listed": listed["counts"], "tested": tested["counts"]}
    require(cases["before.opaal"] == cases["after.opaal"], "grouped test results differ")
    for name in ("standalone.opaal", "conflict-recovery.opaal"):
        success(invoke("check", name), "inferred check")
        success(invoke("format", "--check", name), "inferred canonical bytes")
        require(success(invoke(name), "inferred script") == b"", "inferred script printed")
    for name, diagnostic in (("invalid-name.opaal", b"as alias"), ("conflict.opaal", b"MOD011")):
        result = invoke("check", name)
        require(result.returncode == 1 and diagnostic in result.stderr, "invalid import diagnostic differs")
    shutil.copyfile(destination / "helpers.opaal", destination / "build-tools.opaal")
    (destination / "recovered.opaal").write_text("import './build-tools.opaal' as tools\nlet result = tools::twice(3)\n")
    success(invoke("check", "recovered.opaal"), "explicit local alias recovery")
    require(success(invoke("recovered.opaal"), "local alias script") == b"", "recovered script printed")
    success(invoke("format", "--write", "noncanonical.opaal"), "group format")
    require((destination / "noncanonical.opaal").read_bytes() == (destination / "canonical.opaal").read_bytes(),
            "group canonical bytes differ")
    success(invoke("format", "--check", "noncanonical.opaal"), "group idempotence")
    pending = run(cwd=destination, stdin=(destination / "incomplete.opaal").read_bytes())
    require(pending.returncode == 0 and b"SYN002" in pending.stderr, "import group EOF differs")

    project = work / "concise-project"
    success(run("project", "init", "--example", "image-check", "--destination", project,
                "--environment", "offline"), "offline import project")
    source = project / "saved-tasks.opaal"
    old_imports = ("import './policy.opaal' as policy\nimport std::docker as docker\n"
                   "import std::data as data\nimport std::io as io\n")
    original = source.read_text()
    require(original.startswith(old_imports), "offline example imports changed")
    baseline_imports = old_imports + "import project::context as context\nimport project::tools as locked\n"
    grouped_imports = ("import {\n    './policy.opaal',\n    std::docker,\n    std::data,\n"
                       "    std::io,\n    project::context,\n    project::tools as locked,\n}\n")
    checks, inspections, listings = [], [], []
    for imports in (baseline_imports, grouped_imports):
        source.write_text(original.replace(old_imports, imports, 1))
        success(run("check", "policy-tests.opaal", cwd=project), "offline policy check")
        success(run("test", "policy-tests.opaal", cwd=project), "offline policy tests")
        success(run("format", "--check", "saved-tasks.opaal", cwd=project), "offline canonical bytes")
        listings.append(json.loads(success(run("task", "list", "--project", "opaal.toml", "--format", "json",
                                              cwd=project), "offline task list")))
        inspections.append(json.loads(success(run("task", "inspect", "--project", "opaal.toml", "saved",
                                                  "--format", "json", cwd=project), "offline task inspect")))
        checks.append(json.loads(success(run("check", "--project", "opaal.toml", "--task", "saved",
            "--environment", "offline", "--input", "expected_id=sha256:" + "a" * 64,
            "--input", "expected_platform=linux/amd64", "--input", "expected_revision=abc123",
            "--format", "json", cwd=project), "offline project check")))
    require(checks[0]["digest"] != checks[1]["digest"] and
            checks[0]["sources"] != checks[1]["sources"], "source rewrite did not change identities")
    require(normalized_artifact(checks[0]) == normalized_artifact(checks[1]), "project contract semantics changed")
    for observations in (inspections, listings):
        for observation in observations:
            for task in observation["tasks"]:
                task.pop("contract_digest")
        require(observations[0] == observations[1], "project task metadata changed")
    return {"pure_cases": cases, "project_checks": checks,
            "assertions": ["literal aliases and explicit recovery", "canonical groups and incomplete EOF",
                           "offline task identity/effects preserved; source identities changed"]}


def explicit_member_imports(run, work: Path, fixtures: Path, target: str) -> dict:
    destination = work / "explicit-member-imports"
    original = fixtures / "explicit-member-imports"
    require({path.name for path in original.iterdir()} == set(MEMBER_IMPORT_FIXTURES), "member fixture census differs")
    shutil.copytree(original, destination)

    def invoke(*arguments):
        return run(*arguments, cwd=destination)

    cases = {}
    for name in ("before.opaal", "after.opaal", "alternate.opaal", "local-main.opaal", "pipeline.opaal"):
        success(invoke("check", name), "member source check")
        success(invoke("format", "--check", name), "member canonical bytes")
        result = json.loads(success(invoke("test", name, "--case", "test_value", "--format", "json"), "member pure test"))
        require(result["counts"]["selected"] == result["counts"]["passed"] == 1 and result["completeness"] == "complete", "member pure result differs")
        require(success(invoke(name), "member script") == b"", "member source printed implicitly")
        cases[name] = result["counts"]
    require(cases["before.opaal"] == cases["after.opaal"] == cases["alternate.opaal"], "member spelling changed results")
    require(success(invoke("io-allowed.opaal"), "member I/O") == b"ready\n", "member I/O bytes differ")
    denied = invoke("test", "io-pure.opaal", "--format", "json")
    value = json.loads(denied.stdout)
    require(denied.returncode == 2 and value["counts"]["passed"] == 0 and value["counts"]["unrun"] == 1 and b"TEST_EFFECT" in denied.stdout and b"must-not-print" not in denied.stdout + denied.stderr, "selected I/O acquired pure authority")
    for name, code in (("missing.opaal", b"MOD014"), ("private.opaal", b"MOD014"), ("duplicate.opaal", b"MOD011")):
        result = invoke("check", name)
        require(result.returncode == 1 and not result.stdout and code in result.stderr, "member diagnostic differs")
    success(invoke("format", "--write", "noncanonical.opaal"), "member format")
    require((destination / "noncanonical.opaal").read_bytes() == (destination / "canonical.opaal").read_bytes(), "member canonical bytes differ")
    success(invoke("format", "--check", "noncanonical.opaal"), "member idempotence")
    pending = run(cwd=destination, stdin=(destination / "incomplete.opaal").read_bytes())
    require(pending.returncode == 0 and b"SYN002" in pending.stderr, "member incomplete EOF differs")
    raw = run(cwd=destination, stdin=b"import std::math::{\nabs as magnitude_of,\n}\nmagnitude_of(-7)\n")
    require(raw.returncode == 0 and not raw.stderr and b"7\n" in raw.stdout, "member raw continuation or retention differs")
    project = work / "member-project"
    project_report = project_workflow(lambda *args: run(*args, cwd=project), project, fixtures, target, member_imports=True)
    return {"fixture_cases": sorted(MEMBER_IMPORT_FIXTURES), "pure_cases": cases, "project": project_report,
            "assertions": ["before/after/alternate Int 7 and original facade identity", "allowed I/O and pure refusal", "missing/private/duplicate diagnostics", "canonical comments and raw multiline retention"]}


def compound_assignment(run, work: Path, fixtures: Path, target: str) -> dict:
    original = fixtures / "compound-assignment"
    destination = work / "compound-assignment"
    require({path.name for path in original.iterdir()} == set(COMPOUND_ASSIGNMENT_FIXTURES),
            "compound fixture census differs")
    shutil.copytree(original, destination)

    def invoke(*arguments, **options):
        return run(*arguments, cwd=destination, **options)

    cases = {}
    for name in ("before.opaal", "after.opaal", "alternate.opaal", "failure.opaal", "policy-tests.opaal"):
        success(invoke("check", name), "compound source check")
        success(invoke("format", "--check", name), "compound canonical bytes")
        value = json.loads(success(invoke("test", name, "--case", "test_value", "--format", "json"),
                                   "compound pure test"))
        require(value["counts"]["selected"] == value["counts"]["passed"] == 1 and
                value["completeness"] == "complete", "compound pure result differs")
        require(success(invoke(name), "compound script") == b"", "compound source printed implicitly")
        cases[name] = value["counts"]
    require(success(invoke("io-allowed.opaal", stdin=b"23"), "compound once-only RHS") == b"30\n",
            "compound RHS order/count/result differs")
    denied = invoke("test", "io-pure.opaal", "--format", "json")
    value = json.loads(denied.stdout)
    require(denied.returncode == 2 and value["counts"]["passed"] == 0 and
            value["counts"]["unrun"] == 1 and b"TEST_EFFECT" in denied.stdout and
            b"private-sentinel" not in denied.stdout + denied.stderr, "compound RHS acquired pure authority")
    for name, code in (("wrong-type.opaal", b"SIG008"), ("immutable.opaal", b"BND001"),
                       ("captured.opaal", b"BND003")):
        result = invoke("check", name)
        require(result.returncode == 1 and not result.stdout and code in result.stderr,
                "compound diagnostic differs")
    success(invoke("format", "--write", "noncanonical.opaal"), "compound format")
    require((destination / "noncanonical.opaal").read_bytes() == (destination / "canonical.opaal").read_bytes(),
            "compound canonical bytes differ")
    success(invoke("format", "--check", "noncanonical.opaal"), "compound idempotence")
    pending = run(cwd=destination, stdin=(destination / "incomplete.opaal").read_bytes())
    require(pending.returncode == 0 and b"SYN002" in pending.stderr, "compound incomplete EOF differs")
    raw = run(cwd=destination, stdin=b"mut total = 7\ntotal +=\n2\ntotal\n")
    require(raw.returncode == 0 and not raw.stderr and b"...> " in raw.stdout and b"9\n" in raw.stdout,
            "compound raw continuation or retained value differs")
    project = work / "compound-project"
    report = project_workflow(lambda *args: run(*args, cwd=project), project, fixtures, target,
                              compound_assignments=True)
    return {"fixture_cases": sorted(COMPOUND_ASSIGNMENT_FIXTURES), "pure_cases": cases, "project": report,
            "assertions": ["five operators; before/after Int 2; negative division/remainder; failed store",
                           "once-only RHS and pure authority refusal", "type/immutable/captured diagnostics",
                           "canonical comments; raw continuation and retained value"]}


def list_literal_spread(run, work: Path, fixtures: Path, target: str) -> dict:
    original = fixtures / "list-literal-spread"
    destination = work / "list-literal-spread"
    require({path.name for path in original.iterdir()} == set(LIST_LITERAL_SPREAD_FIXTURES),
            "list literal spread fixture census differs")
    shutil.copytree(original, destination)

    def invoke(*arguments, **options):
        return run(*arguments, cwd=destination, **options)

    cases = {}
    for name, expected in (("before.opaal", b"[1, 2, 3]"), ("after.opaal", b"[1, 2, 3]"),
                           ("alternate.opaal", b"[1, 2, 3, [4]]")):
        success(invoke("check", name), "list source check")
        success(invoke("format", "--check", name), "list canonical bytes")
        require(success(invoke(name), "list script") == b"", "list script printed implicitly")
        observed = invoke(stdin=(destination / name).read_bytes())
        require(observed.returncode == 0 and not observed.stderr and expected + b"\n" in observed.stdout,
                "list before/after value differs")
        cases[name] = expected.decode()
    invalid = invoke("format", "--check", "invalid.opaal")
    require(invalid.returncode == 1 and not invalid.stdout and b"OP1000" in invalid.stderr,
            "invalid list spread grammar differs")
    success(invoke("check", "failure.opaal"), "dynamic spread check")
    for name, code in (("failure.opaal", b"RUN001"), ("known-non-list.opaal", b"SIG020")):
        failed = invoke(name) if name == "failure.opaal" else invoke("check", name)
        require(failed.returncode == 1 and not failed.stdout and code in failed.stderr,
                "list spread failure class differs")
    require(success(invoke("io-allowed.opaal"), "list once-only ordered operands") == b"first\nmiddle\nlast\n",
            "list operand order/count differs")
    failed = invoke("io-failure.opaal")
    require(failed.returncode == 1 and failed.stdout == b"first\n" and b"never" not in failed.stdout,
            "failed spread evaluated a later operand or lost earlier output")
    denied = invoke("test", "io-pure.opaal", "--format", "json")
    result = json.loads(denied.stdout)
    require(denied.returncode == 2 and result["counts"]["passed"] == 0 and
            result["counts"]["unrun"] == 1 and b"TEST_EFFECT" in denied.stdout and
            b"private-sentinel" not in denied.stdout + denied.stderr, "spread acquired pure authority")
    success(invoke("format", "--write", "noncanonical.opaal"), "list format")
    require((destination / "noncanonical.opaal").read_bytes() == (destination / "canonical.opaal").read_bytes(),
            "list canonical bytes differ")
    success(invoke("format", "--check", "noncanonical.opaal"), "list idempotence")
    pending = invoke(stdin=(destination / "incomplete.opaal").read_bytes())
    require(pending.returncode == 0 and b"SYN002" in pending.stderr, "list incomplete EOF differs")
    raw = invoke(stdin=b"let items = [1, 2]\nlet result = [\n...items,\n3]\nresult\n")
    require(raw.returncode == 0 and not raw.stderr and b"...> " in raw.stdout and
            b"[1, 2, 3]\n" in raw.stdout, "list continuation/retention differs")
    project = work / "list-project"
    report = project_workflow(lambda *args: run(*args, cwd=project), project, fixtures, target,
                              list_spreads=True)
    return {"fixture_cases": sorted(LIST_LITERAL_SPREAD_FIXTURES), "values": cases, "project": report,
            "assertions": ["before/after/alternate exact values", "static and dynamic non-list refusal",
                           "once-only ordered operands and early failure", "pure authority refusal",
                           "canonical comments; raw continuation and retained value"]}


def record_shorthand(run, work: Path, fixtures: Path, target: str) -> dict:
    original = fixtures / "record-shorthand"
    destination = work / "record-shorthand"
    require({path.name for path in original.iterdir()} == set(RECORD_SHORTHAND_FIXTURES),
            "record shorthand fixture census differs")
    shutil.copytree(original, destination)

    def invoke(*arguments, **options):
        return run(*arguments, cwd=destination, **options)

    cases = {}
    for name, expected in (("before.opaal", b'["api", "api", 8080]'), ("after.opaal", b'["api", "api", 8080]'),
                           ("alternate.opaal", b'["api", 8080]'), ("qualified.opaal", b"7"),
                           ("patterns.opaal", b"[8080, 0]")):
        success(invoke("check", name), "record source check")
        success(invoke("format", "--check", name), "record canonical bytes")
        require(success(invoke(name), "record script") == b"", "record source printed implicitly")
        observed = invoke(stdin=(destination / name).read_bytes())
        require(observed.returncode == 0 and not observed.stderr and expected + b"\n" in observed.stdout,
                f"{name}: record value differs: {observed.stdout!r} {observed.stderr!r}")
        cases[name] = expected.decode()
    for name, code, location in (("failure.opaal", b"MOD009", b":1:15"),
                                ("unknown.opaal", b"SIG015", b":3:22"),
                                ("missing.opaal", b"SIG016", b":3:14"),
                                ("wrong-type.opaal", b"SIG004", b":3:22")):
        failed = invoke("check", name)
        require(failed.returncode == 1 and not failed.stdout and code in failed.stderr and
                name.encode() + location in failed.stderr,
                f"{name}: record diagnostic differs")
    duplicate = invoke("duplicate.opaal")
    require(duplicate.returncode == 1 and not duplicate.stdout and b"duplicate record key" in duplicate.stderr,
            "record duplicate key validation differs")
    invalid = invoke("format", "--check", "invalid.opaal")
    require(invalid.returncode == 1 and not invalid.stdout and b"OP1000" in invalid.stderr,
            "quoted record key accepted without a value")
    require(success(invoke("io-allowed.opaal"), "record ordered fields") == b"first\nmiddle\nlast\n",
            "record field evaluation order/count differs")
    failed = invoke("io-failure.opaal")
    require(failed.returncode == 1 and failed.stdout == b"first\n" and b"RUN001" in failed.stderr,
            "failed record field evaluated a later operand or lost earlier output")
    denied = invoke("test", "io-pure.opaal", "--format", "json")
    result = json.loads(denied.stdout)
    require(denied.returncode == 2 and result["counts"]["passed"] == 0 and
            result["counts"]["unrun"] == 1 and b"TEST_EFFECT" in denied.stdout and
            b"private-sentinel" not in denied.stdout + denied.stderr, "record acquired pure authority")
    success(invoke("format", "--write", "noncanonical.opaal"), "record format")
    require((destination / "noncanonical.opaal").read_bytes() == (destination / "canonical.opaal").read_bytes(),
            "record canonical bytes differ")
    success(invoke("format", "--check", "noncanonical.opaal"), "record idempotence")
    pending = invoke(stdin=(destination / "incomplete.opaal").read_bytes())
    require(pending.returncode == 0 and b"SYN002" in pending.stderr, "record incomplete EOF differs")
    raw = invoke(stdin=b"let name = 7\nlet fields = {\nname,\n}\nfields.name\n")
    require(raw.returncode == 0 and not raw.stderr and b"...> " in raw.stdout and b"7\n" in raw.stdout,
            "record raw continuation or retained value differs")
    project = work / "record-project"
    report = project_workflow(lambda *args: run(*args, cwd=project), project, fixtures, target,
                              record_shorthands=True)
    return {"fixture_cases": sorted(RECORD_SHORTHAND_FIXTURES), "values": cases, "project": report,
            "assertions": ["before/after/alternate/qualified/pattern exact values", "field/name/type validation",
                           "ordered fields and early failure", "pure authority refusal",
                           "canonical chosen spelling and comments; raw continuation and retention"]}


def grouped_export_prefixes(run, work: Path, fixtures: Path, target: str) -> dict:
    original = fixtures / "grouped-export-prefixes"
    destination = work / "grouped-export-prefixes"
    require({path.name for path in original.iterdir()} == set(GROUPED_EXPORT_FIXTURES),
            "grouped export fixture census differs")
    shutil.copytree(original, destination)

    def invoke(*arguments, **options):
        return run(*arguments, cwd=destination, **options)

    def tests(name, *, list_only=False, case=None, exit_code=0, failure=None):
        result = invoke("test", *(('--list',) if list_only else ()), name,
                        *(('--case', case) if case else ()), "--format", "json")
        require(result.returncode == exit_code and not result.stderr,
                f"{name}: test status differs: {result.returncode} {result.stdout!r} {result.stderr!r}")
        value = json.loads(result.stdout)
        require(value["failure"] == failure and b"GROUPED_EXPORT_CANARY" not in result.stdout,
                f"{name}: admission or redaction differs")
        return value

    inventories = []
    for name in ("before.opaal", "after.opaal", "main.opaal", "explicit-reexports.opaal"):
        success(invoke("check", name), "prefix module check")
        success(invoke("format", "--check", name), "prefix canonical bytes")
    for name in ("before.opaal", "after.opaal"):
        listed = tests(name, list_only=True)
        executed = tests(name)
        require([case["name"] for case in listed["cases"]] == ["test_caught", "test_pass"] and
                executed["counts"]["passed"] == 2, "prefix policy inventory/results differ")
        # These files intentionally differ only in their root source identity.
        for case in listed["cases"]:
            case["callable_id"].pop("module")
        inventories.append(listed["cases"])
    require(inventories[0] == inventories[1], "explicit/prefix callable signatures differ")
    require(tests("after.opaal", case="test_pass")["counts"]["passed"] == 1,
            "exact selection changed")
    after = destination / "after.opaal"
    after.write_bytes(after.read_bytes() + b"def test_added() -> Bool\n{\n    return true\n}\n")
    require([case["name"] for case in tests("after.opaal", list_only=True)["cases"]] ==
            ["test_added", "test_caught", "test_pass"] and tests("after.opaal")["counts"]["passed"] == 3,
            "new declaration required an export-list edit")
    require([case["name"] for case in tests("main.opaal", list_only=True)["cases"]] == ["test_model"],
            "nominal/variant inventory differs")
    tests("main.opaal", exit_code=2, failure="TEST_EFFECT")
    observed = invoke(stdin=b"import './main.opaal' as api\napi::test_model()\n")
    require(observed.returncode == 0 and not observed.stderr and b"true\n" in observed.stdout,
            "ordinary nominal/variant invocation differs")
    for name, code, location in (("no-match.opaal", b"MOD005", b":1:10"),
                                ("duplicate.opaal", b"MOD006", b":1:18"),
                                ("imports-only.opaal", b"MOD005", b":3:10")):
        result = invoke("check", name)
        require(result.returncode == 1 and not result.stdout and code in result.stderr and
                name.encode() + location in result.stderr, f"{name}: selector diagnostic differs")
        tests(name, exit_code=2, failure="TEST_SOURCE")
    for list_only, case in ((True, None), (False, None), (False, "test_good")):
        tests("invalid-inventory.opaal", list_only=list_only, case=case,
              exit_code=2, failure="TEST_SIGNATURE")
    require(len(tests("purity.opaal", list_only=True)["cases"]) == 2, "list checked function purity")
    tests("purity.opaal", exit_code=2, failure="TEST_EFFECT")
    require(tests("purity.opaal", case="test_good")["counts"]["passed"] == 1,
            "unselected effectful body blocked pure case")
    purity = destination / "purity.opaal"
    purity.write_bytes(b"let initial = env('GROUPED_EXPORT_CANARY')\n" + purity.read_bytes())
    tests("purity.opaal", list_only=True)
    tests("purity.opaal", case="test_good", exit_code=2, failure="TEST_EFFECT")
    success(invoke("format", "--write", "noncanonical.opaal"), "prefix format")
    require((destination / "noncanonical.opaal").read_bytes() == (destination / "canonical.opaal").read_bytes(),
            "prefix canonical bytes differ")
    success(invoke("format", "--check", "noncanonical.opaal"), "prefix idempotence")
    for name, code in (("invalid.opaal", b"OP1000"), ("incomplete.opaal", b"SYN002")):
        path = destination / name
        before = path.read_bytes()
        invalid = invoke("format", "--write", name)
        require(invalid.returncode == 1 and code in invalid.stderr and path.read_bytes() == before,
                "invalid prefix rewrite changed source")
    pending = invoke(stdin=(destination / "incomplete.opaal").read_bytes())
    require(pending.returncode == 0 and pending.stderr.count(b"SYN002") == 1,
            "prefix raw EOF differs")
    raw = invoke(stdin=b"import './model.opaal' as model\nmodel::check_sum()\nlet retained = model::check_sum()\nexport {\nmissing_*,\n}\nretained\nlet missing_later = 1\nretained\n")
    require(raw.returncode == 0 and not raw.stderr and raw.stdout.count(b"5\n") == 3 and b"...> " in raw.stdout,
            "prefix raw file import/retention or inert export differs")
    adversarial = destination / "large.opaal"
    declarations = b"".join(f"let check_{index:04} = {index}\n".encode() for index in range(1024))
    for selectors, code in ((b"check_*", None), (b"check_*, check*", b"MOD006"), (b"absent_*", b"MOD005")):
        adversarial.write_bytes(b"export {" + selectors + b"}\n" + declarations)
        checked = invoke("check", adversarial.name)
        require(checked.returncode == (1 if code else 0) and (code in checked.stderr if code else not checked.stderr),
                "bounded prefix analysis differs")
    examples = {}
    for folder, names in (
        ("image-check", ["test_empty_expectation", "test_empty_revision", "test_invalid_decoder", "test_json_roundtrip",
                         "test_missing_revision", "test_negative_id", "test_negative_platform", "test_negative_revision",
                         "test_pass", "test_unsupported_descriptor"]),
        ("repository-check", ["test_acquisition", "test_bad_shape", "test_findings", "test_inconsistent", "test_manifest", "test_pass"]),
    ):
        example = destination / folder
        shutil.copytree(ROOT / "examples" / folder, example)
        policy = example / "policy-tests.opaal"
        automatic = policy.read_bytes()
        baseline = automatic.replace(b"export { test_* }", ("export { " + ", ".join(names) + " }").encode())
        results = []
        for source in (baseline, automatic):
            policy.write_bytes(source)
            success(invoke("check", policy), "real policy check")
            success(invoke("format", "--check", policy), "real policy format")
            listed = tests(str(policy), list_only=True)
            result = tests(str(policy))
            require([case["name"] for case in listed["cases"]] == names and result["counts"]["passed"] == len(names),
                    "real policy inventory/results changed")
            results.append((listed["cases"], result["cases"]))
        require(results[0] == results[1], "real policy callable identities changed")
        examples[folder] = names
    project = work / "prefix-project"
    report = project_workflow(lambda *args, **options: run(*args, cwd=project, **options), project, fixtures, target,
                              grouped_exports=True)
    return {"fixture_cases": sorted(GROUPED_EXPORT_FIXTURES), "examples": examples, "project": report,
            "assertions": ["explicit/prefix inventory and callable parity; automatic addition",
                           "local general exports, member imports and nominal/variant identity",
                           "no-match/import exclusion/duplicate spans; invalid inventory before cases",
                           "list skips purity; selected calls and all initializers retain admission",
                           "canonical bytes, invalid/incomplete preservation; raw retained import and inert exports",
                           "1024 declarations, overlaps and no-match under existing deadlines"]}


def framed_formatting(binary: Path, work: Path, fixtures: Path, environment: dict) -> list[str]:
    observations = []
    for encoding in ("utf-8", "utf-16"):
        with subprocess.Popen([binary], cwd=work, env=environment, stdin=subprocess.PIPE,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0) as child:
            os.set_blocking(child.stdin.fileno(), False)
            buffer = b""
            serial = 0
            seen = set()

            def send(*messages):
                frames = []
                for message in messages:
                    body = json.dumps({"jsonrpc": "2.0", **message}).encode()
                    frames.append(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
                data = b"".join(frames)
                deadline = time.monotonic() + 30
                while data:
                    delay = deadline - time.monotonic()
                    require(delay > 0 and bool(select.select([], [child.stdin], [], max(0, delay))[1]),
                            "language server input timed out")
                    try:
                        count = os.write(child.stdin.fileno(), data)
                    except BlockingIOError:
                        continue
                    require(bool(count), "language server input closed")
                    data = data[count:]

            def receive(identity):
                nonlocal buffer
                deadline = time.monotonic() + 30
                while True:
                    headers, separator, remaining = buffer.partition(b"\r\n\r\n")
                    if separator:
                        match = re.fullmatch(rb"Content-Length: (\d+)", headers)
                        require(match is not None, "invalid language server frame")
                        count = int(match[1])
                        require(count <= 8 * 1024 * 1024, "language server response exceeds bound")
                        if len(remaining) >= count:
                            message = json.loads(remaining[:count])
                            buffer = remaining[count:]
                            if "id" in message:
                                require(message["id"] not in seen, "duplicate language server response")
                                seen.add(message["id"])
                                require(message["id"] == identity, "unexpected language server response")
                                return message
                            continue
                    delay = deadline - time.monotonic()
                    require(delay > 0 and bool(select.select([child.stdout], [], [], max(0, delay))[0]),
                            "language server response timed out")
                    chunk = os.read(child.stdout.fileno(), 65536)
                    require(bool(chunk), "language server output closed")
                    buffer += chunk

            def request(method, params=None):
                nonlocal serial
                serial += 1
                send({"id": serial, "method": method, "params": params or {}})
                return receive(serial)

            try:
                initialized = request("initialize", {"capabilities": {"general": {"positionEncodings": [encoding]}}})
                require(initialized["result"]["capabilities"]["positionEncoding"] == encoding,
                        "language server encoding differs")
                send({"method": "initialized", "params": {}})
                documents = [(name, (fixtures / name).read_text()) for name in
                             ("legacy.opaal", "canonical.opaal", "invalid.opaal", "incomplete.opaal")]
                documents.extend((name, (fixtures / name).read_text()) for name in (
                    "concise-imports/noncanonical.opaal", "concise-imports/canonical.opaal",
                    "concise-imports/incomplete.opaal", "concise-imports/invalid-name.opaal",
                    "explicit-member-imports/noncanonical.opaal", "explicit-member-imports/canonical.opaal",
                    "explicit-member-imports/incomplete.opaal", "compound-assignment/noncanonical.opaal",
                    "compound-assignment/canonical.opaal", "compound-assignment/incomplete.opaal",
                    "list-literal-spread/noncanonical.opaal", "list-literal-spread/canonical.opaal",
                    "list-literal-spread/incomplete.opaal", "list-literal-spread/invalid.opaal",
                    "record-shorthand/noncanonical.opaal", "record-shorthand/canonical.opaal",
                    "record-shorthand/incomplete.opaal", "record-shorthand/invalid.opaal",
                    "grouped-export-prefixes/noncanonical.opaal", "grouped-export-prefixes/canonical.opaal",
                    "grouped-export-prefixes/incomplete.opaal", "grouped-export-prefixes/invalid.opaal"))
                documents.append(("invalid-body.opaal", "def waiting()\nlet x = 1\n"))
                for index, (name, text) in enumerate(documents):
                    uri = (work / f"document-{index}.opaal").as_uri()
                    send({"method": "textDocument/didOpen", "params": {"textDocument": {
                        "uri": uri, "languageId": "opaal", "version": 1, "text": text}}})
                    params = {"textDocument": {"uri": uri}, "options": {"tabSize": 8, "insertSpaces": False}}
                    result = request("textDocument/formatting", params)["result"]
                    if name in ("legacy.opaal", "concise-imports/noncanonical.opaal", "explicit-member-imports/noncanonical.opaal",
                                "compound-assignment/noncanonical.opaal", "list-literal-spread/noncanonical.opaal",
                                "record-shorthand/noncanonical.opaal", "grouped-export-prefixes/noncanonical.opaal"):
                        canonical = (fixtures / ("canonical.opaal" if name == "legacy.opaal" else
                                                  name.replace("noncanonical.opaal", "canonical.opaal"))).read_text()
                        require(result == [{"range": {"start": {"line": 0, "character": 0},
                                                       "end": {"line": len(text.splitlines()), "character": 0}},
                                            "newText": canonical}], "framed formatter bytes/range differ")
                        send({"method": "textDocument/didChange", "params": {
                            "textDocument": {"uri": uri, "version": 2}, "contentChanges": [{"text": canonical}]}})
                        require(request("textDocument/formatting", params)["result"] == [], "LSP is not idempotent")
                    else:
                        require(result == [], f"{name}: language server produced an edit")
                uri = (work / "concise-imports/after.opaal").as_uri()
                text = (fixtures / "concise-imports/after.opaal").read_text()
                send({"method": "textDocument/didOpen", "params": {"textDocument": {
                    "uri": uri, "languageId": "opaal", "version": 1, "text": text}}})
                alias = {"textDocument": {"uri": uri}, "position": {"line": 3, "character": 8}}
                definition = request("textDocument/definition", alias)["result"]
                require(definition == {"uri": uri, "range": {"start": {"line": 3, "character": 7},
                        "end": {"line": 3, "character": 14}}}, "inferred alias definition differs")
                require("module `helpers`" in request("textDocument/hover", alias)["result"]["contents"]["value"],
                        "inferred alias hover differs")
                uri = (work / "compound-query.opaal").as_uri()
                text = "# 界🙂\nmut total: Int = 7\ntotal += 2\ntotal\n"
                send({"method": "textDocument/didOpen", "params": {"textDocument": {
                    "uri": uri, "languageId": "opaal", "version": 1, "text": text}}})
                target = {"textDocument": {"uri": uri}, "position": {"line": 2, "character": 1}}
                definition = request("textDocument/definition", target)["result"]
                require(definition == {"uri": uri, "range": {"start": {"line": 1, "character": 4},
                        "end": {"line": 1, "character": 9}}}, "compound target definition differs")
                require("Int" in request("textDocument/hover", target)["result"]["contents"]["value"],
                        "compound target hover differs")
                references = request("textDocument/references", {**target, "context": {"includeDeclaration": True}})["result"]
                require(references == [definition] + [
                    {"uri": uri, "range": {"start": {"line": line, "character": 0},
                                           "end": {"line": line, "character": 5}}} for line in (2, 3)],
                    "compound read/write references duplicate or lose the source token")
                uri = (work / "record-query.opaal").as_uri()
                lines = ["type Item = {name: Int}", "let name: Int = 7", "let item = Item {name}",
                         "let fields = {label: '界🙂', name}", "def pick(Item {name}: Item) -> Int",
                         "{", "    return name", "}"]
                send({"method": "textDocument/didOpen", "params": {"textDocument": {
                    "uri": uri, "languageId": "opaal", "version": 1, "text": "\n".join(lines) + "\n"}}})

                def record_position(line, start, end):
                    def units(prefix):
                        return len(prefix.encode("utf-8")) if encoding == "utf-8" else len(prefix.encode("utf-16-le")) // 2
                    return {"uri": uri, "range": {"start": {"line": line, "character": units(lines[line][:start])},
                                                  "end": {"line": line, "character": units(lines[line][:end])}}}

                original = record_position(1, 4, 8)
                binding = record_position(4, lines[4].index("name"), lines[4].index("name") + 4)
                references = [record_position(line, lines[line].rindex("name"), lines[line].rindex("name") + 4)
                              for line in (2, 3)]
                for reference in references:
                    target = {"textDocument": {"uri": uri}, "position": reference["range"]["start"]}
                    require(request("textDocument/definition", target)["result"] == original,
                            "record shorthand lost its lexical definition")
                    require("Int" in request("textDocument/hover", target)["result"]["contents"]["value"],
                            "record shorthand hover differs")
                    require(request("textDocument/references", {**target, "context": {"includeDeclaration": True}})["result"] ==
                            [original] + references, "record shorthand reference locations differ")
                target = {"textDocument": {"uri": uri}, "position": binding["range"]["start"]}
                require(request("textDocument/definition", target)["result"] == binding,
                        "record shorthand pattern did not declare a fresh binding")
                require(request("textDocument/references", {**target, "context": {"includeDeclaration": True}})["result"] ==
                        [binding, record_position(6, 11, 15)], "record shorthand binding references differ")
                uri = (work / "prefix-query.opaal").as_uri()
                text = "export { check_* }\nlet check_value: Int = 5\nlet result = ['界🙂', check_value]\n"
                send({"method": "textDocument/didOpen", "params": {"textDocument": {
                    "uri": uri, "languageId": "opaal", "version": 1, "text": text}}})
                for character in range(9, 16):
                    target = {"textDocument": {"uri": uri}, "position": {"line": 0, "character": character}}
                    require(request("textDocument/hover", target)["result"] is None and
                            request("textDocument/definition", target)["result"] is None and
                            request("textDocument/references", {**target, "context": {"includeDeclaration": True}})["result"] == [],
                            "prefix selector acquired a singular query target")
                prefix = "let result = ['界🙂', "
                character = len(prefix.encode()) if encoding == "utf-8" else len(prefix.encode("utf-16-le")) // 2
                target = {"textDocument": {"uri": uri}, "position": {"line": 2, "character": character}}
                definition = {"uri": uri, "range": {"start": {"line": 1, "character": 4},
                                                   "end": {"line": 1, "character": 15}}}
                require(request("textDocument/definition", target)["result"] == definition and
                        "Int" in request("textDocument/hover", target)["result"]["contents"]["value"],
                        "prefix concrete use lost definition or type")
                require(request("textDocument/references", {**target, "context": {"includeDeclaration": True}})["result"] ==
                        [definition, {"uri": uri, "range": {"start": {"line": 2, "character": character},
                                                           "end": {"line": 2, "character": character + 11}}}],
                        "prefix metadata leaked into concrete references")
                updated = text + "let check_added = 1\ncheck_added\n"
                send({"method": "textDocument/didChange", "params": {"textDocument": {"uri": uri, "version": 2},
                     "contentChanges": [{"text": updated}]}})
                target = {"textDocument": {"uri": uri}, "position": {"line": 4, "character": 1}}
                require(request("textDocument/definition", target)["result"] == {"uri": uri, "range": {
                    "start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 15}}},
                    "prefix changed snapshot did not expose new declaration")
                uri = (work / "spread-query.opaal").as_uri()
                text = "import std::list\nlet items = [1, 2]\nlet result = ['界🙂', ...list::reverse(items)]\n"
                send({"method": "textDocument/didOpen", "params": {"textDocument": {
                    "uri": uri, "languageId": "opaal", "version": 1, "text": text}}})
                prefix = "let result = ['界🙂', ...list::reverse("
                character = len(prefix.encode()) if encoding == "utf-8" else len(prefix.encode("utf-16-le")) // 2
                target = {"textDocument": {"uri": uri}, "position": {"line": 2, "character": character}}
                definition = request("textDocument/definition", target)["result"]
                require(definition == {"uri": uri, "range": {"start": {"line": 1, "character": 4},
                        "end": {"line": 1, "character": 9}}}, "spread operand definition differs")
                require("items" in request("textDocument/hover", target)["result"]["contents"]["value"],
                        "spread operand hover differs")
                require(bool(request("textDocument/signatureHelp", target)["result"]["signatures"]),
                        "spread operand call signature was lost")
                references = request("textDocument/references", {**target, "context": {"includeDeclaration": True}})["result"]
                require(references == [definition, {"uri": uri, "range": {
                    "start": {"line": 2, "character": character},
                    "end": {"line": 2, "character": character + 5}}}], "spread references/Unicode positions differ")
                uri = (work / "unicode.opaal").as_uri()
                text = (fixtures / "legacy.opaal").read_text() + "# 界🙂"
                expected = (fixtures / "canonical.opaal").read_text() + "# 界🙂\n"
                send({"method": "textDocument/didOpen", "params": {"textDocument": {
                    "uri": uri, "languageId": "opaal", "version": 1, "text": text}}})
                params = {"textDocument": {"uri": uri}, "options": {}}
                edits = request("textDocument/formatting", params)["result"]
                require(len(edits) == 1 and edits[0]["newText"] == expected and
                        edits[0]["range"]["end"] == {"line": 7, "character": 9 if encoding == "utf-8" else 5},
                        "Unicode formatting position/bytes differ")
                # Queue behind bounded diagnostic work so cancel/change reach pending snapshots.
                large = ("# 界🙂\n" * 16000) + (fixtures / "legacy.opaal").read_text()
                serial += 1
                send({"method": "textDocument/didChange", "params": {
                         "textDocument": {"uri": uri, "version": 2}, "contentChanges": [{"text": large}]}},
                     {"id": serial, "method": "textDocument/formatting", "params": params},
                     {"method": "$/cancelRequest", "params": {"id": serial}})
                require(receive(serial).get("error", {}).get("code") == -32800, "pending format was not cancelled")
                serial += 1
                send({"id": serial, "method": "textDocument/formatting", "params": params},
                     {"method": "textDocument/didChange", "params": {
                         "textDocument": {"uri": uri, "version": 3}, "contentChanges": [{"text": expected}]}})
                require(receive(serial).get("error", {}).get("code") == -32801, "stale format returned an edit")
                require(request("textDocument/formatting", params)["result"] == [], "current snapshot differs")
                require(request("shutdown")["result"] is None, "language server shutdown differs")
                send({"method": "exit"})
                child.stdin.close()
                require(child.wait(timeout=30) == 0 and not child.stderr.read(), "language server exit differs")
                require(not buffer and not child.stdout.read(), "unexpected trailing language server response")
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait()
        observations.append(encoding + ": exact shared bytes, idempotence, negatives, Unicode range, cancellation, stale snapshot")
    return observations


def terminal_continuation(binary: Path, work: Path, environment: dict) -> None:
    pid, descriptor = pty.fork()
    if pid == 0:
        os.chdir(work)
        os.execve(binary, [str(binary)], {**environment, "TERM": "xterm-256color"})
    captured = b""
    reaped = False

    def read_until(pattern: bytes) -> None:
        nonlocal captured
        captured = b""
        deadline = time.monotonic() + 30
        while not re.search(pattern, re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", captured), re.DOTALL):
            remaining = deadline - time.monotonic()
            require(remaining > 0 and bool(select.select([descriptor], [], [], max(0, remaining))[0]),
                    f"terminal continuation timed out waiting for {pattern!r}: {captured[-2000:]!r}")
            chunk = os.read(descriptor, 65536)
            require(bool(chunk), "terminal closed before expected output")
            previous = captured[-3:]
            captured += chunk
            for _ in range((previous + chunk).count(b"\x1b[6n")):
                os.write(descriptor, b"\x1b[1;1R")

    try:
        read_until(rb">> ")
        for signature, body in ((b"def identity(value: String) -> String", b"    return value"),
                                (b"def five()", b"    return 5")):
            for line in (signature, b"{", body):
                os.write(descriptor, line + b"\r")
                read_until(rb"\.\.\.> ")
                require(b"error[" not in captured, "terminal submitted an incomplete declaration")
            os.write(descriptor, b"}\r")
            read_until(rb">> ")
            require(b"error[" not in captured, "terminal rejected a complete declaration")
        for expression, value in ((b"identity('ready')", b"ready"), (b"five()", b"5")):
            os.write(descriptor, expression + b"\r")
            read_until(rb"(?:^|\n)" + value + rb"\r?\n.*>> ")
        for line in (b"import {", b"std::math,"):
            os.write(descriptor, line + b"\r")
            read_until(rb"\.\.\.> ")
            require(b"error[" not in captured, "terminal submitted an open import group")
        os.write(descriptor, b"std::value as values}\r")
        read_until(rb">> ")
        require(b"error[" not in captured, "terminal rejected an import group")
        os.write(descriptor, b"math::abs(-7) + values::length([1, 2, 3])\r")
        read_until(rb"(?:^|\n)10\r?\n.*>> ")
        for line in (b"import std::math::{", b"abs as magnitude_of,"):
            os.write(descriptor, line + b"\r")
            read_until(rb"\.\.\.> ")
            require(b"error[" not in captured, "terminal submitted an open member selector")
        os.write(descriptor, b"}\r")
        read_until(rb">> ")
        require(b"error[" not in captured, "terminal rejected a member selector")
        os.write(descriptor, b"magnitude_of(-7)\r")
        read_until(rb"(?:^|\n)7\r?\n.*>> ")
        os.write(descriptor, b"mut total = 7\r")
        read_until(rb">> ")
        os.write(descriptor, b"total +=\r")
        read_until(rb"\.\.\.> ")
        require(b"error[" not in captured, "terminal submitted an incomplete compound RHS")
        os.write(descriptor, b"2\r")
        read_until(rb">> ")
        require(b"error[" not in captured, "terminal rejected compound assignment")
        os.write(descriptor, b"total\r")
        read_until(rb"(?:^|\n)9\r?\n.*>> ")
        for line in (b"let spread = [", b"...[1, 2],"):
            os.write(descriptor, line + b"\r")
            read_until(rb"\.\.\.> ")
            require(b"error[" not in captured, "terminal submitted an incomplete list spread")
        os.write(descriptor, b"3]\r")
        read_until(rb">> ")
        os.write(descriptor, b"spread\r")
        read_until(rb"(?:^|\n)\[1, 2, 3\]\r?\n.*>> ")
        os.write(descriptor, b"let record_name = 7\r")
        read_until(rb">> ")
        for line in (b"let record = {", b"record_name,"):
            os.write(descriptor, line + b"\r")
            read_until(rb"\.\.\.> ")
            require(b"error[" not in captured, "terminal submitted an incomplete record")
        os.write(descriptor, b"}\r")
        read_until(rb">> ")
        os.write(descriptor, b"record.record_name\r")
        read_until(rb"(?:^|\n)7\r?\n.*>> ")
        os.write(descriptor, b"import './grouped-export-prefixes/model.opaal' as model\r")
        read_until(rb">> ")
        require(b"error[" not in captured, "terminal prefix module import failed")
        os.write(descriptor, b"model::check_sum()\r")
        read_until(rb"(?:^|\n)5\r?\n.*>> ")
        os.write(descriptor, b"let retained = model::check_sum()\r")
        read_until(rb">> ")
        for line in (b"export {", b"missing_*,"):
            os.write(descriptor, line + b"\r")
            read_until(rb"\.\.\.> ")
        os.write(descriptor, b"}\r")
        read_until(rb">> ")
        require(b"error[" not in captured, "terminal direct prefix export became strict")
        for line in (b"export {", b"missing_*,"):
            os.write(descriptor, line + b"\r")
            read_until(rb"\.\.\.> ")
        os.write(descriptor, b"\x03")
        read_until(rb">> ")
        os.write(descriptor, b"retained\r")
        read_until(rb"(?:^|\n)5\r?\n.*>> ")
        os.write(descriptor, b"let missing_later = 1\r")
        read_until(rb">> ")
        os.write(descriptor, b"retained\r")
        read_until(rb"(?:^|\n)5\r?\n.*>> ")
        os.write(descriptor, b"def cancelled()\r")
        read_until(rb"\.\.\.> ")
        os.write(descriptor, b"\x03")
        read_until(rb">> ")
        os.write(descriptor, b"cancelled()\r")
        read_until(rb"error\[.*>> ")
        os.write(descriptor, b"five()\r")
        read_until(rb"(?:^|\n)5\r?\n.*>> ")
        os.write(descriptor, b"\x04")
        deadline = time.monotonic() + 30
        while True:
            waited, status = os.waitpid(pid, os.WNOHANG)
            if waited == pid:
                reaped = True
                require(status == 0, "terminal EOF exit differs")
                break
            require(time.monotonic() < deadline, "terminal EOF timed out")
            if select.select([descriptor], [], [], 0.05)[0]:
                try:
                    chunk = os.read(descriptor, 65536)
                except OSError:
                    chunk = b""
                for _ in range((captured[-3:] + chunk).count(b"\x1b[6n")):
                    os.write(descriptor, b"\x1b[1;1R")
                captured += chunk
    finally:
        os.close(descriptor)
        if not reaped:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)


def qualify(archive: Path | None, fixture_archive: Path | None, binaries: Path | None,
            source: str, target: str) -> dict:
    require(SOURCE.fullmatch(source) is not None, "expected source must be a full Git identity")
    require((platform.system(), platform.machine()) == {
        "macos-arm64": ("Darwin", "arm64"), "linux-x86_64": ("Linux", "x86_64"),
        "linux-aarch64": ("Linux", "aarch64")}[target], "host differs")
    require(subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip() == source,
            "expected source differs from checkout head")
    paths = public_paths(ROOT)
    snapshot = {name: digest((ROOT / name).read_bytes()) for name in paths if (ROOT / name).is_file()}
    fixture_hashes = {name[len(FIXTURES) + 1:]: value for name, value in snapshot.items()
                      if name.startswith(FIXTURES + "/")}
    dirty = bool(subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT))
    require((archive is None) == (fixture_archive is None), "binary and fixture archives must be qualified together")
    require(archive is None or not dirty, "archive qualification requires a clean committed checkout")
    with tempfile.TemporaryDirectory(prefix="opaal-source-formatting-") as temporary:
        root = Path(temporary).resolve()
        require(not root.is_relative_to(ROOT), "qualification must run outside the checkout")
        if archive is not None:
            installed, binary_manifest = unpack(archive, root, kind="binaries", version=VERSION, source=source, platform=target)
            fixtures, fixture_manifest = unpack(fixture_archive, root, kind="formatting-fixtures",
                                               version=VERSION, source=source, platform="portable")
        else:
            require(binaries is not None, "missing working programs")
            installed = root / "bin"
            installed.mkdir()
            for name in ("opaal", "opaal-language-server"):
                shutil.copy2(binaries / name, installed / name)
            fixtures = root / "fixtures"
            shutil.copytree(ROOT / FIXTURES, fixtures)
            binary_manifest = fixture_manifest = {"sha256": None}
        observed = {path.relative_to(fixtures).as_posix(): digest(path.read_bytes())
                    for path in fixtures.rglob("*") if path.is_file()}
        require(observed == fixture_hashes, "fixture members/bytes differ from expected source")
        environment = {"PATH": os.defpath}
        binary = installed / "opaal"

        def run(*arguments, cwd=root, stdin=None):
            return subprocess.run([str(binary), *map(str, arguments)], cwd=cwd, env=environment,
                                  input=stdin, capture_output=True, timeout=30)

        require(success(run("--version"), "version") == f"opaal {VERSION}\n".encode(), "version differs")
        for legacy, canonical in PAIRS:
            path = root / legacy
            before, expected = (fixtures / legacy).read_bytes(), (fixtures / canonical).read_bytes()
            path.write_bytes(before)
            path.chmod(0o751)
            result = run("format", "--check", "--", path.name)
            require(result.returncode == 1 and not result.stdout and b"error[FMT001]" in result.stderr and
                    path.read_bytes() == before, "legacy check result/bytes differ")
            if legacy == "legacy.opaal":
                require(success(run(path.name), "legacy script") == b"", "legacy script output differs")
                success(run("check", path.name), "legacy script check")
            for _ in range(2):
                require(success(run("format", "--write", "--", path.name), "write") == b"", "write printed output")
                require(path.read_bytes() == expected and path.stat().st_mode & 0o777 == 0o751,
                        "canonical bytes/mode differ")
                inode = path.stat().st_ino
                require(success(run("format", "--write", "--", path.name), "unchanged write") == b"" and
                        path.stat().st_ino == inode, "unchanged write replaced its inode")
            require(success(run("format", "--check", "--", path.name), "canonical check") == b"", "check printed output")
            if legacy == "legacy.opaal":
                require(success(run(path.name), "canonical script") == b"", "script execution changed")
                success(run("check", path.name), "canonical script check")
        negatives = [(name, (fixtures / name).read_bytes(), code) for name, code in
                     (("invalid.opaal", "OP1000"), ("incomplete.opaal", "SYN002"), ("missing-semicolon.opaal", "OP1000"))]
        negatives.append(("invalid-body.opaal", b"def waiting()\nlet x = 1\n", "OP1000"))
        for name, data, code in negatives:
            path = root / name
            path.write_bytes(data)
            result = run("format", "--write", "--", name)
            require(result.returncode == 1 and not result.stdout and code.encode() in result.stderr and
                    path.read_bytes() == data, "negative write changed bytes or diagnostics")
        pending = run(stdin=b"def waiting()\n")
        require(pending.returncode == 0 and pending.stdout == b">> ...> " and
                pending.stderr.count(b"SYN002") == 1, "incomplete raw EOF differs")
        require(run(stdin=b"").stdout == b">> ", "empty raw EOF differs")
        interactive = run(stdin=(fixtures / "canonical.opaal").read_bytes() +
                          b"identity('ready')\nfive()\n| broken\nfive()\n")
        require(interactive.returncode == 0 and interactive.stderr.count(b"error[OP1000]") == 1 and
                interactive.stdout.count(b"...> ") == 10 and interactive.stdout.count(b"ready\n") == 2 and
                interactive.stdout.count(b"5\n") == 3, "interactive submissions/retained values differ")
        sentinel = root / "sentinel.opaal"
        sentinel.write_text("import './missing.opaal' as missing\ndef sentinel() { ^touch 'must-not-exist' }\nsentinel()\n")
        success(run("format", "--write", "--", sentinel.name), "nonexecuting format")
        require(not (root / "must-not-exist").exists(), "formatter executed source")
        writable = root / "preflight.opaal"
        original = (fixtures / "legacy.opaal").read_bytes()
        writable.write_bytes(original)
        link = root / "link.opaal"
        link.symlink_to(writable)
        directory = root / "directory.opaal"
        directory.mkdir()
        (root / "wrong.txt").write_bytes(original)
        for operand in ("link.opaal", "directory.opaal", "missing.opaal", "wrong.txt", "invalid.opaal", "preflight.opaal"):
            result = run("format", "--write", "--", writable.name, operand)
            require(result.returncode == 1 and not result.stdout and bool(result.stderr) and
                    writable.read_bytes() == original, "failed preflight wrote source")
        bounded = {}
        cases = {"siblings": "".join(f"def value_{i}() -> Int {{ return {i} }}\n" for i in range(1024)),
                 "nested": "if true {\n" * 64 + "def inner() -> Int { return 1 }\n" + "}\n" * 64,
                 "unicode": "# 界🙂 comment\n" * 16000 + "def value() -> Int { return 5 }\n"}
        for name, text in cases.items():
            path = root / f"bounded-{name}.opaal"
            path.write_text(text)
            success(run("format", "--write", "--", path.name), "bounded format")
            success(run("format", "--check", "--", path.name), "bounded canonical reparse")
            canonical = path.read_bytes()
            success(run("format", "--write", "--", path.name), "bounded idempotence")
            require(path.read_bytes() == canonical, "bounded format is not idempotent")
            bounded[name] = {"input_bytes": len(text.encode()), "input_sha256": digest(text.encode()),
                             "canonical_sha256": digest(canonical)}
        project = root / "project"
        project_report = project_workflow(lambda *args: run(*args, cwd=project), project, fixtures, target)
        imports_report = concise_imports(run, root, fixtures)
        members_report = explicit_member_imports(run, root, fixtures, target)
        compound_report = compound_assignment(run, root, fixtures, target)
        spread_report = list_literal_spread(run, root, fixtures, target)
        record_report = record_shorthand(run, root, fixtures, target)
        exports_report = grouped_export_prefixes(run, root, fixtures, target)
        lsp_report = framed_formatting(installed / "opaal-language-server", root, fixtures, environment)
        terminal_continuation(binary, root, environment)
        require(public_paths(ROOT) == paths and
                {name: digest((ROOT / name).read_bytes()) for name in paths if (ROOT / name).is_file()} == snapshot,
                "source changed during qualification")
        return {"schema_version": 1, "source": source, "committed_source": not dirty,
                "qualification_mode": "archives" if archive else "working-source",
                "source_snapshot_sha256": digest(json.dumps(snapshot, sort_keys=True).encode()),
                "version": VERSION, "platform": target,
                "binary_archive_sha256": binary_manifest["sha256"], "fixture_archive_sha256": fixture_manifest["sha256"],
                "binary_sha256": {name: digest((installed / name).read_bytes())
                                  for name in ("opaal", "opaal-language-server")},
                "fixture_sha256": fixture_hashes, "parser_seeds": seed_census(ROOT, paths),
                "project": project_report, "concise_imports": imports_report, "explicit_member_imports": members_report,
                "compound_assignment": compound_report, "list_literal_spread": spread_report, "record_shorthand": record_report, "grouped_export_prefixes": exports_report,
                "language_server": lsp_report, "bounded_cases": bounded,
                "assertions": ["exact fixture/source/program identity", "legacy/canonical execution and checking",
                               "exact action/function/comment bytes and idempotence", "read-only check and mode/inode retention",
                               "invalid/incomplete/missing-semicolon preserve bytes", "raw typed/untyped continuation and EOF",
                               "formatting never imports or invokes source", "batch preflight refuses symlink/directory/missing/duplicate/non-source",
                               "1024 siblings, 64 nested blocks, 280 KiB Unicode parse/idempotence within 30-second command deadlines",
                               "native terminal function/import/member continuation, retained values, cancel and EOF"]}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    programs = parser.add_mutually_exclusive_group(required=True)
    programs.add_argument("--archive", type=Path)
    programs.add_argument("--binary-directory", type=Path, help="check working programs without an archive qualification claim")
    parser.add_argument("--fixtures", type=Path)
    parser.add_argument("--expected-source", required=True)
    parser.add_argument("--platform", choices=PLATFORMS, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    if bool(args.archive) != bool(args.fixtures):
        parser.error("--archive requires --fixtures; working programs use checkout fixtures")
    result = qualify(args.archive.resolve() if args.archive else None, args.fixtures.resolve() if args.fixtures else None,
                     args.binary_directory.resolve() if args.binary_directory else None, args.expected_source, args.platform)
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(result, sort_keys=True, indent=2) + "\n")
    print("Source formatting qualification passed.")


if __name__ == "__main__":
    main()
