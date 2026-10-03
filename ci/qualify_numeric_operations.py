#!/usr/bin/env python3
"""Execute numeric examples with the exact archived programs outside the checkout."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import tempfile
from pathlib import Path

if __package__:
    from .package_release import ROOT, VERSION, PLATFORMS
    from .qualify_data_processing import command, invalid, language_server, require, success, unpack
else:
    from package_release import ROOT, VERSION, PLATFORMS
    from qualify_data_processing import command, invalid, language_server, require, success, unpack


def qualify(archive: Path | None, binary_directory: Path | None, source: str, target: str) -> dict:
    require((platform.system(), platform.machine()) == {
        "linux-x86_64": ("Linux", "x86_64"), "macos-arm64": ("Darwin", "arm64"),
    }[target], "candidate target differs from running host")
    require(subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip() == source,
            "expected source differs from checkout head")
    fixtures = ROOT / "tests/golden/numeric-operations"
    examples = ROOT / "examples/numeric-report"
    hashes = {str(path.relative_to(ROOT)): hashlib.sha256(path.read_bytes()).hexdigest()
              for directory in (fixtures, examples) for path in sorted(directory.iterdir())
              if path.is_file()}
    with tempfile.TemporaryDirectory(prefix="opaal-numeric-") as temporary:
        directory = Path(temporary)
        if archive is not None:
            binaries, manifest = unpack(archive, directory, kind="binaries", version=VERSION,
                                       source=source, platform=target)
        else:
            require(binary_directory is not None, "missing working source binaries")
            binaries = directory / "bin"
            binaries.mkdir()
            for name in ("opaal", "opaal-language-server"):
                shutil.copy2(binary_directory / name, binaries / name)
            manifest = {"sha256": None}
        binary = binaries / "opaal"
        work = directory / "report"
        shutil.copytree(examples, work)
        shutil.copy(work / ("tools-linux.toml" if target == "linux-x86_64" else "tools-macos.toml"), work / "tools.toml")
        environment = {"PATH": os.defpath}
        def run(*arguments: str, stdin: bytes | None = None):
            return command([binary, *arguments], work, environment, stdin=stdin)
        require(success(run("--version"), "candidate version") == f"opaal {VERSION}\n".encode(), "version differs")
        language_server(binaries / "opaal-language-server", work, environment)
        for name, expected in [("show.opaal", "expected-show.txt"), ("caught.opaal", "expected-caught.txt")]:
            success(run("check", name), f"check {name}")
            require(success(run(name), name) == (fixtures / expected).read_bytes(), f"{name}: bytes differ")
        for path in sorted(work.glob("*.opaal")):
            success(run("format", "--check", path.name), f"format {path.name}")
        invalid(run("domain.opaal"), "uncaught domain")
        invalid(run("check", "mixed.opaal"), "mixed tuple checker")
        transcript = success(run(stdin=b"import './report.opaal' as report\nimport std::math as math\nhelp math::sqrt\nreport::build().percent\nreport::fallback()\nexit 0\n"), "interactive report")
        require(all(text in transcript for text in [b"93.0", b"0.0", b"std::math::sqrt(value: Float) -> Float"]), "interactive values/help differ")
        success(run("task", "inspect", "--project", "opaal.toml", "summarize"), "task inspect")
        success(run("check", "--project", "opaal.toml", "--task", "summarize", "--environment", "ci"), "project check")
        receipt = success(run("plan", "--project", "opaal.toml", "--task", "summarize", "--environment", "ci", "--expires-in", "900s", "--out", "numeric.plan.json"), "plan")
        plan = json.loads((work / "numeric.plan.json").read_bytes())
        digest = receipt.decode().strip().split()[1]
        require(plan["tools"] == [] and plan["secrets"] == [], "numeric task acquired external authority")
        require(len(plan["authority"]["requests"]) == 1, "numeric task effects differ")
        success(run("plan", "inspect", "numeric.plan.json"), "plan inspect")
        success(run("execute", "--plan", "numeric.plan.json", "--accept", digest, "--journal", "numeric.run.jsonl"), "execute")
        report = (work / "report.json").read_bytes()
        require(report == (fixtures / "expected-report.json").read_bytes(), "controlled report differs")
        success(run("audit", "--project", "opaal.toml", "--journal", "numeric.run.jsonl", "--out", "numeric.audit.json"), "audit")
        success(run("audit", "inspect", "numeric.audit.json"), "audit inspect")
        audit = json.loads((work / "numeric.audit.json").read_bytes())
        require(audit["primary"]["class"] == "success" and audit["primary"]["partial"] is False, "audit primary differs")
        effects = [event["payload"]["effect"] for event in audit["events"] if event["kind"] == "effect-before"]
        require(effects == ["filesystem.write"], "math introduced an effect event")
        with (work / "report.opaal").open("ab") as changed:
            changed.write(b"\n")
        require(run("execute", "--plan", "numeric.plan.json", "--accept", digest, "--journal", "stale.run.jsonl").returncode != 0,
                "stale source was accepted")
        require(not (work / "stale.run.jsonl").exists(), "stale source created a journal")
        dirty = bool(subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT))
        paths = subprocess.check_output(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=ROOT).decode().split("\0")
        snapshot = [(name, hashlib.sha256((ROOT / name).read_bytes()).hexdigest())
                    for name in sorted(set(paths)) if name and (ROOT / name).is_file()]
        snapshot_digest = hashlib.sha256(json.dumps(snapshot, separators=(",", ":")).encode()).hexdigest()
        return {"schema_version": 1, "source": source, "committed_source": not dirty,
                "source_snapshot_sha256": snapshot_digest,
                "version": VERSION, "platform": target, "archive_sha256": manifest["sha256"],
                "binary_sha256": {name: hashlib.sha256((binaries / name).read_bytes()).hexdigest()
                                  for name in ("opaal", "opaal-language-server")},
                "fixture_sha256": hashes, "report_sha256": hashlib.sha256(report).hexdigest(),
                "assertions": ["archive identity" if archive else "working source program digests", "language server lifecycle", "check and format",
                               "ordinary report", "caught and uncaught domain", "mixed tuple refusal",
                               "interactive values and help", "plan execute audit", "stale source refusal"]}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    programs = parser.add_mutually_exclusive_group(required=True)
    programs.add_argument("--archive", type=Path)
    programs.add_argument("--binary-directory", type=Path,
                          help="qualify working source programs without a committed archive claim")
    parser.add_argument("--expected-source", required=True)
    parser.add_argument("--platform", choices=PLATFORMS, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    result = qualify(args.archive.resolve() if args.archive else None,
                     args.binary_directory.resolve() if args.binary_directory else None,
                     args.expected_source, args.platform)
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print("Numeric candidate qualification passed.")


if __name__ == "__main__":
    main()
