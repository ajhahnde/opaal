#!/usr/bin/env python3
"""Validate the standalone OPAAL benchmark contract, evidence, and budgets."""

from __future__ import annotations

import argparse
import hashlib
import json
import statistics
import sys
import tomllib
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CONTRACT = ROOT / "benchmarks/contract-v1.toml"
DEFAULT_BUDGETS = ROOT / "benchmarks/budgets-v1.toml"
RESULT_SCHEMA = "opaal-performance-result-v1"
CASE_IDS = {
    "host-startup-cold",
    "host-startup-warm",
    "host-first-prompt-cold",
    "host-first-prompt-warm",
    "host-structured-stream-memory-warm",
    "host-completion-cold",
    "host-completion-warm",
}


class ValidationError(ValueError):
    """One benchmark bundle violates its checked contract."""


def fail(message: str) -> None:
    raise ValidationError(message)


def load_toml(path: Path) -> dict[str, Any]:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def load_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(f"cannot load benchmark result {path}: {error}")
    if not isinstance(value, dict):
        fail("benchmark result must be a JSON object")
    return value


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def exact_keys(value: dict[str, Any], expected: set[str], label: str) -> None:
    observed = set(value)
    if observed != expected:
        fail(
            f"{label} keys differ: missing={sorted(expected - observed)} "
            f"unknown={sorted(observed - expected)}"
        )


def positive_integer(value: Any, label: str, *, allow_zero: bool = False) -> int:
    minimum = 0 if allow_zero else 1
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        fail(f"{label} must be an integer >= {minimum}")
    return value


def validate_contract(contract: dict[str, Any]) -> dict[str, dict[str, Any]]:
    exact_keys(
        contract,
        {
            "schema_version",
            "suite_version",
            "result_schema",
            "supported_host_os",
            "statistics",
            "profiles",
            "cases",
        },
        "contract",
    )
    if contract["schema_version"] != 1 or contract["suite_version"] != 1:
        fail("contract schema_version and suite_version must both be 1")
    if contract["result_schema"] != RESULT_SCHEMA:
        fail(f"contract result_schema must be {RESULT_SCHEMA!r}")
    if contract["supported_host_os"] != ["linux", "macos"]:
        fail("contract must support exactly Linux and macOS hosts")
    if contract["statistics"] != "median and nearest-rank p95 over retained raw samples":
        fail("contract statistics definition drifted")

    profiles = contract["profiles"]
    if not isinstance(profiles, dict) or set(profiles) != {"smoke", "qualification"}:
        fail("contract must define exactly smoke and qualification profiles")
    for name, profile in profiles.items():
        if not isinstance(profile, dict):
            fail(f"profile {name} must be a table")
        exact_keys(profile, {"warmups", "samples", "stream_items"}, f"profile {name}")
        positive_integer(profile["warmups"], f"profile {name} warmups", allow_zero=True)
        positive_integer(profile["samples"], f"profile {name} samples")
        positive_integer(profile["stream_items"], f"profile {name} stream_items")

    cases = contract["cases"]
    if not isinstance(cases, list):
        fail("contract cases must be an array")
    by_id: dict[str, dict[str, Any]] = {}
    expected_keys = {
        "id",
        "surface",
        "environment",
        "metric",
        "unit",
        "direction",
        "sample_class",
    }
    for case in cases:
        if not isinstance(case, dict):
            fail("each contract case must be a table")
        exact_keys(case, expected_keys, "contract case")
        case_id = case["id"]
        if not isinstance(case_id, str) or case_id in by_id:
            fail(f"duplicate or invalid contract case {case_id!r}")
        if case["environment"] != "host":
            fail(f"case {case_id} is not host-only")
        if case["direction"] not in {"maximum", "minimum"}:
            fail(f"case {case_id} has an unknown direction")
        if case["sample_class"] not in {"cold", "warm"}:
            fail(f"case {case_id} has an unknown sample class")
        by_id[case_id] = case
    if set(by_id) != CASE_IDS:
        fail(
            f"contract case set differs: missing={sorted(CASE_IDS - set(by_id))} "
            f"unknown={sorted(set(by_id) - CASE_IDS)}"
        )
    return by_id


def derived_limit(budget: dict[str, Any], direction: str) -> int:
    baseline = positive_integer(budget["baseline"], "budget baseline")
    numerator = positive_integer(budget["factor_numerator"], "budget factor numerator")
    denominator = positive_integer(
        budget["factor_denominator"], "budget factor denominator"
    )
    if direction == "maximum":
        return (baseline * numerator + denominator - 1) // denominator
    return baseline * denominator // numerator


