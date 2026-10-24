"""Mutate independently produced v5 artifacts and observe the candidate CLI readers."""
from __future__ import annotations

import base64
import copy
import hashlib
import json
import shutil
from pathlib import Path

if __package__:
    from .qualify_data_processing import command, require, success
    from .qualify_native_preparation import case_results, host_triple, injected_results, minimal_elf
else:
    from qualify_data_processing import command, require, success
    from qualify_native_preparation import case_results, host_triple, injected_results, minimal_elf


def canonical(value):
    # These closed artifact keys are ASCII; this agrees with their UTF-16 ordering.
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def reseal(value):
    value.pop("digest", None)
    value["digest"] = "sha256:" + hashlib.sha256(canonical(value)).hexdigest()
    return value


def journal_bytes(events):
    """Rebuild sequence and digest links so mutations reach lifecycle validation."""
    previous, lines = None, []
    for sequence, event in enumerate(copy.deepcopy(events)):
        event.update(seq=sequence, previous=previous)
        reseal(event)
        previous = event["digest"]
        lines.append(canonical(event) + b"\n")
    return b"".join(lines)


def process_journal(data):
    events = [json.loads(line) for line in data.splitlines()]
    node = events[1]["payload"]["action_node_id"]
    operation = {"kind": "process", "operation": "std::process::run", "tool": "checker",
                 "argv_count": 1, "argv_bytes": 1, "argv": [], "argv_digest": None,
                 "status": None, "stdout_bytes": None, "stderr_bytes": None, "evidence_digest": None}
    before = {**events[1], "kind": "effect-before", "payload": {
        "action_node_id": node, "effect": "process.run", "scope": {"kind": "tool", "tool": "checker"},
        "attempt": 0, "verdict": "granted-unenforced", "operation": operation}}
    after = copy.deepcopy(before)
    after["kind"] = "effect-after"
    after["payload"].pop("verdict")
    after["payload"].update(outcome=events[2]["payload"]["outcome"], evidence_digest=None)
    after["payload"]["operation"].update(status=0, stdout_bytes=0, stderr_bytes=0)
    return events[:2] + [before, after] + events[2:]


def run_plan(binary, work, digest):
    return command([binary, "task", "run", "--plan", "plan.json", "--accept", digest,
                    "--require-true", "--journal", "run.jsonl", "--receipt-out", "receipt.json",
                    "--audit-out", "audit.json"], work, {})


def prepare_project(binary, work, *, body="return true", input_file=False, canary=False,
                    native_image=None):
    work.mkdir()
    tool = work / "checker"
    tool.write_bytes(native_image if native_image is not None else
                     b"#!/bin/sh\nprintf 'launch\\n' >> checker.launches\n" if canary else minimal_elf())
    tool.chmod(0o700)
    if canary:
        success(command([tool], work, {}), "launch observer positive control")
        require((work / "checker.launches").read_bytes() == b"launch\n", "ineffective launch observer")
        (work / "checker.launches").unlink()
    (work / "opaal.toml").write_text('''schema_version = 1
[project]
name = "native_test"
root_module = "tasks.opaal"
required_opaal = ">=1.6.0,<2.0.0"
[paths]
root = "."
evidence = "output"
[tools.checker]
adapter = "native"
[environments.ci]
authority = "authority.toml"
tool_lock = "tools.toml"
''')
    parameters = "input: Path" if input_file else ""
    source = "action check(" + parameters + ") -> Bool effects {} { " + body + " }\ntask check = check\n"
    if native_image is not None:
        source = ('import std::process as process\nimport project::tools as tools\n'
                  'action check() -> Bool effects { process.run(tools::checker); } {\n'
                  'let result = process::run(tools::checker, [])\nreturn true\n}\ntask check = check\n')
    (work / "tasks.opaal").write_text(source)
    bindings = []
    if input_file:
        (work / "input").write_bytes(b"SENSITIVE_BOUND_INPUT")
        bindings = ["--input-file", "input=input"]
    (work / "authority.toml").write_text('''schema_version = 1
project = "native_test"
environment = "ci"
[[rules]]
effect = "process.run"
scope = "tool.checker"
decision = "grant"
required_enforcement = "acknowledge-unenforced"
''')
    encoded = base64.urlsafe_b64encode(bytes(tool)).decode().rstrip("=")
    (work / "tools.toml").write_text(f'''schema_version = 1
project = "native_test"
environment = "ci"
platform = "{host_triple()}"
[child_environment]
inherit = []
[[tools]]
id = "checker"
adapter = "native"
path = {{ encoding = "base64url-nopad", platform = "unix", value = "{encoded}" }}
digest = "sha256:{hashlib.sha256(tool.read_bytes()).hexdigest()}"
''')
    checked = command([binary, "check", "--project", "opaal.toml", "--task", "check",
                       "--environment", "ci", "--format", "json", *bindings], work, {})
    check = json.loads(success(checked, "evidence seed check"))
    (work / "check.json").write_bytes(canonical(check))
    digest = success(command([binary, "plan", "--project", "opaal.toml", "--task", "check",
                             "--environment", "ci", "--expires-in", "900s", "--out", "plan.json",
                             "--format", "digest", *bindings], work, {}), "evidence seed plan").decode().strip()
    return digest


