#!/usr/bin/env python3
"""Build ccid once using its existing Python process/budget primitives only.

The Crow loader has already verified and privately staged this source. Do not
invoke legacy run_checks: that entry point retains its old pinned cache policy.
"""
import hashlib
import os
import re
import tarfile
from pathlib import Path
import signal
import sys

import ccid


def interrupted(_signal, _frame):
    raise KeyboardInterrupt


def run_build():
    signal.signal(signal.SIGTERM, interrupted)
    environment = dict(os.environ)
    reserve = environment.get("CI_MIN_AVAILABLE_MB")
    if reserve:
        reserve = ccid.positive(reserve, "CI_MIN_AVAILABLE_MB")
        available = next(int(line.split()[1]) // 1024 for line in
                         Path("/proc/meminfo").read_text().splitlines()
                         if line.startswith("MemAvailable:"))
        group = Path("/sys/fs/cgroup")
        current_group = next((line[3:] for line in Path("/proc/self/cgroup").read_text().splitlines()
                              if line.startswith("0::")), "/")
        candidate = group / current_group.lstrip("/")
        if (candidate / "memory.max").is_file():
            group = candidate
        if (group / "memory.max").is_file():
            maximum = (group / "memory.max").read_text().strip()
            if maximum != "max":
                current = int((group / "memory.current").read_text())
                available = min(available, max(0, int(maximum) - current) // (1024 * 1024))
        if available <= reserve:
            raise ccid.Failure("Insufficient memory headroom for ccid bootstrap")
        environment["CI_MEMORY_MB"] = str(min(
            ccid.positive(environment.get("CI_MEMORY_MB") or available - reserve, "CI_MEMORY_MB"),
            available - reserve))
        ccid.event("memory-admission", available_mb=available, reserve_mb=reserve,
                   allocation_mb=int(environment["CI_MEMORY_MB"]))
    resources = ccid.budget(environment)
    environment["CARGO_BUILD_JOBS"] = str(resources["jobs"])
    environment["RUST_TEST_THREADS"] = str(resources["test_threads"])
    ccid.event("bootstrap-budget", **resources)
    runner = ccid.Runner(Path.cwd(), environment, resources["timeout"])
    runner.run(["bash", ".ci/build.sh", environment.get("MODE") or "build"])


def main():
    archive = os.environ.get("DEPENDENCY_SOURCE_ARCHIVE", "")
    digest = os.environ.get("DEPENDENCY_SOURCE_SHA256", "")
    if not archive and not digest:
        return run_build()
    if not archive or not re.fullmatch("[0-9a-f]{64}", digest):
        raise ccid.Failure("Dependency source archive and digest are required together")
    with open(archive, "rb") as stream:
        if hashlib.file_digest(stream, "sha256").hexdigest() != digest:
            raise ccid.Failure("Dependency source archive digest mismatch")
        stream.seek(0)
        with tarfile.open(fileobj=stream, mode="r:") as bundle:
            members = [member for member in bundle if member.name == "closure.py"]
            if len(members) != 1 or not members[0].isfile() or members[0].size > 1024 * 1024:
                raise ccid.Failure("Invalid dependency source consumer")
            program = bundle.extractfile(members[0]).read()
    if hashlib.sha256(program).hexdigest() != "293450018834cd60ff58e0c62d78de8cfcf21c060fb4de5d905200afd0bd84da":
        raise ccid.Failure("Unreviewed dependency source consumer")
    namespace = {"__name__": "verified_dependency_source"}
    exec(compile(program, "verified-dependency-source", "exec"), namespace)
    # The source loader already verified this checkout; the unchanged consumer
    # independently binds its original archive/locks and supplies process-local Git.
    status = namespace["consume"](
        archive, digest, os.environ["SOURCE_ARCHIVE"], os.environ["SOURCE_SHA256"],
        os.environ["CI_COMMIT_SHA"], os.environ["CI_REPOSITORY_URL"],
        os.environ.get("TMPDIR") or "/tmp",
        [sys.executable, "-c", "import runpy; runpy.run_path('.ci/bootstrap.py')['run_build']()"],
    )
    raise SystemExit(status)


if __name__ == "__main__":
    try:
        main()
    except (ccid.Failure, OSError, ValueError, KeyboardInterrupt) as error:
        print(f"ccid bootstrap: {error}", file=sys.stderr)
        raise SystemExit(2) from None
