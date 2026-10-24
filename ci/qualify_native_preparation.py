"""Observe preparation through the candidate CLI; keep private fault cases pending."""
from __future__ import annotations

import base64
import concurrent.futures
import hashlib
import json
import os
import platform
import shutil
import stat
import struct
import subprocess
import threading
import tomllib
from pathlib import Path

if __package__:
    from .qualify_data_processing import command, require, success
else:
    from qualify_data_processing import command, require, success


def case_results(cases, observations, pending, host, *, label="preparation"):
    """Account for every frozen row once, including cases for another host."""
    expected = {row["name"]: row for row in cases}
    names = [row["name"] for row in observations]
    inapplicable = [row["name"] for row in cases if host not in row["hosts"]]
    accounted = names + pending + inapplicable
    require(len(accounted) == len(set(accounted)) and set(accounted) == set(expected),
            label + " result case set differs")
    for observation in observations:
        row = expected[observation["name"]]
        require(host in row["hosts"], label + " result host differs")
        require(json.dumps(observation["observed"], sort_keys=True) == json.dumps(row["expected"], sort_keys=True),
                label + " result differs: " + row["name"])
    return {"observed": observations, "pending": pending, "inapplicable": inapplicable}


def host_triple():
    return {("Linux", "x86_64"): "x86_64-unknown-linux-gnu",
            ("Linux", "aarch64"): "aarch64-unknown-linux-gnu",
            ("Darwin", "arm64"): "aarch64-apple-darwin"}.get((platform.system(), platform.machine()))


def injected_results(completed, cases, pending, host, inventory_digest, *,
                     marker=b"OPAAL_PREPARATION_CASE ", label="preparation"):
    require(completed.returncode == 0, label + " observer tests failed")
    observations = []
    for line in completed.stdout.splitlines():
        _, found, encoded = line.partition(marker)
        if not found:
            continue
        row = json.loads(encoded)
        require(set(row) == {"name", "observed", "host", "inventory_digest"}
                and row["host"] == host and row["inventory_digest"] == inventory_digest,
                label + " observer result identity differs")
        observations.append({"name": row["name"], "observed": row["observed"]})
    require(len(observations) == len(pending)
            and {row["name"] for row in observations} == set(pending),
            label + " observer case set differs")
    expected = [row for row in cases if row["name"] in pending]
    case_results(expected, observations, [], host, label=label)
    return observations


def minimal_elf(foreign=False):
    # macOS must refuse this Linux image before any launch or snapshot attempt.
    image = bytearray(120)
    image[:7] = b"\x7fELF\x02\x01\x01"
    machine = 183 if platform.machine() in {"aarch64", "arm64"} else 62
    if foreign:
        machine = 62 if machine == 183 else 183
    for offset, value in ((16, 2), (18, machine), (52, 64), (54, 56), (56, 1)):
        struct.pack_into("<H", image, offset, value)
    struct.pack_into("<I", image, 20, 1)
    struct.pack_into("<Q", image, 32, 64)
    struct.pack_into("<I", image, 64, 1)
    return bytes(image)


def snapshot(path):
    """Read only regular files, retaining link and FIFO identity without opening them."""
    if not path.exists() and not path.is_symlink():
        return None
    mode = path.lstat().st_mode
    if stat.S_ISLNK(mode):
        return (mode, os.readlink(path))
    if stat.S_ISREG(mode):
        return (mode, hashlib.sha256(path.read_bytes()).hexdigest())
    if stat.S_ISDIR(mode):
        return (mode, {child.name: snapshot(child) for child in sorted(path.iterdir())})
    return (mode, path.lstat().st_ino)


def asset_observations(destination, reference, host):
    files = {path.relative_to(destination).as_posix(): path
             for path in destination.rglob("*") if path.is_file()}
    expected = {path.relative_to(reference).as_posix(): path
                for path in reference.rglob("*") if path.is_file()
                and path.suffix != ".py" and "__pycache__" not in path.parts}
    controls = {"opaal.toml": "offline.toml", "offline-tools.toml":
                "offline-tools-linux.toml" if host == "x86_64-unknown-linux-gnu" else
                "offline-tools-linux-aarch64.toml" if host == "aarch64-unknown-linux-gnu" else
                "offline-tools-macos.toml"}
    omitted = {"offline.toml", "offline-tools-linux.toml", "offline-tools-linux-aarch64.toml", "offline-tools-macos.toml",
               "live.toml", "live-authority.toml", "live-tools.template.toml"}
    expected = {name: path for name, path in expected.items() if name not in omitted}
    expected.update({name: reference / owner for name, owner in controls.items()})
    return (set(files) == set(expected)
            and all(path.read_bytes() == expected[name].read_bytes() for name, path in files.items()))


