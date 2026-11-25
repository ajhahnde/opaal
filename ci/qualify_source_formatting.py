#!/usr/bin/env python3
"""Replay source-formatting CLI, framed LSP and project fixtures outside a checkout."""
from __future__ import annotations

import argparse
import base64
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
    from .package_source_formatting import FIXTURES, MEMBER_IMPORT_FIXTURES, COMPOUND_ASSIGNMENT_FIXTURES, LIST_LITERAL_SPREAD_FIXTURES, RECORD_SHORTHAND_FIXTURES, GROUPED_EXPORT_FIXTURES, ACTION_DECLARATION_FIXTURES, INDEXED_REPLACEMENT_FIXTURES, IF_EXPRESSION_FIXTURES, TYPED_TUPLE_FIXTURES, INDEXED_ITERATION_FIXTURES
    from .package_source_formatting import INLINE_EXPORT_FIXTURES, LIST_COMPREHENSION_FIXTURES, LIST_MAP_FIXTURES, SLICE_FIXTURES
    from .qualify_data_processing import require, success, unpack
else:
    from check_source_formatting import public_paths, seed_census
    from package_data_processing import SOURCE, digest
    from package_release import PLATFORMS, ROOT, VERSION
    from package_source_formatting import FIXTURES, MEMBER_IMPORT_FIXTURES, COMPOUND_ASSIGNMENT_FIXTURES, LIST_LITERAL_SPREAD_FIXTURES, RECORD_SHORTHAND_FIXTURES, GROUPED_EXPORT_FIXTURES, ACTION_DECLARATION_FIXTURES, INDEXED_REPLACEMENT_FIXTURES, IF_EXPRESSION_FIXTURES, TYPED_TUPLE_FIXTURES, INDEXED_ITERATION_FIXTURES
    from package_source_formatting import INLINE_EXPORT_FIXTURES, LIST_COMPREHENSION_FIXTURES, LIST_MAP_FIXTURES, SLICE_FIXTURES
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
                     compound_assignments: bool = False, list_spreads: bool = False, record_shorthands: bool = False, grouped_exports: bool = False, indexed_replacements: bool = False,
                     value_conditionals: bool = False) -> dict:
    shutil.copytree(fixtures / "project", work)
    lock = "tools-macos.toml" if target == "macos-arm64" else "tools-linux-aarch64.toml" if target == "linux-aarch64" else "tools-linux.toml"
    shutil.copyfile(work / lock, work / "tools.toml")
    base = ("--project", "opaal.toml", "--task", "verify", "--environment", "ci")
    original = {path.name: path.read_bytes() for path in work.glob("*.toml")}
    expected_value = b"8" if indexed_replacements or value_conditionals else b"5"

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

    def audit(label: str, value: dict, result_value: bytes = expected_value) -> dict:
        success(execute(label, value), f"{label}: execution")
        success(run("audit", "--project", "opaal.toml", "--journal", f"{label}.run.jsonl",
                    "--out", f"{label}.audit.json"), "audit")
        success(run("audit", "inspect", f"{label}.audit.json"), "audit inspection")
        result = json.loads((work / f"{label}.audit.json").read_bytes())
        require(result["completeness"] == "complete" and result["cleanup"] == [] and
                result["primary"]["class"] == "success" and result["primary"]["partial"] is False and
                result["primary"]["value_digest"] == "sha256:" + digest(result_value),
                "project result digest differs")
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
    if value_conditionals:
        canonical = (fixtures / "if-expressions/project-tasks.opaal").read_bytes()
        source.write_bytes(canonical.replace(b"if true {", b"if true{", 1))
    elif grouped_exports:
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
    elif compound_assignments or list_spreads or record_shorthands or indexed_replacements:
        feature = "indexed-list-replacement" if indexed_replacements else "record-shorthand" if record_shorthands else "list-literal-spread" if list_spreads else "compound-assignment"
        canonical = (fixtures / feature / "project-tasks.opaal").read_bytes()
        source.write_bytes(canonical.replace(b"items[1] = 8", b"items [1]=8", 1) if indexed_replacements else
                           canonical.replace(b"def result() -> Int\n{", b"def result() -> Int {", 1) if record_shorthands else
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

    alternate = {}
    if value_conditionals:
        edited = plan("condition-stale")
        source.write_bytes(canonical.replace(b"if true {", b"if false {", 1))
        refused = execute("condition-stale", edited)
        require(refused.returncode == 1 and not refused.stdout and b"EXECUTE_STALE" in refused.stderr and
                not (work / "condition-stale.run.jsonl").exists(), "changed condition accepted a stale plan")
        success(run("check", *base), "changed condition check")
        selected = plan("condition-fresh")
        require(selected["digest"] != edited["digest"] and selected["authority"]["requests"] == [],
                "changed condition plan identity or effects differ")
        result = audit("condition-fresh", selected, b"5")
        alternate = {"condition_stale_plan": edited["digest"], "condition_fresh_plan": selected["digest"],
                     "condition_task_value_digest": result["primary"]["value_digest"]}
        source.write_bytes(canonical)

    # Declaring an effect is sufficient to require authority even when its body is pure.
    if compound_assignments or list_spreads or record_shorthands or grouped_exports or indexed_replacements or value_conditionals:
        empty = b"    effects {\n    }"
        require(canonical.count(empty) == 1, "expected one empty action prologue")
        source.write_bytes(canonical.replace(b"readiness", b"annotated").replace(b"read*", b"annotated*").replace(
            empty, b"    effects {\n        clock.wall(),\n    }"))
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
            "task_value_digest": legacy_audit["primary"]["value_digest"], **alternate,
            "assertions": ["same task signature/effects with verified source-derived contract regeneration",
                           "same check/plan fields except source-derived digests/sizes and receipt times",
                           f"Int {expected_value.decode()} and identical primary/action audit payloads", "empty tools/secrets/effects",
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


def indexed_list_replacement(run, work: Path, fixtures: Path, target: str) -> dict:
    original = fixtures / "indexed-list-replacement"
    destination = work / "indexed-list-replacement"
    require({path.name for path in original.iterdir()} == set(INDEXED_REPLACEMENT_FIXTURES),
            "indexed replacement fixture census differs")
    shutil.copytree(original, destination)

    def invoke(*arguments, **options):
        return run(*arguments, cwd=destination, **options)

    cases = {}
    for name in ("before.opaal", "after.opaal", "alternate.opaal", "failure.opaal", "policy-tests.opaal"):
        success(invoke("check", name), "indexed source check")
        success(invoke("format", "--check", name), "indexed canonical bytes")
        value = json.loads(success(invoke("test", name, "--case", "test_value", "--format", "json"),
                                   "indexed pure test"))
        require(value["counts"]["selected"] == value["counts"]["passed"] == 1 and
                value["counts"]["failed"] == value["counts"]["error"] == value["counts"]["unrun"] == 0 and
                value["completeness"] == "complete", "indexed pure result differs")
        require(success(invoke(name), "indexed script") == b"", "indexed statement printed implicitly")
        cases[name] = value["counts"]
    require(success(invoke("output.opaal"), "indexed public output") == b"ok\n", "indexed output differs")
    require(success(invoke("io-allowed.opaal"), "indexed once-only index/RHS") == b"index\nrhs\n1\n",
            "indexed evaluation order/count differs")
    require(success(invoke("io-failure.opaal", stdin=b"123"), "indexed bounds before RHS") == b"123\n",
            "indexed bounds evaluated RHS")
    success(invoke("check", "io-pure.opaal"), "indexed static non-execution")
    denied = invoke("test", "io-pure.opaal", "--format", "json")
    value = json.loads(denied.stdout)
    require(denied.returncode == 2 and value["counts"]["passed"] == 0 and
            value["counts"]["unrun"] == 2 and b"TEST_EFFECT" in denied.stdout and
            b"private-sentinel" not in denied.stdout + denied.stderr, "indexed expression acquired pure authority")
    for name, code in (("wrong-target.opaal", b"SIG023"), ("wrong-index.opaal", b"SIG024"),
                       ("wrong-type.opaal", b"SIG008"), ("immutable.opaal", b"BND001"),
                       ("captured.opaal", b"BND003"), ("invalid.opaal", b"OP1000")):
        result = invoke("check", name)
        require(result.returncode == 1 and not result.stdout and code in result.stderr,
                "indexed diagnostic differs")
    failure = invoke("dynamic-failure.opaal")
    require(failure.returncode == 1 and b"RUN001" in failure.stderr and not failure.stdout and
            b"private-sentinel" not in failure.stderr, "indexed dynamic diagnostic leaked payload or changed")
    success(invoke("format", "--write", "noncanonical.opaal"), "indexed format")
    require((destination / "noncanonical.opaal").read_bytes() == (destination / "canonical.opaal").read_bytes(),
            "indexed canonical bytes differ")
    success(invoke("format", "--check", "noncanonical.opaal"), "indexed idempotence")
    pending = run(cwd=destination, stdin=(destination / "incomplete.opaal").read_bytes())
    require(pending.returncode == 0 and b"SYN002" in pending.stderr, "indexed incomplete EOF differs")
    raw = run(cwd=destination, stdin=b"mut items: List[Int] = [1, 2]\nlet saved = items\nitems[\n1] =\n8\n[items, saved]\n")
    require(raw.returncode == 0 and not raw.stderr and b"...> " in raw.stdout and
            b"[[1, 8], [1, 2]]\n" in raw.stdout, "indexed raw continuation or aliases differ")
    project = work / "indexed-project"
    report = project_workflow(lambda *args: run(*args, cwd=project), project, fixtures, target,
                              indexed_replacements=True)
    return {"fixture_cases": sorted(INDEXED_REPLACEMENT_FIXTURES), "pure_cases": cases, "project": report,
            "assertions": ["before/after Int 1; generic/Any and alias isolation; caught failures",
                           "root/index/RHS order and once-only I/O; static non-execution; pure refusal",
                           "target/index/replacement diagnostics; no runtime payload leak",
                           "canonical comments; raw continuation; retained aliases; accepted/stale project Int 8"]}


def if_expressions(run, work: Path, fixtures: Path, target: str) -> dict:
    original = fixtures / "if-expressions"
    destination = work / "if-expressions"
    require({path.name for path in original.iterdir()} == set(IF_EXPRESSION_FIXTURES),
            "if expression fixture census differs")
    shutil.copytree(original, destination)

    def invoke(*arguments, **options):
        return run(*arguments, cwd=destination, **options)

    cases = {}
    for name in ("before", "after"):
        source, driver = name + ".opaal", name + "-driver.opaal"
        success(invoke("check", source), "conditional source check")
        success(invoke("format", "--check", source, driver), "conditional canonical bytes")
        test = json.loads(success(invoke("test", source, "--case", "test_choice", "--format", "json"),
                                  "conditional pure test"))
        require(test["counts"]["discovered"] == test["counts"]["selected"] == test["counts"]["passed"] == 1 and
                test["counts"]["failed"] == test["counts"]["error"] == test["counts"]["unrun"] == 0 and
                test["completeness"] == "complete", "conditional pure result differs")
        require(success(invoke(driver), "conditional driver") == b"", "conditional driver printed output")
        cases[name] = test["counts"]
    after = destination / "after.opaal"
    after.write_bytes(after.read_bytes().replace(b'"fail"\n    }}', b'"wrong"\n    }}', 1))
    corrupted = invoke("test", "after.opaal", "--case", "test_choice", "--format", "json")
    require(corrupted.returncode == 1 and not corrupted.stderr and
            json.loads(corrupted.stdout)["counts"]["failed"] == 1, "conditional test accepted changed result")
    require(invoke("after-driver.opaal").returncode == 1, "conditional driver accepted changed result")

    require(success(invoke("routes.opaal"), "conditionally selected outcomes") == b"", "routes printed output")
    routes = destination / "routes.opaal"
    routes.write_bytes(routes.read_bytes().replace(b"let selected = if true", b"let selected = if false", 1))
    require(invoke("routes.opaal").returncode == 1, "routes accepted an unselected abort")
    for name, code in (("wrong-type.opaal", b"SIG025"), ("invalid.opaal", b"OP1000"),
                       ("incomplete.opaal", b"SYN002")):
        result = invoke("check", name)
        require(result.returncode == 1 and not result.stdout and code in result.stderr,
                "conditional source diagnostic differs")
    success(invoke("check", "wrong-condition.opaal"), "runtime condition admission")
    wrong = invoke("wrong-condition.opaal")
    require(wrong.returncode == 1 and not wrong.stdout and b"RUN001" in wrong.stderr,
            "conditional runtime condition diagnostic differs")
    success(invoke("format", "--write", "noncanonical.opaal"), "conditional format")
    require((destination / "noncanonical.opaal").read_bytes() == (destination / "canonical.opaal").read_bytes(),
            "conditional canonical bytes differ")
    success(invoke("format", "--check", "noncanonical.opaal"), "conditional idempotence")
    pending = invoke(stdin=(destination / "incomplete.opaal").read_bytes())
    require(pending.returncode == 0 and pending.stdout == b">> ...> " and pending.stderr.count(b"SYN002") == 1,
            "conditional pending EOF differs")
    raw = invoke(stdin=(destination / "raw-input.opaal").read_bytes())
    require(raw.returncode == 0 and not raw.stderr and raw.stdout.count(b"...> ") == 1 and
            raw.stdout.count(b"fail\n") == raw.stdout.count(b"2\n") == 1, "conditional raw publication differs")
    project = work / "if-project"
    report = project_workflow(lambda *args: run(*args, cwd=project), project, fixtures, target,
                              value_conditionals=True)
    return {"fixture_cases": sorted(IF_EXPRESSION_FIXTURES), "pure_cases": cases, "project": report}


def typed_tuples(run, fixtures: Path) -> dict:
    folder = fixtures / "typed-tuples"
    require({path.name for path in folder.iterdir()} == set(TYPED_TUPLE_FIXTURES),
            "typed tuple fixture census differs")
    for name in ("before.opaal", "after.opaal", "command-commas.opaal"):
        success(run("check", name, cwd=folder), "typed tuple source check")
    success(run("format", "--check", *TYPED_TUPLE_FIXTURES, cwd=folder), "typed tuple canonical bytes")
    for name in ("before.opaal", "after.opaal"):
        require(success(run(name, cwd=folder), "typed tuple driver") == b"", "typed tuple driver printed output")
    for name, code, detail in (("wrong-pattern.opaal", b"error[SIG017]", b"(Int, String)"),
                               ("wrong-result.opaal", b"error[SIG005]", b"(String, String)")):
        result = run("check", name, cwd=folder)
        require(result.returncode == 1 and not result.stdout and code in result.stderr and detail in result.stderr,
                "typed tuple diagnostic differs")
    raw = run(stdin=b'let (count, label) = (3,\n    "ready")\nlabel\n', cwd=folder)
    require(raw.returncode == 0 and not raw.stderr and raw.stdout == b">> ...> >> ready\n>> ",
            "typed tuple raw continuation differs")
    return {"fixture_cases": sorted(TYPED_TUPLE_FIXTURES), "continuation": True}


def indexed_iteration(run, work: Path, fixtures: Path) -> dict:
    original = fixtures / "indexed-iteration"
    folder = work / "indexed-iteration"
    require({path.name for path in original.iterdir()} == set(INDEXED_ITERATION_FIXTURES),
            "indexed iteration fixture census differs")
    shutil.copytree(original, folder)
    driver = "indexed-and-paired.opaal"
    success(run("check", driver, cwd=folder), "loop source check")
    require(success(run(driver, cwd=folder), "loop driver") == b"", "loop driver printed output")
    success(run("format", "--check", driver, "bad-element.opaal", cwd=folder), "canonical loop source")
    success(run("format", "--write", "noncanonical.opaal", cwd=folder), "format loop headers")
    require((folder / "noncanonical.opaal").read_bytes() == (folder / driver).read_bytes(),
            "indexed iteration formatter bytes differ")
    result = run("check", "--format", "json", "bad-element.opaal", cwd=folder)
    require(result.returncode == 1 and not result.stderr, "bad loop element check exit differs")
    diagnostics = json.loads(result.stdout)["diagnostics"]
    require(len(diagnostics) == 1 and diagnostics[0]["code"] == "SIG017" and
            diagnostics[0]["primary_message"] == "this pattern cannot match `Int`" and
            diagnostics[0]["range"] == {"start": 4, "end": 17}, "loop element diagnostic differs")
    raw = run(stdin=(
        b"def rows() -> List[(List[Int], String)] { return [([], 'a'), ([], 'b')] }\n"
        b"def score[T](items: List[T]) -> Int { return 4 }\n"
        b"mut total = 0\nfor i, (items,\n_) in rows() { total += i + score(items) }\ntotal\n"
    ), cwd=folder)
    require(raw.returncode == 0 and not raw.stderr and raw.stdout == b">> >> >> >> ...> >> 9\n>> ",
            "indexed loop raw continuation differs")
    raw = run(stdin=b"for i, (i, _) in [] {}\nfor i, item in [] { item = i }\n", cwd=folder)
    require(raw.returncode == 0 and b"error[MOD010]" in raw.stderr and b"error[BND001]" in raw.stderr,
            "empty indexed loop binding diagnostics differ")
    return {"fixture_cases": sorted(INDEXED_ITERATION_FIXTURES), "continuation": True}


def list_comprehensions(run, work: Path, fixtures: Path, target: str) -> dict:
    original = fixtures / "list-comprehensions"
    require({path.name for path in original.iterdir()} == set(LIST_COMPREHENSION_FIXTURES),
            "list comprehension fixture census differs")
    lock = {"linux-aarch64": "tools-linux-aarch64.toml", "macos-arm64": "tools-macos.toml"
            }.get(target, "tools-linux.toml")
    outcome_digests = {}
    for spelling in ("loop", "comprehension"):
        folder = work / ("report-" + spelling)
        shutil.copytree(original, folder)
        for name in ("opaal.toml", "authority.toml", lock):
            shutil.copyfile(fixtures / "project" / name, folder / ("tools.toml" if name == lock else name))
        policy = folder / "policy.opaal"
        tasks = folder / "tasks.opaal"
        if spelling == "loop":
            original_policy = policy.read_bytes()
            start = original_policy.index(b"export def report(")
            end = original_policy.index(b"\nexport def summarize(", start)
            policy.write_bytes(original_policy[:start] + (folder / "loop-report.opaal").read_bytes() + original_policy[end:])

        def invoke(*arguments, **options):
            return run(*arguments, cwd=folder, **options)

        for args in (("format", "--check", "policy.opaal", "report.opaal", "negative.opaal", "bad-input.opaal"),
                     ("check", "report.opaal"), ("report.opaal",), ("check", "negative.opaal")):
            require(success(invoke(*args), f"{spelling} report") == b"", f"{spelling} report wrote output")
        for args, code, message in (
            (("negative.opaal",), b"RUN001", b"negative finding count"),
            (("check", "bad-input.opaal"), b"SIG004", b"List[Int]"),
        ):
            result = invoke(*args)
            require(result.returncode == 1 and not result.stdout and code in result.stderr and message in result.stderr,
                    f"{spelling} report diagnostic differs")
        inspect = json.loads(success(invoke("task", "inspect", "--project", "opaal.toml", "report",
                                            "--format", "json"), "inspect report"))
        identity = f"{policy}::verify"
        inspection, = inspect["tasks"]
        require(inspection["id"] == "formatting_example::report" and inspection["action"] == identity
                and inspection["result"] == "Int" and inspection["parameters"] == inspection["effects"] == [],
                "report inspection action or effects differ")
        options = ("--project", "opaal.toml", "--task", "report", "--environment", "ci")
        require(success(invoke("check", *options), "check pure report") == b"", "report check wrote output")

        def plan(label):
            path = f"{label}.plan.json"
            receipt = success(invoke("plan", *options, "--expires-in", "900s", "--out", path,
                                     "--format", "digest"), "plan pure report").decode().strip()
            sealed = json.loads((folder / path).read_bytes())
            require(receipt == sealed["digest"], "report plan receipt differs")
            expected_sources = {str(path): ("sha256:" + digest(path.read_bytes()), path.stat().st_size)
                                for path in (policy, tasks)}
            require({row["module"]: (row["digest"], row["size"]) for row in sealed["sources"]} == expected_sources,
                    "report plan source census or digest differs")
            action, = sealed["actions"]
            require(sealed["task"]["action_id"] == action["action_id"] == identity
                    and action["contract_digest"] == sealed["task"]["contract_digest"]
                    and action["id"] == action["contract_digest"] + "#000000"
                    and action["dependencies"] == action["requests"] == sealed["authority"]["requests"] == [],
                    "report action node or requests differ")
            success(invoke("plan", "inspect", path), "inspect sealed report plan")
            return sealed

        def execute_report(label, sealed, journal):
            return invoke("execute", "--plan", f"{label}.plan.json", "--accept", sealed["digest"],
                          "--journal", journal)

        def audit(label, sealed):
            journal = f"{label}.run.jsonl"
            success(execute_report(label, sealed, journal), "execute accepted report")
            path = f"{label}.audit.json"
            success(invoke("audit", "--project", "opaal.toml", "--journal", journal, "--out", path),
                    "audit accepted report")
            success(invoke("audit", "inspect", path), "inspect report audit")
            report = json.loads((folder / path).read_bytes())
            value_digest = "sha256:" + digest(b"2")
            require(report["plan_digest"] == report["accepted_plan_digest"] == sealed["digest"]
                    and report["completeness"] == "complete" and report["cleanup"] == []
                    and report["primary"]["class"] == "success" and report["primary"]["partial"] is False
                    and report["primary"]["value_digest"] == value_digest
                    and [event["kind"] for event in report["events"]] == ["action-start", "action-end", "terminal"],
                    "report result digest or audit events differ")
            start, end, _ = report["events"]
            action, = sealed["actions"]
            require(start["payload"]["contract_digest"] == action["contract_digest"]
                    and start["payload"]["action_node_id"] == end["payload"]["action_node_id"] == action["id"],
                    "report audit action provenance differs")
            return value_digest

        sealed = plan("report")
        require(sealed["task"]["contract_digest"] == inspection["contract_digest"]
                and not list(folder.glob("*.run.jsonl")), "inert report commands created a journal or changed identity")
        initial = audit("report", sealed)
        policy.write_bytes(policy.read_bytes() + b"\n# changed after acceptance\n")
        stale = execute_report("report", sealed, "stale.run.jsonl")
        require(stale.returncode == 1 and b"EXECUTE_STALE" in stale.stderr and not (folder / "stale.run.jsonl").exists(),
                "stale comprehension plan reached journal creation")
        fresh = plan("fresh")
        require(fresh["digest"] != sealed["digest"] and
                fresh["task"]["contract_digest"] != sealed["task"]["contract_digest"],
                "changed report source retained the old plan or contract digest")
        outcome_digests[spelling] = [initial, audit("fresh", fresh)]

    for args in (("check", "typing-and-scope.opaal"), ("typing-and-scope.opaal",)):
        require(success(invoke(*args), "comprehension type and scope evidence") == b"", "typing fixture wrote output")
    require(success(invoke("order.opaal"), "comprehension traversal order") ==
            b"source\n0\n2\nproject 1\n0\n1\nproject 3\n", "source, filter or projection order differs")
    effect = invoke("test", "skipped-effect.opaal", "--format", "json")
    require(effect.returncode == 2 and not effect.stderr and b"skipped projection" not in effect.stdout
            and json.loads(effect.stdout)["failure"] == "TEST_EFFECT",
            "pure checking admitted or executed a skipped projection effect")
    bad_filter = invoke("check", "--format", "json", "bad-filter.opaal")
    diagnostics = json.loads(bad_filter.stdout)["diagnostics"]
    require(bad_filter.returncode == 1 and not bad_filter.stderr and len(diagnostics) == 1
            and diagnostics[0]["code"] == "SIG029" and diagnostics[0]["range"] == {"start": 35, "end": 36},
            "bad comprehension filter diagnostic differs")
    raw = invoke(stdin=b"[(i, n * 2)\n for i, n in 2..=4\n if n > 2]\n"
                       b"let empty = [n * 2 for n in 1..1]\n"
                       b"def keep[T](rows: List[T]) -> List[T] { return rows }\nkeep((empty))\n")
    require(raw.returncode == 0 and not raw.stderr and raw.stdout ==
            b">> ...> ...> [(1, 6), (2, 8)]\n>> >> >> []\n>> ", "comprehension prompt results differ")
    incomplete = invoke(stdin=b"[n for n in [] if true\n")
    require(incomplete.returncode == 0 and b"...> " in incomplete.stdout and b"SYN002" in incomplete.stderr,
            "comprehension prompt EOF differs")
    return {"fixture_cases": sorted(LIST_COMPREHENSION_FIXTURES),
            "report": {"positions": [1, 3], "rows": 2}, "failure_codes": ["RUN001", "SIG004", "SIG029"],
            "continuation": True, "stale_refused_before_journal": True, "result_digests": outcome_digests,
            "typing_and_scope": True}


def list_map(run, work: Path, fixtures: Path, target: str) -> dict:
    source = fixtures / "list-map"
    require({path.name for path in source.iterdir()} == set(LIST_MAP_FIXTURES),
            "List map fixture census differs")
    success(run("format", "--check", *(source / name for name in LIST_MAP_FIXTURES)), "List map format")
    drivers = ["explicit.opaal", "inferred.opaal", "typed-empty.opaal", "grouped.opaal"]
    for name in drivers:
        require(success(run("check", source / name), name) == b""
                and success(run(source / name), name) == b"", f"{name}: unexpected output")
    diagnostics = {}
    for name, code, detail, start, end in (
        ("missing.opaal", "SIG010", "`T`", 26, 52),
        ("missing-result.opaal", "SIG010", "`U`", 47, 76),
        ("parameter-conflict.opaal", "OPR003", "String", 41, 56),
        ("result-conflict.opaal", "SIG005", "String", 61, 66),
        ("incompatible-consumer.opaal", "SIG004", "List[String]", 160, 165),
    ):
        result = run("check", "--format", "json", source / name)
        diagnostic, = json.loads(result.stdout)["diagnostics"]
        require(result.returncode == 1 and not result.stderr and diagnostic["code"] == code
                and detail in diagnostic["message"] and diagnostic["range"] == {"start": start, "end": end},
                f"{name}: List map diagnostic or range differs")
        execution = run(source / name)
        require(execution.returncode == 1 and not execution.stdout and f"error[{code}]".encode() in execution.stderr,
                f"{name}: script admitted rejected source")
        diagnostics[name] = {"code": code, "range": diagnostic["range"]}
    grouped = work / "grouped-map.opaal"
    shutil.copyfile(source / "grouped.opaal", grouped)
    success(run("format", "--write", grouped), "format grouped callback")
    require(grouped.read_bytes() == (source / "grouped.opaal").read_bytes(), "grouped callback or comment changed")

    prompt = run(stdin=b"import std::list as list\nlist::map(\n[1, 2],\n{|x| x + 1}\n)\n"
                 b"let counts: List[Int] = []\nlet marks = list::map(counts, {|x| x + 1})\n"
                 b"list::map(marks, {|x| x * 2})\nlist::map([], {|x| x + 1})\nlist::map(marks, {|x| x})\n")
    require(prompt.returncode == 0 and prompt.stdout == b">> >> ...> ...> ...> [2, 3]\n>> >> >> []\n>> >> []\n>> "
            and prompt.stderr.count(b"error[") == 1 and b"error[RUN001]: cannot infer type argument `T`" in prompt.stderr,
            "List map prompt continuation, retained binding or recovery differs")

    folder = work / "mapped-task"
    shutil.copytree(fixtures / "project", folder)
    lock = {"macos-arm64": "tools-macos.toml", "linux-aarch64": "tools-linux-aarch64.toml",
            "linux-x86_64": "tools-linux.toml"}[target]
    shutil.copyfile(folder / lock, folder / "tools.toml")
    policy, tasks = folder / "policy.opaal", folder / "tasks.opaal"
    shutil.copyfile(source / "tasks.opaal", tasks)
    base = ("--project", "opaal.toml", "--task", "mapped", "--environment", "ci")
    identity = f"{policy}::verify"

    def invoke(*args):
        return run(*args, cwd=folder)

    def plan(label):
        journal_paths = sorted(folder.glob("*.run.jsonl"))
        inspection, = json.loads(success(invoke("task", "inspect", "--project", "opaal.toml", "mapped",
                                               "--format", "json"), "inspect mapped task"))["tasks"]
        require(inspection["id"] == "formatting_example::mapped" and inspection["action"] == identity
                and inspection["result"] == "Int" and inspection["parameters"] == inspection["effects"] == [],
                "mapped task inspection differs")
        require(success(invoke("check", *base), "check mapped task") == b"", "mapped task check wrote output")
        path = f"{label}.plan.json"
        accepted = success(invoke("plan", *base, "--expires-in", "900s", "--out", path,
                                  "--format", "digest"), "plan mapped task").decode().strip()
        sealed = json.loads((folder / path).read_bytes())
        success(invoke("plan", "inspect", path), "inspect sealed mapped task")
        require(sorted(folder.glob("*.run.jsonl")) == journal_paths, "planning mapped task created a journal")
        expected = {str(path): ("sha256:" + digest(path.read_bytes()), path.stat().st_size)
                    for path in (policy, tasks)}
        observed = [row for row in sealed["observations"] if row["kind"] == "source"]
        require(len(sealed["sources"]) == len(observed) == len(expected)
                and {row["module"]: (row["digest"], row["size"]) for row in sealed["sources"]} == expected
                and {row["id"]: (row["digest"], row["size"]) for row in observed} == expected,
                "mapped task source census and bytes differ")
        action, = sealed["actions"]
        contract = inspection["contract_digest"]
        require(sealed["digest"] == accepted and sealed["task"]["action_id"] == action["action_id"] == identity
                and sealed["task"]["contract_digest"] == action["contract_digest"] == contract
                and action["id"] == contract + "#000000" and action["dependencies"] == action["requests"] ==
                sealed["authority"]["requests"] == sealed["tools"] == sealed["secrets"] == [],
                "mapped task plan identity or requests differ")
        return sealed

    def execute(label, sealed, journal):
        return invoke("execute", "--plan", f"{label}.plan.json", "--accept", sealed["digest"], "--journal", journal)

    def audit(label, sealed, *, fails=False):
        journal = f"{label}.run.jsonl"
        require(not (folder / journal).exists(), "pre-execution journal exists")
        result = execute(label, sealed, journal)
        require(result.returncode == int(fails), f"{label}: mapped task execution status differs")
        audit_path = f"{label}.audit.json"
        success(invoke("audit", "--project", "opaal.toml", "--journal", journal, "--out", audit_path), "audit mapped task")
        success(invoke("audit", "inspect", audit_path), "inspect mapped task audit")
        report = json.loads((folder / audit_path).read_bytes())
        primary = report["primary"]
        value_digest = None if fails else "sha256:" + digest(b"3")
        require(report["plan_digest"] == report["accepted_plan_digest"] == sealed["digest"]
                and report["completeness"] == "complete" and report["cleanup"] == []
                and primary["class"] == ("error" if fails else "success") and primary["partial"] is False
                and primary["value_digest"] == value_digest
                and (not fails or primary["code"] == "EXECUTE_LANGUAGE")
                and [event["kind"] for event in report["events"]] == ["action-start", "action-end", "terminal"],
                f"{label}: mapped task outcome or journal events differ")
        start, end, terminal = (event["payload"] for event in report["events"])
        action, = sealed["actions"]
        require(start == {"action_id": identity, "action_node_id": action["id"], "contract_digest": action["contract_digest"]}
                and end["action_node_id"] == action["id"]
                and all(end["outcome"][key] == primary[key] for key in ("class", "partial", "value_digest"))
                and terminal["primary"] == primary and terminal["complete"], "mapped task audit provenance differs")
        return {"plan": sealed["digest"], "action_node": action["id"], "outcome": primary["code"], "result": value_digest}

    inferred = (source / "policy.opaal").read_bytes()
    policy.write_bytes(inferred.replace(b"{|x|", b"{|x| -> Int"))
    reports = {"explicit": audit("explicit", plan("explicit"))}
    policy.write_bytes(inferred)
    sealed = plan("inferred")
    reports["inferred"] = audit("inferred", sealed)
    policy.write_bytes(inferred + b"\n# Changed after acceptance.\n")
    stale = execute("inferred", sealed, "stale.run.jsonl")
    require(stale.returncode == 1 and b"EXECUTE_STALE" in stale.stderr and not (folder / "stale.run.jsonl").exists(),
            "changed List map source reached the stale journal")
    reports["replanned"] = audit("replanned", plan("replanned"))
    require(reports["replanned"]["action_node"] != reports["inferred"]["action_node"],
            "source edit retained the old action identity")
    helper = b'def fail(x: Int) -> Int\n{\n    throw "map callback reached execution"\n}\n\n'
    policy.write_bytes(inferred.replace(b"export action", helper + b"export action", 1)
                       .replace(b"x + 1", b"fail(x)"))
    reports["throwing"] = audit("throwing", plan("throwing"), fails=True)
    return {"drivers": drivers, "diagnostics": diagnostics, "project": reports,
            "stale_refused_before_journal": True, "piped_continuation_and_recovery": True}


def list_string_slicing(run, work: Path, fixtures: Path, target: str) -> dict:
    source = fixtures / "list-string-slicing"
    require({path.name for path in source.iterdir()} == set(SLICE_FIXTURES), "slice fixture census differs")
    folder = work / "slice-preview"
    shutil.copytree(fixtures / "project", folder)
    lock = {"macos-arm64": "tools-macos.toml", "linux-aarch64": "tools-linux-aarch64.toml",
            "linux-x86_64": "tools-linux.toml"}[target]
    shutil.copyfile(folder / lock, folder / "tools.toml")
    for name in SLICE_FIXTURES:
        shutil.copyfile(source / name, folder / name)

    def invoke(*arguments, **options):
        return run(*arguments, cwd=folder, **options)

    excluded = {"incomplete.opaal", "invalid.opaal", "noncanonical.opaal"}
    require(success(invoke("format", "--check", *(name for name in SLICE_FIXTURES if name not in excluded)),
                    "canonical slice fixture format") == b"", "slice format check wrote output")
    for name, code in (("invalid.opaal", b"OP1000"), ("incomplete.opaal", b"SYN002")):
        result = invoke("format", "--write", name)
        require(result.returncode == 1 and not result.stdout and b"error[" + code + b"]" in result.stderr
                and (folder / name).read_bytes() == (source / name).read_bytes(), f"slice {name} changed")
    require(invoke("format", "--check", "noncanonical.opaal").returncode == 1,
            "slice format check accepted noncanonical source")
    success(invoke("format", "--write", "noncanonical.opaal", "compatibility.opaal"), "format slice fixtures")
    require((folder / "noncanonical.opaal").read_bytes() == (source / "canonical.opaal").read_bytes(),
            "slice formatting or comment bytes differ")
    require((folder / "compatibility.opaal").read_bytes() == (source / "compatibility.opaal").read_bytes()
            and (folder / "compatibility.opaal").read_bytes().count(b"^true a:b:c && true") == 3,
            "slice formatter changed command words")
    for name in ("before.opaal", "preview.opaal", "typed-empty.opaal", "qualified-bounds.opaal"):
        require(success(invoke("check", name), name) == success(invoke(name), name) == b"",
                f"{name}: slice workflow wrote output")
    require(success(invoke("check", "compatibility.opaal"), "check slice compatibility") == b"",
            "passive compatibility check wrote output")

    diagnostics = {}
    for name, code, spelling in (("bad-target.opaal", "SIG031", b"receiver"),
                                 ("bad-operand.opaal", "SIG032", b'"three"')):
        result = invoke("check", "--format", "json", name)
        diagnostic, = json.loads(result.stdout)["diagnostics"]
        start = (source / name).read_bytes().rindex(spelling)
        require(result.returncode == 1 and not result.stderr and diagnostic["code"] == code
                and diagnostic["range"] == {"start": start, "end": start + len(spelling)},
                f"{name}: slice operand diagnostic differs")
        diagnostics[name] = {"code": code, "range": diagnostic["range"]}
    for name, line, column in (("past-end.opaal", 2, 23), ("rethrow.opaal", 3, 27)):
        require(success(invoke("check", name), name) == b"", "slice interval was checked too early")
        result = invoke(name)
        require(result.returncode == 1 and not result.stdout and
                b"error[RUN001]: slice: endpoint 3 is outside length 2" in result.stderr and
                f"/{name}:{line}:{column}\n".encode() in result.stderr, f"{name}: slice runtime span differs")
    effect = invoke("test", "effect.opaal", "--format", "json")
    require(effect.returncode == 2 and not effect.stderr and b"unselected endpoint" not in effect.stdout
            and json.loads(effect.stdout)["failure"] == "TEST_EFFECT", "slice operand effect reached a pure test")
    prompt = invoke(stdin=b"import std::list as list\nlet counts: List[Int] = [10, 20, 30]\n"
                         b"let empty = counts[\n3:3\n]\nlist::map(empty, {|count| count + 1})\n"
                         b"counts[1:4]\ncounts[1:3]\n")
    require(prompt.returncode == 0 and prompt.stdout == b">> >> >> ...> ...> >> []\n>> >> [20, 30]\n>> "
            and prompt.stderr.count(b"error[") == 1 and b"endpoint 4 is outside length 3" in prompt.stderr,
            "slice prompt continuation, retained type or recovery differs")
    eof = invoke(stdin=b"let empty = [10][0:\n")
    require(eof.returncode == 0 and b"...> " in eof.stdout and b"SYN002" in eof.stderr,
            "slice prompt incomplete EOF differs")

    options = ("--project", "opaal.toml", "--task", "preview", "--environment", "ci")
    policy, tasks = folder / "policy.opaal", folder / "tasks.opaal"

    def plan(label):
        journals = sorted(folder.glob("*.run.jsonl"))
        inspection, = json.loads(success(invoke("task", "inspect", "--project", "opaal.toml", "preview",
                                               "--format", "json"), "inspect slice preview"))["tasks"]
        require(inspection["id"] == "formatting_example::preview" and inspection["action"] == f"{policy}::verify"
                and inspection["result"] == "Int" and inspection["parameters"] == inspection["effects"] == [],
                "slice preview contract differs")
        require(success(invoke("check", *options), "check slice preview") == b"", "project check wrote output")
        path = f"{label}.plan.json"
        accepted = success(invoke("plan", *options, "--expires-in", "900s", "--out", path,
                                  "--format", "digest"), "plan slice preview").decode().strip()
        sealed = json.loads((folder / path).read_bytes())
        success(invoke("plan", "inspect", path), "inspect slice plan")
        action, = sealed["actions"]
        expected = {str(path): ("sha256:" + digest(path.read_bytes()), path.stat().st_size)
                    for path in (policy, tasks)}
        require(sealed["digest"] == accepted and
                sealed["task"]["contract_digest"] == action["contract_digest"] == inspection["contract_digest"] and
                sealed["task"]["action_id"] == action["action_id"] == inspection["action"] and
                {row["module"]: (row["digest"], row["size"]) for row in sealed["sources"]} == expected and
                action["requests"] == sealed["authority"]["requests"] == [] and
                sorted(folder.glob("*.run.jsonl")) == journals, "slice plan sources, effects or receipt differ")
        return accepted

    def execute(label, accepted, journal):
        return invoke("execute", "--plan", f"{label}.plan.json", "--accept", accepted, "--journal", journal)

    def audit(label, accepted, fails):
        journal = f"{label}.run.jsonl"
        result = execute(label, accepted, journal)
        require(result.returncode == int(fails), f"{label}: slice project exit differs: {result.stderr!r}")
        path = f"{label}.audit.json"
        success(invoke("audit", "--project", "opaal.toml", "--journal", journal, "--out", path), "audit slice preview")
        success(invoke("audit", "inspect", path), "inspect slice audit")
        receipt = json.loads((folder / path).read_bytes())
        primary = receipt["primary"]
        require(receipt["plan_digest"] == receipt["accepted_plan_digest"] == accepted
                and receipt["completeness"] == "complete" and receipt["cleanup"] == []
                and primary["class"] == ("error" if fails else "success") and primary["partial"] is False
                and primary["value_digest"] == (None if fails else "sha256:" + digest(b"20"))
                and (not fails or primary["code"] == "EXECUTE_LANGUAGE")
                and [event["kind"] for event in receipt["events"]] == ["action-start", "action-end", "terminal"],
                f"{label}: slice audit result, classification or events differ")
        return {"plan": accepted, "outcome": primary["code"], "result": primary["value_digest"]}

    initial = plan("preview")
    preview = audit("preview", initial, False)
    policy.write_bytes(policy.read_bytes().replace(b"counts[1:3]", b"counts[1:4]"))
    stale = execute("preview", initial, "stale.run.jsonl")
    require(stale.returncode == 1 and b"EXECUTE_STALE" in stale.stderr and not (folder / "stale.run.jsonl").exists(),
            "slice policy drift reached journal creation")
    changed = plan("past-end")
    require(changed != initial, "slice policy edit kept the old digest")
    return {"fixture_cases": sorted(SLICE_FIXTURES), "diagnostics": diagnostics, "prompt": True,
            "stale_refused_before_journal": True, "preview": preview, "past_end": audit("past-end", changed, True)}


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


def action_declarations(run, binary: Path, root: Path, fixtures: Path, target: str) -> dict:
    inputs = fixtures / "action-declarations"
    require({path.name for path in inputs.iterdir()} == set(ACTION_DECLARATION_FIXTURES),
            "action declaration fixture census differs")
    canonical = (inputs / "canonical.opaal").read_bytes()
    path = root / "action-format.opaal"
    path.write_bytes((inputs / "noncanonical.opaal").read_bytes())
    success(run("format", "--write", path), "contained action format")
    require(path.read_bytes() == canonical, "contained action canonical bytes differ")
    success(run("format", "--check", path), "contained action idempotence")
    success(run("check", path), "contained action check")
    for name in ("pure.opaal", "requests.opaal"):
        before = (inputs / name).read_bytes()
        expected = before
        if name == "requests.opaal":
            require(before.count(b"clock.wall() #") == 1, "expected one missing request comma")
            expected = before.replace(b"clock.wall() #", b"clock.wall(), #")
        path.write_bytes(before)
        success(run("format", "--write", path), f"contained action {name} format")
        require(path.read_bytes() == expected, f"{name}: contained action canonical bytes differ")
        success(run("format", "--check", path), f"contained action {name} idempotence")
    errors = {}
    for name in ACTION_DECLARATION_FIXTURES:
        if name in ("canonical.opaal", "noncanonical.opaal", "pure.opaal", "io.opaal", "requests.opaal"):
            continue
        before = (inputs / name).read_bytes()
        path.write_bytes(before)
        result = run("format", "--write", path)
        code = b"SYN002" if name == "incomplete.opaal" else b"OP1000"
        require(result.returncode == 1 and not result.stdout and code in result.stderr and path.read_bytes() == before,
                f"{name}: invalid action was rewritten or accepted")
        errors[name] = code.decode()
    interactive = run(stdin=canonical + b"identity('ready')\n"
                      b"action bad() -> Int { effects {} effects {} return 1 }\nbad()\nidentity('retained')\n")
    require(interactive.returncode == 0 and b"ready\n" in interactive.stdout and b"retained\n" in interactive.stdout
            and interactive.stderr.count(b"error[") == 2 and b"OP1000" in interactive.stderr,
            "contained action continuation or invalid-cell publication differs")
    pending = run(stdin=(inputs / "incomplete.opaal").read_bytes())
    require(pending.returncode == 0 and pending.stdout.count(b"...> ") == 4 and b"SYN002" in pending.stderr,
            "inner effects close completed an action")

    work = root / "action-stream"
    shutil.copytree(fixtures / "project", work)
    lock = "tools-macos.toml" if target == "macos-arm64" else "tools-linux-aarch64.toml" if target == "linux-aarch64" else "tools-linux.toml"
    shutil.copyfile(work / lock, work / "tools.toml")
    source = work / "tasks.opaal"
    stream = (inputs / "io.opaal").read_bytes()
    # The same bounded task is used for static, accepted, stale and failed transfers.
    source.write_bytes(stream.replace(b"    effects {", b"    effects{"))
    invoke = lambda *args, **kwargs: run(*args, cwd=work, **kwargs)
    base = ("--project", "opaal.toml", "--task", "echo", "--environment", "ci")
    denied = json.loads(invoke("check", *base, "--format", "json").stdout)
    require(denied["outcome"]["class"] == "refused" and
            {row["verdict"] for row in denied["authority"]["requests"]} == {"denied"},
            "stream declaration granted authority")
    authority = 'schema_version = 1\nproject = "formatting_example"\nenvironment = "ci"\n'
    for role in ("stdin.read", "stdout.write"):
        authority += f'[[rules]]\ndecision = "grant"\neffect = "{role}"\nscope = "evaluation"\nrequired_enforcement = "enforced"\n'
    (work / "authority.toml").write_text(authority)
    checked = json.loads(success(invoke("check", *base, "--format", "json"), "stream check"))
    require([row["effect"] for row in checked["authority"]["requests"]] == ["stdin.read", "stdout.write"],
            "stream normalized requests differ")
    success(invoke("plan", *base, "--expires-in", "900s", "--out", "old.plan.json"), "noncanonical stream plan")
    old = json.loads((work / "old.plan.json").read_bytes())
    success(invoke("format", "--write", source), "stream format")
    stale = invoke("execute", "--plan", "old.plan.json", "--accept", old["digest"], "--journal", "stale.jsonl", "--receipt-out", "stale.receipt.json")
    require(stale.returncode == 1 and b"EXECUTE_STALE" in stale.stderr and not (work / "stale.jsonl").exists(),
            "formatting accepted a stale stream plan")
    success(invoke("plan", "inspect", "old.plan.json"), "old plan inspection")
    success(invoke("plan", *base, "--expires-in", "900s", "--out", "plan.json"), "fresh stream plan")
    plan = json.loads((work / "plan.json").read_bytes())
    require(plan["task"]["contract_digest"] != old["task"]["contract_digest"] and not list(work.glob("*.jsonl")),
            "source migration identity or nonexecuting planning differs")
    inert = root / "action-inert.opaal"
    inert.write_bytes(stream.replace(b"task echo = echo_input\n", b""))
    result = run(inert, stdin=b"unconsumed")
    require(result.returncode == 0 and not result.stdout and not result.stderr, "declaration performed I/O")
    for name, payload, expected in (("empty", b"", b""), ("bytes", b"\x00\xffGr\xc3\xbc\xc3\x9fe\n", b"\x00\xffGr\xc3\xbc\xc3\x9fe\n"),
                                    ("cap", b"a" * 4096, b"a" * 4096), ("excess", b"a" * 4097, b"")):
        result = invoke("execute", "--plan", "plan.json", "--accept", plan["digest"], "--journal", name + ".jsonl",
                        "--receipt-out", name + ".receipt.json", stdin=payload)
        receipt = json.loads((work / (name + ".receipt.json")).read_bytes())
        require(result.returncode == (1 if name == "excess" else 0) and result.stdout == expected and not result.stderr
                and receipt["primary"]["class"] == ("error" if name == "excess" else "success")
                and receipt["progress"]["stdin.read"]["confirmed_bytes"] == len(payload)
                and receipt["progress"]["stdout.write"]["confirmed_bytes"] == len(expected),
                f"contained stream {name}: bytes/outcome/progress differ")
        success(invoke("audit", "--project", "opaal.toml", "--journal", name + ".jsonl", "--out", name + ".audit.json"), "stream audit")
        audit = json.loads((work / (name + ".audit.json")).read_bytes())
        require(audit["primary"] == receipt["primary"] and audit["completeness"] == "complete", "stream audit differs")
    reader, writer = os.pipe()
    os.close(reader)
    try:
        child = subprocess.Popen([str(binary), "execute", "--plan", "plan.json", "--accept", plan["digest"],
                                  "--journal", "broken.jsonl", "--receipt-out", "broken.receipt.json"],
                                 cwd=work, env={"PATH": os.defpath}, stdin=subprocess.PIPE, stdout=writer, stderr=subprocess.PIPE)
    finally:
        os.close(writer)
    _, diagnostics = child.communicate(b"hello", timeout=30)
    receipt = json.loads((work / "broken.receipt.json").read_bytes())
    require(child.returncode == 1 and not diagnostics and receipt["primary"]["class"] == "error"
            and receipt["primary"]["code"] == "EXECUTE_LANGUAGE" and receipt["primary"]["partial"] is True
            and receipt["progress"]["stdin.read"]["confirmed_bytes"] == 5
            and receipt["progress"]["stdout.write"]["confirmed_bytes"] == 0,
            "contained stream Broken Pipe outcome/progress differs")
    return {"fixtures": list(ACTION_DECLARATION_FIXTURES), "errors": errors,
            "assertions": ["one grammar and contextual effects name", "canonical bytes and no invalid rewrites",
                           "pure task formatting; final request comma precedes comment and preserves literal punctuation",
                           "outer-brace raw continuation; invalid cell publishes nothing", "declaration/check/plan perform no transfers",
                           "denied grants; exact requests", "formatting stales accepted identity; old plan inspectable; fresh plan succeeds",
                           "empty/binary/exact-cap/first-excess and Broken Pipe transfers with exact receipts and audit"]}


def inline_export_projects(run, work: Path, fixtures: Path, target: str) -> dict:
    source = fixtures / "inline-module-exports"
    lock = {"macos-arm64": "tools-macos.toml", "linux-aarch64": "tools-linux-aarch64.toml",
            "linux-x86_64": "tools-linux.toml"}[target]
    base = ("--project", "opaal.toml", "--task", "verify", "--environment", "ci")
    trials = {}
    outcomes = []
    for label, initial in (("grouped", "grouped-policy.opaal"), ("inline", "policy.opaal"),
                           ("stale", "policy.opaal")):
        folder = work / ("inline-export-project-" + label)
        shutil.copytree(fixtures / "project", folder)
        shutil.copyfile(folder / lock, folder / "tools.toml")
        shutil.copyfile(source / "project-tasks.opaal", folder / "tasks.opaal")
        policy = folder / "policy.opaal"
        original_action = str(policy) + "::ready"

        def invoke(*args):
            return run(*args, cwd=folder)

        for index, spelling in enumerate((initial, "grouped-policy.opaal") if label == "stale" else (initial,)):
            shutil.copyfile(source / spelling, policy)
            inspected = json.loads(success(invoke("task", "inspect", "--project", "opaal.toml",
                                                    "verify", "--format", "json"), "inline task inspect"))
            task, = inspected["tasks"]
            require(task["action"] == original_action and task["result"] == "Int"
                    and task["effects"] == task["parameters"] == [], "inline task identity/signature differs")
            success(invoke("check", *base), "inline task check")
            plan_name = f"accepted-{index}.plan.json"
            accepted = success(invoke("plan", *base, "--expires-in", "900s", "--out", plan_name,
                                      "--format", "digest"), "inline task plan").decode().strip()
            plan = json.loads((folder / plan_name).read_bytes())
            success(invoke("plan", "inspect", plan_name), "inline plan inspect")
            expected_sources = {
                str(path): ("sha256:" + digest(path.read_bytes()), path.stat().st_size)
                for path in (policy, folder / "tasks.opaal")
            }
            require({row["module"]: (row["digest"], row["size"]) for row in plan["sources"]} == expected_sources
                    and {row["id"]: (row["digest"], row["size"]) for row in plan["observations"]
                         if row["kind"] == "source"} == expected_sources, "inline task source closure differs")
            action, = plan["actions"]
            node = task["contract_digest"] + "#000000"
            require(plan["digest"] == accepted and plan["task"]["action_id"] == action["action_id"] == original_action
                    and plan["task"]["contract_digest"] == action["contract_digest"] == task["contract_digest"]
                    and action["id"] == node and action["dependencies"] == action["requests"] ==
                    plan["tools"] == plan["secrets"] == plan["authority"]["requests"] == [],
                    "inline plan action, identity or authority differs")
            result = {"plan": accepted, "node": node, "policy": expected_sources[str(policy)][0]}
            trials[label + ("-fresh" if index else "")] = result
            journal = f"accepted-{index}.run.jsonl"
            if label == "stale" and index == 0:
                shutil.copyfile(source / "grouped-policy.opaal", policy)
                refused = invoke("execute", "--plan", plan_name, "--accept", accepted, "--journal", journal)
                require(refused.returncode == 1 and not refused.stdout and b"EXECUTE_STALE" in refused.stderr
                        and not (folder / journal).exists(), "inline spelling edit accepted a stale plan")
                result["failure"] = "EXECUTE_STALE"
                continue

            success(invoke("execute", "--plan", plan_name, "--accept", accepted, "--journal", journal),
                    "inline accepted task")
            success(invoke("audit", "--project", "opaal.toml", "--journal", journal,
                           "--out", "accepted.audit.json"), "inline task audit")
            success(invoke("audit", "inspect", "accepted.audit.json"), "inline audit inspect")
            audit = json.loads((folder / "accepted.audit.json").read_bytes())
            primary, events = audit["primary"], audit["events"]
            require(audit["completeness"] == "complete" and audit["cleanup"] == []
                    and audit["plan_digest"] == audit["accepted_plan_digest"] == accepted
                    and primary["class"] == "success" and primary["partial"] is False
                    and primary["value_digest"] == "sha256:" + digest(b"14")
                    and [event["kind"] for event in events] == ["action-start", "action-end", "terminal"],
                    "inline task result or events differ")
            require(events[0]["payload"] == {"action_node_id": node, "action_id": original_action,
                                              "contract_digest": task["contract_digest"]}
                    and events[1]["payload"]["action_node_id"] == node, "inline audit action identity differs")
            result["value_digest"] = primary["value_digest"]
            result["events"] = [event["kind"] for event in events]
            outcomes.append((task["id"], task["result"], primary, events[1]["payload"]["outcome"]))
    require(all(outcome == outcomes[0] for outcome in outcomes), "grouped and inline task outcomes differ")
    require(all(trials["stale"][key] != trials["stale-fresh"][key] for key in ("plan", "node", "policy")),
            "inline spelling edit did not change source-bound identities")
    return trials


def inline_module_exports(run, work: Path, fixtures: Path, target: str) -> dict:
    original = fixtures / "inline-module-exports"
    require({path.name for path in original.iterdir()} == set(INLINE_EXPORT_FIXTURES),
            "inline export fixture census differs")
    destination = work / "inline-module-exports"
    tests = (("check-policy.opaal", "test_total", 0, None), ("test-signature.opaal", "test_bad", 2, "TEST_SIGNATURE"),
             ("test-effect.opaal", "test_effect", 2, "TEST_EFFECT"))
    inventories = {}
    for spelling in ("inline", "grouped"):
        folder = destination / spelling
        shutil.copytree(original, folder)
        if spelling == "grouped":
            shutil.copyfile(folder / "grouped-policy.opaal", folder / "policy.opaal")
        for args in (("format", "--check", "main.opaal", "facade.opaal", "policy.opaal"), ("check", "main.opaal"),
                     ("main.opaal",)):
            require(success(run(*args, cwd=folder), "inline module workflow") == b"", "unexpected module output")
        for name, case, code, failure in tests:
            path = folder / name
            if spelling == "grouped":
                path.write_bytes(f"export {{ {case} }}\n".encode() + path.read_bytes().replace(b"export ", b"", 1))
            module = {"encoding": "base64url-nopad", "platform": "unix",
                      "value": base64.urlsafe_b64encode(os.fsencode(path)).decode().rstrip("=")}
            for list_only in (True, False):
                result = run("test", *(("--list",) if list_only else ()), name, "--format", "json", cwd=folder)
                expected_exit = 0 if list_only and failure == "TEST_EFFECT" else code
                expected_failure = None if expected_exit == 0 else failure
                require(result.returncode == expected_exit and not result.stderr
                        and b"INLINE_EXPORT_PURITY_CANARY" not in result.stdout,
                        f"{spelling} {name} test status/redaction differs")
                view = json.loads(result.stdout)
                discovered, = view["cases"]
                require(view["failure"] == expected_failure and view["counts"]["discovered"] == 1
                        and discovered["callable_id"] == {"module": module, "name": case}
                        and view["counts"]["passed"] == int(not list_only and failure is None),
                        f"{spelling} {name} test inventory or outcome differs")
                inventories[f"{spelling}/{name}/{'list' if list_only else 'run'}"] = {
                    "name": case, "state": discovered["state"], "exit": expected_exit, "failure": expected_failure}
        prompt = run(cwd=folder, stdin=b"import './policy.opaal' as policy\nimport './policy.opaal'::{total}\n"
                     b"help total\npolicy::total()\nexport def local_total() -> Int\n{\n"
                     b"    return 5\n}\nlocal_total()\n")
        expected_location = b":9:12" if spelling == "inline" else b":10:5"
        require(prompt.returncode == 0 and not prompt.stderr and b"signature: def total() -> Int" in prompt.stdout
                and os.fsencode(folder / "policy.opaal") + expected_location in prompt.stdout
                and b"Calculate the report total." in prompt.stdout
                and re.findall(rb">> (14|5)\n", prompt.stdout) == [b"14", b"5"], "inline prompt help or values differ")

    folder = destination / "inline"
    require(success(run("check", "failed-init.opaal", cwd=folder), "passive initializer check") == b"",
            "passive initializer check produced output")
    failed = run(cwd=folder, stdin=b"let retained = 7\nimport './failed-init.opaal'::{good}\nretained\ngood\n")
    require(failed.returncode == 0 and b">> 7\n" in failed.stdout and failed.stderr.count(b"error[RUN001]") == 2
            and b"value does not match the pattern" in failed.stderr and b"unknown bare name" in failed.stderr,
            "failed inline initializer published a binding or lost prior state")
    refused = run("plan", "main.opaal", cwd=folder)
    require(refused.returncode == 1 and b"PLAN004" in refused.stderr, "source-only plan refusal differs")

    errors = {}
    for name, code, locations in (
        ("duplicate-exact", "MOD006", [(b"value", b"value")]),
        ("reverse-exact", "MOD006", [(b"value", b"value")]),
        ("duplicate-prefix", "MOD006", [(b"detail_*", b"detail_a"), (b"detail_*", b"detail_z")]),
        ("reverse-prefix", "MOD006", [(b"detail_z", b"detail_*"), (b"detail_a", b"detail_*")]),
        ("alias-conflict", "MOD011", [(b"model", b"model")]),
        ("member-conflict", "MOD011", [(b"value", b"value")]),
        ("private", "MOD014", [(b"hidden", b"hidden")]),
        ("invalid", "OP1000", []), ("incomplete", "SYN002", []),
    ):
        path = folder / (name + ".opaal")
        source = path.read_bytes()
        machine = run("check", "--format", "json", path, cwd=folder)
        human = run("check", path, cwd=folder)
        report = json.loads(machine.stdout)
        diagnostics = report["diagnostics"]
        require(report["outcome"] == "invalid" and machine.returncode == human.returncode == 1
                and not machine.stderr and not human.stdout and f"error[{code}]".encode() in human.stderr
                and [row["code"] for row in diagnostics] == [code] * max(1, len(locations)),
                f"{name} diagnostics differ")
        for diagnostic, (primary, related) in zip(diagnostics, locations):
            first = source.index(primary) if code == "MOD011" else source.rindex(primary)
            owner = folder / "policy.opaal" if code == "MOD014" else path
            related_source = owner.read_bytes()
            second = related_source.rindex(related) if code == "MOD011" else related_source.index(related)
            location, = diagnostic["related"]
            require(diagnostic["source"] == str(path)
                    and diagnostic["range"] == {"start": first, "end": first + len(primary)}
                    and location["source"] == str(owner)
                    and location["range"] == {"start": second, "end": second + len(related)}
                    and location["message"].encode() in human.stderr,
                    f"{name} diagnostic ranges or order differ")
        errors[name] = [{"code": row["code"], "range": row["range"],
                         "related": [location["range"] for location in row["related"]]} for row in diagnostics]
        if name in ("invalid", "incomplete"):
            require(run("format", "--write", path, cwd=folder).returncode == 1 and path.read_bytes() == source,
                    f"{name} inline format wrote source")
    pending = run(cwd=folder, stdin=(original / "incomplete.opaal").read_bytes())
    require(pending.returncode == 0 and b"...>" in pending.stdout and b"SYN002" in pending.stderr,
            "inline declaration EOF outcome differs")
    invalid = run(cwd=folder, stdin=b"export let unpublished = 99; export let [_, 1] = [2, 1]\nunpublished\n")
    require(invalid.returncode == 0 and b"99\n" not in invalid.stdout and b"OP1000" in invalid.stderr
            and b"unknown bare name" in invalid.stderr, "invalid inline submission published its parsed prefix")
    for _ in range(2):
        success(run("format", "--write", "noncanonical.opaal", cwd=folder), "inline format write")
        require((folder / "noncanonical.opaal").read_bytes() == (original / "policy.opaal").read_bytes(),
                "inline format differs from canonical bytes")
    large = folder / "large.opaal"
    large.write_text("".join(f"export let public_{i:04} = {i}\n" for i in range(1024)) + "export { public_* }\n")
    repeated = run("check", "--format", "json", large, cwd=folder)
    require(repeated.returncode == 1 and not repeated.stderr
            and [row["code"] for row in json.loads(repeated.stdout)["diagnostics"]] == ["MOD006"] * 1024,
            "1024 inline declarations did not report their overlapping prefix")
    return {"fixtures": list(INLINE_EXPORT_FIXTURES), "diagnostics": errors, "tests": inventories,
            "prompt_values": [14, 5], "failed_initializer_retained": 7, "bounded_duplicates": 1024,
            "project": inline_export_projects(run, work, fixtures, target)}


def inline_export_queries(send, receive, request, folder: Path, encoding: str) -> None:
    def opened(name):
        path = folder / name
        uri = path.as_uri()
        text = path.read_text()
        send({"method": "textDocument/didOpen", "params": {"textDocument": {
            "uri": uri, "languageId": "opaal", "version": 1, "text": text}}})
        diagnostics = receive((uri, 1))["params"]["diagnostics"]
        return uri, text.splitlines(), diagnostics

    def span(line, text, name):
        def units(prefix):
            return len(prefix.encode("utf-8")) if encoding == "utf-8" else len(prefix.encode("utf-16-le")) // 2
        start = text[line].index(name)
        return {"start": {"line": line, "character": units(text[line][:start])},
                "end": {"line": line, "character": units(text[line][:start + len(name)])}}

    uri, text, diagnostics = opened("reverse-exact.opaal")
    diagnostic, = diagnostics
    require(diagnostic["code"] == "MOD006" and diagnostic["range"] == span(1, text, "value")
            and [row["location"] for row in diagnostic["relatedInformation"]] ==
            [{"uri": uri, "range": span(0, text, "value")}],
            f"{encoding} inline MOD006 ranges differ")
    policy, policy_lines, diagnostics = opened("policy.opaal")
    require(not diagnostics, "inline policy has LSP diagnostics")
    main, lines, diagnostics = opened("main.opaal")
    require(not diagnostics, "inline importer has LSP diagnostics")

    def at(uri, line, character):
        return {"textDocument": {"uri": uri}, "position": {"line": line, "character": character}}

    binder = at(policy, 8, 11)
    declaration = {"uri": policy, "range": {"start": binder["position"],
                                           "end": {"line": 8, "character": 16}}}
    require(request("textDocument/definition", binder)["result"] == declaration,
            "inline function definition differs")
    hover = request("textDocument/hover", binder)["result"]["contents"]["value"]
    require("def total() -> Int" in hover and "Calculate the report total." in hover,
            "inline function hover differs")
    action = next(index for index, value in enumerate(policy_lines) if value.startswith("export action ready"))
    action_hover = request("textDocument/hover", at(policy, action, policy_lines[action].index("ready")))["result"]
    require("action ready() -> Int" in action_hover["contents"]["value"], "inline action hover differs")
    require(request("textDocument/definition", at(policy, 8, 0))["result"] is None
            and request("textDocument/hover", at(policy, 3, 24))["result"] is None,
            "inline modifier or ignored slot became a query target")
    without, with_declaration = [
        request("textDocument/references", {**binder, "context": {"includeDeclaration": include}})["result"]
        for include in (False, True)
    ]
    require(declaration not in without and with_declaration.count(declaration) == 1
            and [location for location in with_declaration if location != declaration] == without
            and any(location["uri"] == (folder / "facade.opaal").as_uri() for location in without),
            "inline function references lost origin or repeated the declaration")
    line = next(index for index, value in enumerate(lines) if "policy::detail_value" in value)
    completion = request("textDocument/completion", at(main, line, lines[line].index("detail_value")))["result"]
    labels = {item["label"] for item in completion}
    require(labels == {f"policy::{name}" for name in (
        "Choice", "Report", "detail_value", "left", "ready", "retries", "right", "threshold", "total"
    )}, f"inline completion public names differ: {sorted(labels)}")


def comprehension_queries(send, receive, request, work: Path, fixtures: Path, encoding: str) -> None:
    uri = (work / "comprehension-query.opaal").as_uri()
    lines = [
        "let item: List[Int] = [1, 2]",
        "let rows = [('界🙂', i, item) for i, item in item if (item > 0 && i >= 0)]",
        "let later = item",
        "let nested = [[item for item in [item]] for item in item]",
    ]
    text = "\n".join(lines) + "\n"
    document = {"uri": uri}
    send({"method": "textDocument/didOpen", "params": {"textDocument": {
        **document, "languageId": "opaal", "version": 1, "text": text}}})
    require(receive((uri, 1))["params"]["diagnostics"] == [], "comprehension source did not check")

    def position(line, column):
        prefix = lines[line][:column]
        character = len(prefix.encode("utf-8")) if encoding == "utf-8" else len(prefix.encode("utf-16-le")) // 2
        return {"line": line, "character": character}

    def location(line, column, length):
        return {"uri": uri, "range": {"start": position(line, column), "end": position(line, column + length)}}

    def query(method, line, column, **options):
        return request("textDocument/" + method, {"textDocument": document,
                                                   "position": position(line, column), **options})["result"]

    projection, header, iterable, guard = [match.start() for match in re.finditer(r"\bitem\b", lines[1])]
    binder = location(1, header, 4)
    outer = location(0, lines[0].index("item"), 4)
    for line, column, destination, value_type in (
        (1, projection, binder, "Int"), (1, guard, binder, "Int"), (1, header, binder, "Int"),
        (1, iterable, outer, "List[Int]"), (2, lines[2].index("item"), outer, "List[Int]"),
    ):
        require(query("definition", line, column) == destination, "comprehension name resolved to wrong declaration")
        require(query("hover", line, column)["contents"]["value"] == f"```opaal\nlet item: {value_type}\n```",
                "comprehension scope hover differs")
    for include in (False, True):
        references = query("references", 1, projection, context={"includeDeclaration": include})
        expected = [location(1, projection, 4), location(1, guard, 4)] + ([binder] if include else [])
        require(len(references) == len(expected) and all(site in references for site in expected),
                "comprehension references duplicate a declaration or cross its scope gap")
    for line, column, visible in ((1, projection, True), (1, guard, True),
                                  (1, iterable, False), (2, lines[2].index("item"), False)):
        labels = {item["label"] for item in query("completion", line, column + 1)}
        require("item" in labels and ("i" in labels) == visible, "clause completion crossed a scope boundary")
    inner, inner_header, inner_iterable, outer_header, outer_iterable = [
        match.start() for match in re.finditer(r"\bitem\b", lines[3])]
    for column, definition in ((inner, location(3, inner_header, 4)),
                               (inner_iterable, location(3, outer_header, 4)), (outer_iterable, outer)):
        require(query("definition", 3, column) == definition, "nested comprehension shadowing differs")
    send({"method": "textDocument/didChange", "params": {
        "textDocument": {**document, "version": 2},
        "contentChanges": [{"text": (fixtures / "list-comprehensions/bad-filter.opaal").read_text()}]}})
    diagnostics = receive((uri, 2))["params"]["diagnostics"]
    require([item["code"] for item in diagnostics] == ["SIG029"], "framed filter diagnostic differs")
    send({"method": "textDocument/didChange", "params": {
        "textDocument": {**document, "version": 3}, "contentChanges": [{"text": "let rows = [n for n in [] if true"}]}})
    require([item["code"] for item in receive((uri, 3))["params"]["diagnostics"]] == ["SYN002"],
            "framed incomplete comprehension diagnostic differs")
    send({"method": "textDocument/didChange", "params": {
        "textDocument": {**document, "version": 2}, "contentChanges": [{"text": text}]}})
    require(query("definition", 0, 4) is None, "stale comprehension edit restored invalid bindings")


def list_map_queries(send, receive, request, work: Path, fixtures: Path, encoding: str) -> None:
    uri = (work / "map-query.opaal").as_uri()
    document = {"uri": uri}
    source = fixtures / "list-map"
    send({"method": "textDocument/didOpen", "params": {"textDocument": {
        **document, "languageId": "opaal", "version": 1, "text": (source / "inferred.opaal").read_text()}}})
    require(receive((uri, 1))["params"]["diagnostics"] == [], "inferred map driver did not check")

    def change(version, text):
        send({"method": "textDocument/didChange", "params": {"textDocument": {**document, "version": version},
              "contentChanges": [{"text": text}]}})
        return receive((uri, version))["params"]["diagnostics"]

    prefix = "import std::list as list\n"
    line = "list::map(['界🙂'], {|item| item})"
    unicode_source = prefix + line + "\n"
    require(change(2, unicode_source) == [], "Unicode map did not check")

    def position(text, column):
        prefix = text[:column]
        length = len(prefix.encode("utf-8")) if encoding == "utf-8" else len(prefix.encode("utf-16-le")) // 2
        return {"line": 1, "character": length}

    def source_range(text, start, end):
        return {"start": position(text, start), "end": position(text, end)}

    def query(method, column):
        return request("textDocument/" + method, {"textDocument": document, "position": position(line, column)})["result"]

    binder, reference = line.index("item"), line.rindex("item")
    require(query("definition", reference) == {"uri": uri, "range": source_range(line, binder, binder + 4)},
            "map callback did not resolve to its original binder")
    signature = query("signatureHelp", line.rindex(")"))
    require(signature["activeParameter"] == 1 and signature["signatures"][0]["label"] ==
            "std::list::map[T, U](input: List[T], transform: Callable(T) -> U) -> List[U]",
            "map signature differs")
    require("item" in {item["label"] for item in query("completion", reference + 1)},
            "map completion lost its callback binder")

    conflict = line.replace("|item|", "|item: Int|")
    diagnostic, = change(3, prefix + conflict + "\n")
    require(diagnostic["code"] == "OPR003" and diagnostic["range"] ==
            source_range(conflict, conflict.index("{"), conflict.index("}") + 1), "map conflict range differs")
    for version, name, expected in ((4, "missing.opaal", "SIG010"),
                                    (5, "missing-result.opaal", "SIG010"),
                                    (6, "parameter-conflict.opaal", "OPR003"),
                                    (7, "result-conflict.opaal", "SIG005"),
                                    (8, "incompatible-consumer.opaal", "SIG004")):
        require([item["code"] for item in change(version, (source / name).read_text())] == [expected],
                f"{name}: framed map diagnostic differs")
    require([item["code"] for item in change(9, prefix + "list::map([1], {|item| item")] == ["SYN002"],
            "incomplete map edit lacks its parse diagnostic")
    require(change(10, unicode_source) == [], "repaired map retained stale diagnostics")
    require(query("definition", reference) == {"uri": uri, "range": source_range(line, binder, binder + 4)},
            "repaired map did not restore navigation")


def slice_queries(send, receive, request, work: Path, fixtures: Path, encoding: str) -> None:
    uri = (work / "slice-query.opaal").as_uri()
    lines = ["let first = 0", "let last = 2", 'let label = "é界🙂"[first:last]']
    document = {"uri": uri}
    send({"method": "textDocument/didOpen", "params": {"textDocument": {
        **document, "languageId": "opaal", "version": 1, "text": "\n".join(lines) + "\n"}}})
    require(receive((uri, 1))["params"]["diagnostics"] == [], "slice operands did not check")

    def position(line, column):
        prefix = lines[line][:column]
        character = len(prefix.encode("utf-8")) if encoding == "utf-8" else len(prefix.encode("utf-16-le")) // 2
        return {"line": line, "character": character}

    def location(line, column, length):
        return {"uri": uri, "range": {"start": position(line, column), "end": position(line, column + length)}}

    for declaration, name in enumerate(("first", "last")):
        start = lines[2].index(name)
        at = {"textDocument": document, "position": position(2, start + 1)}
        require(request("textDocument/definition", at)["result"] == location(declaration, 4, len(name)),
                "slice endpoint definition range differs")
        require(request("textDocument/references", {**at, "context": {"includeDeclaration": False}})["result"]
                == [location(2, start, len(name))], "slice endpoint reference range differs")
        require(name in {item["label"] for item in request("textDocument/completion", at)["result"]},
                "slice endpoint completion omitted the binding")

    bad = (fixtures / "list-string-slicing/bad-operand.opaal").read_text()
    send({"method": "textDocument/didChange", "params": {"textDocument": {**document, "version": 2},
                                                              "contentChanges": [{"text": bad}]}})
    diagnostic, = receive((uri, 2))["params"]["diagnostics"]
    require(diagnostic["code"] == "SIG032" and diagnostic["range"] == {
        "start": {"line": 1, "character": 22}, "end": {"line": 1, "character": 29}},
        "slice endpoint diagnostic range differs")


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
                            if message.get("method") == "textDocument/publishDiagnostics":
                                params = message["params"]
                                if (params["uri"], params.get("version")) == identity:
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
                initialized = request("initialize", {"capabilities": {
                    "general": {"positionEncodings": [encoding]},
                    "textDocument": {"publishDiagnostics": {"relatedInformation": True}},
                }})
                require(initialized["result"]["capabilities"]["positionEncoding"] == encoding,
                        "language server encoding differs")
                send({"method": "initialized", "params": {}})
                inline_export_queries(send, receive, request, work / "inline-module-exports/inline", encoding)
                observations.append(f"{encoding}: inline exports and original diagnostic/declaration ranges")
                comprehension_queries(send, receive, request, work, fixtures, encoding)
                observations.append(f"{encoding}: comprehension original binders, clause completion, diagnostics and stale edit")
                list_map_queries(send, receive, request, work, fixtures, encoding)
                observations.append(f"{encoding}: List map binder, signature, completion and diagnostic ranges")
                slice_queries(send, receive, request, work, fixtures, encoding)
                observations.append(f"{encoding}: slice endpoint definitions, references, completion and diagnostics")
                documents = [(name, (fixtures / name).read_text()) for name in
                             ("legacy.opaal", "canonical.opaal", "invalid.opaal", "incomplete.opaal")]
                documents.extend((name, (fixtures / name).read_text()) for name in (
                    "concise-imports/noncanonical.opaal", "concise-imports/canonical.opaal",
                    "concise-imports/incomplete.opaal", "concise-imports/invalid-name.opaal",
                    "explicit-member-imports/noncanonical.opaal", "explicit-member-imports/canonical.opaal",
                    "explicit-member-imports/incomplete.opaal", "compound-assignment/noncanonical.opaal",
                    "compound-assignment/canonical.opaal", "compound-assignment/incomplete.opaal",
                    "indexed-list-replacement/noncanonical.opaal", "indexed-list-replacement/canonical.opaal",
                    "indexed-list-replacement/incomplete.opaal", "indexed-list-replacement/invalid.opaal",
                    "if-expressions/noncanonical.opaal", "if-expressions/canonical.opaal",
                    "if-expressions/incomplete.opaal", "if-expressions/invalid.opaal",
                    "indexed-iteration/noncanonical.opaal", "indexed-iteration/indexed-and-paired.opaal",
                    "list-literal-spread/noncanonical.opaal", "list-literal-spread/canonical.opaal",
                    "list-literal-spread/incomplete.opaal", "list-literal-spread/invalid.opaal",
                    "list-string-slicing/noncanonical.opaal", "list-string-slicing/canonical.opaal",
                    "list-string-slicing/incomplete.opaal", "list-string-slicing/invalid.opaal",
                    "record-shorthand/noncanonical.opaal", "record-shorthand/canonical.opaal",
                    "record-shorthand/incomplete.opaal", "record-shorthand/invalid.opaal",
                    "grouped-export-prefixes/noncanonical.opaal", "grouped-export-prefixes/canonical.opaal",
                    "grouped-export-prefixes/incomplete.opaal", "grouped-export-prefixes/invalid.opaal",
                    "action-declarations/noncanonical.opaal", "action-declarations/canonical.opaal",
                    "action-declarations/old-form.opaal", "action-declarations/semicolon.opaal",
                    "action-declarations/duplicate-block.opaal", "action-declarations/incomplete.opaal"))
                documents.append(("invalid-body.opaal", "def waiting()\nlet x = 1\n"))
                for index, (name, text) in enumerate(documents):
                    uri = (work / f"document-{index}.opaal").as_uri()
                    send({"method": "textDocument/didOpen", "params": {"textDocument": {
                        "uri": uri, "languageId": "opaal", "version": 1, "text": text}}})
                    params = {"textDocument": {"uri": uri}, "options": {"tabSize": 8, "insertSpaces": False}}
                    result = request("textDocument/formatting", params)["result"]
                    if name in ("legacy.opaal", "concise-imports/noncanonical.opaal", "explicit-member-imports/noncanonical.opaal",
                                "compound-assignment/noncanonical.opaal", "indexed-list-replacement/noncanonical.opaal", "list-literal-spread/noncanonical.opaal",
                                "if-expressions/noncanonical.opaal",
                                "indexed-iteration/noncanonical.opaal",
                                "list-string-slicing/noncanonical.opaal",
                                "record-shorthand/noncanonical.opaal", "grouped-export-prefixes/noncanonical.opaal",
                                "action-declarations/noncanonical.opaal"):
                        if name == "indexed-iteration/noncanonical.opaal":
                            canonical = (fixtures / "indexed-iteration/indexed-and-paired.opaal").read_text()
                        else:
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
                    if name == "indexed-iteration/indexed-and-paired.opaal":
                        lines = text.splitlines()
                        for binding, ty, declaration, use in (
                            ("i", "Int", (2, 4), (3, lines[3].index("i +"))),
                            ("number", "Int", (2, 8), (3, lines[3].index("number"))),
                            ("label", "String", (6, 13), (7, lines[7].index("label"))),
                        ):
                            params = {"textDocument": {"uri": uri}, "position": {"line": use[0], "character": use[1]}}
                            definition = request("textDocument/definition", params)["result"]
                            require(definition == {"uri": uri, "range": {
                                "start": {"line": declaration[0], "character": declaration[1]},
                                "end": {"line": declaration[0], "character": declaration[1] + len(binding)},
                            }}, "loop binding definition differs")
                            hover = request("textDocument/hover", params)["result"]
                            require(hover["contents"]["value"] == f"```opaal\nlet {binding}: {ty}\n```",
                                    "loop binding hover differs")
                            references = request("textDocument/references",
                                                 {**params, "context": {"includeDeclaration": False}})["result"]
                            require({"uri": uri, "range": {
                                "start": params["position"],
                                "end": {"line": use[0], "character": use[1] + len(binding)},
                            }} in references, "loop binding reference differs")
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
                uri = (work / "indexed-query.opaal").as_uri()
                lines = ["mut items: List[Int] = [1]", "let position = 0", "let replacement = 8",
                         "# 界🙂", "items[({label: '界🙂', position}).position] = replacement", "items"]
                send({"method": "textDocument/didOpen", "params": {"textDocument": {
                    "uri": uri, "languageId": "opaal", "version": 1, "text": "\n".join(lines) + "\n"}}})
                def indexed_range(line, start, end):
                    def units(prefix):
                        return len(prefix.encode("utf-8")) if encoding == "utf-8" else len(prefix.encode("utf-16-le")) // 2
                    return {"uri": uri, "range": {"start": {"line": line, "character": units(lines[line][:start])},
                                                  "end": {"line": line, "character": units(lines[line][:end])}}}
                for name, declared_line, declared_start in (("items", 0, 4), ("position", 1, 4), ("replacement", 2, 4)):
                    start = lines[4].index(name)
                    reference = indexed_range(4, start, start + len(name))
                    params = {"textDocument": {"uri": uri}, "position": reference["range"]["start"]}
                    definition = indexed_range(declared_line, declared_start, declared_start + len(name))
                    require(request("textDocument/definition", params)["result"] == definition,
                            "indexed original definition differs")
                    references = request("textDocument/references", {**params, "context": {"includeDeclaration": False}})["result"]
                    require(references.count(reference) == 1, "indexed reference missing or duplicated")
                    require(request("textDocument/hover", params)["result"] is not None, "indexed hover missing")
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
                uri = (work / "action-query.opaal").as_uri()
                text = (fixtures / "action-declarations/canonical.opaal").read_text() + "let result = identity('界🙂')\n"
                send({"method": "textDocument/didOpen", "params": {"textDocument": {
                    "uri": uri, "languageId": "opaal", "version": 1, "text": text}}})
                def action_position(offset):
                    prefix = text[:offset]
                    line = prefix.count("\n")
                    tail = prefix.rsplit("\n", 1)[-1]
                    character = len(tail.encode()) if encoding == "utf-8" else len(tail.encode("utf-16-le")) // 2
                    return {"line": line, "character": character}
                declaration = text.index("action identity") + len("action ")
                call = text.rindex("identity(")
                definition = {"uri": uri, "range": {"start": action_position(declaration),
                                                   "end": action_position(declaration + len("identity"))}}
                target = {"textDocument": {"uri": uri}, "position": action_position(call)}
                require(request("textDocument/definition", target)["result"] == definition and
                        "action identity(value: String) -> String" in request("textDocument/hover", target)["result"]["contents"]["value"],
                        "contained action lost its original signature or definition")
                references = request("textDocument/references", {**target, "context": {"includeDeclaration": True}})["result"]
                require(references == [definition, {"uri": uri, "range": {"start": action_position(call),
                        "end": action_position(call + len("identity"))}}], "contained action reference ranges differ")

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
                large = ("# 界🙂\n" * 16000) + "export let value = 7\n" + (fixtures / "legacy.opaal").read_text()
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
        for line in (b"action action_identity(value: String) -> String", b"{", b"    effects {", b"    }", b"    return value"):
            os.write(descriptor, line + b"\r")
            read_until(rb"\.\.\.> ")
            require(b"error[" not in captured, "terminal completed an action before its outer close")
        os.write(descriptor, b"}\r")
        read_until(rb">> ")
        os.write(descriptor, b"action_identity('action-ready')\r")
        read_until(rb"(?:^|\n)action-ready\r?\n.*>> ")
        os.write(descriptor, b"action broken() -> Int { effects {} effects {} return 1 }\r")
        read_until(rb"error\[OP1000\].*>> ")
        os.write(descriptor, b"broken()\r")
        read_until(rb"error\[.*>> ")
        for line in (b"action cancelled_action() -> Int", b"{", b"effects {}"):
            os.write(descriptor, line + b"\r")
            read_until(rb"\.\.\.> ")
        os.write(descriptor, b"\x03")
        read_until(rb">> ")
        for expression, value in ((b"identity('ready')", b"ready"), (b"five()", b"5")):
            os.write(descriptor, expression + b"\r")
            read_until(rb"(?:^|\n)" + value + rb"\r?\n.*>> ")
        for line in (b"[(i, n * 2)", b" for i, n in 2..=4"):
            os.write(descriptor, line + b"\r")
            read_until(rb"\.\.\.> ")
            require(b"error[" not in captured, "terminal submitted an open comprehension")
        os.write(descriptor, b" if n > 2]\r")
        read_until(rb"(?:^|\n)\[\(1, 6\), \(2, 8\)\]\r?\n.*>> ")
        os.write(descriptor, b"import std::list as lists\r")
        read_until(rb">> ")
        for line in (b"lists::map(", b"[1, 2],", b"{|x| x + 1}"):
            os.write(descriptor, line + b"\r")
            read_until(rb"\.\.\.> ")
            require(b"error[" not in captured, "terminal submitted an open List map")
        os.write(descriptor, b")\r")
        read_until(rb"(?:^|\n)\[2, 3\]\r?\n.*>> ")
        os.write(descriptor, b"let slice_rows: List[Int] = [10, 20, 30]\r")
        read_until(rb"\r?\n.*>> ")
        for line in (b"let empty_slice = slice_rows[", b"3:3"):
            os.write(descriptor, line + b"\r")
            read_until(rb"\.\.\.> ")
            require(b"error[" not in captured, "terminal submitted an open slice")
        os.write(descriptor, b"]\r")
        read_until(rb"\r?\n.*>> ")
        os.write(descriptor, b"lists::map(empty_slice, {|x| x + 1})\r")
        read_until(rb"(?:^|\n)\[\]\r?\n.*>> ")
        os.write(descriptor, b"let cancelled_slice = slice_rows[1:\r")
        read_until(rb"\.\.\.> ")
        os.write(descriptor, b"\x03")
        read_until(rb">> ")
        os.write(descriptor, b"cancelled_slice\r")
        read_until(rb"error\[.*>> ")
        os.write(descriptor, b"slice_rows[1:3]\r")
        read_until(rb"(?:^|\n)\[20, 30\]\r?\n.*>> ")
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
        os.write(descriptor, b"mut items: List[Int] = [1, 2]\r")
        read_until(rb"\r?\n.*>> ")
        os.write(descriptor, b"let saved = items\r")
        read_until(rb"\r?\n.*>> ")
        for line in (b"items[", b"1] ="):
            os.write(descriptor, line + b"\r")
            read_until(rb"\r?\n.*\.\.\.> ")
            require(b"error[" not in captured, "terminal submitted incomplete indexed replacement")
        os.write(descriptor, b"8\r")
        read_until(rb"\r?\n.*>> ")
        os.write(descriptor, b"[items, saved]\r")
        read_until(rb"(?:^|\n)\[\[1, 8\], \[1, 2\]\]\r?\n.*>> ")
        require(b"error[" not in captured, "terminal rejected indexed replacement")
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
        os.write(descriptor, b"let label = if false { 'pass' }\r")
        read_until(rb"\.\.\.> ")
        os.write(descriptor, b"else { 'fail' }\r")
        read_until(rb">> ")
        os.write(descriptor, b"label\r")
        read_until(rb"(?:^|\n)fail\r?\n.*>> ")
        os.write(descriptor, b"let cancelled_label = if true { 'unpublished' }\r")
        read_until(rb"\.\.\.> ")
        os.write(descriptor, b"\x03")
        read_until(rb">> ")
        os.write(descriptor, b"cancelled_label\r")
        read_until(rb"error\[.*>> ")
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
        os.write(descriptor, b"export def cancelled()\r")
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
                     (("invalid.opaal", "OP1000"), ("incomplete.opaal", "SYN002"), ("missing-comma.opaal", "OP1000"))]
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
        indexed_report = indexed_list_replacement(run, root, fixtures, target)
        conditionals_report = if_expressions(run, root, fixtures, target)
        tuples_report = typed_tuples(run, fixtures)
        loops_report = indexed_iteration(run, root, fixtures)
        comprehensions_report = list_comprehensions(run, root, fixtures, target)
        map_report = list_map(run, root, fixtures, target)
        slices_report = list_string_slicing(run, root, fixtures, target)
        spread_report = list_literal_spread(run, root, fixtures, target)
        record_report = record_shorthand(run, root, fixtures, target)
        exports_report = grouped_export_prefixes(run, root, fixtures, target)
        inline_report = inline_module_exports(run, root, fixtures, target)
        actions_report = action_declarations(run, binary, root, fixtures, target)
        lsp_report = framed_formatting(installed / "opaal-language-server", root, fixtures, environment)
        terminal_continuation(binary, root, environment)
        map_report["terminal_continuation"] = True
        slices_report["terminal_continuation"] = True
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
                "compound_assignment": compound_report, "indexed_list_replacement": indexed_report, "list_literal_spread": spread_report, "record_shorthand": record_report, "grouped_export_prefixes": exports_report,
                "if_expressions": conditionals_report,
                "typed_tuples": tuples_report,
                "indexed_iteration": loops_report,
                "list_comprehensions": comprehensions_report,
                "list_map": map_report,
                "list_string_slicing": slices_report,
                "inline_module_exports": inline_report,
                "action_declarations": actions_report, "language_server": lsp_report, "bounded_cases": bounded,
                "assertions": ["exact fixture/source/program identity", "legacy/canonical execution and checking",
                               "exact action/function/comment bytes and idempotence", "read-only check and mode/inode retention",
                               "invalid/incomplete/missing-comma preserve bytes", "raw typed/untyped continuation and EOF",
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
