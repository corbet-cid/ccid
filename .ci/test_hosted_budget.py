#!/usr/bin/env python3
"""Exercise the reusable workflow's real resource guards without hosted work."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest


WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/reusable-check.yml"


def step(name):
    lines = WORKFLOW.read_text().splitlines()
    start = lines.index("      - name: " + name)
    end = next((i for i in range(start + 1, len(lines)) if lines[i].startswith("      - name:")), len(lines))
    block = lines[start:end]
    command = block.index("        run: |") + 1
    return textwrap.dedent("\n".join(block[command:]))


class HostedBudgetTests(unittest.TestCase):
    def environment(self, **overrides):
        return {**os.environ, "CI_JOBS": "1", "CI_TEST_THREADS": "1",
                "CI_MIN_AVAILABLE_MB": "8192", "CI_TIMEOUT": "900", **overrides}

    def validate(self, **overrides):
        return subprocess.run(["bash", "-c", step("Validate resource allocation")],
                              env=self.environment(**overrides), capture_output=True, text=True)

    def test_one_job_and_original_defaults_are_admitted(self):
        self.assertEqual(self.validate().returncode, 0)
        self.assertEqual(self.validate(CI_JOBS="4", CI_TEST_THREADS="4",
                                       CI_MIN_AVAILABLE_MB="4096", CI_TIMEOUT="1800").returncode, 0)

    def test_invalid_resource_never_reaches_source_or_commands(self):
        for variable in ("CI_JOBS", "CI_TEST_THREADS", "CI_MIN_AVAILABLE_MB", "CI_TIMEOUT"):
            for value in ("", "0", "-1", "1.5", "1; exit 0", " 1"):
                with self.subTest(variable=variable, value=value):
                    self.assertNotEqual(self.validate(**{variable: value}).returncode, 0)

    def receipt(self, directory, allocation, exit_code="0"):
        if allocation is not None:
            (directory / "check.log").write_text(json.dumps({"event": "allocation", "budget": allocation}) + "\n")
        body = step("Write the result receipt").split("<<'PY'\n", 1)[1].rsplit("\nPY", 1)[0]
        args = [str(directory), "a" * 40, "catalog,governor", "request", "b" * 64,
                "c" * 40, "12", "34", "d" * 64, "e" * 64, "f" * 64, "1" * 64,
                "2" * 64, exit_code, "0", "rustc fixture", "node fixture", ""]
        return subprocess.run([sys.executable, "-c", body, *args], env=self.environment(),
                              capture_output=True, text=True)

    def test_success_records_actual_one_job_budget_and_runtime(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            budget = {"jobs": 1, "test_threads": 1, "timeout": 900, "memory_mb": 4096}
            self.assertEqual(self.receipt(root, budget).returncode, 0)
            receipt = json.loads((root / "result.json").read_text())
            self.assertEqual(receipt["budget"], budget)
            self.assertEqual(receipt["minimum_available_mb"], 8192)
            self.assertEqual(receipt["python"], sys.version)

    def test_success_with_missing_or_incompatible_budget_is_rejected(self):
        for budget in (None, {"jobs": 4, "test_threads": 1, "timeout": 900},
                       {"jobs": 1, "test_threads": 4, "timeout": 900},
                       {"jobs": 1, "test_threads": 1, "timeout": 1800}):
            with self.subTest(budget=budget), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                self.assertNotEqual(self.receipt(root, budget).returncode, 0)
                self.assertEqual((root / "status").read_text(), "125\n")
                self.assertFalse((root / "result.json").exists())

    def test_failure_without_allocation_remains_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.assertEqual(self.receipt(root, None, "2").returncode, 0)
            receipt = json.loads((root / "result.json").read_text())
            self.assertEqual(receipt["exit_code"], 2)
            self.assertIsNone(receipt["budget"])

    def contract(self, checks, *, jobs=1):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / ".ci").mkdir()
            (root / "Cargo.lock").write_bytes(b"locked fixture\n")
            (root / ".ci/providers.toml").write_text(
                '[github]\nchecks = ["catalog", "governor"]\n'
                '[github.resources]\njobs = 1\ntest_threads = 1\n'
                'minimum_available_mb = 8192\ntimeout = 900\n'
                '[dependencies]\nfiles = ["Cargo.lock"]\n')
            snapshot = hashlib.sha256(json.dumps(
                {"Cargo.lock": hashlib.sha256(b"locked fixture\n").hexdigest()},
                sort_keys=True, separators=(",", ":")).encode()).hexdigest()
            body = step("Fetch and guard the exact caller source").split("<<'PY'\n", 1)[1].rsplit("\nPY", 1)[0]
            return subprocess.run([sys.executable, "-c", body, str(root), snapshot],
                                  env=self.environment(CHECKS=checks, CI_JOBS=str(jobs)),
                                  capture_output=True, text=True)

    def test_public_contract_admits_only_reviewed_selection_and_exact_budget(self):
        self.assertEqual(self.contract("catalog,governor").returncode, 0)
        for selected in ("private-probe", "rust", "catalog,catalog", "catalog,", ""):
            with self.subTest(selected=selected):
                self.assertNotEqual(self.contract(selected).returncode, 0)
        self.assertNotEqual(self.contract("catalog", jobs=4).returncode, 0)


if __name__ == "__main__":
    lint = "--lint" in sys.argv
    if lint:
        sys.argv.remove("--lint")
    result = unittest.main(exit=False)
    if not result.result.wasSuccessful():
        raise SystemExit(1)
    if lint:
        def existing_tool(name):
            found = shutil.which(name)
            if not found:
                candidates = sorted(Path("/nix/store").glob("*-" + name + "-*/bin/" + name))
                found = str(candidates[-1]) if candidates else None
            if not found:
                raise SystemExit(name + " must already be provisioned; no installation is performed")
            return found
        subprocess.run([existing_tool("actionlint"), "-shellcheck", existing_tool("shellcheck"),
                        str(WORKFLOW), str(WORKFLOW.parents[2] / "adapters/github.yml")], check=True)
