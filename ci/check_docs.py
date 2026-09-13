#!/usr/bin/env python3
"""Check public documentation against its local links and executable catalogs."""

from __future__ import annotations

import argparse
import ast
import json
import re
import subprocess
import sys
import tempfile
import tomllib
from collections import Counter, deque
from pathlib import Path
from typing import Callable, Sequence
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
PRODUCT = ("learn", "how-to", "reference", "concepts", "specification")
PRODUCT_PAGES = tuple("""README.md
learn/README.md
learn/getting-started.md
learn/language-tour.md
learn/operational-model.md
learn/first-project.md
how-to/README.md
how-to/run-a-script.md
how-to/check-source.md
how-to/format-source.md
how-to/create-a-project.md
how-to/define-a-task.md
how-to/inspect-project-and-artifacts.md
how-to/run-an-external-program.md
how-to/use-filesystem-access.md
how-to/make-an-http-request.md
how-to/use-secrets.md
how-to/review-and-execute-a-plan.md
how-to/audit-a-run.md
how-to/configure-the-language-server.md
reference/README.md
reference/language/README.md
reference/language/syntax-names-and-expressions.md
reference/language/values-types-and-patterns.md
reference/language/control-flow-and-functions.md
reference/language/modules-and-imports.md
reference/language/commands-pipelines-and-streams.md
reference/language/core-commands.md
reference/language/actions-and-effects.md
reference/language/outcomes-and-errors.md
reference/std/README.md
reference/std/value.md
reference/std/outcome.md
reference/std/data.md
reference/std/path.md
reference/std/filesystem.md
reference/std/time.md
reference/std/version.md
reference/std/integrity.md
reference/std/url.md
reference/std/http.md
reference/std/process.md
reference/operational/README.md
reference/operational/projects-and-tasks.md
reference/operational/authority.md
reference/operational/inputs-tools-endpoints-and-secrets.md
reference/operational/capability-matrix.md
reference/operational/lifecycle.md
reference/formats/README.md
reference/formats/common-conventions.md
reference/formats/opaal-toml.md
reference/formats/authority-toml.md
reference/formats/tool-lock.md
reference/formats/check-artifact.md
reference/formats/plan-artifact.md
reference/formats/journal-artifact.md
reference/formats/audit-artifact.md
reference/tooling/README.md
reference/tooling/cli.md
reference/tooling/interactive-client.md
reference/tooling/lsp.md
reference/embedding/README.md
reference/embedding/operational-context.md
reference/embedding/compatibility.md
reference/diagnostics.md
reference/limits.md
reference/platform-support.md
reference/versioning-and-compatibility.md
reference/glossary.md
concepts/README.md
concepts/language-and-command-model.md
concepts/actions-effects-and-authority.md
concepts/check-plan-execute-audit.md
concepts/reproducibility-and-stale-plans.md
concepts/resources-and-lifetimes.md
concepts/security-model.md
specification/README.md
specification/1.0.md""".splitlines())
LINK = re.compile(r"(?<!!)\[[^]]*\]\(([^)]+)\)")
TYPO = re.compile(r"\b(teh|recieve|seperate|occurence|enviroment|definately|wich|lenght|authroity|exectue)\b", re.I)
NUMBER = re.compile(r"(?<![\w.])(\d[\d,]*)(?:\s+(KiB|MiB))?")
Runner = Callable[..., subprocess.CompletedProcess[str]]


def prose_lines(path: Path) -> list[tuple[int, str]]:
    result, fenced = [], False
    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if line.lstrip().startswith((chr(96) * 3, "~~~")):
            fenced = not fenced
        elif not fenced:
            result.append((line_number, line))
    return result


def prose(path: Path) -> list[str]:
    return [line for _, line in prose_lines(path)]


def pages(root: Path) -> list[Path]:
    result = list((root / "docs").rglob("*.md"))
    result += [root / name for name in ("README.md", "SECURITY.md", "CONTRIBUTING.md", "CHANGELOG.md")]
    for folder in ("benchmarks", "fuzz", "tests"):
        result += list((root / folder).rglob("*.md"))
    return sorted(set(result))


def links(path: Path) -> list[Path]:
    result = []
    for line in prose(path):
        for raw in LINK.findall(line):
            parsed = urlsplit(raw.strip().strip("<>").split(" ", 1)[0])
            if not parsed.scheme and not parsed.netloc and parsed.path:
                result.append((path.parent / unquote(parsed.path)).resolve())
    return result


