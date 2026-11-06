#!/usr/bin/env python3
"""Standard-library tests of adapters/remote_ci.py with a fake provider API and a fake git copy."""
from __future__ import annotations

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("remote_ci", ROOT / "adapters/remote_ci.py")
rc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rc)

A, B, C = "a" * 40, "b" * 40, "c" * 40
TOKEN = "TOKEN-must-never-leak-1234"
PLAN = {"scheduler": "remote", "provider": "gitlab", "platform": "windows-x64", "workflow": "ccid-remote",
        "checks": ["test"], "push_branches": ["main"], "timeout_minutes": 60}


class FakeGit:
    def __init__(self, head=A, plans=None, changed=None):
        self.heads = {"main": head}
        self.plans = {"windows": dict(PLAN)} if plans is None else plans
        self.files = changed or ["src/lib.rs"]
        self.tag_list = {}

    def tags(self, repo, pattern):
        return dict(self.tag_list)

    def head(self, repo, branch):
        return self.heads.get(branch)

    def jobs(self, repo, sha):
        return self.plans

    def contains(self, repo, ancestor, sha):
        return True

    def changed(self, repo, base, sha):
        return self.files


class FakeHttp:
    """Answers GitLab and Forgejo calls; records every call without its headers' values."""

    def __init__(self):
        self.calls, self.statuses = [], []
        self.mirror_head, self.pipeline_sha, self.pipeline_status = A, A, "running"
        self.dispatch_status = 201
        self.tag_head = C
        self.project = {"builds_access_level": "enabled", "shared_runners_enabled": True}

    def __call__(self, method, url, headers, body=None, pace=True):
        assert TOKEN in "".join(headers.values()), "credential must travel in a header"
        self.calls.append((method, url, body))
        if "/statuses/" in url:
            self.statuses.append(body)
            return 201, {}
        if url.endswith("/projects/o%2Fr") and method == "GET":
            return 200, self.project
        if url.endswith("/projects/o%2Fr") and method == "PUT":
            self.project = {**self.project, **body}
            return 200, self.project
        if "/repository/branches/" in url:
            return 200, {"commit": {"id": self.mirror_head}}
        if "/repository/tags/" in url:
            return 200, {"commit": {"id": self.tag_head}}
        if url.endswith("/pipeline") and method == "POST":
            return self.dispatch_status, {"id": 7, "sha": self.pipeline_sha, "web_url": "https://gitlab.example/p/7",
                                          "message": "bad request"}
        if "/pipelines/7/cancel" in url:
            return 200, {}
        if "/pipelines/7" in url:
            return 200, {"status": self.pipeline_status}
        raise AssertionError(f"unexpected call {method} {url}")


def broker(state, http, git, **extra):
    config = {"forgejo": {"api": "https://forge.example/api/v1", "token_env": "T_FORGE"},
              "git_copy": "/nonexistent/{repo}.git", "state_dir": str(state),
              "providers": {"gitlab": {"api": "https://gitlab.example/api/v4", "token_env": "T_GITLAB"}},
              "repos": {"o/r": {"gitlab": "o/r"}}, "platforms": {"windows": True, "linux": True}, **extra}
    return rc.Broker(config, http=http, git=git)


class RemoteCiTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.state = Path(self.directory.name)
        self.env = mock.patch.dict("os.environ", {"T_FORGE": TOKEN, "T_GITLAB": TOKEN})
        self.env.start()
        self.stderr = io.StringIO()
        self.redirect = contextlib.redirect_stderr(self.stderr)
        self.redirect.__enter__()

    def tearDown(self):
        self.redirect.__exit__(None, None, None)
        self.env.stop()
        self.directory.cleanup()

    def record(self):
        return json.loads((self.state / "o__r__windows.json").read_text())

    def test_dispatch_poll_and_report(self):
        http, git = FakeHttp(), FakeGit()
        b = broker(self.state, http, git)
        self.assertEqual(b.sweep(), 0)
        self.assertEqual([s["state"] for s in http.statuses], ["pending", "pending"])
        self.assertEqual(http.statuses[1]["target_url"], "https://gitlab.example/p/7")
        self.assertEqual(http.statuses[0]["context"], "remote / windows")
        post = [c for c in http.calls if c[1].endswith("/pipeline")][0]
        variables = {v["key"]: v["value"] for v in post[2]["variables"]}
        self.assertEqual((post[2]["ref"], variables["CCID_EXPECTED_SHA"], variables["CCID_REMOTE_JOB"]), ("main", A, "windows"))
        b.sweep()  # still running: nothing new
        self.assertEqual(len(http.statuses), 2)
        http.pipeline_status = "success"
        b.sweep()
        self.assertEqual(http.statuses[-1]["state"], "success")
        self.assertIn("gitlab windows-x64: passed", http.statuses[-1]["description"])
        self.assertEqual(self.record()["last_result"]["state"], "success")
        self.assertNotIn("inflight", self.record())
        b.sweep()  # same head: never dispatched twice
        self.assertEqual(sum(1 for c in http.calls if c[1].endswith("/pipeline")), 1)

    def test_failed_pipeline_is_a_failure(self):
        http = FakeHttp()
        b = broker(self.state, http, FakeGit())
        b.sweep()
        http.pipeline_status = "failed"
        b.sweep()
        self.assertEqual(http.statuses[-1]["state"], "failure")

    def test_mirror_lag_waits_without_status(self):
        http = FakeHttp()
        http.mirror_head = B
        b = broker(self.state, http, FakeGit())
        b.sweep()
        self.assertEqual(http.statuses, [])
        self.assertFalse(any(c[1].endswith("/pipeline") for c in http.calls))
        http.mirror_head = A
        b.sweep()
        self.assertTrue(any(c[1].endswith("/pipeline") for c in http.calls))

    def test_interval_and_paths_pace_dispatches(self):
        http, git = FakeHttp(), FakeGit()
        b = broker(self.state, http, git)
        b.sweep()
        http.pipeline_status = "success"
        b.sweep()
        git.heads["main"] = http.mirror_head = http.pipeline_sha = B
        b.sweep()  # inside the interval
        self.assertEqual(sum(1 for c in http.calls if c[1].endswith("/pipeline")), 1)
        record = self.record()
        record["last_dispatched"]["at"] -= 25 * 3600
        (self.state / "o__r__windows.json").write_text(json.dumps(record))
        git.plans["windows"]["paths"] = ["docs/**"]
        b.sweep()  # interval over, but no declared path changed
        self.assertEqual(sum(1 for c in http.calls if c[1].endswith("/pipeline")), 1)
        git.files = ["docs/rendering.md"]
        b.sweep()
        self.assertEqual(sum(1 for c in http.calls if c[1].endswith("/pipeline")), 2)

    def test_timeout_cancels_and_reports_error(self):
        http = FakeHttp()
        b = broker(self.state, http, FakeGit())
        b.sweep()
        record = self.record()
        record["inflight"]["deadline"] = 0
        (self.state / "o__r__windows.json").write_text(json.dumps(record))
        b.sweep()
        self.assertTrue(any("/cancel" in c[1] for c in http.calls))
        self.assertEqual(http.statuses[-1]["state"], "error")
        self.assertIn("timed out", http.statuses[-1]["description"])

    def test_moved_mirror_cancels_and_retries(self):
        http = FakeHttp()
        http.pipeline_sha = C
        b = broker(self.state, http, FakeGit())
        b.sweep()
        self.assertTrue(any("/cancel" in c[1] for c in http.calls))
        self.assertNotIn("inflight", self.record())
        self.assertNotIn("last_dispatched", self.record())

    def test_refused_dispatch_reports_error_once(self):
        http = FakeHttp()
        http.dispatch_status = 400
        b = broker(self.state, http, FakeGit())
        self.assertEqual(b.sweep(), 1)
        self.assertEqual(http.statuses[-1]["state"], "error")
        b.sweep()
        self.assertEqual(sum(1 for c in http.calls if c[1].endswith("/pipeline")), 1)

    def test_ci_switched_on_when_off(self):
        http = FakeHttp()
        http.project = {"builds_access_level": "disabled", "shared_runners_enabled": False,
                        "ci_pipeline_variables_minimum_override_role": "no_one_allowed"}
        b = broker(self.state, http, FakeGit())
        b.sweep()
        put = [c for c in http.calls if c[0] == "PUT"]
        self.assertEqual(put[0][2], {"builds_access_level": "enabled", "shared_runners_enabled": True,
                                     "ci_pipeline_variables_minimum_override_role": "maintainer"})
        http.project["builds_access_level"] = "enabled"
        b.provider("gitlab").ensure("o/r")  # only what differs, and nothing when nothing does
        self.assertEqual(len([c for c in http.calls if c[0] == "PUT"]), 1)

    def test_undeclared_provider_and_non_remote_jobs_are_ignored(self):
        http = FakeHttp()
        plans = {"mac": dict(PLAN, provider="github")}
        broker(self.state, http, FakeGit(plans=plans)).sweep()
        self.assertEqual(http.calls, [])

    def test_release_tags_baseline_then_one_run_per_new_tag(self):
        http, git = FakeHttp(), FakeGit()
        git.tag_list = {"v0.9.0": B}
        b = broker(self.state, http, git)
        b.sweep()  # first sight: v0.9.0 is the baseline; the branch head runs as usual
        posts = lambda: [c for c in http.calls if c[1].endswith("/pipeline")]
        self.assertEqual([c[2]["ref"] for c in posts()], ["main"])
        http.pipeline_status = "success"
        b.sweep()
        git.tag_list["v1.0.0"] = C  # a new release tag: runs at once, inside the interval
        http.pipeline_sha = C
        http.pipeline_status = "running"
        b.sweep()
        self.assertEqual([c[2]["ref"] for c in posts()], ["main", "v1.0.0"])
        self.assertTrue(any("/repository/tags/v1.0.0" in c[1] for c in http.calls))
        self.assertIn("v1.0.0", self.record()["tags_seen"])
        variables = {v["key"]: v["value"] for v in posts()[-1][2]["variables"]}
        self.assertEqual(variables["CCID_EXPECTED_SHA"], C)
        http.pipeline_status = "success"
        b.sweep()
        b.sweep()  # nothing new: no third pipeline
        self.assertEqual(len(posts()), 2)

    def test_release_tag_on_a_proven_commit_is_not_run_again(self):
        http, git = FakeHttp(), FakeGit()
        b = broker(self.state, http, git)
        b.sweep()
        http.pipeline_status = "success"
        b.sweep()
        git.tag_list["v1.0.0"] = A  # the commit the branch run already proved
        b.sweep()
        self.assertEqual(sum(1 for c in http.calls if c[1].endswith("/pipeline")), 1)
        self.assertIn("v1.0.0", self.record()["tags_seen"])

    def test_release_tag_waits_for_the_mirror(self):
        http, git = FakeHttp(), FakeGit()
        b = broker(self.state, http, git)
        b.sweep()
        http.pipeline_status = "success"
        b.sweep()
        git.tag_list["v1.0.0"] = C
        http.tag_head = B  # the mirror has not received the tag's commit yet
        b.sweep()
        self.assertNotIn("v1.0.0", self.record()["tags_seen"])
        http.tag_head = http.pipeline_sha = C
        b.sweep()
        self.assertIn("v1.0.0", self.record()["tags_seen"])
        self.assertEqual(sum(1 for c in http.calls if c[1].endswith("/pipeline")), 2)

    def test_platform_switch_off_refuses_before_any_provider_call(self):
        http = FakeHttp()
        b = broker(self.state, http, FakeGit(), platforms={"windows": False, "linux": True})
        self.assertEqual(b.sweep(), 0)
        b.sweep()
        self.assertEqual(http.calls, [], "no provider or forge call for a switched-off platform")
        self.assertEqual(self.stderr.getvalue().count("needs Julian's yes"), 1, "logged once per commit")
        self.assertNotIn("last_dispatched", self.record())
        with self.assertRaises(rc.Refused):
            b.dispatch_now("o/r", "windows", A, "main")

    def test_absent_platform_switch_refuses_everything(self):
        http = FakeHttp()
        config_platforms = {}
        b = broker(self.state, http, FakeGit(), platforms=config_platforms)
        b.sweep()
        self.assertEqual(http.calls, [])

    def test_linux_arm64_runs_while_windows_is_off(self):
        http, git = FakeHttp(), FakeGit(plans={"linux-arm64": dict(PLAN, platform="linux-arm64")})
        broker(self.state, http, git, platforms={"windows": False, "macos": False, "linux": True}).sweep()
        self.assertTrue(any(c[1].endswith("/pipeline") for c in http.calls))

    def test_github_requires_the_gate(self):
        with self.assertRaises(rc.Failure):
            rc.GitHub(FakeHttp(), {})

    def test_github_dispatch_goes_through_the_gate(self):
        runs = {"workflow_runs": [{"id": 9, "display_title": "ccid remote macos REQ", "head_sha": A,
                                   "html_url": "https://github.example/run/9"}]}
        http = mock.Mock(return_value=(200, runs))
        github = rc.GitHub(http, {"api": "https://api.github.example", "gate_dispatch": ["gate", "dispatch"]})
        done = mock.Mock(returncode=0, stdout='{"verdict":"dispatched"}', stderr="")
        with mock.patch.object(rc.subprocess, "run", return_value=done) as run, \
             mock.patch.object(rc.time, "sleep"), mock.patch.object(rc.secrets, "token_hex", return_value="REQ"):
            handle = github.dispatch("o/r", "main", A, "macos", {"workflow": "ccid-remote"}, "REQ")
        argv = run.call_args[0][0]
        self.assertEqual(argv[:5], ["gate", "dispatch", "o/r", "ccid-remote.yml", "main"])
        self.assertIn(f"sha={A}", argv)
        self.assertEqual(handle["id"], 9)
        for call in http.call_args_list:
            self.assertEqual(call[0][0], "GET", "the adapter itself never writes to GitHub")

    def test_secrets_never_reach_state_or_logs(self):
        http = FakeHttp()
        http.dispatch_status = 400
        broker(self.state, http, FakeGit()).sweep()
        for file in self.state.iterdir():
            self.assertNotIn(TOKEN, file.read_text())
        self.assertNotIn(TOKEN, self.stderr.getvalue())

    def test_http_refuses_plain_http(self):
        with self.assertRaises(rc.Failure):
            rc.Http()("GET", "http://example.invalid/", {})

    def test_paths_glob(self):
        self.assertTrue(rc.matches(["src/**"], ["src/a/b.rs"]))
        self.assertTrue(rc.matches(["Cargo.lock"], ["Cargo.lock"]))
        self.assertFalse(rc.matches(["src/**"], ["docs/x.md"]))


if __name__ == "__main__":
    unittest.main()
