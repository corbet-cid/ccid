#!/usr/bin/env python3
"""Small, provider-independent CI checks on an existing build worker."""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tarfile
import time
import tomllib


class Failure(Exception):
    pass


def event(name, **values):
    print(json.dumps({"event": name, **values}, sort_keys=True), flush=True)


def positive(value, name):
    if not re.fullmatch(r"[1-9][0-9]*", str(value)):
        raise Failure(f"{name} must be a positive integer")
    return int(value)


def cpu_budget():
    """Respect the worker CPU allocation; never assume all host cores are ours."""
    available = len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else os.cpu_count() or 1
    try:
        quota, period = Path("/sys/fs/cgroup/cpu.max").read_text().split()
        if quota != "max":
            available = min(available, max(1, math.floor(int(quota) / int(period))))
    except (OSError, ValueError):
        # An operator must explicitly allocate more than four CPUs when no
        # readable cgroup quota exists, e.g. a bare worker outside a container.
        available = min(available, 4)
    return max(1, available)


def budget(environment):
    jobs = positive(environment.get("CI_JOBS") or environment.get("CARGO_BUILD_JOBS") or cpu_budget(), "CI_JOBS")
    memory = environment.get("CI_MEMORY_MB") or None
    per_job = environment.get("CI_MEMORY_PER_JOB_MB") or None
    if memory is not None:
        memory = positive(memory, "CI_MEMORY_MB")
    if per_job is not None:
        per_job = positive(per_job, "CI_MEMORY_PER_JOB_MB")
        if memory is None or memory < per_job:
            raise Failure("CI_MEMORY_PER_JOB_MB requires an adequate explicit CI_MEMORY_MB allocation")
        jobs = min(jobs, memory // per_job)
    nix_jobs = positive(environment.get("CI_NIX_JOBS") or 1, "CI_NIX_JOBS")
    if nix_jobs > jobs:
        raise Failure("CI_NIX_JOBS exceeds the allocated CPU budget")
    return {"jobs": jobs,
            "test_threads": positive(environment.get("CI_TEST_THREADS") or jobs, "CI_TEST_THREADS"),
            "nix_jobs": nix_jobs, "nix_cores": max(1, jobs // nix_jobs),
            "memory_mb": memory,
            "timeout": positive(environment.get("CI_TIMEOUT") or 2700, "CI_TIMEOUT")}


class Runner:
    def __init__(self, root, environment, timeout):
        self.root, self.environment = root, environment
        self.deadline = time.monotonic() + timeout

    def run(self, argv, capture=False):
        if not isinstance(argv, list) or not argv or not all(isinstance(a, str) and a for a in argv):
            raise Failure("Commands must be nonempty arrays of nonempty strings")
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise Failure("Selected checks exceeded their total deadline")
        started = time.monotonic()
        process = subprocess.Popen(argv, cwd=self.root, env=self.environment,
                                   stdout=subprocess.PIPE if capture else None,
                                   text=True, start_new_session=True)
        try:
            output, _ = process.communicate(timeout=remaining)
        except (subprocess.TimeoutExpired, KeyboardInterrupt):
            previous = {sig: signal.signal(sig, signal.SIG_IGN) for sig in (signal.SIGTERM, signal.SIGINT)}
            try:
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    process.communicate(timeout=10)
                except subprocess.TimeoutExpired:
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    process.communicate()
                # The immediate child may exit while a descendant ignores TERM.
                # Clean the remaining owned group before releasing its cache lock.
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            finally:
                for sig, handler in previous.items():
                    signal.signal(sig, handler)
            raise Failure("Check interrupted or timed out; its process group was terminated") from None
        event("command", executable=Path(argv[0]).name,
              seconds=round(time.monotonic() - started, 3), exit_code=process.returncode)
        if process.returncode:
            raise Failure(f"{Path(argv[0]).name} failed with exit {process.returncode}")
        return (output or "").strip()


def verify_source(archive, digest, commit, destination):
    if not re.fullmatch(r"[0-9a-f]{64}", digest) or not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", commit):
        raise Failure("Source SHA-256 and Git commit must be complete lowercase identities")
    archive = Path(archive).resolve(strict=True)
    with archive.open("rb") as stream:
        if hashlib.file_digest(stream, "sha256").hexdigest() != digest:
            raise Failure("Source archive SHA-256 mismatch")
    with archive.open("rb") as stream:
        result = subprocess.run(["git", "get-tar-commit-id"], stdin=stream,
                                stdout=subprocess.PIPE, text=True, check=False)
    if result.returncode or result.stdout.strip() != commit:
        raise Failure("Source archive embedded commit mismatch")
    destination = Path(destination).resolve()
    if destination.exists() and any(destination.iterdir()):
        raise Failure("Source destination must be empty; existing work is never overwritten")
    with tarfile.open(archive, "r:") as stream:
        members = stream.getmembers()
        for member in members:
            if member.name.startswith("/") or ".." in Path(member.name).parts:
                raise Failure("Source archive contains an unsafe path")
            if member.isdev() or member.isfifo():
                raise Failure("Source archive contains a special file")
            if member.issym() or member.islnk():
                target = (destination / member.name).parent / member.linkname if member.issym() else destination / member.linkname
                if not target.resolve().is_relative_to(destination):
                    raise Failure("Source archive contains an escaping link")
        destination.mkdir(parents=True, exist_ok=True)
        # Python's data filter also rejects unsafe links and special members.
        stream.extractall(destination, members=members, filter="data")
    for member in members:
        path = destination / member.name
        if path.exists() and not path.is_symlink():
            os.utime(path, None)
    event("source", commit=commit, sha256=digest, files=len(members))


def mold_driver(environment):
    executable = shutil.which("mold", path=environment.get("PATH"))
    if not executable:
        raise Failure("CI_LINKER=mold requires an installed mold executable")
    # Nix's linker wrapper prepends flags, but mold requires -run first.
    origin = Path(executable).resolve().parent.parent / "nix-support/orig-bintools"
    return str(Path(origin.read_text().strip()) / "bin/mold") if origin.is_file() else executable


def cargo_check(config, runner):
    toolchain = config.get("toolchain", "system")
    cargo = ["cargo"] if toolchain == "system" else ["rustup", "run", toolchain, "cargo"]
    linker = runner.environment.get("CI_LINKER") or "system"
    if linker == "mold":
        driver = mold_driver(runner.environment)
        runner.run([driver, "--version"])
        cargo = [driver, "-run"] + cargo
    elif linker != "system":
        raise Failure("CI_LINKER must be system or mold")
    rustc = ["rustc"] if toolchain == "system" else ["rustup", "run", toolchain, "rustc"]
    runner.run(rustc + ["--version"])
    options = ["--locked"]
    if config.get("workspace"):
        options += ["--workspace"]
    if config.get("all_features"):
        options += ["--all-features"]
    for package in config.get("packages", []):
        options += ["--package", package]
    for package in config.get("exclude", []):
        options += ["--exclude", package]
    if config.get("features"):
        options += ["--features", ",".join(config["features"])]
    if config.get("release"):
        options += ["--release"]
    actions = config.get("actions", ["fmt", "test", "clippy"])
    if not actions:
        raise Failure("Cargo action selection is empty")
    for action in actions:
        if action == "fmt":
            runner.run(cargo + ["fmt", "--all", "--", "--check"])
        elif action == "test":
            test_options = options + (["--all-targets"] if config.get("all_targets") else [])
            test_runner = config.get("test_runner", "cargo")
            if test_runner == "cargo":
                runner.run(cargo + ["test"] + test_options)
            elif test_runner == "nextest":
                runner.run(cargo + ["nextest", "run", "--test-threads", runner.environment["RUST_TEST_THREADS"]] + options)
                # Nextest does not run Rust doctests. Preserve that coverage.
                runner.run(cargo + ["test", "--doc"] + options)
            else:
                raise Failure("Unknown Cargo test runner")
        elif action == "clippy":
            runner.run(cargo + ["clippy", "--all-targets"] + options + ["--", "-D", "warnings"])
        elif action in {"build", "check"}:
            runner.run(cargo + [action] + options + (["--all-targets"] if config.get("all_targets") else []))
        else:
            raise Failure(f"Unknown Cargo action: {action}")


def nix_check(config, runner):
    if getattr(runner, "nix_inventory", None) is None:
        system = runner.run(["nix", "eval", "--impure", "--raw", "--expr", "builtins.currentSystem"], capture=True)
        inventory = json.loads(runner.run(["nix", "eval", "--no-update-lock-file", "--json",
                                          f".#checks.{system}", "--apply", "builtins.attrNames"], capture=True))
        runner.nix_inventory = (system, inventory)
    system, inventory = runner.nix_inventory
    if not isinstance(inventory, list) or not inventory or not all(isinstance(x, str) for x in inventory):
        raise Failure("No native checks declared; refusing empty success")
    if "expected_checks" in config and sorted(inventory) != sorted(config["expected_checks"]):
        raise Failure("Declared native check inventory differs from expected_checks")
    mode = config.get("mode", "native")
    event("nix-inventory", system=system, checks=inventory, mode=mode)
    if mode == "list":
        return
    base = ["nix", "flake", "check", "--no-update-lock-file", "--keep-going", "--print-build-logs"]
    if mode == "native":
        runner.run(base)
    elif mode == "eval":
        runner.run(base + ["--all-systems", "--no-build"])
    elif mode == "all-systems":
        runner.run(base + ["--all-systems"])
    elif mode == "named":
        selected = config.get("checks", [])
        if not selected or any(name not in inventory for name in selected):
            raise Failure("Named Nix checks must be a nonempty subset of native checks")
        runner.run(["nix", "build", "--no-update-lock-file", "--no-link", "--keep-going", "--print-build-logs"] +
                   [f".#checks.{system}.{json.dumps(name)}" for name in selected])
    else:
        raise Failure(f"Unknown Nix mode: {mode}")


def javascript_check(config, runner):
    manager = config.get("manager", "npm")
    install = {"npm": ["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"],
               "bun": ["bun", "install", "--frozen-lockfile"],
               "pnpm": ["pnpm", "install", "--frozen-lockfile"]}
    if manager not in install:
        raise Failure("JavaScript manager must be npm, bun, or pnpm")
    scripts = config.get("scripts", ["test"])
    if not scripts:
        raise Failure("JavaScript script selection is empty")
    if config.get("install", True):
        runner.run(install[manager])
    for script in scripts:
        runner.run([manager, "run", script])


def run_checks(args):
    root = Path(args.repo).resolve(strict=True)
    manifest = root / args.manifest
    config = tomllib.loads(manifest.read_text())
    if config.get("schema") != 1 or not isinstance(config.get("project"), str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", config["project"]):
        raise Failure("Manifest requires schema=1 and a plain project slug")
    selected = list(dict.fromkeys(name for item in args.check for name in item.split(",") if name))
    checks = config.get("checks", {})
    if not selected or any(name not in checks for name in selected):
        raise Failure("Select one or more declared, nonempty check names")
    environment = os.environ.copy()
    resources = budget(environment)
    linker = environment.get("CI_LINKER") or "system"
    if linker not in {"system", "mold"}:
        raise Failure("CI_LINKER must be system or mold")
    cache_root = Path(environment.get("CI_CACHE_ROOT") or str(Path(environment.get("CARGO_HOME") or str(Path.home() / ".cargo")) / "ccid")).resolve()
    project = config["project"]
    cache = cache_root / project
    environment.update(CARGO_BUILD_JOBS=str(resources["jobs"]), RUST_TEST_THREADS=str(resources["test_threads"]))
    for variable, folder in {"CARGO_TARGET_DIR": "target", "UV_CACHE_DIR": "uv", "BUN_INSTALL_CACHE_DIR": "bun", "npm_config_cache": "npm"}.items():
        environment[variable] = str(cache / folder)
    environment["TMPDIR"] = str(cache / "tmp")
    environment["NIX_CONFIG"] = environment.get("NIX_CONFIG", "") + f"\nmax-jobs = {resources['nix_jobs']}\ncores = {resources['nix_cores']}\n"
    event("plan", project=project, checks=selected, budget=resources, linker_request=linker,
          repository=environment.get("CI_REPO"), source_commit=environment.get("CI_COMMIT_SHA"),
          manifest_sha256=hashlib.sha256(manifest.read_bytes()).hexdigest())
    if args.plan:
        return
    cache.mkdir(parents=True, exist_ok=True)
    (cache / "tmp").mkdir(exist_ok=True)
    with (cache_root / (project + ".lock")).open("a") as lock:
        lock_deadline = time.monotonic() + 60
        while True:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= lock_deadline:
                    raise Failure("Project cache is in use; no duplicate work started") from None
                time.sleep(0.1)
        runner = Runner(root, environment, resources["timeout"])
        for name in selected:
            check = checks[name]
            event("check-start", check=name)
            started = time.monotonic()
            kind = check.get("kind")
            if kind == "cargo":
                cargo_check(check, runner)
            elif kind == "nix":
                nix_check(check, runner)
            elif kind == "javascript":
                javascript_check(check, runner)
            elif kind == "commands":
                # Repository commands may generate or change Nix inputs.
                runner.nix_inventory = None
                commands = check.get("commands", [])
                if not commands:
                    raise Failure("Custom command selection is empty")
                for command in commands:
                    runner.run(command)
            else:
                raise Failure(f"Unknown check kind: {kind}")
            event("check-success", check=name, seconds=round(time.monotonic() - started, 3))


def main():
    def interrupted(_signal, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="action", required=True)
    source = subparsers.add_parser("verify-source")
    for name in ["archive", "sha256", "commit", "destination"]:
        source.add_argument("--" + name, required=True)
    check = subparsers.add_parser("check")
    check.add_argument("--repo", default=".")
    check.add_argument("--manifest", default=".ci/ccid.toml")
    check.add_argument("--check", action="append", required=True)
    check.add_argument("--plan", action="store_true")
    args = parser.parse_args()
    try:
        if args.action == "verify-source":
            verify_source(args.archive, args.sha256, args.commit, args.destination)
        else:
            run_checks(args)
    except (Failure, OSError, ValueError, KeyError) as error:
        print(f"ccid: {error}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print("ccid: interrupted before a command started", file=sys.stderr)
        return 130
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
