#!/usr/bin/env python3
"""Measure the bounded Flash host performance contract and retain raw samples."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import pty
import re
import resource
import select
import signal
import statistics
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CONTRACT_PATH = Path(__file__).with_name("contract-v1.toml")
RESULT_SCHEMA = "flash-performance-result-v1"
PROMPT = b">> "
CSI_SEQUENCE = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")
DSR_QUERY = b"\x1b[6n"
DSR_RESPONSE = b"\x1b[1;1R"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--profile", choices=("smoke", "qualification"), default="qualification"
    )
    parser.add_argument("--output", type=Path)
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument(
        "--budget-environment",
        help="Evaluate the qualification result against one matching budget",
    )
    return parser.parse_args()


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def pinned_tool(name: str) -> Path:
    try:
        selected = subprocess.check_output(
            ["rustup", "which", name], cwd=ROOT, text=True
        ).strip()
    except (OSError, subprocess.CalledProcessError) as error:
        raise SystemExit(
            f"cannot resolve pinned {name} through rustup: {error}"
        ) from error
    path = Path(selected)
    if not path.is_file():
        raise SystemExit(f"rustup selected a missing {name}: {path}")
    return path


def nearest_rank(values: list[int], percentile: float) -> int:
    ordered = sorted(values)
    rank = max(1, (len(ordered) * int(percentile * 100) + 99) // 100)
    return ordered[min(rank, len(ordered)) - 1]


def summary(values: list[int]) -> dict[str, int]:
    return {
        "minimum": min(values),
        "median": int(statistics.median(values)),
        "p95": nearest_rank(values, 0.95),
        "maximum": max(values),
    }


def timed_run(
    command: list[str], *, cwd: Path, env: dict[str, str]
) -> tuple[int, bytes]:
    started = time.perf_counter_ns()
    run = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        capture_output=True,
        check=False,
    )
    elapsed = time.perf_counter_ns() - started
    if run.returncode != 0:
        raise RuntimeError(
            f"command failed ({run.returncode}): {' '.join(command)}\n"
            + run.stderr.decode(errors="replace")
        )
    return elapsed, run.stdout


def first_prompt(binary: Path, *, cwd: Path, env: dict[str, str]) -> int:
    started = time.perf_counter_ns()
    pid, descriptor = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.execve(
            binary,
            [str(binary), "--no-config", "--no-history"],
            env,
        )
    captured = bytearray()
    answered_queries = 0
    deadline = time.monotonic() + 15
    try:
        while PROMPT not in CSI_SEQUENCE.sub(b"", bytes(captured)):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("timed out waiting for the first prompt")
            ready, _, _ = select.select([descriptor], [], [], min(0.25, remaining))
            if not ready:
                continue
            chunk = os.read(descriptor, 65536)
            if not chunk:
                transcript = CSI_SEQUENCE.sub(b"", bytes(captured)).decode(
                    errors="replace"
                )
                raise RuntimeError(f"fsh exited before the first prompt: {transcript}")
            captured.extend(chunk)
            observed_queries = bytes(captured).count(DSR_QUERY)
            while answered_queries < observed_queries:
                os.write(descriptor, DSR_RESPONSE)
                answered_queries += 1
        elapsed = time.perf_counter_ns() - started
        os.write(descriptor, b"exit\n")
        exit_deadline = time.monotonic() + 5
        status = None
        while status is None:
            waited, candidate = os.waitpid(pid, os.WNOHANG)
            if waited == pid:
                status = candidate
                break
            if time.monotonic() >= exit_deadline:
                os.kill(pid, signal.SIGKILL)
                _, status = os.waitpid(pid, 0)
                raise RuntimeError("interactive fsh did not exit after `exit`")
            ready, _, _ = select.select([descriptor], [], [], 0.05)
            if ready:
                try:
                    chunk = os.read(descriptor, 65536)
                except OSError:
                    chunk = b""
                if chunk:
                    captured.extend(chunk)
                    observed_queries = bytes(captured).count(DSR_QUERY)
                    while answered_queries < observed_queries:
                        os.write(descriptor, DSR_RESPONSE)
                        answered_queries += 1
        if status != 0:
            raise RuntimeError(f"interactive fsh exited with wait status {status}")
        return elapsed
    finally:
        try:
            os.close(descriptor)
        except OSError:
            pass
        try:
            os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            pass


def peak_stream_rss(
    fixture: Path, items: int, *, cwd: Path, env: dict[str, str]
) -> int:
    run = subprocess.run(
        [sys.executable, __file__, "--rss-worker", str(fixture), str(items)],
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    if run.returncode != 0:
        raise RuntimeError(f"structured-stream RSS worker failed: {run.stderr}")
    return int(run.stdout.strip())


def rss_worker(fixture: Path, items: int) -> int:
    process = subprocess.Popen(
        [fixture, "structured-stream", str(items)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None
    assert process.stderr is not None
    if process.stdout.readline() != "ready\n":
        raise RuntimeError("structured-stream fixture did not become ready")
    process.stdin.write("run\n")
    process.stdin.flush()
    output, diagnostics = process.communicate()
    if process.returncode != 0 or f"count={items}\n" not in output:
        raise RuntimeError(
            f"structured-stream fixture failed ({process.returncode}): {diagnostics}"
        )
    peak = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    peak_bytes = int(peak) if sys.platform == "darwin" else int(peak) * 1024
    print(peak_bytes)
    return 0


def completion_samples(
    fixture: Path,
    warmups: int,
    samples: int,
    *,
    cwd: Path,
    env: dict[str, str],
) -> tuple[int, list[int], list[int]]:
    run = subprocess.run(
        [fixture, "completion", str(warmups), str(samples)],
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    if run.returncode != 0:
        raise RuntimeError(f"completion fixture failed: {run.stderr}")
    classes: dict[str, list[int]] = {"cold": [], "warmup": [], "sample": []}
    for line in run.stdout.splitlines():
        name, value = line.split("=", 1)
        classes[name.removesuffix("_ns")].append(int(value))
    if len(classes["cold"]) != 1 or len(classes["sample"]) != samples:
        raise RuntimeError("completion fixture returned an incomplete sample set")
    return classes["cold"][0], classes["warmup"], classes["sample"]


def record(
    case_id: str,
    unit: str,
    values: list[int],
    *,
    warmups: list[int] | None = None,
) -> dict[str, object]:
    return {
        "case_id": case_id,
        "unit": unit,
        "warmup_samples": warmups or [],
        "samples": values,
        "summary": summary(values),
    }


def main() -> int:
    args = parse_args()
    started_utc = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    contract = tomllib.loads(CONTRACT_PATH.read_text())
    settings = contract["profiles"][args.profile]
    warmups = int(settings["warmups"])
    samples = int(settings["samples"])
    cargo = pinned_tool("cargo")
    rustc = pinned_tool("rustc")

    if sys.platform not in {"linux", "darwin"}:
        raise SystemExit("Flash host benchmarks support Linux and macOS")
    if not args.no_build:
        build_environment = dict(os.environ)
        build_environment["RUSTC"] = str(rustc)
        subprocess.run(
            [
                cargo,
                "build",
                "--release",
                "--locked",
                "-p",
                "flash-cli",
                "--bin",
                "fsh",
                "--bin",
                "flash-benchmark-fixture",
            ],
            cwd=ROOT,
            env=build_environment,
            check=True,
        )
    binary = (ROOT / "target/release/fsh").resolve()
    fixture = (ROOT / "target/release/flash-benchmark-fixture").resolve()
    if not binary.is_file() or not fixture.is_file():
        raise SystemExit("optimized benchmark binaries are missing")

    with tempfile.TemporaryDirectory(prefix="flash-benchmark-") as temporary:
        work = Path(temporary)
        home = work / "home"
        run_dir = work / "run"
        completion_dir = work / "completion"
        path_dir = work / "commands"
        for directory in (home, run_dir, completion_dir, path_dir):
            directory.mkdir()
        for index in range(256):
            (completion_dir / f"benchmark-path-{index:04}").write_text("fixture\n")
            command = path_dir / f"benchmark-command-{index:04}"
            command.write_text("#!/bin/sh\nexit 0\n")
            command.chmod(0o755)

        environment = dict(os.environ)
        environment.update(
            {
                "HOME": str(home),
                "XDG_CONFIG_HOME": str(home / "config"),
                "XDG_CACHE_HOME": str(home / "cache"),
                "XDG_STATE_HOME": str(home / "state"),
                "PATH": os.pathsep.join((str(path_dir), os.environ.get("PATH", ""))),
                "LC_ALL": "C",
                "LANG": "C",
                "TERM": "xterm-256color",
            }
        )
        completion_environment = dict(environment)
        completion_environment["PATH"] = str(path_dir)
        empty_script = run_dir / "empty.fsh"
        empty_script.write_text("")
        command_count = int(settings["command_iterations"])
        command_script = run_dir / "commands.fsh"
        command_script.write_text("^/usr/bin/true\n" * command_count)
        pipeline_bytes = int(settings["pipeline_bytes"])
        pipeline_input = run_dir / "pipeline-input.bin"
        with pipeline_input.open("wb") as output:
            output.truncate(pipeline_bytes)
        pipeline_script = run_dir / "pipeline.fsh"
        pipeline_script.write_text(
            f"^/bin/cat {pipeline_input} | ^/bin/cat | ^/usr/bin/wc -c\n"
        )

        measurements: list[dict[str, object]] = []
        cold_startup, _ = timed_run(
            [str(binary), str(empty_script)], cwd=run_dir, env=environment
        )
        startup_warmups = [
            timed_run([str(binary), str(empty_script)], cwd=run_dir, env=environment)[0]
            for _ in range(warmups)
        ]
        startup_samples = [
            timed_run([str(binary), str(empty_script)], cwd=run_dir, env=environment)[0]
            for _ in range(samples)
        ]
        measurements.append(record("host-startup-cold", "ns", [cold_startup]))
        measurements.append(
            record(
                "host-startup-warm",
                "ns",
                startup_samples,
                warmups=startup_warmups,
            )
        )

        cold_prompt = first_prompt(binary, cwd=run_dir, env=environment)
        prompt_warmups = [
            first_prompt(binary, cwd=run_dir, env=environment) for _ in range(warmups)
        ]
        prompt_samples = [
            first_prompt(binary, cwd=run_dir, env=environment) for _ in range(samples)
        ]
        measurements.append(record("host-first-prompt-cold", "ns", [cold_prompt]))
        measurements.append(
            record(
                "host-first-prompt-warm",
                "ns",
                prompt_samples,
                warmups=prompt_warmups,
            )
        )

        command_warmups = [
            timed_run([str(binary), str(command_script)], cwd=run_dir, env=environment)[
                0
            ]
            // command_count
            for _ in range(warmups)
        ]
        command_samples = [
            timed_run([str(binary), str(command_script)], cwd=run_dir, env=environment)[
                0
            ]
            // command_count
            for _ in range(samples)
        ]
        measurements.append(
            record(
                "host-command-overhead-warm",
                "ns/command",
                command_samples,
                warmups=command_warmups,
            )
        )

        def pipeline_sample() -> int:
            elapsed, output = timed_run(
                [str(binary), str(pipeline_script)], cwd=run_dir, env=environment
            )
            if str(pipeline_bytes).encode() not in output:
                raise RuntimeError("pipeline fixture returned the wrong byte count")
            return pipeline_bytes * 1_000_000_000 // elapsed

        pipeline_warmups = [pipeline_sample() for _ in range(warmups)]
        pipeline_samples = [pipeline_sample() for _ in range(samples)]
        measurements.append(
            record(
                "host-pipeline-throughput-warm",
                "bytes/second",
                pipeline_samples,
                warmups=pipeline_warmups,
            )
        )

        stream_items = int(settings["stream_items"])
        stream_warmups = [
            peak_stream_rss(fixture, stream_items, cwd=run_dir, env=environment)
            for _ in range(warmups)
        ]
        stream_samples = [
            peak_stream_rss(fixture, stream_items, cwd=run_dir, env=environment)
            for _ in range(samples)
        ]
        measurements.append(
            record(
                "host-structured-stream-memory-warm",
                "bytes",
                stream_samples,
                warmups=stream_warmups,
            )
        )

        completion_cold, completion_warmups, completion_measured = completion_samples(
            fixture,
            warmups,
            samples,
            cwd=completion_dir,
            env=completion_environment,
        )
        measurements.append(record("host-completion-cold", "ns", [completion_cold]))
        measurements.append(
            record(
                "host-completion-warm",
                "ns",
                completion_measured,
                warmups=completion_warmups,
            )
        )

    result = {
        "schema": RESULT_SCHEMA,
        "suite_version": contract["suite_version"],
        "profile": args.profile,
        "started_utc": started_utc,
        "finished_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "contract_sha256": digest(CONTRACT_PATH),
        "binary_sha256": digest(binary),
        "environment": {
            "kind": "host",
            "os": "macos" if sys.platform == "darwin" else platform.system().lower(),
            "os_release": platform.release(),
            "architecture": platform.machine().lower(),
            "python": platform.python_version(),
            "rustc": subprocess.check_output([rustc, "--version"], text=True).strip(),
            "cargo": subprocess.check_output([cargo, "--version"], text=True).strip(),
            "logical_cpus": os.cpu_count(),
            "load_average_at_finish": list(os.getloadavg()),
        },
        "noise_controls": {
            "optimized_binary": True,
            "isolated_home": True,
            "config_disabled": True,
            "history_disabled": True,
            "locale": "C",
            "completion_path_isolated": True,
            "manual_cache_flush": False,
            "sample_order": "surface-grouped; cold first; warmups discarded",
        },
        "parameters": settings,
        "measurements": measurements,
    }
    output = args.output
    if output is None:
        timestamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
        output = ROOT / "benchmarks/results" / f"{timestamp}-{args.profile}-host.json"
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(result, indent=2) + "\n")
    checker = [sys.executable, ROOT.parents[1] / "ci/flash_benchmarks.py"]
    if args.budget_environment:
        if args.profile != "qualification":
            raise SystemExit("budget evaluation requires the qualification profile")
        checker.extend(["--evaluate", output, "--environment", args.budget_environment])
    else:
        checker.extend(["--result", output])
    subprocess.run(checker, check=True)
    print(f"benchmark result: {output}")
    for measurement in measurements:
        print(f"{measurement['case_id']}: {measurement['summary']}")
    return 0


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "--rss-worker":
        raise SystemExit(rss_worker(Path(sys.argv[2]), int(sys.argv[3])))
    raise SystemExit(main())
