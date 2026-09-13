#!/usr/bin/env python3
"""Qualify the non-publishing OPAAL operational-core workflow."""

from __future__ import annotations

import argparse
import base64
import contextlib
import hashlib
import http.server
import json
import os
import re
import shutil
import ssl
import subprocess
import sys
import tempfile
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator, Sequence


ROOT = Path(__file__).resolve().parents[1]
TEMPLATE = ROOT / "tests/golden/release-readiness"
TLS_FIXTURES = ROOT / "crates/opaal-platform-posix/tests/fixtures"
DEFAULT_BINARY = ROOT / "target/release/opaal"
CANARY = b"opaal-qualification-canary"
MAX_CANDIDATE_BYTES = 16 * 1024 * 1024
RUN_ID_ZERO = "00000000000000000000000000000001"
RUN_ID_READINESS = "00000000000000000000000000000002"
PLAN_DIGEST = re.compile(r"\bsha256:[0-9a-f]{64}\b")
SEMVER = re.compile(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
)


class QualificationError(RuntimeError):
    """One required qualification assertion failed."""


@dataclass(frozen=True)
class Tool:
    name: str
    adapter: str
    path: Path
    version: str
    digest: str


@dataclass
class CommandResult:
    arguments: tuple[str, ...]
    returncode: int
    stdout: bytes
    stderr: bytes


def fail(message: str) -> None:
    raise QualificationError(message)


def sha256_bytes(value: bytes) -> str:
    return "sha256:" + hashlib.sha256(value).hexdigest()


def sha256_file(path: Path) -> str:
    return sha256_bytes(path.read_bytes())


def contains_canary(payload: bytes) -> bool:
    hexadecimal = CANARY.hex().encode("ascii")
    percent_upper = b"".join(f"%{byte:02X}".encode("ascii") for byte in CANARY)
    encodings = (
        CANARY,
        hexadecimal,
        hexadecimal.upper(),
        base64.b64encode(CANARY).rstrip(b"="),
        base64.urlsafe_b64encode(CANARY).rstrip(b"="),
        percent_upper,
        percent_upper.lower(),
    )
    return any(encoding in payload for encoding in encodings)


def native_value(path: Path) -> str:
    return base64.urlsafe_b64encode(os.fsencode(path)).rstrip(b"=").decode("ascii")


def parse_version(output: bytes, prefix: str) -> str:
    try:
        first = output.decode("utf-8").splitlines()[0]
    except (UnicodeDecodeError, IndexError) as error:
        fail(f"{prefix.strip()} version output is not supported: {error}")
    if not first.startswith(prefix):
        fail(f"version output must start with {prefix!r}: {first!r}")
    version = first[len(prefix) :].split()[0]
    if SEMVER.fullmatch(version) is None:
        fail(f"tool version is not canonical SemVer: {version!r}")
    return version


def run_raw(
    arguments: Sequence[os.PathLike[str] | str],
    *,
    cwd: Path,
    environment: dict[str, str] | None = None,
    stdin: bytes | None = None,
    timeout: int = 600,
) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        [os.fspath(argument) for argument in arguments],
        cwd=cwd,
        env=environment,
        input=stdin,
        capture_output=True,
        check=False,
        timeout=timeout,
    )


def required_tool(name: str, *, rustup: bool = False) -> Path:
    if rustup:
        completed = run_raw(["rustup", "which", name], cwd=ROOT, timeout=30)
        if completed.returncode != 0:
            fail(f"rustup could not resolve {name}: {completed.stderr.decode(errors='replace').strip()}")
        path = Path(os.fsdecode(completed.stdout.strip())).resolve()
    else:
        found = shutil.which(name)
        if found is None:
            fail(f"required host tool is unavailable: {name}")
        path = Path(found).resolve()
    if not path.is_file() or path.is_symlink():
        fail(f"required host tool is not one exact regular file: {path}")
    return path


