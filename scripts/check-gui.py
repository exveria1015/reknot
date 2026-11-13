"""Exercise the real Tauri WebView through tauri-driver (Linux CI uses Xvfb)."""

import argparse
import base64
from http.client import RemoteDisconnected
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen

ROOT = Path(__file__).resolve().parents[1]
ELEMENT = "element-6066-11e4-a52e-4f735466cecf"


class WebDriver:
    def __init__(self, application):
        self.session = None
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
        request = Request(
            "http://127.0.0.1:4444" + path,
            data=data,
            method=method,
            headers={"Content-Type": "application/json"},
        )
        # A disconnect leaves clicks and session mutations ambiguous. Only
        # reads, including WebDriver's POST element lookup, may be repeated.
        read_only = method == "GET" or (
            method == "POST" and re.fullmatch(r"/session/[^/]+/element", path)
        )
        for attempt in range(2):
            try:
                with urlopen(request, timeout=120) as response:
                    value = json.load(response)["value"]
                break
            except HTTPError as error:
                raise RuntimeError(error.read().decode(errors="replace")) from error
            except RemoteDisconnected:
                if not read_only or attempt:
                    raise
                print(f"Retrying WebDriver read after connection closed: {method} {path}",
                      file=sys.stderr)
        if isinstance(value, dict) and value.get("error"):
            raise RuntimeError(json.dumps(value))
        return value

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
        if self.session:
            self.request("DELETE", "/session/" + self.session)
            self.session = None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug" / (
        "tauri_tokio.exe" if os.name == "nt" else "tauri_tokio"
    ))
    options = parser.parse_args()
    application = options.binary.resolve(strict=True)
    output = ROOT / "target/gui-smoke"
    output.mkdir(parents=True, exist_ok=True)
    source = ROOT / "examples/tauri_tokio_logic/src/lib.rs"
    original = source.read_bytes()
    driver = None
    checks = []
    env = os.environ.copy()
    env["REKNOT_CACHE_ROOT"] = str(output / "native-cache")
    flags = subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0
    with (output / "driver.log").open("w", encoding="utf-8") as log:
        server = subprocess.Popen(
            ["tauri-driver"], cwd=ROOT, env=env, stdout=log,
            stderr=subprocess.STDOUT, creationflags=flags,
        )
        try:
            deadline = time.monotonic() + 20
            while True:
                if server.poll() is not None:
                    raise RuntimeError("tauri-driver exited; see driver.log")
                try:
                    with socket.create_connection(("127.0.0.1", 4444), timeout=1):
                        break
                except OSError:
                    if time.monotonic() >= deadline:
                        raise TimeoutError("tauri-driver did not listen")
                    time.sleep(0.1)
            driver = WebDriver(application)
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
            (output / "result.json").write_text(
                json.dumps({"host": host, "checks": checks}, indent=2) + "\n",
                encoding="utf-8",
            )
            print(json.dumps({"passed": checks}))
        except Exception:
            if driver:
                try:
                    driver.screenshot(output / "failure.png")
                except Exception:
                    pass
            raise
        finally:
            try:
                if driver:
                    driver.close()
            finally:
                source.write_bytes(original)
                server.terminate()
                try:
                    server.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait()


if __name__ == "__main__":
    main()
