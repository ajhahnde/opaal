#!/usr/bin/env python3
"""Measure the bounded OPAAL host performance contract and retain raw samples."""

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
from typing import Callable

ROOT = Path(__file__).resolve().parents[1]
CONTRACT_PATH = Path(__file__).with_name("contract-v1.toml")
RESULT_SCHEMA = "opaal-performance-result-v1"
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
    command: list[str],
    *,
    cwd: Path,
    env: dict[str, str],
    expected_returncodes: set[int] | None = None,
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
    expected = expected_returncodes or {0}
    if run.returncode not in expected:
        raise RuntimeError(
            f"command failed ({run.returncode}): {' '.join(command)}\n"
            + run.stderr.decode(errors="replace")
        )
    return elapsed, run.stdout


def warm_command_samples(
    command: list[str],
    warmups: int,
    samples: int,
    *,
    cwd: Path,
    env: dict[str, str],
    expected_returncodes: set[int] | None = None,
    validator: Callable[[bytes], None] | None = None,
) -> tuple[list[int], list[int]]:
    measured: list[int] = []
    discarded: list[int] = []
    for index in range(warmups + samples):
        elapsed, output = timed_run(
            command,
            cwd=cwd,
            env=env,
            expected_returncodes=expected_returncodes,
        )
        if validator is not None:
            validator(output)
        (discarded if index < warmups else measured).append(elapsed)
    return discarded, measured


