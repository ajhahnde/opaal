from __future__ import annotations

import unittest

from ci.tests import test_data_processing_candidate as candidates
from ci.qualify_data_processing import QualificationError
from ci.qualify_random_values import byte_count, domains


class RandomFixtureIdentityTests(candidates.CandidateIdentityTests):
    directory = "tests/golden/random-values"
    bundle = "random-values"
    kind = "random-fixtures"


class RandomDomainTests(unittest.TestCase):
    def test_domains_reject_upper_bounds_nonfinite_and_nonlattice_samples(self):
        for shard, fraction in [("4", "0"), ("-1", "0"), ("0", "1"), ("0", "nan"),
                                ("0", "inf"), ("0", "-0.1"), ("0", str(2**-54))]:
            with self.assertRaises(QualificationError):
                domains(shard, fraction)
        domains("0", "0")
        domains("3", str(1 - 2**-53))

    def test_byte_rendering_counts_escaped_values_and_rejects_incomplete_escapes(self):
        self.assertEqual(byte_count('a\\x00\\xFF\\\\\\"'), 5)
        for value in ['\\', '\\x', '\\x0', '\\xgg', '\\n', '\u0100', '\n']:
            with self.assertRaises(QualificationError):
                byte_count(value)


if __name__ == "__main__":
    unittest.main()