def project(binary, work, **options):
    return run_plan(binary, work, prepare_project(binary, work, **options))


def explain(binary, work):
    return command([binary, "task", "explain", "--receipt", "receipt.json", "--audit", "audit.json",
                    "--format", "json"], work, {})


def acceptance_case(binary, work, changed):
    require(project(binary, work, input_file=True, canary=True).returncode == 0,
            "acceptance positive control failed")
    require(not (work / "checker.launches").exists(), "unused tool launched during positive control")
    names = ("run.jsonl", "receipt.json", "audit.json")
    for name in names:
        (work / name).unlink()
    plan = json.loads((work / "plan.json").read_bytes())
    accepted = plan["digest"]
    paths = {"source": "tasks.opaal", "manifest": "opaal.toml", "authority": "authority.toml",
             "tool-lock": "tools.toml", "input-bytes": "input", "native-bytes": "checker"}
    if changed == "acceptance":
        accepted = "sha256:" + "0" * 64
    elif changed in paths:
        path = work / paths[changed]
        path.write_bytes(path.read_bytes() + b"\n# changed\n")
    elif changed == "native-path":
        replacement = work / "replacement"
        shutil.copyfile(work / "checker", replacement)
        replacement.chmod(0o700)
        path = work / "tools.toml"
        path.write_text(path.read_text().replace(
            base64.urlsafe_b64encode(bytes(work / "checker")).decode().rstrip("="),
            base64.urlsafe_b64encode(bytes(replacement)).decode().rstrip("=")))
    elif changed == "child-environment":
        path = work / "tools.toml"
        path.write_text(path.read_text() + '''\n[[child_environment.variables]]
name = "CANARY"
value = { encoding = "base64url-nopad", platform = "unix", value = "Y2hhbmdlZA" }
''')
    else:
        if changed == "host":
            plan["platform"]["triple"] = "unsupported"
            plan["tools"][0]["platform"] = "unsupported"
        elif changed == "root":
            plan["project"]["root"]["value"] = base64.urlsafe_b64encode(bytes(work / "other")).decode().rstrip("=")
        else:
            require(changed == "expiry", "unknown acceptance control")
            plan.update(created_at="2026-01-01T00:00:00.000000000Z", expires_at="2026-01-01T00:15:00.000000000Z")
            for observation in plan["observations"]:
                if observation["kind"] == "wall-clock":
                    observation["observed_at"] = plan["created_at"]
        reseal(plan)
        (work / "plan.json").write_bytes(canonical(plan))
        # Establish that the mutation is a well-formed plan, not a parser refusal.
        success(command([binary, "plan", "inspect", "plan.json"], work, {}), "mutated plan reader control")
        accepted = plan["digest"]
    run = run_plan(binary, work, accepted)
    (work / "stdout").write_bytes(run.stdout)
    (work / "stderr").write_bytes(run.stderr)
    require(not run.stdout and run.stderr and b"SENSITIVE_BOUND_INPUT" not in run.stderr,
            "acceptance refusal leaked a report or bound input")
    return {"exit": run.returncode, "launches": len((work / "checker.launches").read_bytes().splitlines())
            if (work / "checker.launches").exists() else 0,
            "evidence_created": any((work / name).exists() for name in names)}


