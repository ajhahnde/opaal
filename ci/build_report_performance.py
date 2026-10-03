#!/usr/bin/env python3
"""Build optimized report harnesses in an isolated source-only workspace."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def measurement_source(text: str, phases: str) -> str:
    """Keep evaluator behavior and replace semantic tests with phase fixtures."""
    text = text.split("#[cfg(test)]\nmod tests {", 1)[0]
    if "CLASSIFICATION_PARSES" in text:
        text = text.replace(
            "    #[cfg(test)]\n    CLASSIFICATION_PARSES.with(|count| count.set(count.get() + 1));\n", ""
        )
        text = text.replace(
            "                #[cfg(test)]\n                observe_cold_classification(self);\n", ""
        )
        text, count = re.subn(
            r"    #\[cfg\(test\)\]\n    RESTORED_FAMILIES.with\(\|families\| \{.*?\n    \}\);\n",
            "", text, flags=re.S,
        )
        if count != 1:
            raise ValueError("restoration observer layout changed")
        start = text.index("#[cfg(test)]\nthread_local! {")
        end = text.index("impl CallableValue {", start)
        text = text[:start] + text[end:]
    if any(name in text for name in ["CLASSIFICATION_PARSES", "COLD_OBSERVER", "RESTORED_FAMILIES", "observe_cold_classification"]):
        raise ValueError("semantic instrumentation remains in measured source")
    return text + "#[cfg(test)]\nmod tests {\n    use super::*;\n    use opaal_syntax::{ParseOutcome, SourceId, parse_opaal};\n" + phases + "\n}\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True, help="isolated source-only export; never the checkout")
    parser.add_argument("--output", type=Path, required=True, help="new artifact directory")
    args = parser.parse_args()
    source = args.source.resolve()
    output = args.output.resolve()
    public_directories = {".github", "benchmarks", "ci", "crates", "examples", "fuzz", "tests", "target", "dist"}
    if source == ROOT or (source / ".git").exists() or any(path.is_dir() and path.name not in public_directories for path in source.iterdir()):
        parser.error("source must be an isolated public source export")
    output.mkdir(parents=True, exist_ok=False)
    env = os.environ.copy()
    for name in ["RUSTFLAGS", "RUSTC_BOOTSTRAP", "CARGO_ENCODED_RUSTFLAGS"]:
        env.pop(name, None)
    cargo = env.get("CARGO", "cargo")
    rustc = env.get("RUSTC", "rustc")
    pinned = tomllib.loads((source / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
    if subprocess.check_output([rustc, "--version"], text=True).split()[1] != pinned:
        parser.error("rustc must match the source toolchain declaration")
    if any(name.startswith("CARGO_PROFILE_") for name in env):
        parser.error("custom Cargo profile overrides are not qualification inputs")
    env.pop("CARGO_TARGET_DIR", None)
    result = {"source_files_sha256": {}, "commands": [], "binaries": {}}
    result["observer_templates_sha256"] = {
        name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
        for name in ["benchmarks/report_raw.rs", "benchmarks/report_phases.rs", "ci/build_report_performance.py"]
    }
    for folder in ["crates", "ci", "benchmarks", "tests"]:
        for path in sorted((source / folder).rglob("*")):
            if path.is_file() and "__pycache__" not in path.parts:
                result["source_files_sha256"][str(path.relative_to(source))] = hashlib.sha256(path.read_bytes()).hexdigest()
    for name in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"]:
        result["source_files_sha256"][name] = hashlib.sha256((source / name).read_bytes()).hexdigest()

    def run(label: str, command: list[str]) -> list[dict]:
        result["commands"].append(command)
        with (output / f"{label}.stdout").open("w") as out, (output / f"{label}.stderr").open("w") as err:
            completed = subprocess.run(command, cwd=source, env=env, stdout=out, stderr=err)
        if completed.returncode:
            raise SystemExit(f"{label} failed; see retained build logs")
        if "--message-format=json" in command:
            return [json.loads(line) for line in (output / f"{label}.stdout").read_text().splitlines()]
        return []

    artifacts = run("cli-build", [cargo, "build", "--release", "--offline", "--locked", "-p", "opaal-cli", "--bin", "opaal", "--message-format=json"])
    result["binaries"]["cli"] = str(source / "target/release/opaal")
    libraries = {}
    for name in ["opaal_runtime", "opaal_syntax"]:
        matches = [row for row in artifacts if row.get("reason") == "compiler-artifact" and row["target"]["name"] == name and not row["profile"]["test"]]
        if len(matches) != 1:
            raise SystemExit(f"ambiguous library artifact: {name}")
        libraries[name] = next(path for path in matches[0]["filenames"] if path.endswith(".rlib"))
    raw = output / "report-raw"
    command = [rustc, "--edition=2024", "-C", "opt-level=3", str(ROOT / "benchmarks/report_raw.rs"), "-o", str(raw), "-L", f"dependency={source}/target/release/deps"]
    for name, path in libraries.items():
        command += ["--extern", f"{name}={path}"]
    run("raw-build", command)
    result["binaries"]["raw"] = str(raw)
    evaluator = source / "crates/opaal-runtime/src/eval.rs"
    callbacks = source / "crates/opaal-runtime/src/eval/list_operations.rs"
    original = evaluator.read_bytes(), callbacks.read_bytes()
    try:
        evaluator.write_text(measurement_source(original[0].decode(), (ROOT / "benchmarks/report_phases.rs").read_text()))
        callbacks.write_text(re.sub(r"    #\[cfg\(test\)\]\n    pub\(super\) fn test_callback_shape\(.*?\n    \}\n\n", "", original[1].decode(), flags=re.S))
        (output / "measurement-eval.rs").write_bytes(evaluator.read_bytes())
        (output / "measurement-list_operations.rs").write_bytes(callbacks.read_bytes())
        artifacts = run("phases-build", [cargo, "test", "--release", "--offline", "--locked", "-p", "opaal-runtime", "--lib", "--no-run", "--message-format=json"])
        matches = [row["executable"] for row in artifacts if row.get("reason") == "compiler-artifact" and row["target"]["name"] == "opaal_runtime" and row["profile"]["test"] and row.get("executable")]
        if len(matches) != 1:
            raise SystemExit("ambiguous phase executable")
        result["binaries"]["phases"] = matches[0]
    finally:
        evaluator.write_bytes(original[0])
        callbacks.write_bytes(original[1])
    result["binaries_sha256"] = {name: hashlib.sha256(Path(path).read_bytes()).hexdigest() for name, path in result["binaries"].items()}
    result["rustc"] = subprocess.check_output([rustc, "-vV"], text=True)
    result["cargo"] = subprocess.check_output([cargo, "-vV"], text=True)
    (output / "build.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result["binaries"], indent=2))


if __name__ == "__main__":
    main()
