#!/usr/bin/env python3
"""Build Linux Rust library objects with a captured, identity-checked foundation.

Requires Python 3.9+, rustup/Cargo and the x86_64-unknown-linux-gnu host.
The producer, Cargo configuration, build scripts and native code must be trusted.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

TARGET = "x86_64-unknown-linux-gnu"
FLAGS = "-C prefer-dynamic=yes -C relocation-model=pic -C force-unwind-tables=yes -C panic=unwind -C lto=off"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def publish(path, data, unique=False):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
        if unique:
            os.link(temporary, path)
        else:
            os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def immutable(path, data):
    try:
        publish(path, data, unique=True)
    except FileExistsError:
        pass
    require(path.read_bytes() == data, f"Immutable artifact cache differs: {path}")
    return {"path": str(path), "sha256": digest(data), "bytes": len(data)}


def arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("ManifestPath", "Package", "TargetDirectory", "SupportDll", "Output"):
        aliases = ["-" + name]
        if name == "SupportDll":
            aliases += ["-SupportLibrary"]
        parser.add_argument(*aliases, required=True)
    parser.add_argument("-Toolchain", default="stable")
    parser.add_argument("-Features", default="")
    parser.add_argument("-Optimization", type=int, choices=range(4), default=0)
    parser.add_argument("-DebugInfo", choices=("0", "1", "2", "line-tables-only"), default="0")
    for name in ("ReceiptPath", "CacheDirectory", "SessionId", "FoundationBuildKey", "FoundationManifest"):
        parser.add_argument("-" + name, default="")
    for name in ("Revision", "JobId"):
        parser.add_argument("-" + name, type=int, default=0)
    parser.add_argument("-MaxCacheBytes", type=int, default=4 * 1024**3)
    for name in ("Offline", "NoDefaultFeatures"):
        parser.add_argument("-" + name, action="store_true")
    return parser.parse_args()


class Builder:
    def __init__(self, args):
        self.a = args
        self.env = os.environ.copy()
        self.cwd = Path(args.ManifestPath).resolve(strict=True).parent
        self.cache = Path(args.CacheDirectory or Path(args.Output).parent).resolve()
        self.status = "BuildFailed"
        self.started = time.monotonic()
        self.lock_ms = 0.0

    def run(self, command, records=False):
        result = subprocess.run(command, cwd=self.cwd, env=self.env, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True)
        if result.stderr:
            print(result.stderr, end="", file=sys.stderr)
        parsed = []
        if records:
            for line in result.stdout.splitlines():
                item = json.loads(line)
                if item.get("reason") == "compiler-message":
                    print(item["message"].get("rendered", ""), end="", file=sys.stderr)
                parsed.append(item)
        require(result.returncode == 0, f"Command failed ({result.returncode}): {command[0:3]}")
        return parsed if records else result.stdout.strip()

    def capacity(self, additional=0):
        used = sum(p.stat().st_size for p in self.cache.rglob("*") if p.is_file())
        if used + additional >= self.a.MaxCacheBytes:
            self.status = "CacheFull"
            raise RuntimeError("Layered cache capacity reached; no automatic deletion is performed")

    def receipt(self, **fields):
        if self.a.ReceiptPath:
            data = {"version": 1, "status": self.status, "session": self.a.SessionId,
                    "revision": self.a.Revision, "job": self.a.JobId,
                    "foundation_build_key": self.a.FoundationBuildKey,
                    "build_ms": (time.monotonic() - self.started) * 1000,
                    "lock_wait_ms": self.lock_ms, **fields}
            publish(Path(self.a.ReceiptPath).resolve(), json.dumps(data, indent=2).encode(), unique=True)

    def build(self):
        a = self.a
        target = Path(a.TargetDirectory).resolve()
        target.mkdir(parents=True, exist_ok=True)
        self.cache.mkdir(parents=True, exist_ok=True)
        with (target / ".reknot-layered-writer.lock").open("a+b") as lock:
            start = time.monotonic()
            while True:
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    break
                except BlockingIOError:
                    require(time.monotonic() - start < 120, "Timed out waiting for Cargo writer lock")
                    time.sleep(0.05)
            self.lock_ms = (time.monotonic() - start) * 1000
            self.build_locked(target)

    def build_locked(self, target):
        a = self.a
        output = Path(a.Output).resolve()
        manifest_path = Path(str(output) + ".json")
        if a.ReceiptPath:
            require(not output.exists() and not manifest_path.exists(), "Development job output must be unique")
        output.parent.mkdir(parents=True, exist_ok=True)
        self.capacity()
        require(not self.env.get("RUSTC_WRAPPER") and not self.env.get("RUSTC_WORKSPACE_WRAPPER"),
                "Compiler wrappers need their own provenance contract")
        compiler = self.run(["rustup", "which", "--toolchain", a.Toolchain, "rustc"])
        info = self.run([compiler, "-vV"])
        require(f"host: {TARGET}" in info, f"Builder requires a {TARGET} compiler")
        libdir = Path(self.run([compiler, "--print", "target-libdir", "--target", TARGET]))
        stds = list(libdir.glob("libstd-*.so"))
        require(len(stds) == 1, "Expected one matching Rust std library")
        std = stds[0]
        std_bytes = std.read_bytes()
        self.env.update(RUSTC=compiler, RUSTFLAGS=FLAGS, CARGO_INCREMENTAL="0")
        self.env.pop("CARGO_ENCODED_RUSTFLAGS", None)
        base = ["--locked", "--manifest-path", str(Path(a.ManifestPath).resolve())]
        if a.Offline:
            base += ["--offline"]
        feature_args = (["--no-default-features"] if a.NoDefaultFeatures else [])
        features = [f for f in a.Features.split(",") if f]
        metadata_args = feature_args + (["--features", ",".join(f if "/" in f else a.Package + "/" + f for f in features)] if features else [])
        cargo = ["cargo", "+" + a.Toolchain]
        metadata = json.loads(self.run(cargo + ["metadata"] + base +
                                      ["--format-version", "1", "--filter-platform", TARGET] + metadata_args))
        support_path = Path(a.SupportDll).resolve()
        support_name = support_path.name.removeprefix("lib").removesuffix(".so")
        logic_packages = [p for p in metadata["packages"] if p["name"] == a.Package]
        support_packages = [p for p in metadata["packages"] if any(t["name"] == support_name and
                            "dylib" in t["crate_types"] for t in p["targets"])]
        require(len(logic_packages) == len(support_packages) == 1, "Select one logic and one support dylib package")
        logic_id, support_id = logic_packages[0]["id"], support_packages[0]["id"]
        nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
        pending, seen = [logic_id], set()
        while pending:
            node = pending.pop()
            if node not in seen:
                seen.add(node)
                pending.extend(d["pkg"] for d in nodes[node]["deps"])
        require(support_id != logic_id and support_id in seen, "Support must be a dependency of logic")
        lock = Path(metadata["workspace_root"]) / "Cargo.lock"
        lock_bytes = lock.read_bytes()
        build_base = base + ["--target", TARGET, "--target-dir", str(target), "--message-format", "json-render-diagnostics"]

        def support_artifact(records):
            matches = [r for r in records if r.get("reason") == "compiler-artifact" and
                       r["package_id"] == support_id and "dylib" in r["target"]["crate_types"]]
            require(len(matches) == 1, "Expected one Cargo support dylib artifact")
            artifact = matches[0]
            libraries = [Path(p).resolve() for p in artifact["filenames"] if p.endswith(".so")]
            require(libraries == [support_path], "SupportLibrary must be the actual Cargo output")
            return artifact

        prior_foundation = None
        if a.FoundationManifest:
            prior = json.loads(Path(a.FoundationManifest).read_text())
            prior_foundation = prior["foundation"]
            require(prior["schema"] == 3 and prior["profile"] == "layered-rust-library-object" and
                    prior_foundation["target"] == TARGET and prior_foundation["rustc"].strip() == info and
                    prior_foundation["rustflags"] == FLAGS and
                    prior_foundation["std_library"]["sha256"] == digest(std_bytes) and
                    prior_foundation["std_library"]["bytes"] == len(std_bytes) and
                    prior_foundation["cargo_lock_sha256"] == digest(lock_bytes) and
                    prior_foundation["support_build"]["package_id"] == support_id, "Foundation reuse identity differs")
        fd, staging = tempfile.mkstemp(suffix=".o", prefix="logic-", dir=output.parent)
        os.close(fd)
        try:
            records = self.run(cargo + ["rustc"] + build_base +
                               ["--package", logic_id, "--lib", "--crate-type", "lib"] + feature_args +
                               (["--features", ",".join(features)] if features else []) +
                               ["--", "-C", "codegen-units=1", "-C", "lto=off", "-C", "panic=unwind",
                                "-C", f"opt-level={a.Optimization}", "-C", f"debuginfo={a.DebugInfo}", f"--emit=obj={staging}"], records=True)
            # Capture the dependency configuration Cargo actually used for logic.
            support_build = self.build_profile(support_artifact(records), support_id)
            support_bytes = support_path.read_bytes()
            if prior_foundation is not None:
                expected = prior_foundation["support_library"]
                require(support_build == prior_foundation["support_build"] and
                        digest(support_bytes) == expected["sha256"] and len(support_bytes) == expected["bytes"],
                        "Logic support build differs from the requested foundation")
            require(support_path.read_bytes() == support_bytes and std.read_bytes() == std_bytes and
                    lock.read_bytes() == lock_bytes and self.run([compiler, "-vV"]) == info,
                    "Foundation changed during logic build; no manifest published")
            obj = Path(staging).read_bytes()
            require(obj.startswith(b"\x7fELF") and len(obj) <= 64 * 1024**2, "Invalid or oversized ELF object")
            self.capacity(len(std_bytes) + len(support_bytes) + 2 * len(obj))
            key = digest((digest(std_bytes) + "\n" + digest(support_bytes) + "\n" + info).encode())
            directory = self.cache / "foundation" / key
            foundation = {"rustc": info, "target": TARGET, "handler_abi": 5, "panic": "unwind",
                          "tls": "generation-local", "prefer_dynamic": True, "rustflags": FLAGS,
                          "cargo_lock_sha256": digest(lock_bytes), "support_build": support_build,
                          "std_library": immutable(directory / std.name, std_bytes),
                          "support_library": immutable(directory / support_path.name, support_bytes)}
            record = {"schema": 3, "profile": "layered-rust-library-object", "foundation": foundation,
                      "logic": {"artifact": immutable(self.cache / "objects" / digest(obj) / "logic.o", obj),
                                "crate_type": "lib", "codegen_units": 1, "lto": "off",
                                "opt_level": a.Optimization, "debuginfo": a.DebugInfo}}
            manifest = json.dumps(record, indent=2).encode()
            publish(output, obj, unique=bool(a.ReceiptPath))
            publish(manifest_path, manifest, unique=bool(a.ReceiptPath))
            self.status = "Built"
            self.receipt(manifest=str(manifest_path), manifest_sha256=digest(manifest),
                         object_sha256=digest(obj), reused=prior_foundation is not None)
            print(manifest.decode())
        finally:
            Path(staging).unlink(missing_ok=True)

    @staticmethod
    def build_profile(artifact, package_id):
        profile = artifact["profile"]
        require(not profile["test"], "Test profiles are unsupported")
        return {"package_id": package_id, "features": sorted(artifact["features"]),
                **{k: profile[k] for k in ("opt_level", "debuginfo", "debug_assertions", "overflow_checks")}}


if __name__ == "__main__":
    builder = Builder(arguments())
    try:
        builder.build()
    except Exception as error:
        builder.receipt(message=str(error))
        print(str(error), file=sys.stderr)
        sys.exit(1)
