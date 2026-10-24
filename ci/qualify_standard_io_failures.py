#!/usr/bin/env python3
"""Retain native standard-stream failures and actual bounded file-sink failures."""
from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import os
import platform
import resource
import select
import shutil
import signal
import socket
import stat
import subprocess
import tempfile
import threading
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


def worker_has_pipe(pid: int) -> bool:
    if platform.system() == "Linux":
        try:
            descriptors = list(Path(f"/proc/{pid}/fd").iterdir())
        except FileNotFoundError:
            return False
        for descriptor in descriptors:
            try:
                if int(descriptor.name) > 2 and stat.S_ISFIFO(descriptor.stat().st_mode):
                    return True
            except FileNotFoundError:
                pass
        return False
    library = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    library.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64,
                                    ctypes.c_void_p, ctypes.c_int]
    library.proc_pidinfo.restype = ctypes.c_int
    # Darwin proc_fdinfo is one int32 fd and one uint32 type; PIPE has type 6.
    storage = (ctypes.c_uint32 * 32)()
    count = library.proc_pidinfo(pid, 1, 0, storage, ctypes.sizeof(storage))
    require(0 <= count < ctypes.sizeof(storage) and count % 8 == 0,
            "could not inspect owned worker descriptors")
    return any(storage[index] > 2 and storage[index + 1] == 6
               for index in range(0, count // 4, 2))


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
    require((platform.system(), platform.machine()) in [("Linux", "x86_64"), ("Linux", "aarch64"), ("Darwin", "arm64")],
            "unsupported qualification host")
    output.mkdir(parents=True, exist_ok=True)
    cases = []
    with tempfile.TemporaryDirectory(prefix="opaal-stream-failures-") as temporary:
        work = Path(temporary)
        fixtures = ROOT / "tests/golden/random-values"
        shutil.copyfile(fixtures / "opaal.toml", work / "opaal.toml")
        manifest = work / "opaal.toml"
        manifest.write_text(manifest.read_text().replace(">=1.3.0", f">={VERSION}"))
        target = {("Linux", "x86_64"): "linux", ("Linux", "aarch64"): "linux-aarch64",
                  ("Darwin", "arm64"): "macos"}[(platform.system(), platform.machine())]
        shutil.copyfile(fixtures / f"tools-{target}.toml", work / "tools.toml")
        (work / "authority.toml").write_text(
            'schema_version = 1\nproject = "random_sample"\nenvironment = "ci"\n' + "".join(
                f'[[rules]]\ndecision = "grant"\neffect = "{role}"\nscope = "evaluation"\n'
                'required_enforcement = "enforced"\n'
                for role in ("entropy.system", "stdin.read", "stdout.write", "stderr.write")))

        def run(*args, **kwargs):
            return subprocess.run([str(binary), *args], cwd=work, capture_output=True,
                                  timeout=20, **kwargs)

        def plan(body, roles=("entropy.system", "stdin.read", "stdout.write", "stderr.write")):
            (work / "plan.json").unlink(missing_ok=True)
            (work / "tasks.opaal").write_text(
                "import std::io as io\nimport std::random as random\nimport std::string as string\n"
                "action sample() -> Bool effects { " + " ".join(role + "();" for role in roles) + " } {\n"
                + body.replace("return ", "") + "\nreturn true\n}\ntask sample = sample\n")
            result = run("plan", "--project", "opaal.toml", "--task", "sample", "--environment", "ci",
                         "--expires-in", "900s", "--out", "plan.json")
            require(result.returncode == 0, f"plan failed: {result.stderr!r}")
            document = json.loads((work / "plan.json").read_bytes())
            require(document["schema"] == "opaal.plan.v4", "plan is not metadata-only")
            return ["execute", "--plan", "plan.json", "--accept", document["digest"],
                    "--run-id", RUN_ID, "--require-true"], document["digest"]

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
            require(document["schema"] == "opaal.execution-receipt.v2"
                    and document["result_policy"] == "require-true", "selected receipt format differs")
            require(document["assessment"] == ({"kind": "bool", "value": True}
                    if document["primary"]["class"] == "success" else None), "assessment differs")
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
            require(not stdout and code == (0 if error == "success" else 2), f"{name} exit/data differs")
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
                        # Null worker stdio and the completed prior write leave only the lent read pipe.
                        if worker_has_pipe(owned[0]):
                            worker = owned[0]
                            break
                    time.sleep(0.001)
                require(worker is not None, "no owned worker during blocked read")
                os.kill(worker if kill_worker else child.pid, signal.SIGKILL if kill_worker else signal.SIGTERM)
                require(child.wait(timeout=10) > 0, "interrupted command succeeded")
                stdout, stderr = child.communicate(timeout=5)
            finally:
                stop(child)
            reaped(worker)
            require(child.returncode == (2 if kill_worker else 130), "selected interruption exit differs")
            require(not stdout, "cancelled read emitted partial data")
            document, journal = retain(name, b"ready", stderr, child.returncode, digest,
                                       complete=not fail_journal)
            require(document["primary"]["class"] == ("error" if kill_worker else "cancelled"),
                    "interruption primary was replaced")
            require(document["primary"]["cancel_reason"] == (None if kill_worker else "requested"),
                    "typed cancellation reason differs")
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
        require(code == 2 and not stdout and b"JOURNAL005" in stderr and CANARY not in stderr,
                "pre-admission journal failure differs")
        require(not (work / f"{name}.jsonl").exists() and not (work / f"{name}.receipt.json").read_bytes(),
                "pre-admission sink failure persisted unexpected evidence")
        cases.append({"name": name, "exit": code, "stdout_bytes": 0, "journal_present": False,
                      "receipt_bytes": 0, "pre_admission": True})

        def task_arguments(name, body, roles):
            arguments, digest = plan(body, roles)
            return ["task", "run", *arguments[1:], "--journal", name + ".jsonl",
                    "--receipt-out", name + ".receipt.json", "--audit-out", name + ".audit.json"], digest

        def task_observation(name, child, stdout, stderr, digest, expected_exit):
            require(child.returncode == expected_exit, name + ": native run exit differs")
            retained = {}
            for suffix in ("jsonl", "receipt.json", "audit.json"):
                path = work / (name + "." + suffix)
                if path.exists():
                    data = path.read_bytes()
                    require(CANARY not in data, name + ": evidence leaked payload")
                    (output / path.name).write_bytes(data)
                    retained[suffix] = len(data)
            require(CANARY not in stderr, name + ": summary leaked payload")
            (output / (name + ".stderr.txt")).write_bytes(stderr)
            (output / (name + ".stdout.txt")).write_bytes(stdout)
            if retained.get("receipt.json"):
                receipt = json.loads((work / (name + ".receipt.json")).read_bytes())
                require(receipt["plan_digest"] == digest, name + ": receipt identity differs")
            cases.append({"name": name, "exit": child.returncode, "artifacts": retained})

        # Pre-effect evidence failure retains the exclusively reserved audit.
        name = "task-baseline"
        arguments, digest = task_arguments(name, "", ())
        baseline = run(*arguments, input=b"")
        task_observation(name, baseline, baseline.stdout, baseline.stderr, digest, 0)
        name = "task-evidence-write-failure"
        arguments, digest = task_arguments(name, "", ())
        failed = run(*arguments, input=b"", preexec_fn=limited_sink(0))
        task_observation(name, failed, failed.stdout, failed.stderr, digest, 2)
        require(not (work / (name + ".jsonl")).exists()
                and (work / (name + ".receipt.json")).read_bytes() == b""
                and (work / (name + ".audit.json")).read_bytes() == b"",
                "failed evidence admission fabricated or removed reserved artifacts")

        for name, closed, requested in (("task-summary-failure", True, False),
                                        ("task-requested-cancel", False, True),
                                        ("task-cancel-summary-failure", True, True)):
            body = "io::print('ready')\nlet input = io::read_stdin(64)" if requested else ""
            roles = ("stdin.read", "stdout.write") if requested else ()
            arguments, digest = task_arguments(name, body, roles)
            child = subprocess.Popen([str(binary), *arguments], cwd=work, stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            if closed:
                child.stderr.close()
                child.stderr = None
            worker = None
            try:
                if requested:
                    require(select.select([child.stdout], [], [], 10)[0], "task read marker timed out")
                    require(child.stdout.read(5) == b"ready", "task did not reach blocked read")
                    deadline = time.monotonic() + 5
                    while time.monotonic() < deadline:
                        owned = children(child.pid)
                        rows = (work / (name + ".jsonl")).read_bytes().splitlines()
                        if owned and rows and json.loads(rows[-1])["kind"] == "effect-before" \
                                and json.loads(rows[-1])["payload"]["effect"] == "stdin.read":
                            require(len(owned) == 1, "unexpected task worker count")
                            worker = owned[0]
                            break
                        time.sleep(0.001)
                    require(worker is not None, "task did not own a blocked-read worker")
                    os.kill(child.pid, signal.SIGTERM)
                    child.wait(timeout=10)
                    child.stdin.close()
                    child.stdin = None
                stdout, stderr = child.communicate(b"" if not requested else None, timeout=10)
            finally:
                stop(child)
            if worker is not None:
                reaped(worker)
            task_observation(name, child, stdout or b"", stderr or b"", digest, 130 if requested else 2)
            receipt = json.loads((work / (name + ".receipt.json")).read_bytes())
            require(receipt["primary"]["class"] == ("cancelled" if requested else "success"),
                    name + ": summary failure replaced the primary")
            require(receipt["primary"]["cancel_reason"] == ("requested" if requested else None),
                    name + ": cancellation reason differs")

        def full_pipe():
            read_fd, write_fd = os.pipe()
            os.set_blocking(write_fd, False)
            try:
                while True:
                    os.write(write_fd, b"x" * 4096)
            except BlockingIOError:
                pass
            os.set_blocking(write_fd, True)
            return read_fd, write_fd

        # The administrative writer must remain cancellable after task finish.
        for name, explain, requested in (("task-blocked-summary-cancel", False, True),
                                         ("task-blocked-explanation-cancel", True, True),
                                         ("task-blocked-summary-timeout", False, False)):
            arguments, digest = task_arguments(name, "", ())
            if explain:
                arguments = ["task", "explain", "--receipt", "task-baseline.receipt.json",
                             "--audit", "task-baseline.audit.json", "--format", "json"]
            read_fd, write_fd = full_pipe()
            child = subprocess.Popen([str(binary), *arguments], cwd=work, stdin=subprocess.DEVNULL,
                                     stdout=write_fd if explain else subprocess.PIPE,
                                     stderr=subprocess.PIPE if explain else write_fd)
            os.close(write_fd)
            worker = None
            try:
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    owned = children(child.pid)
                    if owned:
                        require(len(owned) == 1, "unexpected view worker count")
                        worker = owned[0]
                        break
                    require(child.poll() is None, "view exited before blocked output")
                    time.sleep(0.001)
                require(worker is not None, "view did not own a blocked-output worker")
                if requested:
                    os.kill(child.pid, signal.SIGTERM)
                stdout, stderr = child.communicate(timeout=5 if requested else 40)
            finally:
                stop(child)
                os.close(read_fd)
            reaped(worker)
            task_observation(name, child, stdout or b"", stderr or b"", digest, 130 if requested else 2)
            if not explain:
                receipt = json.loads((work / (name + ".receipt.json")).read_bytes())
                require(receipt["primary"]["class"] == "success"
                        and receipt["assessment"] == {"kind": "bool", "value": True},
                        "summary cancellation replaced the completed task")

        # Explicit secret stdin is separate from controlled task stdin.
        with manifest.open("a") as document:
            document.write('\n[endpoints.cancel]\nurl = "http://127.0.0.1:9/unused"\n'
                           'methods = ["GET"]\nsecret_headers = ["authorization"]\ntls = false\n'
                           '[secrets.token]\nkind = "injected"\n')
        with (work / "authority.toml").open("a") as authority:
            for effect, scope in (("network.http", "endpoint.cancel"),
                                  ("secret.reveal", "secret.token@endpoint.cancel")):
                authority.write(f'[[rules]]\ndecision = "grant"\neffect = "{effect}"\n'
                                f'scope = "{scope}"\nrequired_enforcement = "enforced"\n')
        (work / "tasks.opaal").write_text(
            'import project::endpoints as endpoints\nimport project::secrets as secrets\n'
            'import std::http as http\n'
            'action sample() -> Bool effects { network.http(endpoints::cancel); '
            'secret.reveal(secrets::token, endpoints::cancel); } {\n'
            'let header = http::secret_header("authorization", secrets::token)\n'
            'http::request(endpoints::cancel, "GET", {}, header, null, 1024)\n'
            'return true\n}\ntask sample = sample\n')
        planned = run("plan", "--project", "opaal.toml", "--task", "sample", "--environment", "ci",
                      "--expires-in", "900s", "--out", "secret.plan.json", "--format", "digest")
        require(planned.returncode == 0, "secret cancellation plan failed")
        digest = planned.stdout.decode().strip()
        for name, requested in (("task-blocked-secret-cancel", True),
                                ("task-blocked-secret-timeout", False)):
            arguments = ["task", "run", "--plan", "secret.plan.json", "--accept", digest,
                         "--secret-stdin", "token", "--journal", name + ".jsonl",
                         "--receipt-out", name + ".receipt.json", "--audit-out", name + ".audit.json",
                         "--require-true"]
            child = subprocess.Popen([str(binary), *arguments], cwd=work, stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            worker = None
            try:
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    owned = children(child.pid)
                    if owned:
                        require(len(owned) == 1, "unexpected secret-input worker count")
                        worker = owned[0]
                        break
                    require(child.poll() is None, "secret run exited before blocked input")
                    time.sleep(0.001)
                require(worker is not None, "secret run did not own its input worker")
                if requested:
                    os.kill(child.pid, signal.SIGTERM)
                child.wait(timeout=5 if requested else 40)
                child.stdin.close()
                child.stdin = None
                stdout, stderr = child.communicate(timeout=5)
            finally:
                stop(child)
            reaped(worker)
            task_observation(name, child, stdout, stderr, digest, 130 if requested else 2)
            receipt = json.loads((work / (name + ".receipt.json")).read_bytes())
            require(receipt["primary"]["class"] == "cancelled"
                    and receipt["primary"]["cancel_reason"] == ("requested" if requested else "timeout"),
                    "secret read did not retain typed cancellation")

        # A completed secret read reaches the ordinary HTTP owner exactly once.
        name = "task-secret-complete"
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            listener.settimeout(5)
            manifest.write_text(manifest.read_text().replace("127.0.0.1:9/unused",
                                f"127.0.0.1:{listener.getsockname()[1]}/unused"))
            received = []
            def serve():
                with listener.accept()[0] as connection:
                    connection.settimeout(5)
                    request = b""
                    while b"\r\n\r\n" not in request:
                        chunk = connection.recv(4096)
                        require(chunk and len(request) < 16_384, "HTTP request incomplete or oversized")
                        request += chunk
                    received.append(request)
                    connection.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            server = threading.Thread(target=serve)
            server.start()
            try:
                planned = run("plan", "--project", "opaal.toml", "--task", "sample", "--environment", "ci",
                              "--expires-in", "900s", "--out", "complete.plan.json", "--format", "digest")
                require(planned.returncode == 0, "completed secret plan failed")
                digest = planned.stdout.decode().strip()
                completed = run("task", "run", "--plan", "complete.plan.json", "--accept", digest,
                                "--secret-stdin", "token", "--journal", name + ".jsonl",
                                "--receipt-out", name + ".receipt.json", "--audit-out", name + ".audit.json",
                                "--require-true", input=CANARY)
            finally:
                server.join(timeout=10)
            require(not server.is_alive() and len(received) == 1
                    and b"authorization: " + CANARY + b"\r\n" in received[0].lower(),
                    "completed secret input was not sent once to the selected sink")
        task_observation(name, completed, completed.stdout, completed.stderr, digest, 0)
        receipt = json.loads((work / (name + ".receipt.json")).read_bytes())
        require(receipt["primary"]["class"] == "success" and receipt["assessment"]["value"] is True,
                "completed secret read did not preserve the successful task")

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