def code_blocks(path: Path) -> tuple[list[tuple[str, int, str]], list[str]]:
    blocks: list[tuple[str, int, str]] = []
    errors: list[str] = []
    language, start, lines = "", 0, []
    marker = ""
    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        stripped = line.lstrip()
        if not marker and stripped.startswith((chr(96) * 3, "~~~")):
            marker = stripped[:3]
            language = stripped[3:].strip().lower()
            start = line_number + 1
            lines = []
        elif marker and stripped.startswith(marker):
            blocks.append((language, start, "\n".join(lines) + "\n"))
            language, start, lines, marker = "", 0, [], ""
        elif marker:
            lines.append(line)
    if marker:
        errors.append(f"{path}: unclosed fenced code block starting at line {start - 1}")
    return blocks, errors


def data_examples(root: Path) -> list[str]:
    errors = []
    for page in sorted((root / "docs").rglob("*.md")):
        blocks, block_errors = code_blocks(page)
        errors += [error.replace(str(root) + "/", "") for error in block_errors]
        for language, line, source in blocks:
            try:
                if language == "json":
                    json.loads(source)
                elif language == "toml":
                    tomllib.loads(source)
            except (json.JSONDecodeError, tomllib.TOMLDecodeError) as error:
                errors.append(f"{page.relative_to(root)}:{line}: invalid {language}: {error}")
    return errors


def run_example(command: Sequence[str], root: Path, run: Runner) -> str | None:
    completed = run(command, cwd=root, capture_output=True, text=True)
    if completed.returncode == 0:
        return None
    detail = completed.stderr.strip() or completed.stdout.strip() or f"exit {completed.returncode}"
    return detail.splitlines()[0]


def opaal_examples(root: Path, binary: Path, run: Runner = subprocess.run) -> list[str]:
    errors = []
    with tempfile.TemporaryDirectory(prefix="opaal-doc-examples-") as temporary:
        scratch = Path(temporary)
        for page in sorted((root / "docs").rglob("*.md")):
            blocks, _ = code_blocks(page)
            for index, (language, line, source) in enumerate(blocks):
                if language != "opaal":
                    continue
                example = scratch / f"example-{index}.opaal"
                example.write_text(source, encoding="utf-8")
                commands = [[str(binary), "format", "--check", str(example)]]
                if not re.search(r"(?m)^\s*task\b|^\s*import\s+['\"]", source):
                    commands.append([str(binary), "check", str(example)])
                for command in commands:
                    detail = run_example(command, root, run)
                    if detail:
                        action = command[1]
                        errors.append(
                            f"{page.relative_to(root)}:{line}: OPAAL example {action} failed: {detail}"
                        )
        checked_in = root / "examples/language-foundation.opaal"
        for command in (
            [str(binary), "format", "--check", str(checked_in)],
            [str(binary), "check", str(checked_in)],
            [str(binary), str(checked_in)],
        ):
            detail = run_example(command, root, run)
            if detail:
                errors.append(f"examples/language-foundation.opaal: documented command failed: {detail}")
    return errors


def navigation(root: Path) -> list[str]:
    errors = []
    root = root.resolve()
    for page in pages(root):
        for target in links(page):
            if not target.is_relative_to(root):
                errors.append(f"{page.relative_to(root)}: link escapes repository: {target}")
            elif not target.exists():
                errors.append(f"{page.relative_to(root)}: missing link {target}")
    entry = root / "docs/README.md"
    required = {root / "docs" / relative for relative in PRODUCT_PAGES}
    errors += [f"{path.relative_to(root)}: required product page is missing"
               for path in sorted(required) if not path.is_file()]
    seen, queue = {entry}, deque([entry] if entry.is_file() else [])
    while queue:
        for target in links(queue.popleft()):
            if (target.is_relative_to(root) and target.is_file()
                    and target.suffix == ".md" and target not in seen):
                seen.add(target)
                queue.append(target)
    product = {entry}
    for section in PRODUCT:
        product.update(path.resolve() for path in (root / "docs" / section).rglob("*.md"))
    errors += [f"{path.relative_to(root)}: unreachable from docs/README.md"
               for path in sorted(product - seen)]
    return errors


