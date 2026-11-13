"""Exercise the real Tauri WebView through tauri-driver (Linux CI uses Xvfb)."""

import argparse
import base64
from datetime import datetime, timezone
from http.client import HTTPConnection, HTTPException, RemoteDisconnected
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
ELEMENT = "element-6066-11e4-a52e-4f735466cecf"


def wait_for_driver(server, *, port=4444, native_port=4445, seconds=20, diagnostics=None):
    # The proxy can listen before its native child is accepting connections.
    # Probe both HTTP endpoints before sending the non-replayable POST /session.
    deadline = time.monotonic() + seconds
    last_error = None
    while time.monotonic() < deadline:
        if server.poll() is not None:
            raise RuntimeError("tauri-driver exited; see driver.log")
        for endpoint in (native_port, port):
            connection = HTTPConnection("127.0.0.1", endpoint, timeout=1)
            record = {"time": datetime.now(timezone.utc).isoformat(),
                      "port": endpoint, "method": "GET", "path": "/status"}
            try:
                connection.request("GET", "/status")
                with connection.getresponse() as response:
                    record["status"] = response.status
                    body = response.read()
                value = json.loads(body).get("value", {})
                if record["status"] != 200 or not isinstance(value, dict) or value.get("ready") is not True:
                    raise RuntimeError(f"WebDriver is not ready: {body!r}")
            except (OSError, HTTPException, ValueError, RuntimeError) as error:
                last_error = error
                record["error"] = f"{type(error).__name__}: {error}"
                break
            finally:
                connection.close()
                if diagnostics is not None:
                    diagnostics.write(json.dumps(record) + "\n")
                    diagnostics.flush()
        else:
            return
        time.sleep(0.1)
    raise TimeoutError(f"WebDriver endpoints did not become ready: {last_error}")