def passive_case(binary, work, observer):
    require(project(binary, work, canary=True).returncode == 0, "passive project positive control failed")
    source = '''import project::tools as tools
import std::process as process
^printf "SENSITIVE_PASSIVE_INITIALIZER"
action check() -> Bool effects { process.run(tools::checker); } {
    process::run(tools::checker, ["SENSITIVE_PASSIVE_ARGV"])
    throw "SENSITIVE_PASSIVE_ACTION"
}
task check = check
'''
    (work / "tasks.opaal").write_text(source)
    shared = ["--project", "opaal.toml", "--task", "check", "--environment", "ci"]
    args = {"task-list": ["task", "list", "--project", "opaal.toml", "--format", "json"],
            "task-inspect": ["task", "inspect", "check", "--project", "opaal.toml", "--format", "json"],
            "check": ["check", *shared, "--format", "json"],
            "doctor": ["project", "doctor", *shared, "--format", "json"],
            "plan": ["plan", *shared, "--expires-in", "900s", "--out", "observer.plan.json"],
            "plan-inspect": ["plan", "inspect", "observer.plan.json"],
            "help": ["--help"],
            "interactive-prompt": []}
    if observer == "plan-inspect":
        prepared = command([binary, *args["plan"]], work, {})
        require(prepared.returncode in {0, 1} and b"SENSITIVE_PASSIVE" not in prepared.stdout + prepared.stderr,
                "passive plan control failed")
    completed = command([binary, *args[observer]], work, {},
                        stdin=b"exit\n" if observer == "interactive-prompt" else None)
    (work / "stdout").write_bytes(completed.stdout)
    (work / "stderr").write_bytes(completed.stderr)
    require(completed.returncode in {0, 1} and completed.stdout
            and (observer == "plan" or not completed.stderr),
            "passive observer failed before producing a view")
    if observer == "help":
        require(completed.returncode == 0 and b"Usage:" in completed.stdout,
                "passive help did not produce usage")
    if observer == "interactive-prompt":
        require(completed.returncode == 0 and completed.stdout == b">> ",
                "passive interactive prompt differs")
    if observer in {"plan", "plan-inspect"}:
        value = json.loads((work / "observer.plan.json").read_bytes())
        require(value["schema"] == "opaal.plan.v5" and value["tools"][0]["adapter"] == "native",
                "passive plan did not retain the native contract")
    data = completed.stdout + completed.stderr
    require(b"SENSITIVE_PASSIVE" not in data, "passive observer evaluated source")
    return {"launches": len((work / "checker.launches").read_bytes().splitlines())
            if (work / "checker.launches").exists() else 0,
            "evaluated_initializers": int(b"SENSITIVE_PASSIVE_INITIALIZER" in data),
            "evaluated_actions": int(b"SENSITIVE_PASSIVE_ACTION" in data)}


