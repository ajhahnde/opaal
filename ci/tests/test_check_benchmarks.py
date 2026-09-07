from __future__ import annotations

import copy
import hashlib
import json
import re
import tempfile
import unittest
from pathlib import Path

from ci import check_benchmarks as checker


ROOT = Path(__file__).resolve().parents[2]


class BenchmarkCheckerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="opaal-benchmark-check-")
        self.root = Path(self.temporary.name)
        self.contract = self.root / "contract-v1.toml"
        self.budgets = self.root / "budgets-v1.toml"
        self.result = self.root / "result.json"
        self.contract.write_bytes((ROOT / "benchmarks/contract-v1.toml").read_bytes())
        budget_text = (ROOT / "benchmarks/budgets-v1.toml").read_text(encoding="utf-8")
        self.budgets.write_text(budget_text, encoding="utf-8")
        self.document = self.valid_result("qualification")
        self.write_result()

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def valid_result(self, profile: str) -> dict[str, object]:
        contract = checker.load_toml(self.contract)
        settings = contract["profiles"][profile]
        measurements = []
        for case in contract["cases"]:
            cold = case["sample_class"] == "cold"
            sample_count = 1 if cold else settings["samples"]
            warmup_count = 0 if cold else settings["warmups"]
            samples = [10] * sample_count
            measurements.append(
                {
                    "case_id": case["id"],
                    "unit": case["unit"],
                    "warmup_samples": [11] * warmup_count,
                    "samples": samples,
                    "summary": checker.expected_summary(samples),
                }
            )
        return {
            "schema": checker.RESULT_SCHEMA,
            "suite_version": 1,
            "profile": profile,
            "started_utc": "2026-09-06T00:00:00Z",
            "finished_utc": "2026-09-06T00:00:01Z",
            "contract_sha256": hashlib.sha256(self.contract.read_bytes()).hexdigest(),
            "binary_sha256": "0" * 64,
            "environment": {
                "kind": "host",
                "os": "macos",
                "os_release": "test",
                "architecture": "arm64",
                "python": "test",
                "rustc": "test",
                "cargo": "test",
                "logical_cpus": 1,
                "load_average_at_finish": [0.0, 0.0, 0.0],
            },
            "noise_controls": {"isolated": True},
            "parameters": settings,
            "measurements": measurements,
        }

    def write_result(self) -> None:
        self.result.write_text(json.dumps(self.document) + "\n", encoding="utf-8")

    def validate(self) -> None:
        checker.validate_bundle(self.contract, self.budgets, self.result)

    def assert_invalid(self, pattern: str) -> None:
        self.write_result()
        with self.assertRaisesRegex(checker.ValidationError, pattern):
            self.validate()

    def test_valid_host_contract_result_and_budget_bundle(self) -> None:
        self.validate()

    def test_unknown_result_schema_is_rejected(self) -> None:
        self.document["schema"] = "opaal-performance-result-v2"
        self.assert_invalid("schema or suite")

    def test_unknown_case_is_rejected(self) -> None:
        self.document["measurements"][0]["case_id"] = "host-unknown"
        self.assert_invalid("unknown case")

    def test_unknown_environment_is_rejected(self) -> None:
        self.document["environment"]["kind"] = "target"
        self.assert_invalid("must be a host")

    def test_duplicate_case_is_rejected(self) -> None:
        self.document["measurements"].append(
            copy.deepcopy(self.document["measurements"][0])
        )
        self.assert_invalid("duplicate or invalid measurement")

    def test_missing_measurement_is_rejected(self) -> None:
        self.document["measurements"].pop()
        self.assert_invalid("case set differs")

    def test_contract_digest_drift_is_rejected(self) -> None:
        self.document["contract_sha256"] = "f" * 64
        self.assert_invalid("contract digest")

    def test_wrong_summary_and_sample_count_are_rejected(self) -> None:
        with self.subTest("summary"):
            document = copy.deepcopy(self.document)
            document["measurements"][0]["summary"]["maximum"] = 11
            self.document = document
            self.assert_invalid("summary")
        with self.subTest("sample-count"):
            self.document = self.valid_result("qualification")
            self.document["measurements"][1]["samples"].pop()
            self.assert_invalid("expected")

    def test_invalid_budget_derivation_is_rejected(self) -> None:
        text = self.budgets.read_text(encoding="utf-8")
        changed, replacements = re.subn(
            r"(?m)^limit = (\d+)$",
            lambda match: f"limit = {int(match.group(1)) - 1}",
            text,
            count=1,
        )
        self.assertEqual(replacements, 1)
        self.budgets.write_text(
            changed,
            encoding="utf-8",
        )
        with self.assertRaisesRegex(checker.ValidationError, "derived limit"):
            self.validate()

    def test_regression_failure_is_rejected(self) -> None:
        measurement = self.document["measurements"][0]
        measurement["samples"] = [4_000_000_001]
        measurement["summary"] = checker.expected_summary(measurement["samples"])
        self.assert_invalid("fails maximum budget")


if __name__ == "__main__":
    unittest.main()
