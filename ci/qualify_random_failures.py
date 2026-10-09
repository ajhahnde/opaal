#!/usr/bin/env python3
"""Exercise native Random failures with actual journal persistence failures."""
from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import os
import platform
import resource
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
    from .qualify_operational_core import Tool, sanitized_environment, tool_lock_text
else:
    from check_source_formatting import public_paths
    from package_release import ROOT, VERSION
    from qualify_data_processing import require
    from qualify_operational_core import Tool, sanitized_environment, tool_lock_text

RUN_ID = "0123456789abcdef0123456789abcdef"


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def children(pid: int) -> list[int]:
    if platform.system() == "Linux":
        try:
            return [int(value) for value in Path(f"/proc/{pid}/task/{pid}/children").read_text().split()]
        except FileNotFoundError:
            return []
    library = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    library.proc_listchildpids.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
    library.proc_listchildpids.restype = ctypes.c_int
    storage = (ctypes.c_int * 16)()
    require(library.proc_listchildpids(pid, storage, ctypes.sizeof(storage)) >= 0,
            "could not identify owned worker")
    return [value for value in storage if value > 0]


def reaped(pid: int) -> None:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return
    raise RuntimeError(f"owned child {pid} was not reaped")


def qualify(binary: Path, output: Path) -> dict:
    require((platform.system(), platform.machine()) in [("Linux", "x86_64"), ("Darwin", "arm64")],
            "unsupported qualification host")
    target = "linux" if platform.system() == "Linux" else "macos"
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="opaal-random-failures-") as temporary:
        work = Path(temporary).resolve()
        fixtures = ROOT / "tests/golden/random-values"
        for name in ("opaal.toml", "authority.toml"):
            shutil.copyfile(fixtures / name, work / name)
        shutil.copyfile(fixtures / f"tools-{target}.toml", work / "tools.toml")
        manifest = work / "opaal.toml"
        manifest.write_text(manifest.read_text().replace(">=1.3.0", f">={VERSION}"))
        (work / "tasks.opaal").write_text("""import std::random as random
action sample() -> Bytes effects { entropy.system(); } {
    return random::bytes(1048576)
}
task sample = sample
""")

        def run(*args: str, preexec_fn=None):
            return subprocess.run([str(binary), *args], cwd=work, capture_output=True,
                                  timeout=20, preexec_fn=preexec_fn)

        def plan() -> list[str]:
            result = run("plan", "--project", "opaal.toml", "--task", "sample", "--environment", "ci",
                         "--expires-in", "900s", "--out", "sample.plan.json")
            require(result.returncode == 0, f"plan failed: {result.stderr!r}")
            document = json.loads((work / "sample.plan.json").read_bytes())
            require(document["schema"] == "opaal.plan.v3", "expected metadata-only plan")
            return ["execute", "--plan", "sample.plan.json", "--accept", document["digest"],
                    "--run-id", RUN_ID, "--journal"]

        def limited_sink():
            signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
            resource.setrlimit(resource.RLIMIT_FSIZE, (sink_limit, sink_limit))

        def retain(name: str) -> list[dict]:
            data = (work / name).read_bytes()
            (output / name).write_bytes(data)
            return [json.loads(line) for line in data.splitlines()]

        arguments = plan()
        baseline = run(*arguments, "baseline.jsonl")
        require(baseline.returncode == 0, f"native baseline failed: {baseline.stderr!r}")
        rows = retain("baseline.jsonl")
        require([row["kind"] for row in rows] == ["header", "action-start", "effect-before", "effect-after", "action-end", "terminal"],
                "baseline event lifecycle differs")
        transfer = rows[3]["payload"]["operation"]
        require(transfer["confirmed_bytes"] == 1048576 and transfer["uncertain_bytes_upper_bound"] == 0,
                "native baseline progress differs")
        sink_limit = sum(len(line) for line in (work / "baseline.jsonl").read_bytes().splitlines(keepends=True)[:3])
        attempts = []
        for attempt in range(4):
            fail_sink = attempt > 0
            journal = f"worker-{attempt}.jsonl"
            child = subprocess.Popen([str(binary), *arguments, journal], cwd=work, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, preexec_fn=limited_sink if fail_sink else None)
            worker = None
            try:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and child.poll() is None:
                    owned = children(child.pid)
                    if owned:
                        require(len(owned) == 1, "unexpected additional child during native operation")
                        worker = owned[0]
                        os.kill(worker, signal.SIGKILL)
                        break
                    time.sleep(0.0001)
                stdout, stderr = child.communicate(timeout=10)
            finally:
                if child.poll() is None:
                    for owned in children(child.pid):
                        try:
                            os.kill(owned, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                    child.kill()
                    child.communicate()
            require(worker is not None and child.returncode > 0, "native failure did not terminate unsuccessfully")
            reaped(worker)
            rows = retain(journal)
            if fail_sink:
                require(stdout == b"" and b"JOURNAL005" in stderr and
                        b"error[EXECUTE_LANGUAGE]" in stderr and b"incomplete or unavailable" in stderr,
                        f"native primary/persistence failure differs: {stdout!r} {stderr!r}")
                require([row["kind"] for row in rows] == ["header", "action-start", "effect-before"] and
                        (work / journal).stat().st_size == sink_limit, "failed sink persisted unexpected records")
            else:
                require(rows[-1]["payload"]["primary"]["code"] == "EXECUTE_LANGUAGE", "native failure primary differs")
                transfer = rows[3]["payload"]["operation"]
                require(rows[3]["payload"]["outcome"]["class"] == "error" and
                        transfer["confirmed_bytes"] < 1048576 and transfer["uncertain_bytes_upper_bound"] <= 256,
                        "failed native progress differs")
            audit_name = f"worker-{attempt}-audit.json"
            audit = run("audit", "--project", "opaal.toml", "--journal", journal, "--out", audit_name)
            require((audit.returncode > 0) == fail_sink and not audit.stderr, "audit exit class differs")
            audited = json.loads((work / audit_name).read_bytes())
            require(audited["completeness"] == ("incomplete" if fail_sink else "complete"), "audit completeness differs")
            if fail_sink:
                require(audited["primary"] is None, "failed sink invented durable primary")
            shutil.copyfile(work / audit_name, output / audit_name)
            attempts.append({"sink_failed": fail_sink, "exit": child.returncode, "worker_pid": worker,
                             "worker_reaped": True, "diagnostic": stderr.decode(), "audit_complete": not fail_sink})

        probe = {"supported": target == "linux"}
        if target == "linux":
            # A real maintained probe exits nonzero after recording its own PID.
            (work / "probe.c").write_text("""#include <stdio.h>
#include <unistd.h>
int main(void) {
    FILE *file = fopen("probe.pid", "a");
    if (!file) return 99;
    fprintf(file, "%ld\\n", (long)getpid());
    if (fclose(file)) return 98;
    puts("probe-stdout-canary");
    fputs("probe-stderr-canary\\n", stderr);
    return 17;
}
""")
            tool = work / "locked-git"
            subprocess.run(["cc", str(work / "probe.c"), "-o", str(tool)], check=True, timeout=20)
            manifest.write_text(manifest.read_text() + '\n[tools.git]\nadapter = "git"\nversion = ">=2.50.0,<3.0.0"\n')
            authority = work / "authority.toml"
            authority.write_text(authority.read_text() + '\n[[rules]]\ndecision = "grant"\neffect = "process.run"\nscope = "tool.git"\nrequired_enforcement = "acknowledge-unenforced"\n')
            probe_tool = Tool("git", "git", tool, "2.50.0", f"sha256:{sha(tool.read_bytes())}")
            environment = sanitized_environment(work, [probe_tool], Path("/usr/bin/false"), Path("/usr/bin/false"))
            tools = work / "tools.toml"
            tools.write_text(tool_lock_text("x86_64-unknown-linux-gnu", environment, [probe_tool]).replace(
                'project = "opaal_golden_readiness"', 'project = "random_sample"'))
            (work / "tasks.opaal").write_text("""import std::random as random
import std::process as process
import project::tools as tools
action sample() -> Int effects { entropy.system(); process.run(tools::git); } {
    let ignored = process::run(tools::git, [])
    return random::int(7, 8)
}
task sample = sample
""")
            (work / "sample.plan.json").unlink()
            arguments = plan()
            baseline = run(*arguments, "probe-baseline.jsonl")
            require(baseline.returncode > 0, "native probe unexpectedly succeeded")
            rows = retain("probe-baseline.jsonl")
            require([row["kind"] for row in rows] == ["header", "effect-before", "effect-after", "terminal"] and
                    rows[-1]["payload"]["primary"]["code"] == "EXECUTE_STALE" and
                    rows[-1]["payload"]["primary"]["partial"] is True, "probe failure lifecycle differs")
            sink_limit = sum(len(line) for line in (work / "probe-baseline.jsonl").read_bytes().splitlines(keepends=True)[:2])
            failed = run(*arguments, "probe-failed.jsonl", preexec_fn=limited_sink)
            require(failed.returncode > 0 and not failed.stdout and b"JOURNAL005" in failed.stderr and
                    b"refused[EXECUTE_STALE]" in failed.stderr, "probe primary lost on actual persistence failure")
            rows = retain("probe-failed.jsonl")
            require([row["kind"] for row in rows] == ["header", "effect-before"] and
                    (work / "probe-failed.jsonl").stat().st_size == sink_limit, "probe journal prefix differs")
            pids = [int(pid) for pid in (work / "probe.pid").read_text().splitlines()]
            require(len(pids) == len(set(pids)) == 2, "native probe was skipped or retried")
            for pid in pids:
                reaped(pid)
            for data in (baseline.stdout, baseline.stderr, failed.stdout, failed.stderr,
                         (work / "probe-baseline.jsonl").read_bytes(), (work / "probe-failed.jsonl").read_bytes()):
                for canary in (b"probe-stdout-canary", b"probe-stderr-canary"):
                    require(canary not in data and sha(canary).encode() not in data, "probe payload or digest leaked")
            audit = run("audit", "--project", "opaal.toml", "--journal", "probe-failed.jsonl", "--out", "probe-audit.json")
            require(audit.returncode > 0 and not audit.stderr, "probe prefix audit exit differs")
            audited = json.loads((work / "probe-audit.json").read_bytes())
            require(audited["completeness"] == "incomplete" and audited["primary"] is None,
                    "probe audit invented durable completion")
            shutil.copyfile(work / "probe-audit.json", output / "probe-audit.json")
            probe.update({"count": len(pids), "reaped": True, "exit": failed.returncode,
                          "diagnostic": failed.stderr.decode()})
        source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip()
        snapshot = {name: sha((ROOT / name).read_bytes()) for name in public_paths(ROOT)}
        return {"source": source, "committed_source": not bool(subprocess.check_output(
                    ["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT)),
                "source_snapshot_sha256": sha(json.dumps(snapshot, sort_keys=True, separators=(",", ":")).encode()),
                "binary_sha256": sha(binary.read_bytes()), "host": platform.platform(),
                "worker_failures": attempts, "probe_failure": probe,
                "limits": ["worker death may occur during launch, READY or fill",
                           "backend EIO, cancellation and cleanup combined with persistence failure are separate gates",
                           "timing and memory guarantees are not inferred"]}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    result = qualify(args.binary.resolve(), args.output.resolve())
    (args.output / "failures.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print("Native Random failure qualification passed.")


if __name__ == "__main__":
    main()
