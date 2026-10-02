"""Nonbuilding Git/source fixtures for the one-time diagnostic selector."""
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("transition", Path(__file__).with_name("cqlt_main_lock.py"))
transition = importlib.util.module_from_spec(spec)
spec.loader.exec_module(transition)


class TransitionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "original"; self.repo.mkdir()
        self.env = {**transition.clean_environment(), "GIT_AUTHOR_NAME": "Fixture", "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
                    "GIT_COMMITTER_NAME": "Fixture", "GIT_COMMITTER_EMAIL": "fixture@example.invalid"}
        def git(*args):
            return subprocess.check_output(["git", "-C", str(self.repo), *args], env=self.env, text=True, stderr=subprocess.DEVNULL).strip()
        self.git = git
        git("init", "--initial-branch=main", "--quiet")
        (self.repo / "Cargo.toml").write_text("[package]\nname='fixture'\nversion='1.0.0'\n")
        git("add", "Cargo.toml"); git("commit", "--quiet", "-m", "Initial fixture")
        self.old = git("rev-parse", "HEAD")
        (self.repo / "README.md").write_text("fixture\n")
        git("add", "README.md"); git("commit", "--quiet", "-m", "Document fixture")
        self.main = git("rev-parse", "HEAD")
        bundle = self.root / "source.bundle"
        git("bundle", "create", str(bundle), "refs/heads/main")
        self.payload = {"cqlt.bundle": bundle.read_bytes()}
        self.receipt = {"schema": 1, "purpose": "ccid-cqlt-main-transition", "repository": transition.URL,
            "historical_commit": self.old, "main_commit": self.main, "main_observed_at": "2026-10-02T00:00:00+00:00",
            "commits": {self.main: git("rev-parse", self.main + "^{tree}"), self.old: git("rev-parse", self.old + "^{tree}")},
            "bundle_sha256": transition.digest(self.payload["cqlt.bundle"])}

    def mirror(self, receipt=None, payload=None):
        scratch = self.root / "mirror"; scratch.mkdir()
        return transition.mirror(payload or self.payload, receipt or self.receipt, scratch, historical=self.old)

    def test_genuine_main_and_historical_ancestry_roundtrip(self):
        mirrored = self.mirror()
        self.assertEqual(transition.git(mirrored, "rev-parse", "refs/heads/main"), self.main)
        self.assertEqual(transition.git(mirrored, "rev-parse", self.old + "^{commit}"), self.old)
        self.assertEqual(transition.git(mirrored, "for-each-ref", "--format=%(refname)"), "refs/heads/main")
        self.assertEqual(transition.git(mirrored, "symbolic-ref", "HEAD"), "refs/heads/main")
        self.assertEqual(transition.git(mirrored, "ls-remote", str(mirrored), "HEAD"), self.main + "\tHEAD")

    def test_foreign_repo_refuses_before_git_import(self):
        receipt = {**self.receipt, "repository": "https://github.com/example/foreign"}
        with self.assertRaisesRegex(ValueError, "foreign transition"):
            self.mirror(receipt=receipt)

    def test_tampered_bundle_and_unexpected_ref_refuse(self):
        with self.assertRaisesRegex(ValueError, "bundle digest"):
            self.mirror(payload={"cqlt.bundle": b"tampered"})
        (self.root / "mirror").rmdir()
        self.git("branch", "foreign")
        bundle = self.root / "other.bundle"; self.git("bundle", "create", str(bundle), "--branches")
        payload = {"cqlt.bundle": bundle.read_bytes()}
        receipt = {**self.receipt, "bundle_sha256": transition.digest(payload["cqlt.bundle"])}
        with self.assertRaisesRegex(ValueError, "unexpected bundle refs"):
            self.mirror(receipt=receipt, payload=payload)

    def test_archive_traversal_refuses_even_with_correct_digest(self):
        path = self.root / "bad.tar"
        with tarfile.open(path, "w") as archive:
            entry = tarfile.TarInfo("../escape"); entry.size = 1; archive.addfile(entry, io.BytesIO(b"x"))
        with self.assertRaisesRegex(ValueError, "unsafe archive"):
            transition.read_archive(path, transition.file_digest(path))

    def test_actual_command_path_runs_one_update_then_locked_metadata(self):
        tools = self.root / "tools"; tools.mkdir()
        log = self.root / "commands"
        script = tools / "cargo"
        script.write_text('''#!/bin/sh
set -eu
printf '%s\\n' "$*" >> "$COMMAND_LOG"
case "$1" in --config) [ "$2" = net.offline=false ]; [ "$3" = update ]; [ "$#" = 3 ] ;;
metadata) [ "$2" = --locked ]; [ "$3" = --format-version ]; [ "$4" = 1 ]; printf '{}';;
*) exit 66;; esac
''')
        script.chmod(0o755)
        env = transition.scoped_environment(self.repo)
        env.update(PATH=str(tools), COMMAND_LOG=str(log))
        self.assertEqual(transition.resolve(self.root, env), b"{}")
        self.assertEqual(log.read_text().splitlines(), ["--config net.offline=false update", "metadata --locked --format-version 1"])
        rewrites = [env["GIT_CONFIG_VALUE_" + str(i)] for i in range(int(env["GIT_CONFIG_COUNT"]))
                    if env["GIT_CONFIG_KEY_" + str(i)].endswith(".insteadOf")]
        self.assertEqual(rewrites, [transition.URL])


if __name__ == "__main__":
    unittest.main()