def reference_input_cases(binary, inputs, output, cases):
    output.mkdir()
    compiler = shutil.which("cc")
    require(compiler is not None, "child input observer requires the host C compiler")
    checker = output / "checker"
    source = inputs / "child-input-checker.c"
    success(command([compiler, "-O2", "-Wall", "-Wextra", "-Werror", source, "-o", checker],
                    output, {"PATH": "/usr/bin:/bin"}), "build child input observer")
    observations = []
    for row in cases:
        work = output / row["name"]
        success(command([binary, "project", "init", "--example", "repository-check", "--destination", work,
                         "--environment", "ci", "--tool", checker], output, {}), "prepare child input observer")
        script = work / "fixtures/check.sh"
        before = script.read_bytes()
        (work / "checker.change").write_text("0")
        args = [checker, "--norc", "--format=json1", "--shell=sh", "--severity=style", "--", script]
        control = success(command(args, work, {"LC_ALL": "C"}), "child input positive control")
        require(control == b'{"comments":[]}' and script.read_bytes() == before
                and (work / "checker.saw").read_bytes() == before
                and (work / "checker.launches").read_bytes() == b"launch\n", "child input positive control differs")
        (work / "checker.launches").unlink()
        restore = row["input"]["change"] == "child-change-and-restore"
        (work / "checker.change").write_text("2" if restore else "1")
        digest = success(command([binary, "plan", "--project", "opaal.toml", "--task", "repository",
                                 "--environment", "ci", "--input-file", "script=fixtures/check.sh",
                                 "--input-file", "manifest=fixtures/repository.toml", "--expires-in", "900s",
                                 "--out", "plan.json", "--format", "digest"], work, {}),
                         "child input fresh plan").decode().strip()
        run = command([binary, "task", "run", "--plan", "plan.json", "--accept", digest,
                       "--require-true", "--journal", "run.jsonl", "--receipt-out", "receipt.json",
                       "--audit-out", "audit.json"], work, {})
        (work / "stdout").write_bytes(run.stdout)
        (work / "stderr").write_bytes(run.stderr)
        seen = (work / "checker.saw").read_bytes()
        require(seen != before and b"SENSITIVE_CHILD_INPUT" in seen, "child did not reopen changed bytes")
        require((script.read_bytes() == before) == restore, "child restoration differs")
        require((work / "checker.launches").read_bytes() == b"launch\n", "child input attempt count differs")
        events = [json.loads(line) for line in (work / "run.jsonl").read_bytes().splitlines()]
        require(sum(event["kind"] == "effect-before" and event["payload"]["effect"] == "process.run"
                    for event in events) == 1, "reference child retried or probed")
        if restore:
            require(run.returncode == 0 and run.stdout == b'{"findings":0,"manifest_ok":true,"passed":true}',
                    "change-and-restore counterexample did not pass the byte-only observer")
            observed = {"detection_guaranteed": False, "child_bytes_proven": False}
        else:
            observed = {"exit": run.returncode, "report": bool(run.stdout)}
        for name in ("plan.json", "run.jsonl", "receipt.json", "audit.json"):
            require(b"SENSITIVE_CHILD_INPUT" not in (work / name).read_bytes(), "child input payload leaked")
        observations.append({"name": row["name"], "observed": observed, "route": "accepted-reference-child",
                             "observations": {"before_sha256": hashlib.sha256(before).hexdigest(),
                                              "child_sha256": hashlib.sha256(seen).hexdigest(),
                                              "after_sha256": hashlib.sha256(script.read_bytes()).hexdigest(),
                                              "exit": run.returncode}})
    return observations, {"checker_sha256": hashlib.sha256(checker.read_bytes()).hexdigest(),
                          "source_sha256": hashlib.sha256(source.read_bytes()).hexdigest()}


def cli_evidence_cases(binary, test_binary, inputs, output, cases, host):
    digest = hashlib.sha256(test_binary.read_bytes()).hexdigest()
    completed = command([test_binary, "native_frozen_cli_evidence_cases", "--exact", "--nocapture",
                         "--test-threads=1"], output, {})
    (output / "cli.stdout").write_bytes(completed.stdout)
    (output / "cli.stderr").write_bytes(completed.stderr)
    cli_digest = "sha256:" + hashlib.sha256(binary.read_bytes()).hexdigest()
    identities = [line.partition(b"OPAAL_EVIDENCE_CLI ")[2].decode() for line in completed.stdout.splitlines()
                  if b"OPAAL_EVIDENCE_CLI " in line]
    require(identities == [cli_digest], "CLI evidence candidate identity differs")
    names = [row["name"] for row in cases if host in row["hosts"] and (
        row["input"]["kind"] == "redaction" or (row["input"]["kind"] == "input-race"
        and row["input"]["phase"] == "after-acceptance-before-read"))]
    records = injected_results(completed, cases, names, host,
                               "sha256:" + hashlib.sha256((inputs / "evidence-cases.json").read_bytes()).hexdigest(),
                               marker=b"OPAAL_EVIDENCE_CASE ", label="CLI evidence")
    require(hashlib.sha256(test_binary.read_bytes()).hexdigest() == digest, "CLI evidence test binary changed")
    return records, {"binary_sha256": digest, "cli_digest": cli_digest,
                     "scope": "accepted CLI redaction and bound-input reads"}