def fixture_timing_samples(
    fixture: Path,
    mode: str,
    warmups: int,
    samples: int,
    output: Path,
    *,
    cwd: Path,
    env: dict[str, str],
) -> tuple[list[int], list[int]]:
    run = subprocess.run(
        [fixture, mode, str(warmups), str(samples), str(output.resolve())],
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    if run.returncode != 0:
        raise RuntimeError(f"{mode} fixture failed: {run.stderr}")
    classes: dict[str, list[int]] = {"warmup": [], "sample": []}
    for line in run.stdout.splitlines():
        name, value = line.split("=", 1)
        classes[name.removesuffix("_ns")].append(int(value))
    if len(classes["warmup"]) != warmups or len(classes["sample"]) != samples:
        raise RuntimeError(f"{mode} fixture returned an incomplete sample set")
    return classes["warmup"], classes["sample"]


def first_prompt(binary: Path, *, cwd: Path, env: dict[str, str]) -> int:
    started = time.perf_counter_ns()
    pid, descriptor = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.execve(
            binary,
            [str(binary)],
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
                raise RuntimeError(f"opaal exited before the first prompt: {transcript}")
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
                raise RuntimeError("interactive opaal did not exit after `exit`")
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
            raise RuntimeError(f"interactive opaal exited with wait status {status}")
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


def prepare_operational_project(
    binary: Path, destination: Path
) -> tuple[Path, str, dict[str, str]]:
    sys.path.insert(0, str(ROOT))
    from ci import qualify_operational_core as qualification

    project, triple, environment, _ = qualification.prepare_workspace(
        binary, destination
    )
    return project, triple, environment


def validate_task_inspect(output: bytes) -> None:
    if b"task opaal_golden_readiness::release_readiness\n" not in output:
        raise RuntimeError("task inspection omitted the readiness task identity")


def validate_project_check(output: bytes) -> None:
    try:
        document = json.loads(output)
    except (UnicodeError, json.JSONDecodeError) as error:
        raise RuntimeError(f"project check returned invalid JSON: {error}") from error
    if document.get("schema") != "opaal.check.v2":
        raise RuntimeError("project check returned the wrong artifact schema")


def generate_maximum_journal(
    fixture: Path, output: Path, *, cwd: Path, env: dict[str, str]
) -> dict[str, int | str]:
    run = subprocess.run(
        [fixture, "maximum-journal", str(output)],
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    if run.returncode != 0:
        raise RuntimeError(f"maximum-journal fixture failed: {run.stderr}")
    metadata: dict[str, int | str] = {}
    for line in run.stdout.splitlines():
        name, value = line.split("=", 1)
        metadata[name] = int(value) if value.isdecimal() else value
    expected = {"journal_bytes", "journal_lines", "journal_byte_first_excess"}
    if set(metadata) != expected:
        raise RuntimeError("maximum-journal fixture returned incomplete metadata")
    return metadata


def journal_audit_render(
    binary: Path,
    project: Path,
    journal: Path,
    audit: Path,
    *,
    env: dict[str, str],
) -> int:
    started = time.perf_counter_ns()
    generated = subprocess.run(
        [
            binary,
            "audit",
            "--project",
            "opaal.toml",
            "--journal",
            str(journal.relative_to(project)),
            "--out",
            str(audit.relative_to(project)),
        ],
        cwd=project,
        env=env,
        capture_output=True,
        check=False,
    )
    if generated.returncode != 0:
        raise RuntimeError(
            "maximum journal audit failed: " + generated.stderr.decode(errors="replace")
        )
    rendered = subprocess.run(
        [binary, "audit", "inspect", str(audit.relative_to(project))],
        cwd=project,
        env=env,
        capture_output=True,
        check=False,
    )
    elapsed = time.perf_counter_ns() - started
    if rendered.returncode != 0 or b"completeness: complete\n" not in rendered.stdout:
        raise RuntimeError(
            "maximum journal audit rendering failed: "
            + rendered.stderr.decode(errors="replace")
        )
    return elapsed


def journal_samples(
    binary: Path,
    project: Path,
    journal: Path,
    output_directory: Path,
    warmups: int,
    samples: int,
    *,
    env: dict[str, str],
) -> tuple[list[int], list[int]]:
    discarded: list[int] = []
    measured: list[int] = []
    for index in range(warmups + samples):
        audit = output_directory / f"maximum-{index:04}.audit.json"
        elapsed = journal_audit_render(
            binary, project, journal, audit, env=env
        )
        (discarded if index < warmups else measured).append(elapsed)
    return discarded, measured


def peak_journal_rss(
    binary: Path,
    project: Path,
    journal: Path,
    audit: Path,
    *,
    cwd: Path,
    env: dict[str, str],
) -> int:
    run = subprocess.run(
        [
            sys.executable,
            __file__,
            "--operational-rss-worker",
            str(binary),
            str(project),
            str(journal),
            str(audit),
        ],
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    if run.returncode != 0:
        raise RuntimeError(f"maximum journal RSS worker failed: {run.stderr}")
    return int(run.stdout.strip())


def operational_rss_worker(
    binary: Path, project: Path, journal: Path, audit: Path
) -> int:
    journal_audit_render(
        binary,
        project,
        journal,
        audit,
        env=dict(os.environ),
    )
    peak = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    peak_bytes = int(peak) if sys.platform == "darwin" else int(peak) * 1024
    print(peak_bytes)
    return 0


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
        raise SystemExit("OPAAL host benchmarks support Linux and macOS")
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
                "opaal-cli",
                "--bin",
                "opaal",
                "--bin",
                "opaal-benchmark-fixture",
            ],
            cwd=ROOT,
            env=build_environment,
            check=True,
        )
    binary = (ROOT / "target/release/opaal").resolve()
    fixture = (ROOT / "target/release/opaal-benchmark-fixture").resolve()
    if not binary.is_file() or not fixture.is_file():
        raise SystemExit("optimized benchmark binaries are missing")

    with tempfile.TemporaryDirectory(prefix="opaal-benchmark-") as temporary:
        work = Path(temporary)
        home = work / "home"
        run_dir = work / "run"
        completion_dir = work / "completion"
        for directory in (home, run_dir, completion_dir):
            directory.mkdir()
        for index in range(256):
            candidate = completion_dir / f"benchmark-candidate-{index:04}"
            candidate.write_text("not executed\n")
            candidate.chmod(0o700)

        environment = dict(os.environ)
        environment.update(
            {
                "HOME": str(home),
                "XDG_CONFIG_HOME": str(home / "config"),
                "XDG_CACHE_HOME": str(home / "cache"),
                "XDG_STATE_HOME": str(home / "state"),
                "PATH": "",
                "LC_ALL": "C",
                "LANG": "C",
                "TERM": "xterm-256color",
            }
        )
        completion_environment = dict(environment)
        completion_environment["PATH"] = str(completion_dir)
        empty_script = run_dir / "minimal.opaal"
        empty_script.write_text("")

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

        operational_project, operational_triple, operational_environment = (
            prepare_operational_project(binary, work / "operational")
        )
        inspect_command = [
            str(binary),
            "task",
            "inspect",
            "--project",
            "opaal.toml",
            "release_readiness",
        ]
        inspect_warmups, inspect_samples = warm_command_samples(
            inspect_command,
            warmups,
            samples,
            cwd=operational_project,
            env=operational_environment,
            validator=validate_task_inspect,
        )
        measurements.append(
            record(
                "operational-task-inspect-warm",
                "ns",
                inspect_samples,
                warmups=inspect_warmups,
            )
        )

        check_command = [
            str(binary),
            "check",
            "--project",
            "opaal.toml",
            "--task",
            "release_readiness",
            "--environment",
            "ci",
            "--input-file",
            "candidate=target/opaal",
            "--format",
            "json",
        ]
        check_warmups, check_samples = warm_command_samples(
            check_command,
            warmups,
            samples,
            cwd=operational_project,
            env=operational_environment,
            expected_returncodes={1} if sys.platform == "darwin" else {0},
            validator=validate_project_check,
        )
        measurements.append(
            record(
                "operational-project-check-warm",
                "ns",
                check_samples,
                warmups=check_warmups,
            )
        )

        plan_output = run_dir / "one-mib.plan.json"
        plan_warmups, plan_samples = fixture_timing_samples(
            fixture,
            "plan-build-render",
            warmups,
            samples,
            plan_output,
            cwd=run_dir,
            env=environment,
        )
        measurements.append(
            record(
                "operational-plan-build-render-warm",
                "ns",
                plan_samples,
                warmups=plan_warmups,
            )
        )

        operational_output = operational_project / "target/opaal-golden"
        maximum_journal = operational_output / "maximum.run.jsonl"
        journal_metadata = generate_maximum_journal(
            fixture,
            maximum_journal,
            cwd=operational_project,
            env=operational_environment,
        )
        journal_warmups, journal_measured = journal_samples(
            binary,
            operational_project,
            maximum_journal,
            operational_output,
            warmups,
            samples,
            env=operational_environment,
        )
        measurements.append(
            record(
                "operational-journal-audit-render-warm",
                "ns",
                journal_measured,
                warmups=journal_warmups,
            )
        )
        rss_values = [
            peak_journal_rss(
                binary,
                operational_project,
                maximum_journal,
                operational_output / f"maximum-rss-{index:04}.audit.json",
                cwd=operational_project,
                env=operational_environment,
            )
            for index in range(warmups + samples)
        ]
        resource_measurements = [
            {
                "case_id": "operational-journal-audit-render-warm",
                "metric": "peak_rss_bytes",
                "unit": "bytes",
                "warmup_samples": rss_values[:warmups],
                "samples": rss_values[warmups:],
                "summary": summary(rss_values[warmups:]),
            }
        ]

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
        "operational_artifacts": {
            "plan_bytes": contract["operational_artifacts"]["plan_bytes"],
            "plan_action_nodes": contract["operational_artifacts"]["plan_action_nodes"],
            "journal_bytes": journal_metadata["journal_bytes"],
            "journal_lines": journal_metadata["journal_lines"],
            "journal_line_limit": contract["operational_artifacts"]["journal_line_limit"],
            "journal_byte_first_excess": journal_metadata["journal_byte_first_excess"],
            "journal_line_exact_limit": "admitted",
            "journal_line_first_excess": "refused",
        },
        "measurements": measurements,
        "resource_measurements": resource_measurements,
    }
    output = args.output
    if output is None:
        timestamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
        output = ROOT / "benchmarks/results" / f"{timestamp}-{args.profile}-host.json"
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(result, indent=2) + "\n")
    checker = [
        sys.executable,
        ROOT / "ci/check_benchmarks.py",
        "--result",
        output,
    ]
    if args.budget_environment:
        if args.profile != "qualification":
            raise SystemExit("budget evaluation requires the qualification profile")
        checker.extend(["--environment", args.budget_environment])
    subprocess.run(checker, check=True, cwd=ROOT)
    print(f"benchmark result: {output}")
    for measurement in measurements:
        print(f"{measurement['case_id']}: {measurement['summary']}")
    return 0


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "--rss-worker":
        raise SystemExit(rss_worker(Path(sys.argv[2]), int(sys.argv[3])))
    if len(sys.argv) == 6 and sys.argv[1] == "--operational-rss-worker":
        raise SystemExit(
            operational_rss_worker(
                Path(sys.argv[2]),
                Path(sys.argv[3]),
                Path(sys.argv[4]),
                Path(sys.argv[5]),
            )
        )
    raise SystemExit(main())
