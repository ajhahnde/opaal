#!/usr/bin/env python3
"""Run Flash v1 exercises through assembled host product entry points."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import subprocess
import tempfile
import tomllib
from dataclasses import dataclass
from pathlib import Path

FLASH_ROOT = Path(__file__).resolve().parents[1]
REPOSITORY_ROOT = FLASH_ROOT.parents[1]
CONTRACT_PATH = FLASH_ROOT / "exercises/v1.toml"
DEFAULT_EVIDENCE = FLASH_ROOT / "exercises/evidence/host-v1.json"


@dataclass(frozen=True)
class Expected:
    code: int
    stdout: tuple[str, ...] = ()
    stderr: tuple[str, ...] = ()


@dataclass(frozen=True)
class Exercise:
    identifier: str
    summary: str
    argv: tuple[str, ...]
    expected: Expected
    source: str | None = None
    files: tuple[tuple[str, str], ...] = ()
    environment: tuple[tuple[str, str], ...] = ()


# These cases use either the assembled binaries directly or a focused
# executable-boundary acceptance test for PTY/protocol behavior. The public
# contract maps every surface's positive and negative exercise to one of these
# owner ids; aliases allow inseparable surfaces to share one short action.
CASE_OWNERS = {
    "language-values": "language-values",
    "invalid-source-and-literals": "invalid-language",
    "invalid-collection-access": "invalid-language",
    "language-composition": "language-composition",
    "invalid-binding-ownership": "invalid-language",
    "invalid-operators": "invalid-language",
    "invalid-control-flow": "invalid-language",
    "functions-and-modules": "functions-and-modules",
    "invalid-call-contract": "invalid-language",
    "invalid-module-contract": "invalid-modules",
    "commands-and-capture": "commands-and-capture",
    "invalid-command-boundary": "invalid-command-boundary",
    "pipelines-and-files": "pipelines-and-files",
    "invalid-carrier-and-redirection": "invalid-command-boundary",
    "structured-errors": "structured-errors",
    "uncaught-error": "uncaught-error",
    "intrinsics": "intrinsics",
    "invalid-intrinsics": "invalid-language",
    "standard-builtins": "standard-builtins",
    "invalid-builtin-contracts": "invalid-builtin-contracts",
    "job-control": "interactive-jobs",
    "invalid-job-options": "interactive-jobs",
    "launcher-frontends": "launcher-frontends",
    "invalid-launcher-options": "invalid-launcher-options",
    "configuration": "interactive-config",
    "invalid-configuration": "interactive-config",
    "language-server": "language-server",
    "invalid-language-server-lifecycle": "language-server",
    "interactive-editor": "interactive-editor",
    "interactive-cancellation": "interactive-editor",
    "processes-and-jobs": "processes-and-jobs",
    "process-failures-and-limits": "processes-and-jobs",
    "platform-user-paths": "platform-user-paths",
    "withheld-signals": "platform-user-paths",
    "documentation-examples": "documentation-examples",
}


def run_command(
    argv: tuple[str, ...] | list[str],
    *,
    cwd: Path,
    environment: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        argv,
        cwd=cwd,
        env=environment,
        text=True,
        capture_output=True,
        check=False,
    )


def tool_version(argv: list[str]) -> str:
    completed = run_command(argv, cwd=FLASH_ROOT)
    if completed.returncode != 0:
        return f"unavailable ({completed.stderr.strip()})"
    return completed.stdout.strip()


def source_digest() -> str:
    paths = run_command(
        ["git", "ls-files", "-co", "--exclude-standard"], cwd=REPOSITORY_ROOT
    )
    if paths.returncode != 0:
        raise SystemExit(f"cannot enumerate candidate sources: {paths.stderr.strip()}")
    digest = hashlib.sha256()
    for relative in sorted(filter(None, paths.stdout.splitlines())):
        if relative.startswith("components/flash/target/"):
            continue
        if relative == "components/flash/exercises/evidence/host-v1.json":
            continue
        path = REPOSITORY_ROOT / relative
        if not path.is_file():
            continue
        digest.update(relative.encode())
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def git_value(*arguments: str) -> str:
    completed = run_command(["git", *arguments], cwd=REPOSITORY_ROOT)
    return completed.stdout.strip() if completed.returncode == 0 else "unavailable"


def assembled_exercises(binary: Path, fixture_directory: Path) -> list[Exercise]:
    status = fixture_directory / "flash-e2e-status-fixture"
    stream = fixture_directory / "flash-e2e-stream-fixture"
    return [
        Exercise(
            "language-values",
            "Values, access, operators, ranges, loops, matching, and "
            "interpolation execute in one script.",
            (str(binary), "{script}"),
            Expected(0),
            source="""