def validate_budgets(
    budgets: dict[str, Any], contract_path: Path, cases: dict[str, dict[str, Any]]
) -> tuple[dict[str, dict[str, Any]], dict[tuple[str, str], dict[str, Any]]]:
    exact_keys(
        budgets,
        {"schema_version", "suite_version", "contract_sha256", "environments", "budgets"},
        "budgets",
    )
    if budgets["schema_version"] != 1 or budgets["suite_version"] != 1:
        fail("budget schema_version and suite_version must both be 1")
    if budgets["contract_sha256"] != sha256(contract_path):
        fail("budget contract digest does not match contract-v1.toml")

    environments: dict[str, dict[str, Any]] = {}
    for environment in budgets["environments"]:
        if not isinstance(environment, dict):
            fail("each environment must be a table")
        exact_keys(environment, {"id", "evidence", "match"}, "budget environment")
        identifier = environment["id"]
        if not isinstance(identifier, str) or identifier in environments:
            fail(f"duplicate or invalid environment {identifier!r}")
        match = environment["match"]
        if (
            not isinstance(match, dict)
            or match.get("kind") != "host"
            or match.get("os") not in {"linux", "macos"}
            or not isinstance(match.get("architecture"), str)
            or "flashos" in json.dumps(match).lower()
        ):
            fail(f"environment {identifier} is not a supported host boundary")
        evidence = Path(environment["evidence"])
        if evidence.is_absolute() or ".." in evidence.parts:
            fail(f"environment {identifier} has an unsafe evidence path")
        environments[identifier] = environment
    if set(environments) != {"host-darwin-arm64"}:
        fail("budgets must define exactly the candidate host-darwin-arm64 environment")

    by_key: dict[tuple[str, str], dict[str, Any]] = {}
    required_budget_keys = {
        "environment",
        "case_id",
        "statistic",
        "baseline",
        "factor_numerator",
        "factor_denominator",
        "limit",
    }
    for budget in budgets["budgets"]:
        if not isinstance(budget, dict):
            fail("each budget must be a table")
        exact_keys(budget, required_budget_keys, "budget")
        key = (budget["environment"], budget["case_id"])
        if key in by_key:
            fail(f"duplicate budget {key!r}")
        if key[0] not in environments or key[1] not in cases:
            fail(f"budget references an unknown environment or case: {key!r}")
        statistic = budget["statistic"]
        if statistic not in {"minimum", "median", "p95", "maximum"}:
            fail(f"budget {key!r} has an unknown statistic")
        expected_limit = derived_limit(budget, cases[key[1]]["direction"])
        if budget["limit"] != expected_limit:
            fail(
                f"budget {key!r} limit {budget['limit']!r} "
                f"does not equal derived limit {expected_limit}"
            )
        by_key[key] = budget
    expected_keys = {(environment, case_id) for environment in environments for case_id in CASE_IDS}
    if set(by_key) != expected_keys:
        fail(
            f"budget set differs: missing={sorted(expected_keys - set(by_key))} "
            f"unknown={sorted(set(by_key) - expected_keys)}"
        )
    return environments, by_key