def host_triple(rustc: Path) -> str:
    completed = run_raw([rustc, "-vV"], cwd=ROOT, timeout=30)
    if completed.returncode != 0:
        fail("rustc -vV failed while identifying the qualification host")
    for line in completed.stdout.decode("utf-8", errors="strict").splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ")
    fail("rustc -vV omitted the host triple")


def sanitized_environment(project: Path, tools: Sequence[Tool], rustc: Path, rustdoc: Path) -> dict[str, str]:
    home = project / ".home"
    temporary = project / ".tmp"
    cargo_home = project / ".cargo-home"
    for path in (home, temporary, cargo_home):
        path.mkdir(mode=0o700)
    empty_git_config = project / ".gitconfig-empty"
    empty_git_config.write_bytes(b"")
    path_entries = [str(tool.path.parent) for tool in tools]
    path_entries.extend([str(rustc.parent), "/usr/bin", "/bin"])
    deduplicated_path = os.pathsep.join(dict.fromkeys(path_entries))
    return {
        "HOME": str(home),
        "TMPDIR": str(temporary),
        "PATH": deduplicated_path,
        "CARGO_HOME": str(cargo_home),
        "RUSTC": str(rustc),
        "RUSTDOC": str(rustdoc),
        "LC_ALL": "C",
        "TZ": "UTC",
        "CARGO_NET_OFFLINE": "true",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": str(empty_git_config),
    }


def tool_lock_text(triple: str, environment: dict[str, str], tools: Sequence[Tool]) -> str:
    rows = [
        'schema_version = 1',
        'project = "opaal_golden_readiness"',
        'environment = "ci"',
        f'platform = "{triple}"',
        "",
        "[child_environment]",
        "inherit = []",
    ]
    for name in (
        "HOME",
        "TMPDIR",
        "PATH",
        "CARGO_HOME",
        "RUSTC",
        "RUSTDOC",
        "LC_ALL",
        "TZ",
        "CARGO_NET_OFFLINE",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_GLOBAL",
    ):
        rows.extend(
            (
                "",
                "[[child_environment.variables]]",
                f'name = "{name}"',
                'value = { encoding = "base64url-nopad", platform = "unix", '
                f'value = "{native_value(Path(environment[name])) if name in {"HOME", "TMPDIR", "CARGO_HOME", "RUSTC", "RUSTDOC", "GIT_CONFIG_GLOBAL"} else base64.urlsafe_b64encode(os.fsencode(environment[name])).rstrip(b"=").decode("ascii")}" }}',
            )
        )
    for tool in tools:
        rows.extend(
            (
                "",
                "[[tools]]",
                f'id = "{tool.name}"',
                f'adapter = "{tool.adapter}"',
                'path = { encoding = "base64url-nopad", platform = "unix", '
                f'value = "{native_value(tool.path)}" }}',
                f'version = "{tool.version}"',
                f'digest = "{tool.digest}"',
            )
        )
    return "\n".join(rows) + "\n"


def exact_schema(path: Path, expected: str) -> dict[str, object]:
    try:
        value = json.loads(path.read_bytes())
    except (OSError, json.JSONDecodeError) as error:
        fail(f"{path.name} is not a JSON artifact: {error}")
    if not isinstance(value, dict) or value.get("schema") != expected:
        fail(f"{path.name} does not use {expected}")
    return value


class ReadinessHandler(http.server.BaseHTTPRequestHandler):
    expected = CANARY.decode("ascii")
    received: list[tuple[str, str | None]] = []

    def do_GET(self) -> None:  # noqa: N802 - inherited HTTP hook
        self.__class__.received.append((self.path, self.headers.get("authorization")))
        permitted = self.path == "/readiness" and self.headers.get("authorization") == self.expected
        body = b"{}"
        self.send_response(200 if permitted else 403)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format: str, *_arguments: object) -> None:
        return


@contextlib.contextmanager
def readiness_server() -> Iterator[type[ReadinessHandler]]:
    handler = type("QualificationReadinessHandler", (ReadinessHandler,), {"received": []})
    server = http.server.HTTPServer(("127.0.0.1", 43119), handler)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(TLS_FIXTURES / "server.pem", TLS_FIXTURES / "server-key.pem")
    server.socket = context.wrap_socket(server.socket, server_side=True)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield handler
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=10)
        if thread.is_alive():
            fail("the TLS loopback server did not stop")