let project = 'Flash'
let values = [1, 2, 3]
let record = {name: "${$project}", count: 3}
mut total = 0
for value in $values { $total = $total + $value }
mut chosen = 'bad'
match $total {
    6 if $record.name == 'Flash' => { $chosen = 'ok' }
    _ => { $chosen = 'bad' }
}
if $chosen == 'ok' && 3 in 1..=3 && !(4 in 1..4) {
    exit 0
} else {
    exit 91
}
""".strip()
            + "\n",
        ),
        Exercise(
            "invalid-language",
            "A chained comparison is rejected through the script frontend.",
            (str(binary), "{script}"),
            Expected(1, stderr=("comparison operators are non-associative",)),
            source="let invalid = 1 < 2 < 3\n",
        ),
        Exercise(
            "language-composition",
            "Bindings, functions, closure capture, conditions, and live "
            "status compose through fsh.",
            (str(binary), "{script}"),
            Expected(0),
            source=f"""
mut base = 2
def add(left: Int, right: Int) -> Int {{ return $left + $right }}
let offset = 3
let advance = {{|value| $value + $offset}}
if add($base, 3) == 5 && $advance(2) == 5 {{
    ^{status} exit 0
}} else {{
    exit 92
}}
if $status.ok {{ exit 0 }} else {{ exit 93 }}
""".strip()
            + "\n",
        ),
        Exercise(
            "functions-and-modules",
            "Named imports initialize once and expose typed functions to "
            "the root module.",
            (str(binary), "{script}"),
            Expected(0),
            source="""
import { answer, add } from './math.fsh'
if $answer == 42 && add(2, 3) == 5 { exit 0 } else { exit 94 }
""".strip()
            + "\n",
            files=(
                (
                    "math.fsh",
                    "let answer: Int = 42\n"
                    "def add(left: Int, right: Int) -> Int { return $left + $right }\n"
                    "export { answer, add }\n",
                ),
            ),
        ),
        Exercise(
            "invalid-modules",
            "A missing imported export is rejected before program execution.",
            (str(binary), "{script}"),
            Expected(1, stderr=("is not exported",)),
            source="import { missing } from './library.fsh'\n",
            files=(("library.fsh", "let present = 1\nexport { present }\n"),),
        ),
        Exercise(
            "commands-and-capture",
            "Dynamic argv, explicit external execution, text capture, byte "
            "capture, and status reach real processes.",
            (str(binary), "{script}"),
            Expected(0),
            source=f"""
let program = '{stream.name}'
let text = $(command $program source 2 0)
let bytes = $(bytes: ^{stream.name} source 2 0)
let repeated = $(bytes: ^{stream.name} source 2 0)
if $text == 'xx' && $bytes == $repeated {{ exit 0 }} else {{ exit 95 }}
""".strip()
            + "\n",
        ),
        Exercise(
            "invalid-command-boundary",
            "An implicit structured-to-byte carrier edge is rejected before execution.",
            (str(binary), "{script}"),
            Expected(1, stderr=("incompatible pipeline edge",)),
            source=f"^{stream.name} source 1 0 | from json | ^{stream.name} sink 0 0\n",
        ),
        Exercise(
            "pipelines-and-files",
            "External, structured, mixed, and file pipelines cross explicit "
            "representation boundaries.",
            (str(binary), "{script}"),
            Expected(0),
            source=f"""
^{stream.name} source 7 0 > input.txt
open input.txt | save copy.txt
open copy.txt | ^{stream.name} sink 7 0
^{stream.name} source 7 0 | decode bytes | encode bytes | ^{stream.name} sink 7 0
""".strip()
            + "\n",
        ),
        Exercise(
            "structured-errors",
            "Throw, catch, rethrow metadata, and rollback remain distinct "
            "from process status.",
            (str(binary), "{script}"),
            Expected(0),
            source="""
mut state = 'before'
try {
    $state = 'discarded'
    throw 'caught'
} catch error {
    if $error.message == 'caught' && $state == 'before' { exit 0 } else { exit 97 }
}
""".strip()
            + "\n",
        ),
        Exercise(
            "uncaught-error",
            "An uncaught structured error terminates the script with an "
            "anchored diagnostic.",
            (str(binary), "{script}"),
            Expected(1, stderr=("uncaught",)),
            source="throw 'uncaught'\n",
        ),
        Exercise(
            "intrinsics",
            "All four v1 intrinsics execute through one assembled script.",
            (str(binary), "{script}"),
            Expected(0),
            source="""
