"""Exercise the GUI client's real HTTP transport without opening a WebView."""

from collections import deque
from contextlib import redirect_stderr
from http.client import RemoteDisconnected
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import io
import json
from pathlib import Path
import socket
import threading
import unittest

SPEC = importlib.util.spec_from_file_location("check_gui", Path(__file__).with_name("check-gui.py"))
GUI = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GUI)


class Peer(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def setup(self):
        super().setup()
        self.server.connections += 1
        self.connection_id = self.server.connections

    def log_message(self, *_):
        pass

    def respond(self):
        data = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        self.server.requests.append({
            "connection": self.connection_id, "method": self.command,
            "path": self.path, "close": self.headers.get("Connection"),
            "body": json.loads(data) if data else None,
        })
        outcome = self.server.outcomes.popleft() if self.server.outcomes else {}
        if outcome == "disconnect":
            # The command arrived, but its response was lost. Replaying a
            # mutation here could execute it twice.
            self.close_connection = True
            self.connection.shutdown(socket.SHUT_RDWR)
            return
        if self.path == "/session":
            value = {"sessionId": "test"}
        elif self.path.endswith("/element"):
            value = {GUI.ELEMENT: "id"}
        else:
            value = "observed state"
        body = outcome.get("body", json.dumps({"value": outcome.get("value", value)}).encode())
        self.send_response(outcome.get("status", 200))
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        if outcome.get("close"):
            self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    do_GET = do_POST = do_DELETE = respond


class Transport(unittest.TestCase):
    def setUp(self):
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Peer)
        self.server.daemon_threads = True
        self.server.connections = 0
        self.server.requests = []
        self.server.outcomes = deque()
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.diagnostics = io.StringIO()
        self.driver = GUI.WebDriver(Path("test-application"), port=self.server.server_port,
                                    diagnostics=self.diagnostics)
        self.server.requests.clear()

    def tearDown(self):
        try:
            self.server.outcomes.clear()
            self.driver.close()
        finally:
            self.server.shutdown()
            self.server.server_close()
            self.thread.join(timeout=5)

    def test_polling_and_clicks_reuse_connection_without_requesting_close(self):
        for _ in range(20):
            self.assertEqual(self.driver.text("#counter"), "observed state")
        self.driver.click("#save")
        self.assertEqual(len(self.server.requests), 42)
        self.assertEqual(self.server.connections, 1)
        self.assertTrue(all(request["close"] is None for request in self.server.requests))
        self.assertEqual(self.server.requests[-1]["path"], "/session/test/element/id/click")
        self.assertEqual(self.driver.retries, 0)

    def test_read_disconnect_recovers_once_on_a_new_connection(self):
        for method, path in (("GET", "/session/test/element/id/text"),
                             ("POST", "/session/test/element")):
            with self.subTest(method=method), redirect_stderr(io.StringIO()) as stderr:
                self.server.requests.clear()
                connections = self.server.connections
                self.server.outcomes.extend(["disconnect", {"value": "recovered"}])
                self.assertEqual(self.driver.request(method, path), "recovered")
                self.assertEqual(len(self.server.requests), 2)
                self.assertEqual(self.server.connections, connections + 1)
                self.assertIn("Retrying WebDriver read", stderr.getvalue())

    def test_state_changing_requests_are_never_replayed(self):
        for method, path in (("POST", "/session"),
                             ("POST", "/session/test/element/id/click"),
                             ("DELETE", "/session/test")):
            with self.subTest(path=path):
                self.server.requests.clear()
                self.server.outcomes.append("disconnect")
                with self.assertRaises(RemoteDisconnected):
                    self.driver.request(method, path)
                self.assertEqual(len(self.server.requests), 1)
                self.assertIsNone(self.driver.connection.sock)
        self.assertEqual(self.driver.retries, 0)

    def test_repeated_read_disconnect_is_reported(self):
        self.server.outcomes.extend(["disconnect", "disconnect"])
        with redirect_stderr(io.StringIO()), self.assertRaises(RemoteDisconnected):
            self.driver.request("GET", "/session/test/screenshot")
        self.assertEqual(len(self.server.requests), 2)
        self.assertEqual(self.driver.retries, 1)

    def test_strict_transport_reports_first_read_disconnect(self):
        self.driver.read_retries = 0
        self.server.outcomes.append("disconnect")
        with self.assertRaises(RemoteDisconnected):
            self.driver.text("#counter")
        self.assertEqual(len(self.server.requests), 1)
        self.assertEqual(self.driver.retries, 0)

    def test_http_and_protocol_errors_are_not_retried(self):
        for status in (200, 500):
            with self.subTest(status=status):
                self.server.requests.clear()
                self.server.outcomes.append({"status": status, "value": {"error": "invalid session id"}})
                with self.assertRaisesRegex(RuntimeError, "invalid session id"):
                    self.driver.request("GET", "/session/test/screenshot")
                self.assertEqual(len(self.server.requests), 1)
                self.assertIsNone(self.driver.connection.sock)
        self.assertEqual(self.driver.retries, 0)

    def test_invalid_json_discards_connection_and_is_not_retried(self):
        self.server.outcomes.append({"body": b"invalid json"})
        with self.assertRaises(json.JSONDecodeError):
            self.driver.request("GET", "/session/test/screenshot")
        self.assertEqual(len(self.server.requests), 1)
        self.assertIsNone(self.driver.connection.sock)

    def test_server_requested_close_reconnects_before_next_command(self):
        self.server.outcomes.append({"close": True})
        self.driver.request("GET", "/status")
        self.driver.request("GET", "/status")
        self.assertEqual(len(self.server.requests), 2)
        self.assertEqual(self.server.connections, 2)
        self.assertEqual(self.driver.retries, 0)

    def test_failed_session_delete_still_releases_transport(self):
        self.server.outcomes.append("disconnect")
        with self.assertRaises(RemoteDisconnected):
            self.driver.close()
        self.assertIsNone(self.driver.connection.sock)
        self.assertIsNone(self.driver.session)
        self.assertEqual(len(self.server.requests), 1)

    def test_diagnostics_record_failed_and_successful_attempts(self):
        self.server.outcomes.extend(["disconnect", {}])
        with redirect_stderr(io.StringIO()):
            self.driver.request("GET", "/status")
        records = [json.loads(line) for line in self.diagnostics.getvalue().splitlines()]
        failed, recovered = records[-2:]
        self.assertEqual(failed["method"], "GET")
        self.assertEqual(failed["path"], "/status")
        self.assertEqual(failed["attempt"], 1)
        self.assertIn("RemoteDisconnected", failed["error"])
        self.assertEqual(recovered["attempt"], 2)
        self.assertEqual(recovered["status"], 200)
        self.assertNotIn("error", recovered)
        self.assertIn("time", failed)
        self.assertGreaterEqual(failed["elapsed_ms"], 0)


if __name__ == "__main__":
    unittest.main()