class Qualification:
    def __init__(
        self,
        binary: Path,
        project: Path,
        triple: str,
        environment: dict[str, str],
        tools: Sequence[Tool],
    ) -> None:
        self.binary = binary
        self.project = project
        self.triple = triple
        self.environment = dict(environment)
        self.tools = tuple(tools)
        self.results: list[CommandResult] = []

    @property
    def is_linux(self) -> bool:
        return self.triple.endswith("-unknown-linux-gnu")

    @property
    def is_macos(self) -> bool:
        return self.triple == "aarch64-apple-darwin"

    def cli(
        self,
        *arguments: str,
        expected: int = 0,
        stdin: bytes | None = None,
    ) -> CommandResult:
        completed = run_raw(
            [self.binary, *arguments],
            cwd=self.project,
            environment=self.environment,
            stdin=stdin,
            timeout=600,
        )
        result = CommandResult(tuple(arguments), completed.returncode, completed.stdout, completed.stderr)
        self.results.append(result)
        if contains_canary(result.stdout) or contains_canary(result.stderr):
            fail("OPAAL command output disclosed the synthetic secret")
        if completed.returncode != expected:
            fail(
                f"opaal {' '.join(arguments)} returned {completed.returncode}, expected {expected}\n"
                f"stdout:\n{completed.stdout.decode(errors='replace')}\n"
                f"stderr:\n{completed.stderr.decode(errors='replace')}"
            )
        return result

    def plan_digest(self, result: CommandResult) -> str:
        match = PLAN_DIGEST.search(result.stdout.decode("utf-8", errors="strict"))
        if match is None:
            fail(f"planner did not report a canonical digest: {result.stdout!r}")
        return match.group(0)

    def inspect_twice(self, kind: str, path: Path) -> bytes:
        first = self.cli(kind, "inspect", str(path))
        second = self.cli(kind, "inspect", str(path))
        if first.stdout != second.stdout or first.stderr or second.stderr:
            fail(f"{kind} inspection is not deterministic and silent on stderr")
        return first.stdout

    def qualify_secret_free(self) -> None:
        inspect = self.cli("task", "inspect", "--project", "opaal.toml", "secretfree")
        if b"task opaal_golden_readiness::secretfree" not in inspect.stdout:
            fail("task inspection omitted the project-qualified secret-free identity")
        check = self.cli(
            "check", "--project", "opaal.toml", "--task", "secretfree",
            "--environment", "ci", "--input", "repo=.", "--format", "json",
        )
        check_value = json.loads(check.stdout)
        if check_value.get("schema") != "opaal.check.v2" or check_value.get("secrets") != []:
            fail("secret-free check artifact is not canonical or requires a secret")
        plan_path = Path("target/opaal-golden/secretfree.plan.json")
        plan = self.cli(
            "plan", "--project", "opaal.toml", "--task", "secretfree",
            "--environment", "ci", "--input", "repo=.", "--expires-in", "900s",
            "--out", str(plan_path),
        )
        digest = self.plan_digest(plan)
        exact_schema(self.project / plan_path, "opaal.plan.v2")
        self.inspect_twice("plan", plan_path)
        journal = Path("target/opaal-golden/secretfree.run.jsonl")
        self.cli(
            "execute", "--plan", str(plan_path), "--accept", digest,
            "--run-id", RUN_ID_ZERO, "--journal", str(journal),
        )
        audit = Path("target/opaal-golden/secretfree.audit.json")
        self.cli("audit", "--project", "opaal.toml", "--journal", str(journal), "--out", str(audit))
        value = exact_schema(self.project / audit, "opaal.audit.v2")
        if value.get("completeness") != "complete":
            fail("secret-free execution did not produce a complete audit")
        self.inspect_twice("audit", audit)

        lines = (self.project / journal).read_bytes().splitlines(keepends=True)
        if len(lines) < 3:
            fail("secret-free journal omitted lifecycle records")
        truncated = Path("target/opaal-golden/secretfree.incomplete.run.jsonl")
        (self.project / truncated).write_bytes(b"".join(lines[:-1]))
        incomplete = Path("target/opaal-golden/secretfree.incomplete.audit.json")
        self.cli(
            "audit", "--project", "opaal.toml", "--journal", str(truncated),
            "--out", str(incomplete), expected=1,
        )
        incomplete_value = exact_schema(self.project / incomplete, "opaal.audit.v2")
        if incomplete_value.get("completeness") != "incomplete":
            fail("a valid truncated journal was not classified as incomplete")
        self.inspect_twice("audit", incomplete)

    def qualify_secret_cardinality(self) -> None:
        arguments = (
            "--project", "opaal.toml", "--task", "manysecrets", "--environment", "ci",
            "--input-file", "candidate=target/opaal",
        )
        check = self.cli("check", *arguments, "--format", "json", expected=1)
        check_value = json.loads(check.stdout)
        findings = check_value.get("findings", [])
        if not any(
            finding.get("code") == "CHECK010"
            and "unsupported secret cardinality" in finding.get("message", "")
            for finding in findings
            if isinstance(finding, dict)
        ):
            fail(
                "many-secret check did not expose the stable refusal\n"
                f"stdout:\n{check.stdout.decode(errors='replace')}\n"
                f"stderr:\n{check.stderr.decode(errors='replace')}"
            )
        plan_path = Path("target/opaal-golden/manysecrets.plan.json")
        plan = self.cli(
            "plan", *arguments, "--expires-in", "900s", "--out", str(plan_path), expected=1,
        )
        if b" refused\n" not in plan.stdout:
            fail("many-secret plan did not report its refused outcome")
        value = exact_schema(self.project / plan_path, "opaal.plan.v2")
        if value.get("outcome", {}).get("class") != "refused":
            fail("many-secret plan is not sealed as refused")
        self.inspect_twice("plan", plan_path)

    def qualify_readiness(self) -> None:
        inspect = self.cli("task", "inspect", "--project", "opaal.toml", "release_readiness")
        for expected in (b"process.run tool.git", b"process.run tool.cargo", b"network.http endpoint.readiness"):
            if expected not in inspect.stdout:
                fail(f"readiness inspection omitted {expected.decode()}")
        arguments = (
            "--project", "opaal.toml", "--task", "release_readiness", "--environment", "ci",
            "--input-file", "candidate=target/opaal",
        )
        expected_status = 0 if self.is_linux else 1
        check = self.cli("check", *arguments, "--format", "json", expected=expected_status)
        check_value = json.loads(check.stdout)
        if check_value.get("schema") != "opaal.check.v2":
            fail("readiness check did not emit an opaal.check.v2 artifact")
        if self.is_macos:
            findings = check_value.get("findings", [])
            if not any(
                finding.get("code") == "CHECK008" and "unsupported" in finding.get("message", "")
                for finding in findings
                if isinstance(finding, dict)
            ):
                fail("macOS readiness check did not report the process boundary")

        plan_path = Path("target/opaal-golden/release.plan.json")
        plan = self.cli(
            "plan", *arguments, "--expires-in", "900s", "--out", str(plan_path),
            expected=expected_status,
        )
        plan_value = exact_schema(self.project / plan_path, "opaal.plan.v2")
        self.assert_readiness_plan(plan_value)
        view = self.inspect_twice("plan", plan_path)
        if CANARY in view:
            fail("plan inspection disclosed the synthetic secret")
        if self.is_macos:
            if plan_value.get("outcome", {}).get("class") != "refused" or b" refused\n" not in plan.stdout:
                fail("macOS process-bearing plan is not a visible refusal")
            if (self.project / "target/opaal-golden/release.run.jsonl").exists():
                fail("macOS process refusal created a run journal")
            if (self.project / "target/opaal-golden/readiness.json").exists():
                fail("macOS process refusal created readiness evidence")
            return

        digest = self.plan_digest(plan)
        journal = Path("target/opaal-golden/release.run.jsonl")
        with readiness_server() as handler:
            execute = self.cli(
                "execute", "--plan", str(plan_path), "--accept", digest,
                "--run-id", RUN_ID_READINESS, "--secret-stdin", "readiness_token",
                "--journal", str(journal), stdin=CANARY,
            )
        observations = [
            (path, authorization == CANARY.decode("ascii"))
            for path, authorization in handler.received
        ]
        if observations != [("/readiness", True)]:
            fail(f"readiness endpoint observations differ: {observations!r}")
        if execute.stdout or execute.stderr:
            fail("successful accepted execution was not silent")
        evidence_path = self.project / "target/opaal-golden/readiness.json"
        evidence = json.loads(evidence_path.read_bytes())
        if evidence.get("candidate_digest") != sha256_file(self.project / "target/opaal"):
            fail("readiness evidence did not bind the candidate digest")
        if evidence.get("manifest_digest") != sha256_file(self.project / "Cargo.toml"):
            fail("readiness evidence did not bind the fixture manifest digest")
        if evidence.get("lock_digest") != sha256_file(self.project / "Cargo.lock"):
            fail("readiness evidence did not bind the fixture lock digest")
        if evidence.get("service_status") != 200:
            fail("readiness evidence did not retain the HTTP status")
        audit = Path("target/opaal-golden/release.audit.json")
        self.cli("audit", "--project", "opaal.toml", "--journal", str(journal), "--out", str(audit))
        audit_value = exact_schema(self.project / audit, "opaal.audit.v2")
        if audit_value.get("completeness") != "complete":
            fail("readiness workflow did not produce a complete audit")
        audit_view = self.inspect_twice("audit", audit)
        for operation in (b"std::process::run", b"std::http::request", b"std::http::secret_reveal", b"std::filesystem::write_atomic"):
            if operation not in audit_view:
                fail(f"readiness audit omitted {operation.decode()}")

    def assert_readiness_plan(self, value: dict[str, object]) -> None:
        project = value.get("project")
        task = value.get("task")
        inputs = value.get("inputs")
        secrets = value.get("secrets")
        authority = value.get("authority")
        planned_tools = value.get("tools")
        actions = value.get("actions")
        if not all(
            isinstance(item, dict)
            for item in (project, task, authority)
        ) or not all(isinstance(item, list) for item in (inputs, secrets, planned_tools, actions)):
            fail("readiness plan omitted a required identity collection")
        assert isinstance(project, dict)
        assert isinstance(task, dict)
        assert isinstance(authority, dict)
        assert isinstance(inputs, list)
        assert isinstance(secrets, list)
        assert isinstance(planned_tools, list)
        assert isinstance(actions, list)
        if (project.get("name"), project.get("environment")) != (
            "opaal_golden_readiness",
            "ci",
        ):
            fail("readiness plan did not bind the project and environment identity")
        if project.get("manifest_digest") != sha256_file(self.project / "opaal.toml"):
            fail("readiness plan did not bind the project manifest digest")
        if project.get("tool_lock_digest") != sha256_file(self.project / "tools-host.toml"):
            fail("readiness plan did not bind the host tool-lock digest")
        tls = project.get("tls")
        expected_tls = (
            "readiness",
            "https://127.0.0.1:43119/readiness",
            ["GET"],
            ["authorization"],
            "opaal-golden.invalid",
            sha256_file(self.project / "ca.pem"),
        )
        if not isinstance(tls, list) or len(tls) != 1 or not isinstance(tls[0], dict):
            fail("readiness plan omitted its exact TLS binding")
        observed_tls = (
            tls[0].get("endpoint"),
            tls[0].get("origin"),
            tls[0].get("methods"),
            tls[0].get("secret_headers"),
            tls[0].get("server_name"),
            tls[0].get("ca_digest"),
        )
        if observed_tls != expected_tls:
            fail(f"readiness plan TLS identity differs: {observed_tls!r}")
        if task.get("id") != "opaal_golden_readiness::release_readiness":
            fail("readiness plan did not bind the project-qualified task identity")
        sources = value.get("sources")
        expected_sources = {
            sha256_file(self.project / "release-readiness.opaal"),
            sha256_file(self.project / "tasks.opaal"),
        }
        if not isinstance(sources, list) or {
            item.get("digest") for item in sources if isinstance(item, dict)
        } != expected_sources:
            fail("readiness plan did not bind the exact source closure")
        expected_input = sha256_file(self.project / "target/opaal")
        if len(inputs) != 1 or not isinstance(inputs[0], dict) or (
            inputs[0].get("name"),
            inputs[0].get("type"),
            inputs[0].get("binding"),
            inputs[0].get("digest"),
        ) != ("candidate", "Path", "file", expected_input):
            fail("readiness plan did not bind the exact candidate input")
        if secrets != [
            {"endpoint": "readiness", "header": "authorization", "id": "readiness_token"}
        ]:
            fail("readiness plan secret requirement differs")
        expected_tools = {
            tool.name: (tool.adapter, tool.version, tool.digest) for tool in self.tools
        }
        observed_tools = {
            item.get("id"): (
                item.get("adapter"),
                item.get("locked_version"),
                item.get("digest"),
            )
            for item in planned_tools
            if isinstance(item, dict)
            and item.get("platform") == self.triple
            and item.get("version_verified") is False
        }
        if observed_tools != expected_tools:
            fail(f"readiness plan tool identities differ: {observed_tools!r}")
        requests = authority.get("requests")
        if not isinstance(requests, list):
            fail("readiness plan omitted authority requests")
        if authority.get("digest") != sha256_file(self.project / "authority-ci.toml"):
            fail("readiness plan did not bind the authority document digest")
        process_verdicts = {
            item.get("scope", {}).get("tool"): item.get("verdict")
            for item in requests
            if isinstance(item, dict)
            and item.get("effect") == "process.run"
            and isinstance(item.get("scope"), dict)
        }
        expected_process = "granted-unenforced" if self.is_linux else "unsupported"
        if process_verdicts != {"cargo": expected_process, "git": expected_process}:
            fail(f"readiness process verdicts differ: {process_verdicts!r}")
        direct_verdicts = {
            item.get("effect"): item.get("verdict")
            for item in requests
            if isinstance(item, dict) and item.get("effect") != "process.run"
        }
        if direct_verdicts != {
            "clock.wall": "granted-enforced",
            "filesystem.read": "granted-enforced",
            "filesystem.write": "granted-enforced",
            "network.http": "granted-enforced",
            "secret.reveal": "granted-enforced",
        }:
            fail(f"readiness direct authority verdicts differ: {direct_verdicts!r}")
        if len(actions) != 1 or not isinstance(actions[0], dict) or actions[0].get("requests") != requests:
            fail("readiness action and authority requests disagree")
        expected_outcome = "success" if self.is_linux else "refused"
        outcome = value.get("outcome")
        if not isinstance(outcome, dict) or outcome.get("class") != expected_outcome:
            fail(f"readiness plan outcome is not {expected_outcome}")

    def assert_no_secret_disclosure(self) -> None:
        payloads = [result.stdout for result in self.results] + [result.stderr for result in self.results]
        artifacts = self.project / "target/opaal-golden"
        payloads.extend(path.read_bytes() for path in artifacts.iterdir() if path.is_file())
        for payload in payloads:
            if contains_canary(payload):
                fail("the synthetic secret crossed a persisted or displayed sink")


