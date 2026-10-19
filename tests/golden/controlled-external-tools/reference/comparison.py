#!/usr/bin/env python3
"""Independent repository policy comparison; requires Python 3.11 or newer."""
import argparse
import datetime
import json
import math
import os
from pathlib import Path
import selectors
import signal
import stat
import subprocess
import sys
import time
import tomllib

SAVED_LIMIT = 1_048_576
FILE_LIMIT = 65_536
OUTPUT_LIMIT = 8_388_608


def require(condition):
    if not condition:
        raise ValueError


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result)
        result[key] = value
    return result


def integer(text):
    value = int(text)
    require(-(2 ** 63) <= value < 2 ** 63)
    return value


def number(text):
    value = float(text)
    require(math.isfinite(value))
    return value


def validate(value, depth=1):
    require(depth <= 64 and not isinstance(value, (datetime.date, datetime.time)))
    if type(value) is float:
        require(math.isfinite(value))
    elif type(value) is dict:
        for key, item in value.items():
            key.encode("utf-8")
            validate(item, depth + 1)
    elif type(value) is list:
        for item in value:
            validate(item, depth + 1)
    elif type(value) is str:
        value.encode("utf-8")


def decode(payload):
    result = json.loads(payload.decode("utf-8"), object_pairs_hook=unique_object,
                        parse_int=integer, parse_float=number, parse_constant=number)
    validate(result)
    return result


def checked(code, findings, manifest):
    require(type(code) is int and code in (0, 1))
    require(type(findings) is dict and type(findings.get("comments")) is list)
    comments = findings["comments"]
    require(len(comments) <= 256)
    for comment in comments:
        require(type(comment) is dict and type(comment.get("code")) is int
                and 0 < comment["code"] <= 99999 and type(comment.get("level")) is str
                and comment["level"] in ("error", "warning", "info", "style"))
    require(type(manifest) is dict and type(manifest.get("repository")) is dict)
    repository = manifest["repository"]
    require(type(repository.get("license")) is str and type(repository.get("private")) is bool)
    require((code == 0) == (len(comments) == 0))
    manifest_ok = repository["license"] == "MIT" and not repository["private"]
    return {"findings": len(comments), "manifest_ok": manifest_ok,
            "passed": not comments and manifest_ok}


def saved(payload):
    try:
        require(len(payload) <= SAVED_LIMIT)
        document = decode(payload)
        require(type(document) is dict)
        return checked(document["code"], document["findings"], document["manifest"])
    except (ValueError, KeyError, TypeError, RecursionError):
        raise ValueError("Saved repository input failed.") from None


def acquire(tool, script):
    """Bound each captured stream and the process group lifetime, discard stderr."""
    with subprocess.Popen([str(tool), "--norc", "--format=json1", "--shell=sh",
                           "--severity=style", "--", str(script)], cwd=Path.cwd(),
                          env={"LC_ALL": "C"}, stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          start_new_session=True) as child:
        try:
            output = bytearray()
            sizes = {child.stdout: 0, child.stderr: 0}
            deadline = time.monotonic() + 600
            with selectors.DefaultSelector() as selected:
                for stream in sizes:
                    os.set_blocking(stream.fileno(), False)
                    selected.register(stream, selectors.EVENT_READ)
                while selected.get_map():
                    remaining = deadline - time.monotonic()
                    require(remaining > 0)
                    for key, _ in selected.select(min(remaining, 0.1)):
                        chunk = os.read(key.fd, 65536)
                        if not chunk:
                            selected.unregister(key.fileobj)
                            continue
                        sizes[key.fileobj] += len(chunk)
                        require(sizes[key.fileobj] <= OUTPUT_LIMIT)
                        if key.fileobj is child.stdout:
                            output.extend(chunk)
            return child.wait(timeout=max(0, deadline - time.monotonic())), bytes(output)
        finally:
            # A descendant may have closed the streams while the leader exited.
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()


def read_file(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
    with os.fdopen(descriptor, "rb") as handle:
        require(stat.S_ISREG(os.fstat(handle.fileno()).st_mode))
        payload = handle.read(FILE_LIMIT + 1)
    require(len(payload) <= FILE_LIMIT)
    return payload


def live(tool, script, manifest):
    try:
        require(tool.is_absolute())
        script = script.resolve(strict=True)
        before = read_file(script)
        manifest_bytes = read_file(manifest)
        code, payload = acquire(tool, script)
        require(before == read_file(script) and code in (0, 1))
    except (ValueError, OSError, subprocess.SubprocessError):
        raise ValueError("Checker acquisition failed.") from None
    try:
        findings = decode(payload)
    except (ValueError, RecursionError):
        raise ValueError("Checker output is invalid.") from None
    try:
        document = tomllib.loads(manifest_bytes.decode("utf-8"))
        validate(document)
    except (ValueError, RecursionError):
        raise ValueError("Repository manifest is invalid.") from None
    try:
        return checked(code, findings, document)
    except (ValueError, KeyError, TypeError):
        raise ValueError("Repository policy input is invalid.") from None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tool", type=Path)
    parser.add_argument("--script", type=Path)
    parser.add_argument("--manifest", type=Path)
    args = parser.parse_args()
    if any((args.tool, args.script, args.manifest)) and not all((args.tool, args.script, args.manifest)):
        parser.error("live comparison requires --tool, --script and --manifest")
    try:
        report = (live(args.tool, args.script, args.manifest) if args.tool
                  else saved(sys.stdin.buffer.read(SAVED_LIMIT + 1)))
        output = memoryview(json.dumps(report, separators=(",", ":"), sort_keys=True).encode())
        while output:
            written = os.write(1, output)
            require(written > 0)
            output = output[written:]
        return int(not report["passed"])
    except KeyboardInterrupt:
        code, message = 130, "Repository check cancelled."
    except (ValueError, OSError) as error:
        code = 2
        message = (str(error) if type(error) is ValueError and str(error) in {
            "Saved repository input failed.", "Checker acquisition failed.",
            "Checker output is invalid.", "Repository manifest is invalid.",
            "Repository policy input is invalid."} else "Repository check failed.")
    try:
        os.write(2, (message + "\n").encode())
    except OSError:
        pass
    return code


if __name__ == "__main__":
    sys.exit(main())