def qualify_preparation(binary, inputs, output, cases, *, test_binary=None):
    host = host_triple()
    require(host is not None, "unsupported preparation qualification host")
    output.mkdir()
    tool = output / "canary-checker"
    compiler = None
    if host in {"x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"}:
        compiler = shutil.which("cc")
        require(compiler is not None, "preparation launch observer requires the host C compiler")
        success(command([compiler, "-O2", inputs / "preparation-checker.c", "-o", tool],
                        output, {"PATH": "/usr/bin:/bin"}),
                "build preparation launch observer")
        success(command([tool], output, {}), "positive preparation launch observer")
        require((output / "checker.launches").read_bytes() == b"launch\n",
                "preparation launch observer is ineffective")
        (output / "checker.launches").unlink()
    else:
        tool.write_bytes(minimal_elf())
        tool.chmod(0o700)
    tool_bytes = tool.read_bytes()
    reference = inputs / "reference"
    observations, pending = [], []
    for row in cases:
        if host not in row["hosts"]:
            continue
        spec, expected = row["input"], row["expected"]
        if "asset_bound" in spec or spec.get("fault") in {
                "asset-collision", "zero-write", "partial-write", "file-sync",
                "directory-sync", "root-sync", "parent-sync"}:
            pending.append(row["name"])
            continue
        work = output / row["name"]
        work.mkdir()
        destination = work / "project"
        selected_tool = work / "checker"
        selected_tool.write_bytes(tool_bytes)
        selected_tool.chmod(0o700)
        args = [binary, "project", "init", "--example", "repository-check", "--destination"]
        environment = spec.get("environment", "ci" if "tool" in spec else "offline")
        kind = spec.get("tool")
        if kind == "relative":
            selected_tool = Path("checker")
        elif kind == "missing":
            selected_tool.unlink()
        elif kind == "symlink":
            selected_tool.unlink()
            selected_tool.symlink_to(tool)
        elif kind == "fifo":
            selected_tool.unlink()
            os.mkfifo(selected_tool)
        elif kind == "nonexec":
            selected_tool.chmod(0o600)
        elif kind == "setid":
            selected_tool.chmod(0o4700)
        elif kind == "script":
            selected_tool.write_bytes(b"#!/bin/sh\nprintf 'launch\\n' >> checker.launches\n")
        elif kind == "foreign":
            image = bytearray(minimal_elf(foreign=True))
            selected_tool.write_bytes(image)
        elif kind == "67108865-bytes":
            with selected_tool.open("r+b") as handle:
                handle.truncate(67108865)
        target = spec.get("destination")
        if target == "empty":
            destination = ""
        elif target == "parent-component":
            destination = str(work) + "/../unexpected"
        elif target == "symlink-parent":
            (work / "real-parent").mkdir()
            (work / "link-parent").symlink_to("real-parent", target_is_directory=True)
            destination = work / "link-parent/project"
        elif target == "missing-parent":
            destination = work / "missing/project"
        elif target == "directory":
            destination.mkdir()
            (destination / "canary").write_bytes(b"preserve")
        elif target == "file":
            destination.write_bytes(b"preserve")
        elif target in {"symlink", "dangling-symlink"}:
            if target == "symlink":
                (work / "original").write_bytes(b"preserve")
            destination.symlink_to("original")
        elif target == "fifo":
            os.mkfifo(destination)
        before = snapshot(destination) if isinstance(destination, Path) else None
        args += [destination, "--environment", environment]
        if kind is not None:
            args += ["--tool", selected_tool]
        if "option" in spec:
            if spec["option"] == "--tool":
                args += ["--tool", selected_tool] * spec["count"]
            else:
                args += [spec["option"], "unrequested"]
        observed = {}
        if spec.get("observer") == "doctor":
            destination.mkdir()
            shutil.copytree(reference / "fixtures", destination / "fixtures")
            for name in ("tasks.opaal", "policy.opaal", "live-authority.toml"):
                shutil.copyfile(reference / name, destination / name)
            shutil.copyfile(reference / "live.toml", destination / "opaal.toml")
            lock = (reference / "live-tools.template.toml").read_text().replace(
                "x86_64-unknown-linux-gnu", host).replace("@TOOL@", base64.urlsafe_b64encode(
                    os.fsencode(selected_tool)).decode().rstrip("=")).replace(
                        "@DIGEST@", "sha256:" + hashlib.sha256(tool_bytes).hexdigest())
            if kind == "missing-tool":
                selected_tool.unlink()
            elif kind == "changed-tool":
                selected_tool.write_bytes(tool_bytes + b"changed")
            elif kind == "foreign-image":
                image = bytearray(minimal_elf(foreign=True))
                selected_tool.write_bytes(image)
                lock = lock.replace("sha256:" + hashlib.sha256(tool_bytes).hexdigest(),
                                    "sha256:" + hashlib.sha256(image).hexdigest())
            (destination / "live-tools.toml").write_text(lock)
            result = command([binary, "project", "doctor", "--project", destination / "opaal.toml",
                              "--task", "repository", "--environment", "ci", "--format", "json",
                              "--input-file", "script=" + str(destination / "fixtures/check.sh"),
                              "--input-file", "manifest=" + str(destination / "fixtures/repository.toml")], work, {})
            view = json.loads(result.stdout)
            require(result.returncode == 1, row["name"] + ": doctor exit differs")
            codes = {finding["code"] for finding in view["findings"]}
            finding = "DOCTOR007" if "DOCTOR007" in codes else "DOCTOR017" if "DOCTOR017" in codes else None
            observed.update(live_ready=view["local_readiness"] == "ready", finding=finding)
        elif spec.get("initializers") == 2:
            barrier = threading.Barrier(2)

            def initialize():
                barrier.wait(timeout=10)
                return command(args, work, {})

            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                futures = [pool.submit(initialize) for _ in range(2)]
                results = [future.result() for future in futures]
            observed.update(successes=sum(result.returncode == 0 for result in results),
                            refusals=sum(result.returncode == 2 for result in results))
            require(asset_observations(destination, reference, host), "concurrent assets differ")
        else:
            if spec.get("fault") == "summary-write":
                read, write = os.pipe()
                os.close(read)
                try:
                    result = subprocess.run([str(arg) for arg in args], cwd=work, env={},
                                            stdin=subprocess.DEVNULL, stdout=write,
                                            stderr=subprocess.PIPE, timeout=120, check=False)
                finally:
                    os.close(write)
            else:
                result = command(args, work, {})
            require(result.returncode == expected.get("exit", 0), row["name"] + ": exit differs")
            if "exit" in expected:
                observed["exit"] = result.returncode
            if "created" in expected:
                observed["created"] = isinstance(destination, Path) and destination.exists() and before is None
                if not expected["created"]:
                    require(not (output / "unexpected").exists()
                            and not (work / "real-parent/project").exists(),
                            "refused preparation created a destination through an invalid path")
            if "preserved" in expected:
                observed["preserved"] = snapshot(destination) == before
                if target == "symlink":
                    observed["preserved"] &= (work / "original").read_bytes() == b"preserve"
            if "complete_assets_retained" in expected or "all_asset_bytes_equal" in expected:
                key = "complete_assets_retained" if "complete_assets_retained" in expected else "all_asset_bytes_equal"
                observed[key] = asset_observations(destination, reference, host)
            if "file_mode" in expected:
                modes = {stat.S_IMODE(path.stat().st_mode) for path in destination.rglob("*") if path.is_file()}
                require(len(modes) == 1, "preparation file modes differ")
                observed["file_mode"] = f"{modes.pop():04o}"
                modes = {stat.S_IMODE(path.stat().st_mode) for path in [destination, *destination.rglob("*")]
                         if path.is_dir()}
                require(len(modes) == 1, "preparation directory modes differ")
                observed["directory_mode"] = f"{modes.pop():04o}"
            if "version_present" in expected:
                lock = tomllib.loads((destination / "live-tools.toml").read_text())
                observed.update(version_present="version" in lock["tools"][0],
                                inherit=lock["child_environment"]["inherit"],
                                environment={entry["name"]: base64.urlsafe_b64decode(
                                    entry["value"]["value"] + "==").decode()
                                    for entry in lock["child_environment"]["variables"]})
        if "launches" in expected:
            markers = list(work.rglob("checker.launches"))
            observed["launches"] = sum(len(path.read_bytes().splitlines()) for path in markers)
        observations.append({"name": row["name"], "observed": observed})
    injected = None
    if test_binary:
        digest = hashlib.sha256(test_binary.read_bytes()).hexdigest()
        completed = command([test_binary, "project::preparation::tests::", "--show-output",
                             "--test-threads=1"], output, {})
        (output / "injected-tests.stdout.txt").write_bytes(completed.stdout)
        (output / "injected-tests.stderr.txt").write_bytes(completed.stderr)
        inventory_digest = "sha256:" + hashlib.sha256((inputs / "prepare-cases.json").read_bytes()).hexdigest()
        observations += injected_results(completed, cases, pending, host, inventory_digest)
        require(hashlib.sha256(test_binary.read_bytes()).hexdigest() == digest,
                "preparation test binary changed during qualification")
        injected = {"binary_sha256": digest, "scope": "private write/collision faults and shared asset creator bounds/sync",
                    "exit_observation": "initializer error mapped to the CLI setup-failure status; no injected CLI process",
                    "launch_observation": "offline initializer has no selected tool",
                    "cases": pending}
        pending = []
    result = case_results(cases, observations, pending, host)
    result.update(host=host, checker_sha256=hashlib.sha256(tool_bytes).hexdigest(),
                  checker_observer="executed-positive-control" if compiler else "unsupported-linux-image",
                  producer_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                  injected=injected)
    return result