let from_env = env('FLASH_V1_EXERCISE')
let as_int = int(3.75)
let as_float = float(3)
let matches = glob('*.fsh')
if $from_env != 'present' { exit 91 }
if $as_int != 3 { exit 92 }
if $as_float != 3.0 { exit 93 }
if $matches[0] == glob('*.fsh')[0] { exit 0 } else { exit 94 }
""".strip()
            + "\n",
            environment=(("FLASH_V1_EXERCISE", "present"),),
        ),
        Exercise(
            "standard-builtins",
            "The exact standard namespace is discoverable through assembled "
            "help and which paths.",
            (str(binary), "{script}"),
            Expected(0),
            source="""
let names = [
    'bg', 'cd', 'check', 'collect', 'command', 'decode', 'each', 'encode',
    'exit', 'fg', 'first', 'from', 'get', 'help', 'jobs', 'kill', 'last',
    'length', 'lines', 'ls', 'open', 'pwd', 'save', 'select', 'sort', 'to',
    'update', 'wait', 'where', 'which',
]
let discovered = "$(which ...$names
    | where {|entry| $entry.kind == 'internal'}
    | length
    | to json)"
if $discovered == '30' { help pwd > help.txt; exit 0 } else { exit 99 }
""".strip()
            + "\n",
        ),
        Exercise(
            "invalid-builtin-contracts",
            "Built-in arity and option misuse is rejected at the user boundary.",
            (str(binary), "{script}"),
            Expected(1, stderr=("expects",)),
            source="pwd unexpected\n",
        ),
        Exercise(
            "processes-and-jobs",
            "Foreground, pipeline, and background processes complete and are "
            "reaped by the script session.",
            (str(binary), "{script}"),
            Expected(0),
            source=f"""