def injected_evidence_cases(test_binary, inputs, output, cases, host, observer):
    tests = {
        "finish": ("project::task::tests::native_frozen_finish_evidence_cases",
                   "persistence/exit selection after injected primaries; no evaluator or child execution"),
        "journal": ("workflow::bound_observers::native_frozen_journal_bound_cases",
                    "isolated line/byte counters; real v5 journals reach the byte ceiling before the line ceiling"),
        "lsp": ("native_frozen_passive_lsp_case", "framed candidate server requests with executable marker controls"),
    }
    name, scope = tests[observer]
    names = [row["name"] for row in cases if host in row["hosts"] and (
        (observer == "finish" and (row["input"]["kind"] == "finish-fault"
         or row["name"] in {"requested-cancellation", "deadline-cancellation"}))
        or (observer == "journal" and row["input"]["kind"] == "journal-bound")
        or (observer == "lsp" and row["name"] == "passive-lsp"))]
    digest = hashlib.sha256(test_binary.read_bytes()).hexdigest()
    completed = command([test_binary, name, "--exact", "--nocapture", "--test-threads=1"], output, {})
    (output / (observer + ".stdout")).write_bytes(completed.stdout)
    (output / (observer + ".stderr")).write_bytes(completed.stderr)
    records = injected_results(completed, cases, names, host,
                               "sha256:" + hashlib.sha256((inputs / "evidence-cases.json").read_bytes()).hexdigest(),
                               marker=b"OPAAL_EVIDENCE_CASE ", label=observer + " evidence")
    require(hashlib.sha256(test_binary.read_bytes()).hexdigest() == digest, observer + " observer binary changed")
    return records, {"binary_sha256": digest, "scope": scope}


def mutate_artifact(value, document, mutation):
    if mutation == "future-schema":
        value["schema_version"] = 99
    elif mutation == "unknown-field":
        value["unknown"] = True
    else:
        field = {"native-required-version": "required_version", "native-locked-version": "locked_version",
                 "native-version-verified": "version_verified"}[mutation]
        if document == "audit":
            # Audit has no tool rows: these fields must refuse at its closed root shape.
            require("tools" not in value, "audit unexpectedly contains tool rows")
            value[field] = True if field == "version_verified" else "1.0.0"
        else:
            value["tools"][0][field] = True if field == "version_verified" else "1.0.0"
    return reseal(value)


