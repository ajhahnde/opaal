"""Independent, offline reference for the synthetic CI job report."""

import argparse
import json
import math
import sys
from pathlib import Path


class InvalidInput(ValueError):
    pass


def pairs(fields):
    result = {}
    for key, value in fields:
        if key in result:
            raise InvalidInput("duplicate JSON key")
        result[key] = value
    return result


def number(token, integer=False):
    if integer:
        value = int(token)
        if -(2**63) <= value < 2**63:
            return value
    value = float(token)
    if not math.isfinite(value):
        raise InvalidInput("nonfinite JSON number")
    return value


def reject_constant(_token):
    raise InvalidInput("invalid JSON constant")


def validate_tree(value, depth=0):
    if isinstance(value, (dict, list)):
        if depth >= 64:
            raise InvalidInput("JSON depth limit")
        children = value.items() if isinstance(value, dict) else enumerate(value)
        for key, child in children:
            if isinstance(key, str):
                key.encode("utf-8", errors="strict")
            validate_tree(child, depth + 1)
    elif isinstance(value, str):
        value.encode("utf-8", errors="strict")


# Match Unicode White_Space used by Rust str::trim, without Python's extra
# U+001C..U+001F control separators.
WHITESPACE = "\t\n\v\f\r \u0085\u00a0\u1680\u2000\u2001\u2002\u2003\u2004\u2005\u2006\u2007\u2008\u2009\u200a\u2028\u2029\u202f\u205f\u3000"
CONCLUSIONS = {
    "success", "failure", "cancelled", "timed_out", "skipped", "neutral",
    "action_required", "stale", "startup_failure",
}


def build(raw):
    if len(raw) > 8 * 1024 * 1024:
        raise InvalidInput("JSON byte limit")
    document = json.loads(
        raw.decode("utf-8"), object_pairs_hook=pairs,
        parse_int=lambda token: number(token, integer=True),
        parse_float=number, parse_constant=reject_constant,
    )
    validate_tree(document)
    if not isinstance(document, dict) or not isinstance(document.get("jobs"), list):
        raise InvalidInput("expected record with jobs list")
    normalized = []
    for job in document["jobs"]:
        if not isinstance(job, dict):
            raise InvalidInput("expected job record")
        name = job.get("name")
        status = job.get("status")
        conclusion = job.get("conclusion", "")
        if not all(isinstance(field, str) for field in (name, status, conclusion)):
            raise InvalidInput("expected String job fields")
        if status not in {"completed", "queued", "in_progress"}:
            raise InvalidInput("unsupported job status")
        if status == "completed" and conclusion not in CONCLUSIONS:
            raise InvalidInput("unsupported completed-job conclusion")
        normalized.append({
            "name": " / ".join(name.strip(WHITESPACE).split("/")),
            "status": status, "conclusion": conclusion,
        })
    failures = [job for job in normalized if job["status"] == "completed"
                and job["conclusion"] not in {"success", "skipped", "neutral"}]
    return {
        "total": len(normalized), "failed": len(failures),
        "pending": len([job for job in normalized if job["status"] != "completed"]),
        "failures": sorted(failures, key=lambda job: job["name"])[:10],
    }


def encode(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"),
                      ensure_ascii=False, allow_nan=False)


def render(report):
    lines = [encode(job["name"]) + " : " + job["conclusion"]
             for job in report["failures"]]
    return f'failed={report["failed"]}, pending={report["pending"]}\n' + "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path)
    parser.add_argument("--text", action="store_true")
    arguments = parser.parse_args()
    try:
        report = build(arguments.input.read_bytes())
        output = render(report) if arguments.text else encode(report)
        sys.stdout.buffer.write(output.encode("utf-8"))
    except OSError:
        print("reference: unavailable input", file=sys.stderr)
        return 1
    except (ValueError, UnicodeError, RecursionError):
        print("reference: invalid input", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
