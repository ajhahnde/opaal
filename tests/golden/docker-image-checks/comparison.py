#!/usr/bin/env python3
"""Independent saved-input Python reference for the three-field image policy."""
import json
import math
import os
import re
import sys

MAX_BYTES = 1_048_576
DIGEST = r"sha256:[0-9a-f]{64}"
SEGMENT = r"[a-z0-9][a-z0-9_.-]*"
MANIFESTS = {"application/vnd.oci.image.manifest.v1+json",
             "application/vnd.docker.distribution.manifest.v2+json"}


def require(condition):
    if not condition:
        raise ValueError("invalid image input")


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


def depth(value, level=1):
    require(level <= 64)
    if isinstance(value, dict):
        for item in value.values():
            depth(item, level + 1)
    elif isinstance(value, list):
        for item in value:
            depth(item, level + 1)


def optional_object(document, key):
    value = document.get(key)
    require(value is None or isinstance(value, dict))
    return value or {}


def assess(payload, expected):
    require(len(expected) == 3 and all(expected) and len(payload) <= MAX_BYTES)
    document = json.loads(payload.decode("utf-8"), object_pairs_hook=unique_object,
                          parse_int=integer, parse_float=number, parse_constant=number)
    require(isinstance(document, dict))
    depth(document)
    require(len(json.dumps(document, ensure_ascii=False, separators=(",", ":"),
                           allow_nan=False).encode("utf-8")) <= MAX_BYTES)
    image_id = document.get("Id")
    require(isinstance(image_id, str) and re.fullmatch(DIGEST, image_id))
    descriptor = document.get("Descriptor")
    require(isinstance(descriptor, dict))
    require(isinstance(descriptor.get("mediaType"), str)
            and descriptor["mediaType"] in MANIFESTS and descriptor.get("digest") == image_id)
    segments = [document.get("Os"), document.get("Architecture"), document.get("Variant", "")]
    for index, segment in enumerate(segments):
        require(isinstance(segment, str) and len(segment) <= 512
                and ((index == 2 and segment == "") or re.fullmatch(SEGMENT, segment)))
    platform = "/".join(segments[:2] + ([segments[2]] if segments[2] else []))
    require(len(platform) <= 512)
    labels = optional_object(optional_object(document, "Config"), "Labels")
    revision = labels.get("org.opencontainers.image.revision")
    require(revision is None or (isinstance(revision, str) and len(revision.encode()) <= 4096))
    repositories = document.get("RepoDigests")
    if repositories is not None:
        require(isinstance(repositories, list) and len(repositories) <= 256)
        for repository in repositories:
            require(isinstance(repository, str) and len(repository) <= 512
                    and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._:/-]*@" + DIGEST, repository))
    checks = [{"name": name, "passed": actual == wanted and (index != 2 or actual != ""),
               "expected": wanted, "actual": actual}
              for index, (name, actual, wanted) in enumerate(zip(
                  ("image-id", "platform", "revision"),
                  (image_id, platform, revision or ""), expected))]
    checks[2]["missing"] = revision is None
    return {"passed": all(check["passed"] for check in checks), "checks": checks}


def main():
    try:
        report = assess(sys.stdin.buffer.read(MAX_BYTES + 1), sys.argv[1:])
        output = memoryview(json.dumps(report, ensure_ascii=False, separators=(",", ":")).encode())
        while output:
            written = os.write(1, output)
            require(written > 0)
            output = output[written:]
        if not report["passed"]:
            os.write(2, b"Image check failed: compare expected and actual report fields.\n")
        return int(not report["passed"])
    except KeyboardInterrupt:
        code, message = 130, b"Image check cancelled.\n"
    except (ValueError, TypeError, RecursionError, OSError):
        code, message = 2, b"Image check failed: invalid input or output failure.\n"
    try:
        os.write(2, message)
    except OSError:
        pass
    return code


if __name__ == "__main__":
    sys.exit(main())