def prepare_workspace(
    binary: Path, destination: Path
) -> tuple[Path, str, dict[str, str], tuple[Tool, ...]]:
    project = destination / "release-readiness"
    shutil.copytree(TEMPLATE, project)
    output = project / "target/opaal-golden"
    output.mkdir(parents=True)
    candidate = project / "target/opaal"
    shutil.copy2(binary, candidate)
    if candidate.stat().st_size > MAX_CANDIDATE_BYTES:
        fail(f"candidate exceeds the 16 MiB readiness input limit: {candidate.stat().st_size}")

    git_path = required_tool("git")
    cargo_path = required_tool("cargo", rustup=True)
    rustc_path = required_tool("rustc", rustup=True)
    rustdoc_path = required_tool("rustdoc", rustup=True)
    git_probe = run_raw([git_path, "--version"], cwd=project, timeout=30)
    cargo_probe = run_raw([cargo_path, "--version", "--verbose"], cwd=project, timeout=30)
    if git_probe.returncode != 0 or cargo_probe.returncode != 0:
        fail("maintained host tool probing failed")
    tools = (
        Tool("git", "git", git_path, parse_version(git_probe.stdout, "git version "), sha256_file(git_path)),
        Tool("cargo", "cargo", cargo_path, parse_version(cargo_probe.stdout, "cargo "), sha256_file(cargo_path)),
    )
    environment = sanitized_environment(project, tools, rustc_path, rustdoc_path)
    triple = host_triple(rustc_path)
    if not (triple.endswith("-unknown-linux-gnu") or triple == "aarch64-apple-darwin"):
        fail(f"qualification does not claim host {triple}")
    (project / "tools-host.toml").write_text(
        tool_lock_text(triple, environment, tools), encoding="utf-8"
    )
    initialized = run_raw([git_path, "init", "-q"], cwd=project, environment=environment, timeout=30)
    staged = run_raw(
        [git_path, "add", "--", "Cargo.toml", "Cargo.lock"],
        cwd=project,
        environment=environment,
        timeout=30,
    )
    if initialized.returncode != 0 or staged.returncode != 0:
        fail("the isolated readiness repository could not be initialized")
    return project, triple, environment, tools