class WebDriver:
    def __init__(self, application, *, port=4444, read_retries=1, diagnostics=None):
        self.session = None
        self.connection = HTTPConnection("127.0.0.1", port, timeout=120)
        self.read_retries = read_retries
        self.diagnostics = diagnostics
        self.requests = 0
        self.retries = 0
        session = self.request(
            "POST",
            "/session",
            {
                "capabilities": {
                    "alwaysMatch": {
                        "browserName": "wry",
                        "tauri:options": {"application": str(application)},
                    }
                }
            },
        )
        self.session = session["sessionId"]

    def request(self, method, path, payload=None):
        data = None if payload is None else json.dumps(payload).encode()
        # urllib forces Connection: close on every request. tauri-driver 2.1.0
        # forwards that hop-by-hop header to its pooled native-driver client,
        # which can reuse the connection while the native peer is closing it.
        # Keep one HTTP/1.1 connection and fully consume each response instead.
        # A disconnect leaves clicks and session mutations ambiguous. Only
        # reads, including WebDriver's POST element lookup, may be repeated.
        read_only = method == "GET" or (
            method == "POST" and re.fullmatch(r"/session/[^/]+/element", path)
        )
        attempts = 1 + (self.read_retries if read_only else 0)
        for attempt in range(attempts):
            started = time.monotonic()
            self.requests += 1
            record = {
                "time": datetime.now(timezone.utc).isoformat(),
                "request": self.requests, "method": method, "path": path,
                "attempt": attempt + 1,
            }
            try:
                self.connection.request(
                    method, path, body=data, headers={"Content-Type": "application/json"},
                )
                with self.connection.getresponse() as response:
                    body = response.read()
                    record["status"] = response.status
                if not 200 <= record["status"] < 300:
                    raise RuntimeError(body.decode(errors="replace"))
                value = json.loads(body)["value"]
                if isinstance(value, dict) and value.get("error"):
                    raise RuntimeError(json.dumps(value))
                return value
            except Exception as error:
                # A failed or partially consumed response must never leave a
                # connection available for the next command.
                self.connection.close()
                record["error"] = f"{type(error).__name__}: {error}"
                if not isinstance(error, RemoteDisconnected) or attempt + 1 == attempts:
                    raise
                self.retries += 1
                print(f"Retrying WebDriver read after connection closed: {method} {path}",
                      file=sys.stderr)
            finally:
                record["elapsed_ms"] = round((time.monotonic() - started) * 1000, 3)
                if self.diagnostics is not None:
                    self.diagnostics.write(json.dumps(record) + "\n")
                    self.diagnostics.flush()

    def command(self, method, path, payload=None):
        return self.request(method, "/session/" + self.session + path, payload)

    def element(self, selector):
        return self.command("POST", "/element", {"using": "css selector", "value": selector})[ELEMENT]

    def click(self, selector):
        self.command("POST", "/element/" + self.element(selector) + "/click", {})

    def text(self, selector):
        return self.command("GET", "/element/" + self.element(selector) + "/text")

    def number(self, selector):
        return int(re.sub(r"[^0-9]", "", self.text(selector)) or "0")

    def wait(self, predicate, label, seconds=150):
        deadline = time.monotonic() + seconds
        last_error = None
        while time.monotonic() < deadline:
            try:
                if predicate():
                    return
            except RuntimeError as error:
                last_error = error
            time.sleep(0.2)
        state = {key: self.text("#" + key) for key in (
            "status", "generation", "worker-generation", "counter", "build-error"
        )}
        raise AssertionError(f"Timed out: {label}; state={state}; error={last_error}")

    def screenshot(self, path):
        path.write_bytes(base64.b64decode(self.command("GET", "/screenshot")))

    def close(self):
        try:
            if self.session:
                self.request("DELETE", "/session/" + self.session)
        finally:
            self.session = None
            self.connection.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug" / (
        "tauri_tokio.exe" if os.name == "nt" else "tauri_tokio"
    ))
    parser.add_argument("--output", type=Path, default=ROOT / "target/gui-smoke")
    parser.add_argument("--strict-transport", action="store_true",
                        help="Fail on the first disconnect, including read-only commands")
    options = parser.parse_args()
    application = options.binary.resolve(strict=True)
    output = options.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    source = ROOT / "examples/tauri_tokio_logic/src/lib.rs"
    original = source.read_bytes()
    driver = None
    checks = []
    primary_error = None
    env = os.environ.copy()
    env["REKNOT_CACHE_ROOT"] = str(output / "native-cache")
    flags = subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0
    with (output / "driver.log").open("w", encoding="utf-8") as log, \
            (output / "requests.jsonl").open("w", encoding="utf-8") as requests:
        server = subprocess.Popen(
            ["tauri-driver"], cwd=ROOT, env=env, stdout=log,
            stderr=subprocess.STDOUT, creationflags=flags,
        )
        try:
            with (output / "startup.jsonl").open("w", encoding="utf-8") as startup:
                wait_for_driver(server, diagnostics=startup)
            driver = WebDriver(application, read_retries=0 if options.strict_transport else 1,
                               diagnostics=requests)
            driver.wait(lambda: driver.number("#counter") > 0, "initial counter")
            host = driver.text("#host-id")
            checks.append("window startup and live counter")

            old = driver.number("#worker-generation")
            driver.click("#preset-v1")
            driver.click("#save")
            driver.wait(
                lambda: driver.number("#worker-generation") > old and
                driver.text("#step") == "+1 / tick",
                "v1 editor save and Restart",
            )
            worker = driver.number("#worker-generation")
            generation = driver.number("#generation")
            count = driver.number("#counter")
            checks.append("editor preset and source save")

            driver.click('#mode option[value="drain"]')
            driver.click("#preset-v2")
            driver.click("#save")
            driver.wait(lambda: driver.number("#generation") > generation, "Drain")
            assert driver.number("#worker-generation") == worker
            assert driver.text("#step") == "+1 / tick"
            assert driver.number("#counter") >= count
            driver.click("#send")
            driver.wait(lambda: "Coral / v2" in driver.text("#replies"), "new-generation reply")
            checks.append("Drain preserves old worker and routes new requests")

            driver.click('#mode option[value="restart"]')
            driver.click("#rebuild")
            driver.wait(
                lambda: driver.number("#worker-generation") > worker and
                driver.text("#step") == "+5 / tick",
                "Restart updates worker",
            )
            assert driver.number("#counter") >= count
            generation = driver.number("#generation")
            checks.append("Restart updates worker and preserves counter")

            driver.click("#preset-error")
            driver.click("#save")
            driver.wait(
                lambda: "intentional build error" in driver.text("#build-error"),
                "build error diagnostics",
            )
            before = driver.number("#counter")
            driver.wait(lambda: driver.number("#counter") > before, "old worker during build error")
            assert driver.number("#generation") == generation
            checks.append("build error leaves old worker running")

            driver.click("#preset-v2")
            driver.click("#save")
            driver.wait(
                lambda: driver.number("#generation") > generation and
                not driver.text("#build-error"),
                "recovery after source repair",
            )
            assert driver.text("#host-id") == host
            assert driver.number("#counter") >= before
            driver.screenshot(output / "recovered.png")
            checks.append("repair recovers in same host")
        except Exception as error:
            primary_error = error
            if driver:
                try:
                    driver.screenshot(output / "failure.png")
                except Exception:
                    pass
            raise
        finally:
            try:
                if driver:
                    try:
                        driver.close()
                    except Exception as error:
                        if primary_error is None:
                            raise
                        print(f"WebDriver cleanup also failed: {error}", file=sys.stderr)
            finally:
                try:
                    source.write_bytes(original)
                finally:
                    server.terminate()
                    try:
                        server.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        server.kill()
                        server.wait()
    result = {"host": host, "checks": checks, "transport": {
        "requests": driver.requests, "retries": driver.retries,
        "strict": options.strict_transport,
    }}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