def calculate(expression: str) -> int:
    node = ast.parse(expression.replace("_", ""), mode="eval").body
    def walk(part: ast.AST) -> int:
        if isinstance(part, ast.Constant) and type(part.value) is int:
            return part.value
        if isinstance(part, ast.BinOp) and isinstance(part.op, ast.Mult):
            return walk(part.left) * walk(part.right)
        raise ValueError(f"unsupported limit: {expression}")
    return walk(node)


def constant(source: str, name: str) -> int:
    match = re.search(rf"\b{name}\s*:\s*\w+\s*=\s*([^;]+);", source)
    if not match:
        raise ValueError(f"missing source limit {name}")
    return calculate(match.group(1).strip())


def display(value: int) -> str:
    return f"{value // 1048576} MiB" if value >= 1048576 and value % 1048576 == 0 else f"{value:,}"


def documented_values(row: str) -> Counter[int]:
    multipliers = {"": 1, "KiB": 1024, "MiB": 1048576}
    return Counter(int(number.replace(",", "")) * multipliers[unit]
                   for number, unit in NUMBER.findall(row))


def limits(root: Path) -> list[str]:
    docs = (root / "docs/reference/limits.md").read_text()
    module = (root / "crates/opaal-runtime/src/module.rs").read_text()
    match = re.search(r"pub const OPAAL: Self = Self \{\s*limits: \[(.*?)\]", module, re.S)
    if not match:
        return ["missing analysis limits source"]
    values = [calculate(x) for x in re.findall(r"Some\(([^)]+)\)", match.group(1))]
    evaluation = (root / "crates/opaal-runtime/src/eval.rs").read_text()
    groups = [("Source closure", values[:4]), ("Analysis", values[4:]),
              ("Evaluation", [constant(evaluation, name) for name in (
                  "DEFAULT_OPAAL_EVALUATION_STEPS", "DEFAULT_OPAAL_CALL_DEPTH",
                  "DEFAULT_OPAAL_COLLECTION_ITEMS", "DEFAULT_OPAAL_COLLECTION_BYTES")])]
    for source_path, label, names in (
        ("crates/opaal-runtime/src/project.rs", "Project control",
         ("MAX_PROJECT_DOCUMENT_BYTES", "MAX_PROJECT_ENTRIES", "MAX_PROJECT_DOCUMENT_DEPTH", "MAX_PROJECT_INPUTS")),
        ("crates/opaal-runtime/src/workflow.rs", "Journal", ("MAX_JOURNAL_BYTES", "MAX_JOURNAL_LINES")),
    ):
        source = (root / source_path).read_text()
        groups.append((label, [constant(source, name) for name in names]))
    errors = []
    for label, group in groups:
        row = next((line for line in docs.splitlines() if line.startswith(f"| {label} |")), "")
        source_values = Counter(group)
        documented = documented_values(row)
        errors += [f"{label}: source limit {display(value)} occurs {count} times, "
                   f"documented {documented[value]}"
                   for value, count in source_values.items()
                   if documented[value] < count]
    return errors


def call_arguments(source: str, callee: str) -> list[list[str]]:
    calls = []
    for match in re.finditer(rf"^[ \t]*{re.escape(callee)}\s*\(", source, re.M):
        arguments = []
        start = match.end()
        depth = 1
        quoted = False
        escaped = False
        for offset in range(start, len(source)):
            character = source[offset]
            if quoted:
                if escaped:
                    escaped = False
                elif character == "\\":
                    escaped = True
                elif character == '"':
                    quoted = False
                continue
            if character == '"':
                quoted = True
            elif character in "([{":
                depth += 1
            elif character in ")]}":
                depth -= 1
                if depth == 0:
                    arguments.append(source[start:offset].strip())
                    calls.append(arguments)
                    break
            elif character == "," and depth == 1:
                arguments.append(source[start:offset].strip())
                start = offset + 1
    return calls


