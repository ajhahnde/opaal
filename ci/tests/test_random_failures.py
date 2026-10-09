from __future__ import annotations

import unittest
from unittest import mock

from ci.qualify_random_failures import children, reaped


class NativeOwnershipTests(unittest.TestCase):
    def test_linux_children_use_the_exact_parent_and_handle_an_exited_parent(self):
        with mock.patch("ci.qualify_random_failures.platform.system", return_value="Linux"):
            with mock.patch("ci.qualify_random_failures.Path.read_text", return_value="12 34\n") as read:
                self.assertEqual(children(71), [12, 34])
                read.assert_called_once()
            with mock.patch("ci.qualify_random_failures.Path.read_text", side_effect=FileNotFoundError):
                self.assertEqual(children(71), [])
            with mock.patch("ci.qualify_random_failures.Path.read_text", side_effect=PermissionError):
                with self.assertRaises(PermissionError):
                    children(71)

    def test_reap_check_rejects_a_live_child_and_does_not_hide_permission_failures(self):
        with mock.patch("ci.qualify_random_failures.os.kill", side_effect=ProcessLookupError):
            reaped(71)
        with mock.patch("ci.qualify_random_failures.os.kill"):
            with self.assertRaises(RuntimeError):
                reaped(71)
        with mock.patch("ci.qualify_random_failures.os.kill", side_effect=PermissionError):
            with self.assertRaises(PermissionError):
                reaped(71)


if __name__ == "__main__":
    unittest.main()