def ensure_binary(path: Path) -> Path:
    if not path.exists():
        completed = run_raw(
            ["cargo", "build", "--release", "--locked", "-p", "opaal-cli", "--bin", "opaal"],
            cwd=ROOT,
            timeout=1200,
        )
        if completed.returncode != 0:
            fail(f"release candidate build failed:\n{completed.stderr.decode(errors='replace')}")
    resolved = path.resolve()
    if not resolved.is_file():
        fail(f"OPAAL candidate binary is unavailable: {resolved}")
    return resolved


def repository_gates() -> None:
    for command in (
        [sys.executable, "ci/check_public_boundary.py"],
        [sys.executable, "ci/check_product.py", "source"],
        [sys.executable, "ci/check_benchmarks.py", "--contract-only"],
    ):
        completed = run_raw(command, cwd=ROOT, timeout=300)
        if completed.returncode != 0:
            fail(
                f"repository gate failed: {' '.join(command)}\n"
                f"{completed.stdout.decode(errors='replace')}"
                f"{completed.stderr.decode(errors='replace')}"
            )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", choices=("qualification",), required=True)
    parser.add_argument("--binary", type=Path, default=DEFAULT_BINARY)
    return parser.parse_args()


def main() -> int:
    arguments = parse_args()
    try:
        binary = ensure_binary(arguments.binary)
        repository_gates()
        with tempfile.TemporaryDirectory(prefix="opaal-operational-core-") as temporary:
            project, triple, environment, tools = prepare_workspace(binary, Path(temporary))
            qualification = Qualification(binary, project, triple, environment, tools)
            qualification.qualify_secret_free()
            qualification.qualify_secret_cardinality()
            qualification.qualify_readiness()
            qualification.assert_no_secret_disclosure()
            report = {
                "schema": "opaal-operational-core-qualification-v1",
                "profile": arguments.profile,
                "host": triple,
                "binary_digest": sha256_file(binary),
                "process_execution": "passed" if qualification.is_linux else "unsupported",
                "scenarios": [
                    "secret-free-execution",
                    "incomplete-journal-audit",
                    "many-secret-refusal",
                    "release-readiness-execution" if qualification.is_linux else "release-readiness-refusal",
                ],
                "publication": "not-performed",
            }
            print(json.dumps(report, sort_keys=True, separators=(",", ":")))
    except (OSError, UnicodeError, json.JSONDecodeError, subprocess.SubprocessError, QualificationError) as error:
        print(f"operational-core qualification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