def qualify_evidence(binary, inputs, output, cases, *, test_binary=None, cli_test_binary=None,
                     finish_test_binary=None, journal_test_binary=None, lsp_test_binary=None):
    host = host_triple()
    require(host is not None, "unsupported evidence qualification host")
    output.mkdir()
    seed = output / "seed"
    run = project(binary, seed)
    require(run.returncode == 0, "evidence seed run failed")
    original = {name: (seed / name).read_bytes()
                for name in ("check.json", "plan.json", "run.jsonl", "receipt.json", "audit.json")}
    for name in ("check.json", "plan.json", "audit.json"):
        value = json.loads(original[name])
        require(reseal(dict(value)) == value, "independent artifact digest differs: " + name)
    require(json.loads(original["plan.json"])["schema"] == "opaal.plan.v5", "evidence seed is not native v5")
    require(json.loads(success(explain(binary, seed), "evidence seed explanation"))["conclusion"] == "passed",
            "evidence reader positive control failed")

    def audit(data, name):
        journal, destination = seed / (name + ".jsonl"), seed / (name + ".audit.json")
        journal.write_bytes(data)
        completed = command([binary, "audit", "--project", seed / "opaal.toml", "--journal", journal,
                             "--out", destination], seed, {})
        return completed, destination

    control, control_path = audit(original["run.jsonl"], "positive-control")
    success(control, "journal audit positive control")
    require(control_path.read_bytes() == original["audit.json"], "journal audit positive control differs")
    process_events = process_journal(original["run.jsonl"])
    control, control_path = audit(journal_bytes(process_events), "process-positive-control")
    success(control, "process journal positive control")
    require(json.loads(control_path.read_bytes())["completeness"] == "complete",
            "process journal positive control is incomplete")
    observations, pending = [], []
    for row in cases:
        if host not in row["hosts"]:
            continue
        spec = row["input"]
        kind, fault = spec["kind"], spec.get("fault")
        if kind in {"process-descriptor", "metadata-injection"}:
            work = output / row["name"]
            work.mkdir()
            events = copy.deepcopy(process_events)
            if kind == "process-descriptor":
                for event in events[2:4]:
                    event["payload"]["operation"]["argv_count"] = spec["argv_count"]
                    event["payload"]["operation"]["argv_bytes"] = spec["argv_count"]
            else:
                field = spec["field"]
                if field in {"message", "value_digest"}:
                    events[3]["payload"]["outcome"][field] = (
                        "SENSITIVE_EVIDENCE_ERROR" if field == "message" else "sha256:" + "1" * 64)
                else:
                    value = ([{"kind": "public", "value": "SENSITIVE_EVIDENCE_ERROR"}] if field == "argv"
                             else "sha256:" + "1" * 64 if field.endswith("digest") else "SENSITIVE_EVIDENCE_ERROR")
                    events[3]["payload"]["operation"][field] = value
            data = journal_bytes(events)
            (work / "run.jsonl").write_bytes(data)
            completed, destination = audit(data, row["name"])
            (work / "stdout").write_bytes(completed.stdout)
            (work / "stderr").write_bytes(completed.stderr)
            admitted = completed.returncode == 0
            require(b"SENSITIVE_EVIDENCE_ERROR" not in completed.stdout + completed.stderr,
                    "metadata refusal leaked injected payload")
            if admitted:
                require(json.loads(destination.read_bytes())["completeness"] == "complete",
                        "admitted metadata journal is incomplete")
            else:
                require(completed.returncode in {1, 2} and completed.stderr and not completed.stdout
                        and not destination.exists(), "metadata refusal published evidence")
            observations.append({"name": row["name"], "observed": {"admitted": admitted},
                                 "route": "candidate-cli-synthetic-journal-no-launch"})
            continue
        if kind == "acceptance" or (kind == "observer" and spec["observer"] in {
                "task-list", "task-inspect", "check", "doctor", "plan", "plan-inspect", "help",
                "interactive-prompt"}):
            work = output / row["name"]
            observed = (acceptance_case(binary, work, spec["changed"]) if kind == "acceptance"
                        else passive_case(binary, work, spec["observer"]))
            observations.append({"name": row["name"], "observed": observed,
                                 "route": "candidate-cli-unused-native-launch-canary"})
            continue
        supported = (kind == "evidence" and fault in {
            "none", "missing-audit", "future-receipt", "future-audit", "receipt-run-id", "audit-run-id",
            "receipt-plan-digest", "receipt-partial", "receipt-progress", "audit-message",
            "audit-value-digest", "audit-extra", "audit-digest", "journal-header-prefix",
            "journal-truncated-terminal", "journal-corrupt-first-line", "journal-corrupt-middle-line",
            "journal-reordered-attempt", "journal-unmatched-after"})
        supported |= kind == "schema" and spec.get("native") is True
        supported |= kind == "artifact" and spec["artifact"] in {"plan", "audit"}
        # Receipt explanation hides its refusal stage; it cannot isolate a size-limit failure.
        supported |= kind == "document-bound" and spec["document"] in {"plan", "audit", "journal"}
        supported |= kind == "encoding-sequence"
        supported |= row["name"] in {"complete-negative", "language-error", "explain-without-project"}
        if not supported:
            pending.append(row["name"])
            continue
        work = output / row["name"]
        if row["name"] in {"complete-negative", "language-error"}:
            completed = project(binary, work, body="return false" if row["name"] == "complete-negative"
                                else 'throw "SENSITIVE_EVIDENCE_ERROR"')
            view = json.loads(success(explain(binary, work), "evidence outcome explanation"))
            observed = {"exit": completed.returncode, "conclusion": view["conclusion"]}
        else:
            work.mkdir()
            for name, data in original.items():
                (work / name).write_bytes(data)
            document = "plan" if kind == "schema" else spec.get("artifact", spec.get("document", "receipt"))
            if fault and fault.startswith("journal-") and kind in {"evidence", "encoding-sequence"}:
                lines = original["run.jsonl"].splitlines(keepends=True)
                if fault == "journal-header-prefix":
                    data = lines[0]
                elif fault == "journal-truncated-terminal":
                    data = original["run.jsonl"][:-4]
                elif fault in {"journal-corrupt-first-line", "journal-corrupt-middle-line"}:
                    index = 0 if fault == "journal-corrupt-first-line" else 1
                    lines[index] = b"!" + lines[index][1:]
                    data = b"".join(lines)
                else:
                    events = copy.deepcopy(process_events)
                    if fault == "journal-duplicate-terminal":
                        events.append(copy.deepcopy(events[-1]))
                    elif fault in {"journal-missing-before", "journal-unmatched-after"}:
                        del events[2]
                    elif fault == "journal-reordered-attempt":
                        events[2]["payload"]["attempt"] = 1
                        events[3]["payload"]["attempt"] = 1
                    else:
                        require(fault == "journal-digest-mismatch", "unknown journal mutation")
                    data = journal_bytes(events)
                    if fault == "journal-digest-mismatch":
                        events = [json.loads(line) for line in data.splitlines()]
                        events[2]["digest"] = "sha256:" + "0" * 64
                        data = b"".join(canonical(event) + b"\n" for event in events)
                (work / "run.jsonl").write_bytes(data)
                completed, destination = audit(data, row["name"])
                if fault in {"journal-header-prefix", "journal-truncated-terminal"}:
                    require(completed.returncode == 1 and destination.is_file(), "incomplete audit not retained")
                    (work / "audit.json").write_bytes(destination.read_bytes())
                    completed = explain(binary, work)
                    observed = {"inspect_exit": completed.returncode,
                                "conclusion": json.loads(success(completed, "incomplete explanation"))["conclusion"]}
                else:
                    observed = ({"audit_refused": completed.returncode != 0} if kind == "evidence"
                                else {"admitted": completed.returncode == 0})
                    require(not destination.exists(), "corrupt journal published an audit")
                    diagnostics = {"journal-reordered-attempt": b"effect attempt identities are not contiguous",
                                   "journal-unmatched-after": b"effect-after has no matching before event",
                                   "journal-missing-before": b"effect-after has no matching before event",
                                   "journal-duplicate-terminal": b"journal content follows terminal",
                                   "journal-digest-mismatch": b"journal line digest mismatch"}
                    if fault in diagnostics:
                        require(b"JOURNAL003" in completed.stderr and diagnostics[fault] in completed.stderr,
                                "journal mutation refused at a different validation stage")
            elif kind == "evidence":
                document = "audit" if fault and fault.startswith("audit-") or fault == "future-audit" else "receipt"
                value = json.loads(original[document + ".json"])
                if fault in {"future-receipt", "future-audit"}:
                    value["schema_version"] = 99
                elif fault in {"receipt-run-id", "audit-run-id"}:
                    value["run_id"] = "2" * 32
                elif fault == "receipt-plan-digest":
                    value["plan_digest"] = "sha256:" + "0" * 64
                elif fault == "receipt-partial":
                    value["primary"]["partial"] = True
                elif fault == "receipt-progress":
                    value["progress"]["stdout.write"]["confirmed_bytes"] = 1
                elif fault == "audit-message":
                    value["primary"]["message"] = "SENSITIVE_EVIDENCE_ERROR"
                elif fault == "audit-value-digest":
                    value["primary"]["value_digest"] = "sha256:" + "1" * 64
                elif fault == "audit-extra":
                    value["extra"] = "SENSITIVE_EVIDENCE_ERROR"
                elif fault == "audit-digest":
                    value["digest"] = "sha256:" + "0" * 64
                if spec.get("reseal"):
                    reseal(value)
                (work / (document + ".json")).write_bytes(canonical(value) + (b"\n" if document == "receipt" else b""))
                if fault == "missing-audit":
                    (work / "audit.json").unlink()
                completed = explain(binary, work)
                observed = {"exit": run.returncode} if fault == "none" else {"inspect_exit": completed.returncode}
                if fault in {"none", "missing-audit"} or row["name"] == "explain-without-project":
                    observed["conclusion"] = json.loads(success(completed, "explanation control"))["conclusion"]
            elif kind in {"artifact", "schema"}:
                value = json.loads(original[document + ".json"])
                if kind == "schema":
                    value["schema_version"] = spec["schema_version"]
                    value["schema"] = "opaal.plan.v" + str(spec["schema_version"])
                    reseal(value)
                else:
                    mutate_artifact(value, document, spec["mutation"])
                (work / (document + ".json")).write_bytes(canonical(value))
                completed = command([binary, document, "inspect", document + ".json"], work, {})
                observed = {"admitted": completed.returncode == 0}
                if kind == "schema" and spec["schema_version"] == 5:
                    observed.update(native_version=value["tools"][0]["locked_version"],
                                    version_verified=value["tools"][0]["version_verified"])
            elif kind == "encoding-sequence":
                data = original["receipt.json"]
                if fault == "receipt-missing-lf":
                    require(data.endswith(b"\n"), "seed receipt has no final LF")
                    data = data[:-1]
                elif fault == "receipt-noncanonical":
                    data = json.dumps(json.loads(data), indent=2).encode() + b"\n"
                elif fault == "receipt-invalid-utf8":
                    data = b"\xff" + data
                else:
                    require(fault == "receipt-extra-field", "unknown receipt encoding fault")
                    value = json.loads(data)
                    value["extra"] = True
                    data = canonical(value) + b"\n"
                (work / "receipt.json").write_bytes(data)
                completed = explain(binary, work)
                observed = {"admitted": completed.returncode == 0}
            else:
                require(kind == "document-bound", "unknown evidence producer route")
                path = work / ("run.jsonl" if document == "journal" else document + ".json")
                path.write_bytes(b" " * spec["bytes"])
                if document in {"plan", "audit"}:
                    args = [binary, document, "inspect", path]
                else:
                    require(document == "journal", "unknown document bound route")
                    completed, destination = audit(path.read_bytes(), row["name"])
                    require(not destination.exists(), "oversize journal published an audit")
                    args = None
                if args is not None:
                    completed = command(args, work, {})
                require(b"PROJECT002" in completed.stderr
                        and (str(spec["bytes"] - 1) + "-byte read limit").encode() in completed.stderr,
                        "document bound did not refuse at the read limit")
                observed = {"admitted": completed.returncode == 0}
        (work / "stdout").write_bytes(completed.stdout)
        (work / "stderr").write_bytes(completed.stderr)
        require(b"SENSITIVE_EVIDENCE_ERROR" not in completed.stdout + completed.stderr,
                "evidence reader leaked raw error data")
        if observed.get("admitted") is False or observed.get("inspect_exit") == 2:
            require(completed.returncode in {1, 2} and not completed.stdout and completed.stderr,
                    "evidence refusal did not use a safe error route")
        route = ("candidate-cli-audit-closed-root-field" if kind == "artifact" and spec["artifact"] == "audit"
                 and spec["mutation"].startswith("native-") else "candidate-cli")
        observations.append({"name": row["name"], "observed": observed, "route": route})
    child = None
    if host in {"x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"}:
        child_cases = [row for row in cases if row["input"]["kind"] == "input-race"
                       and row["input"]["phase"] == "during-child"]
        records, child = reference_input_cases(binary, inputs, output / "reference-inputs", child_cases)
        observations += records
        for row in records:
            pending.remove(row["name"])
    api = None
    if test_binary:
        digest = hashlib.sha256(test_binary.read_bytes()).hexdigest()
        completed = command([test_binary, "native_frozen_evidence_reader_cases", "--exact", "--nocapture",
                             "--test-threads=1"], output, {})
        (output / "api.stdout").write_bytes(completed.stdout)
        (output / "api.stderr").write_bytes(completed.stderr)
        names = [row["name"] for row in cases if row["name"].startswith("check-")
                 or row["name"] in {"receipt-first-excess", "maintained-v4-preserved"}]
        records = injected_results(completed, cases, names, host,
                                   "sha256:" + hashlib.sha256((inputs / "evidence-cases.json").read_bytes()).hexdigest(),
                                   marker=b"OPAAL_EVIDENCE_CASE ", label="evidence")
        for row in records:
            pending.remove(row["name"])
            observations.append({**row, "route": "runtime-api"})
        require(hashlib.sha256(test_binary.read_bytes()).hexdigest() == digest, "evidence API test binary changed")
        api = {"binary_sha256": digest, "scope": "closed artifact readers; no native execution"}
    cli = None
    if cli_test_binary:
        records, cli = cli_evidence_cases(binary, cli_test_binary, inputs, output, cases, host)
        for row in records:
            pending.remove(row["name"])
            observations.append({**row, "route": "candidate-cli-regression"})
    injected = {}
    for observer, observer_binary in (("finish", finish_test_binary), ("journal", journal_test_binary),
                                      ("lsp", lsp_test_binary)):
        if observer_binary:
            records, injected[observer] = injected_evidence_cases(observer_binary, inputs, output, cases, host, observer)
            for row in records:
                pending.remove(row["name"])
                observations.append({**row, "route": "candidate-" + observer + "-observer"})
    result = {"host": host, "producer_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "cli_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "inventory_sha256": hashlib.sha256((inputs / "evidence-cases.json").read_bytes()).hexdigest(),
              "seed_sha256": {name: hashlib.sha256(data).hexdigest() for name, data in original.items()},
              "child_input_observer": child,
              "api_observer": api,
              "cli_observer": cli,
              "injected_observers": injected,
              **case_results(cases, observations, pending, host, label="evidence")}
    (output / "evidence-results.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    return result