def nearest_rank(values: list[int], percentile: int) -> int:
    ordered = sorted(values)
    rank = max(1, (len(ordered) * percentile + 99) // 100)
    return ordered[min(rank, len(ordered)) - 1]


def expected_summary(values: list[int]) -> dict[str, int]:
    return {
        "minimum": min(values),
        "median": int(statistics.median(values)),
        "p95": nearest_rank(values, 95),
        "maximum": max(values),
    }


def matching_environment(
    result_environment: dict[str, Any],
    environments: dict[str, dict[str, Any]],
    selected: str | None,
) -> str:
    if selected is not None:
        if selected not in environments:
            fail(f"unknown budget environment {selected!r}")
        candidates = [selected]
    else:
        candidates = list(environments)
    matches = [
        identifier
        for identifier in candidates
        if all(result_environment.get(key) == value for key, value in environments[identifier]["match"].items())
    ]
    if len(matches) != 1:
        fail(f"result environment matches {matches!r}; expected exactly one budget environment")
    return matches[0]


def validate_result(
    result: dict[str, Any],
    contract: dict[str, Any],
    contract_path: Path,
    cases: dict[str, dict[str, Any]],
    environments: dict[str, dict[str, Any]],
    budgets: dict[tuple[str, str], dict[str, Any]],
    *,
    selected_environment: str | None = None,
    binary_path: Path | None = None,
) -> None:
    required = {
        "schema",
        "suite_version",
        "profile",
        "started_utc",
        "finished_utc",
        "contract_sha256",
        "binary_sha256",
        "environment",
        "noise_controls",
        "parameters",
        "measurements",
    }
    exact_keys(result, required, "result")
    if result["schema"] != RESULT_SCHEMA or result["suite_version"] != 1:
        fail("result schema or suite version is unknown")
    profile_name = result["profile"]
    profiles = contract["profiles"]
    if profile_name not in profiles:
        fail(f"result uses unknown profile {profile_name!r}")
    if result["contract_sha256"] != sha256(contract_path):
        fail("result contract digest does not match contract-v1.toml")
    if not re_full_sha256(result["binary_sha256"]):
        fail("result binary_sha256 is not a lowercase SHA-256")
    if binary_path is not None and binary_path.is_file() and result["binary_sha256"] != sha256(binary_path):
        fail("result binary digest does not match target/release/opaal")
    if result["parameters"] != profiles[profile_name]:
        fail("result parameters do not exactly match the selected contract profile")
    environment = result["environment"]
    if not isinstance(environment, dict) or environment.get("kind") != "host":
        fail("result environment must be a host object")
    if environment.get("os") not in contract["supported_host_os"]:
        fail(f"result uses unknown host OS {environment.get('os')!r}")

    measurements = result["measurements"]
    if not isinstance(measurements, list):
        fail("result measurements must be an array")
    by_id: dict[str, dict[str, Any]] = {}
    for measurement in measurements:
        if not isinstance(measurement, dict):
            fail("each measurement must be an object")
        exact_keys(
            measurement,
            {"case_id", "unit", "warmup_samples", "samples", "summary"},
            "measurement",
        )
        case_id = measurement["case_id"]
        if not isinstance(case_id, str) or case_id in by_id:
            fail(f"duplicate or invalid measurement {case_id!r}")
        if case_id not in cases:
            fail(f"measurement uses unknown case {case_id!r}")
        case = cases[case_id]
        if measurement["unit"] != case["unit"]:
            fail(f"measurement {case_id} uses the wrong unit")
        samples = measurement["samples"]
        warmups = measurement["warmup_samples"]
        if not isinstance(samples, list) or not isinstance(warmups, list):
            fail(f"measurement {case_id} samples must be arrays")
        if any(isinstance(value, bool) or not isinstance(value, int) or value <= 0 for value in samples + warmups):
            fail(f"measurement {case_id} samples must be positive integers")
        profile = profiles[profile_name]
        expected_samples = 1 if case["sample_class"] == "cold" else profile["samples"]
        expected_warmups = 0 if case["sample_class"] == "cold" else profile["warmups"]
        if len(samples) != expected_samples or len(warmups) != expected_warmups:
            fail(
                f"measurement {case_id} has {len(samples)} samples/{len(warmups)} warmups; "
                f"expected {expected_samples}/{expected_warmups}"
            )
        if measurement["summary"] != expected_summary(samples):
            fail(f"measurement {case_id} summary does not match its raw samples")
        by_id[case_id] = measurement
    if set(by_id) != CASE_IDS:
        fail(
            f"result case set differs: missing={sorted(CASE_IDS - set(by_id))} "
            f"unknown={sorted(set(by_id) - CASE_IDS)}"
        )

    if profile_name == "qualification":
        environment_id = matching_environment(environment, environments, selected_environment)
        for case_id, measurement in by_id.items():
            budget = budgets[(environment_id, case_id)]
            observed = measurement["summary"][budget["statistic"]]
            direction = cases[case_id]["direction"]
            passes = observed <= budget["limit"] if direction == "maximum" else observed >= budget["limit"]
            if not passes:
                fail(
                    f"measurement {case_id} {budget['statistic']}={observed} "
                    f"fails {direction} budget {budget['limit']}"
                )


def re_full_sha256(value: Any) -> bool:
    return isinstance(value, str) and len(value) == 64 and all(
        character in "0123456789abcdef" for character in value
    )


def validate_bundle(
    contract_path: Path,
    budgets_path: Path,
    result_path: Path | None,
    *,
    selected_environment: str | None = None,
    check_evidence: bool = False,
    binary_path: Path | None = None,
) -> None:
    contract = load_toml(contract_path)
    cases = validate_contract(contract)
    budget_document = load_toml(budgets_path)
    environments, budgets = validate_budgets(budget_document, contract_path, cases)
    if result_path is not None:
        validate_result(
            load_json(result_path),
            contract,
            contract_path,
            cases,
            environments,
            budgets,
            selected_environment=selected_environment,
            binary_path=binary_path,
        )
    if check_evidence:
        for identifier, environment in environments.items():
            evidence_path = budgets_path.parent / environment["evidence"]
            if not evidence_path.is_file():
                fail(f"environment {identifier} evidence is missing: {environment['evidence']}")
            validate_result(
                load_json(evidence_path),
                contract,
                contract_path,
                cases,
                environments,
                budgets,
                selected_environment=identifier,
            )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--contract", type=Path, default=DEFAULT_CONTRACT)
    parser.add_argument("--budgets", type=Path, default=DEFAULT_BUDGETS)
    selection = parser.add_mutually_exclusive_group(required=True)
    selection.add_argument("--contract-only", action="store_true")
    selection.add_argument("--result", type=Path)
    parser.add_argument("--environment")
    return parser.parse_args()


def main() -> int:
    arguments = parse_args()
    try:
        validate_bundle(
            arguments.contract,
            arguments.budgets,
            arguments.result,
            selected_environment=arguments.environment,
            check_evidence=arguments.contract_only,
            binary_path=ROOT / "target/release/opaal" if arguments.result else None,
        )
    except (OSError, tomllib.TOMLDecodeError, ValidationError) as error:
        print(f"benchmark validation failed: {error}", file=sys.stderr)
        return 1
    print("benchmark validation: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