^{status} exit 0 && ^{stream.name} source 4 0 | ^{stream.name} sink 4 0
^{status} late 10 late.marker 0 &
wait
let marker = "$(open late.marker | decode utf8)"
if $marker == 'late' {{ exit 0 }} else {{ exit 90 }}
""".strip()
            + "\n",
        ),
    ]


def command_exercises() -> list[Exercise]:
    return [
        Exercise(
            "launcher-frontends",
            "Launcher help, version, checker, planner, and formatter use their "
            "public executable modes.",
            (
                "{cargo}",
                "test",
                "--locked",
                "-p",
                "flash-cli",
                "--test",
                "checker_e2e",
                "--test",
                "formatter_e2e",
                "--test",
                "planner_e2e",
            ),
            Expected(0),
        ),
        Exercise(
            "invalid-launcher-options",
            "Launcher and frontend misuse paths retain distinct statuses and channels.",
            ("{cargo}", "test", "--locked", "-p", "flash-cli", "cli::tests", "--lib"),
            Expected(0),
        ),
        Exercise(
            "interactive-config",
            "All six config settings and their refusal paths reach a real "
            "interactive session.",
            (
                "{cargo}",
                "test",
                "--locked",
                "-p",
                "flash-cli",
                "--test",
                "config_startup",
            ),
            Expected(0),
        ),
        Exercise(
            "language-server",
            "The assembled stdio server executes lifecycle, synchronization, "
            "diagnostics, queries, and refusals.",
            ("{cargo}", "test", "--locked", "-p", "flash-lsp", "--test", "server_e2e"),
            Expected(0),
        ),
        Exercise(
            "interactive-editor",
            "The real PTY editor executes prompts, Unicode, multiline input, "
            "completion, history, cancellation, and restoration.",
            (
                "{cargo}",
                "test",
                "--locked",
                "-p",
                "flash-cli",
                "--test",
                "pty",
                "draws_the_primary_prompt_and_runs_a_command",
            ),
            Expected(0),
        ),
        Exercise(
            "interactive-jobs",
            "Real PTY job built-ins execute stop, list, background, foreground, "
            "wait, and signal paths.",
            (
                "{cargo}",
                "test",
                "--locked",
                "-p",
                "flash-cli",
                "--test",
                "pty",
                "job_builtins_",
            ),
            Expected(0),
        ),
        Exercise(
            "platform-user-paths",
            "Portable and selected-adapter operation contracts execute, "
            "including explicit target signal withholding.",
            (
                "{cargo}",
                "test",
                "--locked",
                "-p",
                "flash-platform-posix",
                "-p",
                "flash-platform-flashos",
            ),
            Expected(0),
        ),
        Exercise(
            "documentation-examples",
            "Language and scripting examples are exercised by their assembled "
            "script and frontend owners.",
            ("{cargo}", "test", "--locked", "-p", "flash-cli", "--test", "e2e"),
            Expected(0),
        ),
    ]


def materialize_argv(argv: tuple[str, ...], substitutions: dict[str, str]) -> list[str]:
    return [substitutions.get(argument, argument) for argument in argv]


def stable_text(value: str, temporary: Path) -> str:
    replacements = {
        str(REPOSITORY_ROOT): "<repository>",
        str(REPOSITORY_ROOT.resolve()): "<repository>",
        str(temporary): "<temporary>",
        str(temporary.resolve()): "<temporary>",
    }
    stable = value
    for source in sorted(replacements, key=len, reverse=True):
        stable = stable.replace(source, replacements[source])
    return stable


def execute_case(
    exercise: Exercise,
    *,
    binary: Path,
    cargo: str,
    base_environment: dict[str, str],
) -> dict:
    with tempfile.TemporaryDirectory(
        prefix=f"flash-v1-{exercise.identifier}-"
    ) as temporary:
        directory = Path(temporary)
        substitutions = {"{cargo}": cargo}
        if exercise.source is not None:
            script = directory / "exercise.fsh"
            script.write_text(exercise.source)
            substitutions["{script}"] = str(script)
        for relative, contents in exercise.files:
            path = directory / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents)
        argv = materialize_argv(exercise.argv, substitutions)
        environment = dict(base_environment)
        environment.update(exercise.environment)
        completed = run_command(
            argv,
            cwd=directory if exercise.source else FLASH_ROOT,
            environment=environment,
        )
        stable_argv = [stable_text(argument, directory) for argument in argv]
        stable_stdout = stable_text(completed.stdout, directory)
        stable_stderr = stable_text(completed.stderr, directory)
        stable_input = (
            stable_text(exercise.source, directory)
            if exercise.source is not None
            else "acceptance owner selected by action"
        )
    missing_stdout = [
        value for value in exercise.expected.stdout if value not in completed.stdout
    ]
    missing_stderr = [
        value for value in exercise.expected.stderr if value not in completed.stderr
    ]
    passed = (
        completed.returncode == exercise.expected.code
        and not missing_stdout
        and not missing_stderr
    )
    return {
        "id": exercise.identifier,
        "summary": exercise.summary,
        "action": stable_argv,
        "input": stable_input,
        "expected": {
            "exit_code": exercise.expected.code,
            "stdout_contains": list(exercise.expected.stdout),
            "stderr_contains": list(exercise.expected.stderr),
        },
        "observed": {
            "exit_code": completed.returncode,
            "stdout": stable_stdout,
            "stderr": stable_stderr,
        },
        "result": "pass" if passed else "fail",
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=("smoke", "ci", "full"), default="full")
    parser.add_argument("--record", type=Path)
    parser.add_argument("--no-build", action="store_true")
    args = parser.parse_args()

    cargo = os.environ.get("CARGO", "cargo")
    if not args.no_build:
        build = run_command(
            [cargo, "build", "--workspace", "--bins", "--locked"], cwd=FLASH_ROOT
        )
        if build.returncode != 0:
            raise SystemExit(build.stderr or build.stdout)

    binary = FLASH_ROOT / "target/debug/fsh"
    fixture_directory = binary.parent
    if not binary.is_file():
        raise SystemExit(f"assembled fsh is missing: {binary}")
    base_environment = dict(os.environ)
    base_environment["PATH"] = os.pathsep.join(
        [str(fixture_directory), base_environment.get("PATH", "")]
    )

    exercises = assembled_exercises(binary, fixture_directory)
    if args.profile != "smoke":
        exercises.extend(command_exercises())
    results: list[dict] = []
    for exercise in exercises:
        print(f"Flash v1 exercise: {exercise.identifier}", flush=True)
        result = execute_case(
            exercise,
            binary=binary,
            cargo=cargo,
            base_environment=base_environment,
        )
        results.append(result)
        if result["result"] != "pass":
            print(json.dumps(result, indent=2))
            raise SystemExit(f"Flash v1 exercise failed: {exercise.identifier}")

    with CONTRACT_PATH.open("rb") as source:
        contract = tomllib.load(source)
    report = {
        "schema_version": 1,
        "suite_version": contract["suite_version"],
        "candidate": {
            "commit": git_value("rev-parse", "HEAD"),
            "tree": git_value("rev-parse", "HEAD^{tree}"),
            "source_sha256": source_digest(),
            "worktree": "dirty" if git_value("status", "--porcelain") else "clean",
        },
        "environment": {
            "id": "host-posix",
            "system": platform.system().lower(),
            "architecture": platform.machine().lower(),
            "python": platform.python_version(),
            "rustc": tool_version(["rustc", "--version"]),
            "cargo": tool_version([cargo, "--version"]),
        },
        "profile": args.profile,
        "contract_cases": CASE_OWNERS,
        "results": results,
        "limitations": [
            "Host results do not establish FlashOS target behavior.",
            "Physical-device execution remains identification- and approval-gated.",
        ],
    }
    rendered = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.record is not None:
        args.record.parent.mkdir(parents=True, exist_ok=True)
        args.record.write_text(rendered)
    else:
        print(rendered)
    print(f"Flash v1 exercises: {len(results)} assembled host cases passed")


if __name__ == "__main__":
    main()
