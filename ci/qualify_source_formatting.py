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
    from .package_source_formatting import FIXTURES
    from .qualify_data_processing import require, success, unpack
else:
    from check_source_formatting import public_paths, seed_census
    from package_data_processing import SOURCE, digest
    from package_release import PLATFORMS, ROOT, VERSION
    from package_source_formatting import FIXTURES
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
    if value["schema"] == "opaal.plan.v2":
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


def project_workflow(run, work: Path, fixtures: Path, target: str) -> dict:
    shutil.copytree(fixtures / "project", work)
    shutil.copyfile(work / ("tools-macos.toml" if target == "macos-arm64" else "tools-linux.toml"),
                    work / "tools.toml")
    base = ("--project", "opaal.toml", "--task", "verify", "--environment", "ci")
    original = {path.name: path.read_bytes() for path in work.glob("*.toml")}

    def verify_contract(value: dict, name: str, effects: bytes = b"") -> None:
        path = work / "tasks.opaal"
        require(len(value["sources"]) == 1 and value["sources"][0]["digest"] == "sha256:" + digest(path.read_bytes()) and
                value["sources"][0]["size"] == path.stat().st_size and value["sources"][0]["module"] == str(path),
                "project source digest/size/module differs")
        graph = digest(os.fsencode(path) + b"\0" + path.read_bytes() + b"\xff")
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

    # Declaring an effect is sufficient to require authority even when its body is pure.
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
                documents.append(("invalid-body.opaal", "def waiting()\nlet x = 1\n"))
                for index, (name, text) in enumerate(documents):
                    uri = (work / f"document-{index}.opaal").as_uri()
                    send({"method": "textDocument/didOpen", "params": {"textDocument": {
                        "uri": uri, "languageId": "opaal", "version": 1, "text": text}}})
                    params = {"textDocument": {"uri": uri}, "options": {"tabSize": 8, "insertSpaces": False}}
                    result = request("textDocument/formatting", params)["result"]
                    if name == "legacy.opaal":
                        canonical = (fixtures / "canonical.opaal").read_text()
                        require(result == [{"range": {"start": {"line": 0, "character": 0},
                                                       "end": {"line": len(text.splitlines()), "character": 0}},
                                            "newText": canonical}], "framed formatter bytes/range differ")
                        send({"method": "textDocument/didChange", "params": {
                            "textDocument": {"uri": uri, "version": 2}, "contentChanges": [{"text": canonical}]}})
                        require(request("textDocument/formatting", params)["result"] == [], "LSP is not idempotent")
                    else:
                        require(result == [], f"{name}: language server produced an edit")
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
        "macos-arm64": ("Darwin", "arm64"), "linux-x86_64": ("Linux", "x86_64")}[target], "host differs")
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
                "project": project_report, "language_server": lsp_report, "bounded_cases": bounded,
                "assertions": ["exact fixture/source/program identity", "legacy/canonical execution and checking",
                               "exact action/function/comment bytes and idempotence", "read-only check and mode/inode retention",
                               "invalid/incomplete/missing-semicolon preserve bytes", "raw typed/untyped continuation and EOF",
                               "formatting never imports or invokes source", "batch preflight refuses symlink/directory/missing/duplicate/non-source",
                               "1024 siblings, 64 nested blocks, 280 KiB Unicode parse/idempotence within 30-second command deadlines",
                               "native terminal typed/untyped continuation, retained values, cancel and EOF"]}


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