def catalogs(root: Path) -> list[str]:
    errors = []
    builtin = (root / "crates/opaal-runtime/src/builtin.rs").read_text()
    names = set(re.findall(r'CommandSignature::(?:new|passthrough)\(\s*"([^"]+)"', builtin))
    command_doc = (root / "docs/reference/language/core-commands.md").read_text()
    documented = dict(re.findall(
        r"^\| \x60([a-z]+)\x60 \| \x60([^\x60]+)\x60 \|", command_doc, re.M
    ))
    listed = set(documented)
    if names != listed:
        errors.append(f"core command drift: missing {sorted(names-listed)}, extra {sorted(listed-names)}")
    invocations = {}
    for arguments in call_arguments(builtin, "documented"):
        name = re.search(
            r'CommandSignature::(?:new|passthrough)\(\s*"([^"]+)"', arguments[0]
        )
        invocation = (
            re.fullmatch(r'"(?:\\.|[^"\\])*"', arguments[1])
            if len(arguments) >= 2 else None
        )
        if name and invocation:
            invocations[name.group(1)] = ast.literal_eval(arguments[1])
    if set(invocations) != names:
        errors.append(
            "core command invocation metadata drift: "
            f"missing {sorted(names-set(invocations))}, extra {sorted(set(invocations)-names)}"
        )
    errors += [
        f"core command {name}: source invocation {invocation!r}, "
        f"documented {documented[name]!r}"
        for name, invocation in sorted(invocations.items())
        if name in documented and documented[name] != invocation
    ]
    module = (root / "crates/opaal-runtime/src/module.rs").read_text()
    section = module.split("fn is_standard_module(", 1)[1].split("fn parse_opaal_source(", 1)[0]
    names = set(re.findall(r'"([a-z]+)"', section)) - {"std"}
    index = (root / "docs/reference/std/README.md").read_text()
    listed = set(re.findall(r"\x60std::([a-z]+)\x60", index))
    if names != listed:
        errors.append(f"standard module drift: missing {sorted(names-listed)}, extra {sorted(listed-names)}")
    for name in names - {"value"}:
        match = re.search(rf'const STANDARD_{name.upper()}_MODULE: &str = r#"(.*?)"#;', module, re.S)
        if not match:
            errors.append(f"missing std::{name} module source")
            continue
        exports = re.search(r"export \{ ([^}]+) \}", match.group(1))
        if not exports:
            errors.append(f"missing std::{name} exports")
            continue
        page = (root / f"docs/reference/std/{name}.md").read_text()
        errors += [f"std::{name}: undocumented {export.strip()}"
                   for export in exports.group(1).split(",")
                   if f"{chr(96)}{export.strip()}" not in page]
    workflow = (root / "crates/opaal-runtime/src/workflow.rs").read_text()
    index = (root / "docs/reference/formats/README.md").read_text()
    for kind, filename in (("CHECK", "check-artifact.md"), ("PLAN", "plan-artifact.md"),
                           ("JOURNAL", "journal-artifact.md"), ("AUDIT", "audit-artifact.md")):
        match = re.search(rf'const {kind}_SCHEMA: &str = "([^"]+)";', workflow)
        if not match or match.group(1) not in index or match.group(1) not in (
            root / "docs/reference/formats" / filename).read_text():
            errors.append(f"{kind}: schema documentation drift")
    return errors


def spelling(root: Path) -> list[str]:
    errors = []
    for page in pages(root):
        for line_number, line in prose_lines(page):
            visible = re.sub(r"`[^`]*`", "", line)
            for match in (TYPO.search(visible), re.search(r"\b([A-Za-z]{3,})\s+\1\b", visible, re.I)):
                if match:
                    errors.append(f"{page.relative_to(root)}:{line_number}: spelling {match.group()}")
    return errors


def cli(root: Path, binary: Path) -> list[str]:
    output = subprocess.run([str(binary), "--help"], capture_output=True, text=True, check=True).stdout
    usage = output.split("Usage:\n", 1)[1].split("\n\n", 1)[0]
    expected = {line.strip() for line in usage.splitlines() if line.strip().startswith("opaal")}
    docs = (root / "docs/reference/tooling/cli.md").read_text()
    actual = {line for line in docs.splitlines() if line.startswith("opaal")}
    return [] if expected == actual else [f"CLI help drift: missing {sorted(expected-actual)}, extra {sorted(actual-expected)}"]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cli", type=Path, help="built opaal binary for exact help comparison")
    args = parser.parse_args()
    errors = [error for check in (navigation, catalogs, limits, spelling, data_examples)
              for error in check(ROOT)]
    if args.cli:
        errors += cli(ROOT, args.cli.resolve())
        errors += opaal_examples(ROOT, args.cli.resolve())
    for error in errors:
        print(error, file=sys.stderr)
    if errors:
        return 1
    print("Documentation links, navigation, examples, catalogs, limits, spelling, and CLI help passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
