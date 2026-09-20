import hashlib
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("ccid", Path(__file__).parents[1] / "ccid.py")
ccid = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ccid)


class BudgetTests(unittest.TestCase):
    def test_memory_allocation_limits_cpu_jobs(self):
        actual = ccid.budget({"CI_JOBS": "16", "CI_MEMORY_MB": "8192", "CI_MEMORY_PER_JOB_MB": "2048", "CI_NIX_JOBS": "2"})
        self.assertEqual((actual["jobs"], actual["nix_jobs"], actual["nix_cores"], actual["test_threads"]), (4, 2, 2, 4))

    def test_missing_memory_and_excess_nix_parallelism_fail(self):
        for environment in [{"CI_MEMORY_PER_JOB_MB": "512"}, {"CI_JOBS": "2", "CI_NIX_JOBS": "3"}, {"CI_JOBS": "0"}]:
            with self.subTest(environment=environment), self.assertRaises(ccid.Failure):
                ccid.budget(environment)

    def test_empty_provider_defaults_use_detected_allocation(self):
        with patch.object(ccid, "cpu_budget", return_value=6):
            result = ccid.budget({"CI_JOBS": "", "CI_TIMEOUT": "", "CI_MEMORY_MB": ""})
        self.assertEqual(result["jobs"], 6)
        self.assertEqual(result["nix_cores"], 6)

    def test_empty_pipeline_budget_preserves_worker_allocation(self):
        self.assertEqual(ccid.budget({"CI_JOBS": "", "CARGO_BUILD_JOBS": "8"})["jobs"], 8)


class RecipeTests(unittest.TestCase):
    class Recorder:
        environment = {"RUST_TEST_THREADS": "8"}

        def __init__(self, outputs=()):
            self.commands = []
            self.outputs = iter(outputs)

        def run(self, command, capture=False):
            self.commands.append(command)
            return next(self.outputs) if capture else ""

    def test_nextest_keeps_doctests_and_exact_toolchain(self):
        runner = self.Recorder()
        ccid.cargo_check({"actions": ["test"], "test_runner": "nextest", "toolchain": "1.94.0", "all_features": True}, runner)
        self.assertEqual(runner.commands[1], ["rustup", "run", "1.94.0", "cargo", "nextest", "run", "--test-threads", "8", "--locked", "--all-features"])
        self.assertEqual(runner.commands[2], ["rustup", "run", "1.94.0", "cargo", "test", "--doc", "--locked", "--all-features"])

    def test_empty_nix_checks_cannot_pass(self):
        with self.assertRaises(ccid.Failure):
            ccid.nix_check({"mode": "native"}, self.Recorder(["x86_64-linux", "[]"]))

    def test_unknown_named_check_never_builds(self):
        runner = self.Recorder(["x86_64-linux", '["small"]'])
        with self.assertRaises(ccid.Failure):
            ccid.nix_check({"mode": "named", "checks": ["large"]}, runner)
        self.assertEqual(len(runner.commands), 2)

    def test_mold_resolves_installed_nix_driver_without_wrapper_flags(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            wrapper = root / "wrapped/bin/mold"
            wrapper.parent.mkdir(parents=True)
            wrapper.write_text("#!/bin/sh\nexit 1\n")
            wrapper.chmod(0o755)
            origin = root / "wrapped/nix-support/orig-bintools"
            origin.parent.mkdir()
            origin.write_text(str(root / "unwrapped") + "\n")
            environment = {"PATH": str(wrapper.parent)}
            self.assertEqual(ccid.mold_driver(environment), str(root / "unwrapped/bin/mold"))
            origin.unlink()
            self.assertEqual(ccid.mold_driver(environment), str(wrapper))

    def test_missing_mold_fails_explicitly(self):
        with tempfile.TemporaryDirectory() as root, self.assertRaises(ccid.Failure):
            ccid.mold_driver({"PATH": root})


class SourceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        (self.repo / "source.txt").write_text("exact source\n")
        self.git("add", "source.txt")
        self.git("commit", "-qm", "fixture")
        self.commit = self.git("rev-parse", "HEAD")
        self.archive = self.root / "source.tar"
        self.git("archive", "--format=tar", "--output=" + str(self.archive), self.commit)
        self.digest = hashlib.sha256(self.archive.read_bytes()).hexdigest()

    def tearDown(self):
        self.temp.cleanup()

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.repo, text=True).strip()

    def test_exact_source_and_reject_existing_work(self):
        destination = self.root / "output"
        ccid.verify_source(self.archive, self.digest, self.commit, destination)
        self.assertEqual((destination / "source.txt").read_text(), "exact source\n")
        with self.assertRaises(ccid.Failure):
            ccid.verify_source(self.archive, self.digest, self.commit, destination)

    def test_digest_and_commit_rejected_before_extraction(self):
        destination = self.root / "output"
        for digest, commit in [("0" * 64, self.commit), (self.digest, "0" * 40)]:
            with self.assertRaises(ccid.Failure):
                ccid.verify_source(self.archive, digest, commit, destination)
            self.assertFalse(destination.exists())

    def test_escaping_symlink_rejected_before_extraction(self):
        (self.repo / "escape").symlink_to("../../outside")
        self.git("add", "escape")
        self.git("commit", "-qm", "unsafe fixture")
        commit = self.git("rev-parse", "HEAD")
        self.git("archive", "--format=tar", "--output=" + str(self.archive), commit)
        digest = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        with self.assertRaises(ccid.Failure):
            ccid.verify_source(self.archive, digest, commit, self.root / "output")
        self.assertFalse((self.root / "output").exists())


class ProcessTests(unittest.TestCase):
    def test_deadline_stops_a_command(self):
        with tempfile.TemporaryDirectory() as root:
            runner = ccid.Runner(Path(root), os.environ.copy(), 0.05)
            with self.assertRaises(ccid.Failure):
                runner.run([sys.executable, "-c", "import time; time.sleep(60)"])

    def test_timeout_stops_descendant_even_when_parent_exits_on_term(self):
        with tempfile.TemporaryDirectory() as root:
            pid_file = Path(root) / "child.pid"
            child = "import os,signal,time; from pathlib import Path; signal.signal(signal.SIGTERM,signal.SIG_IGN); Path('child.pid').write_text(str(os.getpid())); time.sleep(60)"
            parent = "import subprocess,sys,time; subprocess.Popen([sys.executable,'-c'," + repr(child) + "]); time.sleep(60)"
            runner = ccid.Runner(Path(root), os.environ.copy(), 0.3)
            with self.assertRaises(ccid.Failure):
                runner.run([sys.executable, "-c", parent])
            self.assertTrue(pid_file.exists())
            pid = int(pid_file.read_text())
            for _ in range(100):
                status = Path(f"/proc/{pid}/stat")
                if not status.exists() or status.read_text().split()[2] == "Z":
                    break
                time.sleep(0.01)
            else:
                self.fail("Owned descendant survived the command deadline")


if __name__ == "__main__":
    unittest.main()
