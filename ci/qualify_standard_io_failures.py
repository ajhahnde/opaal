#!/usr/bin/env python3
"""Retain native standard-stream failures and actual bounded file-sink failures."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import resource
import select
import shutil
import signal
import subprocess
import tempfile
import time
from pathlib import Path

if __package__:
    from .check_source_formatting import public_paths
    from .package_release import ROOT, VERSION
    from .qualify_data_processing import require
    from .qualify_random_failures import RUN_ID, children, reaped
else:
    from check_source_formatting import public_paths
    from package_release import ROOT, VERSION
    from qualify_data_processing import require
    from qualify_random_failures import RUN_ID, children, reaped

CANARY = b"native-stream-payload-canary"


def stop(child: subprocess.Popen) -> None:
    if child.poll() is None:
        for owned in children(child.pid):
            try:
                os.kill(owned, signal.SIGKILL)
            except ProcessLookupError:
                pass
        child.kill()
        child.communicate()


def qualify(binary: Path, output: Path) -> dict:
    require((platform.system(), platform.machine()) in [("Linux", "x86_64"), ("Darwin", "arm64")],
            "unsupported qualification host")
    output.mkdir(parents=True, exist_ok=True)
    cases = []
    with tempfile.TemporaryDirectory(prefix="opaal-stream-failures-") as temporary:
        work = Path(temporary)
        fixtures = ROOT / "tests/golden/random-values"
        shutil.copyfile(fixtures / "opaal.toml", work / "opaal.toml")
        manifest = work / "opaal.toml"
        manifest.write_text(manifest.read_text().replace(">=1.3.0", f">={VERSION}"))
        target = "macos" if platform.system() == "Darwin" else "linux"
        shutil.copyfile(fixtures / f"tools-{target}.toml", work / "tools.toml")
        (work / "authority.toml").write_text(
            'schema_version = 1\nproject = "random_sample"\nenvironment = "ci"\n' + "".join(
                f'[[rules]]\ndecision = "grant"\neffect = "{role}"\nscope = "evaluation"\n'
                'required_enforcement = "enforced"\n'
                for role in ("entropy.system", "stdin.read", "stdout.write", "stderr.write")))

        def run(*args, **kwargs):
            return subprocess.run([str(binary), *args], cwd=work, capture_output=True,
                                  timeout=20, **kwargs)

        def plan(body):
            (work / "plan.json").unlink(missing_ok=True)
            (work / "tasks.opaal").write_text(
                "import std::io as io\nimport std::random as random\nimport std::string as string\n"
                "action sample() -> Null effects { entropy.system(); stdin.read(); stdout.write(); stderr.write(); } {\n"
                + body + "\n}\ntask sample = sample\n")
            result = run("plan", "--project", "opaal.toml", "--task", "sample", "--environment", "ci",
                         "--expires-in", "900s", "--out", "plan.json")
            require(result.returncode == 0, f"plan failed: {result.stderr!r}")
            document = json.loads((work / "plan.json").read_bytes())
            require(document["schema"] == "opaal.plan.v3", "plan is not metadata-only")
            return ["execute", "--plan", "plan.json", "--accept", document["digest"],
                    "--run-id", RUN_ID], document["digest"]

        def limited_sink(limit):
            def apply():
                signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
                resource.setrlimit(resource.RLIMIT_FSIZE, (limit, limit))
            return apply

        def retain(name, stdout, stderr, exit_code, digest, *, complete=True):
            require(stderr == b"", f"administrative text entered stderr: {stderr!r}")
            journal = (work / f"{name}.jsonl").read_bytes()
            receipt_bytes = (work / f"{name}.receipt.json").read_bytes()
            for data in (journal, receipt_bytes, stderr):
                require(CANARY not in data and hashlib.sha256(CANARY).hexdigest().encode() not in data,
                        "payload or payload digest entered metadata-only evidence")
            for suffix, data in (("jsonl", journal), ("receipt.json", receipt_bytes)):
                (output / f"{name}.{suffix}").write_bytes(data)
            document = json.loads(receipt_bytes)
            require(document["run_id"] == RUN_ID and document["plan_digest"] == digest,
                    "receipt identity differs")
            require((document["primary"]["class"] == "success" and not document["secondary"])
                    == (exit_code == 0), "receipt and command outcome disagree")
            audit = run("audit", "--project", "opaal.toml", "--journal", f"{name}.jsonl",
                        "--out", f"{name}.audit.json")
            require(not audit.stderr, "audit diagnostic entered stderr")
            audit_path = work / f"{name}.audit.json"
            if audit_path.exists():
                audit_bytes = audit_path.read_bytes()
                require(CANARY not in audit_bytes and hashlib.sha256(CANARY).hexdigest().encode() not in audit_bytes,
                        "payload or payload digest entered audit evidence")
                audited = json.loads(audit_bytes)
                require(audited["completeness"] == ("complete" if complete else "incomplete"),
                        "audit completeness differs")
                if not complete:
                    require(audited["primary"] is None, "unfinished journal invented a durable primary")
                else:
                    require(audited["primary"] == document["primary"], "audit and receipt primary disagree")
                shutil.copyfile(audit_path, output / audit_path.name)
            require((audit.returncode == 0) == complete, "audit exit class differs")
            cases.append({"name": name, "exit": exit_code, "stdout_bytes": len(stdout),
                          "journal_bytes": len(journal), "receipt_bytes": len(receipt_bytes),
                          "journal_complete": complete, "receipt_complete": True})
            return document, journal

        def execute(name, body, data=b"", *, closed=None, limit=None):
            arguments, digest = plan(body)
            child = subprocess.Popen([str(binary), *arguments, "--journal", f"{name}.jsonl",
                                      "--receipt-out", f"{name}.receipt.json"], cwd=work,
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                     preexec_fn=limited_sink(limit) if limit is not None else None)
            if closed:
                getattr(child, closed).close()
                setattr(child, closed, None)
            try:
                stdout, stderr = child.communicate(data, timeout=20)
            finally:
                stop(child)
            return stdout or b"", stderr or b"", child.returncode, digest

        body = ("let entropy = random::bytes(8)\nlet input = io::read_stdin(64)\n"
                "io::write_stdout(input)\nio::write_stderr(input)\nreturn io::write_stdout(io::read_stdin(0))")
        stdout, stderr, code, digest = execute("roles", body, CANARY)
        require(code == 0 and stdout == stderr == CANARY, "native role bytes differ")
        # Stderr contains only the requested data in this one successful case.
        document, _ = retain("roles", stdout, b"", code, digest)
        for role, count in (("stdin.read", len(CANARY)), ("stdout.write", len(CANARY)),
                            ("stderr.write", len(CANARY)), ("entropy.system", 8)):
            require(document["progress"][role]["confirmed_bytes"] == count and
                    document["progress"][role]["uncertain_bytes_upper_bound"] == 0,
                    f"cumulative {role} counts differ")

        for name, body, data, closed, error in [
            ("overflow", "return io::write_stdout(io::read_stdin(4))", CANARY, None, "error"),
            ("invalid-utf8", "return io::print(string::decode_utf8(io::read_stdin(64)))", b"\xff", None, "error"),
            ("read-limit", "return io::write_stdout(io::read_stdin(1048577))", b"", None, "error"),
            ("broken-stdout", "let input = io::read_stdin(0)\nreturn io::print('native-stream-payload-canary')", b"", "stdout", "error"),
            ("broken-stderr", "let input = io::read_stdin(0)\nreturn io::eprint('native-stream-payload-canary')", b"", "stderr", "error"),
            ("empty-eof", "io::write_stdout(io::read_stdin(0))\nreturn io::write_stdout(io::read_stdin(0))", b"", None, "success"),
        ]:
            stdout, stderr, code, digest = execute(name, body, data, closed=closed)
            require(not stdout and (code == 0) == (error == "success"), f"{name} exit/data differs")
            document, _ = retain(name, stdout, stderr, code, digest)
            require(document["primary"]["class"] == error, f"{name} primary differs")
            consumed = {"overflow": 5, "invalid-utf8": 1}.get(name, 0)
            require(document["progress"]["stdin.read"]["confirmed_bytes"] == consumed,
                    f"{name} consumed count differs")

        blocked = "io::print('ready')\nreturn io::write_stdout(io::read_stdin(64))"
        arguments, digest = plan(blocked)
        baseline = run(*arguments, "--journal", "prefix.jsonl", "--receipt-out", "prefix.receipt.json", input=b"")
        require(baseline.returncode == 0 and baseline.stdout == b"ready" and not baseline.stderr,
                "blocked-read baseline differs")
        lines = (work / "prefix.jsonl").read_bytes().splitlines(keepends=True)
        before_read = next(index for index, line in enumerate(lines) if
                           json.loads(line)["kind"] == "effect-before" and
                           json.loads(line)["payload"]["effect"] == "stdin.read")
        sink_limit = sum(map(len, lines[:before_read + 1]))
        for name, kill_worker, fail_journal in [
            ("signal", False, False), ("worker-death", True, False),
            ("signal-journal-failure", False, True), ("worker-journal-failure", True, True),
        ]:
            child = subprocess.Popen([str(binary), *arguments, "--journal", f"{name}.jsonl",
                                      "--receipt-out", f"{name}.receipt.json"], cwd=work,
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                     preexec_fn=limited_sink(sink_limit) if fail_journal else None)
            worker = None
            try:
                # The marker precedes a read held open by this caller.
                require(select.select([child.stdout], [], [], 10)[0], "blocked-read marker timed out")
                require(child.stdout.read(5) == b"ready", f"{name} did not reach the blocked read")
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    owned = children(child.pid)
                    journal_path = work / f"{name}.jsonl"
                    journal_bytes = journal_path.read_bytes() if journal_path.exists() else b""
                    rows = journal_bytes.splitlines() if journal_bytes.endswith(b"\n") else []
                    reading = rows and json.loads(rows[-1])["kind"] == "effect-before" and \
                        json.loads(rows[-1])["payload"]["effect"] == "stdin.read"
                    if owned and reading:
                        require(len(owned) == 1, "unexpected additional worker")
                        worker = owned[0]
                        break
                    time.sleep(0.001)
                require(worker is not None, "no owned worker during blocked read")
                time.sleep(0.05)
                os.kill(worker if kill_worker else child.pid, signal.SIGKILL if kill_worker else signal.SIGTERM)
                require(child.wait(timeout=10) > 0, "interrupted command succeeded")
                stdout, stderr = child.communicate(timeout=5)
            finally:
                stop(child)
            reaped(worker)
            require(not stdout, "cancelled read emitted partial data")
            document, journal = retain(name, b"ready", stderr, child.returncode, digest,
                                       complete=not fail_journal)
            require(document["primary"]["class"] == ("error" if kill_worker else "cancelled"),
                    "interruption primary was replaced")
            require(document["progress"]["stdout.write"]["confirmed_bytes"] == 5,
                    "earlier output count was lost")
            require(document["progress"]["stdin.read"]["confirmed_bytes"] == 0 and
                    document["progress"]["stdin.read"]["uncertain_bytes_upper_bound"] == 65,
                    "interrupted read progress differs")
            if fail_journal:
                require(len(journal) == sink_limit and document["journal_state"] == "unavailable" and
                        any(item["code"] == "JOURNAL005" for item in document["secondary"]),
                        "real journal failure was not retained")
            cases[-1]["worker_reaped"] = True

        name = "journal-creation-failure"
        stdout, stderr, code, _ = execute(name, "return io::print('native-stream-payload-canary')", limit=0)
        require(code > 0 and not stdout and b"JOURNAL005" in stderr and CANARY not in stderr,
                "pre-admission journal failure differs")
        require(not (work / f"{name}.jsonl").exists() and not (work / f"{name}.receipt.json").read_bytes(),
                "pre-admission sink failure persisted unexpected evidence")
        cases.append({"name": name, "exit": code, "stdout_bytes": 0, "journal_present": False,
                      "receipt_bytes": 0, "pre_admission": True})

    snapshot = {name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
                for name in public_paths(ROOT) if (ROOT / name).is_file()}
    return {"source": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip(),
            "committed_source": not bool(subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT)),
            "source_snapshot_sha256": hashlib.sha256(json.dumps(snapshot, sort_keys=True, separators=(",", ":")).encode()).hexdigest(),
            "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "host": platform.platform(),
            "cases": cases, "limits": ["RLIMIT_FSIZE is a real file write limit, not disk-full or quota exhaustion",
                                        "io_receipt_native separately qualifies a receipt-only kernel file-size write failure",
                                        "receipt sync kernel failures use test-only pipe descriptor substitution; no regular-filesystem EIO or power-loss claim"]}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    result = qualify(args.binary.resolve(), args.output.resolve())
    (args.output / "failures.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print("Native standard I/O failure qualification passed.")


if __name__ == "__main__":
    main()
