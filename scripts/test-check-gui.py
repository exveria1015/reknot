"""Check transport recovery without replaying GUI actions or hiding disconnects."""

from contextlib import redirect_stderr
from http.client import RemoteDisconnected
import importlib.util
import io
from pathlib import Path
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    "check_gui", Path(__file__).with_name("check-gui.py")
)
GUI = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GUI)


class TransportRecovery(unittest.TestCase):
    def setUp(self):
        self.driver = GUI.WebDriver.__new__(GUI.WebDriver)

    def test_read_disconnect_recovers_once(self):
        for method, path in (
            ("GET", "/session/test/element/id/text"),
            ("POST", "/session/test/element"),
        ):
            with self.subTest(method=method), patch.object(
                GUI, "urlopen", side_effect=[
                    RemoteDisconnected("connection closed"),
                    io.BytesIO(b'{"value": "observed state"}'),
                ]
            ) as request, redirect_stderr(io.StringIO()) as diagnostics:
                self.assertEqual(self.driver.request(method, path), "observed state")
                self.assertEqual(request.call_count, 2)
                self.assertIn("Retrying WebDriver read", diagnostics.getvalue())

    def test_state_changing_requests_are_never_replayed(self):
        for method, path in (
            ("POST", "/session"),
            ("POST", "/session/test/element/id/click"),
            ("DELETE", "/session/test"),
        ):
            with self.subTest(path=path), patch.object(
                GUI, "urlopen", side_effect=RemoteDisconnected("connection closed")
            ) as request:
                with self.assertRaises(RemoteDisconnected):
                    self.driver.request(method, path)
                self.assertEqual(request.call_count, 1)

    def test_repeated_read_disconnect_is_reported(self):
        with patch.object(
            GUI, "urlopen", side_effect=RemoteDisconnected("connection closed")
        ) as request, redirect_stderr(io.StringIO()):
            with self.assertRaises(RemoteDisconnected):
                self.driver.request("GET", "/session/test/screenshot")
            self.assertEqual(request.call_count, 2)


if __name__ == "__main__":
    unittest.main()
