#!/usr/bin/env python3
"""Remote CI broker: run a repository's own provider workflow on its mirror and report it on the forge.

A repository declares `scheduler = "remote"` jobs in `.ci/ccid.toml` (validated and recorded in
`.ci/jobs.json` by `ccid render`). The provider workflow file lives in the repository and reaches
the provider through the forge's existing push mirror. This adapter, run on an operator host that
holds the credentials, follows the declared branches, waits until the mirror carries the exact
commit, starts the provider workflow by API, polls it until it ends or times out, and posts the
result as a commit status on the primary forge (Forgejo/Gitea API).

Each platform family (the part of `platform` before the first '-': linux, windows, macos, ...) runs only when
the host configuration's `platforms` switch says `true` for it; anything else (off, absent, no `platforms` at
all) is refused before any provider call and logged as needing the operator's yes.

Credentials are read from environment variables named in the host configuration, at the moment of
use, and only ever travel in request headers (or, for GitHub writes, to the configured write gate
command through its environment). They are never logged, never written to state and never handed to
the provider workflow. GitHub writes (enabling Actions, workflow dispatch) are never made by this
adapter: they go through the configured gate command, which applies its own budgets.

Commands:
  sweep     follow every configured repository once (one run per job at a time, newest head only,
            at most one dispatch per job per `min_interval_hours`, path filter from the declaration);
            every NEW release tag (`release_tags`, default `v[0-9]*`) runs each remote job once, regardless of
            interval and paths (the tags that exist when a job is first seen are the baseline, never run)
  dispatch  start one declared job now for an exact commit (manual; ignores interval and paths)
  status    print the state of every followed job as JSON
"""
from __future__ import annotations

import argparse
import fcntl
import fnmatch
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

SHA = re.compile(r"^[0-9a-f]{40}$")
NAME = re.compile(r"^[A-Za-z0-9_.-]+$")
REPO = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
PROVIDERS = ("gitlab", "github", "circleci", "bitbucket")
DEFAULT_INTERVAL_HOURS = 24
DEFAULT_RELEASE_TAGS = "v[0-9]*"  # the release tags (vX.Y.Z); an empty value follows no tags
PACE_SECONDS = 1.0  # between provider API calls of one sweep: never a burst
HTTP_TIMEOUT = 30
MAX_TRANSIENT = 6  # transient dispatch failures before the commit is reported as an error
DESCRIPTION_MAX = 140


class Failure(Exception):
    """A definite failure: reported, never retried silently."""


class Refused(Exception):
    """The host's platform switch is off for this platform: nothing is dispatched, nothing is reported."""


class Transient(Exception):
    """Provider or forge unavailable: retried on a later sweep, never reported as a result."""


def log(message: str) -> None:
    print(f"remote-ci: {message}", file=sys.stderr, flush=True)


def secret(name: str) -> str:
    value = os.environ.get(name, "")
    if not value or "\n" in value.strip():
        raise Failure(f"credential variable {name} is unset or not one line")
    return value.strip()


class Http:
    """JSON over HTTPS with the credential in a header. Redirects are not followed."""

    class _NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *args, **kwargs):  # noqa: D401 - urllib hook
            return None

    def __init__(self) -> None:
        self.opener = urllib.request.build_opener(self._NoRedirect)
        self.last = 0.0

    def __call__(self, method: str, url: str, headers: dict[str, str], body=None, pace: bool = True):
        if not url.startswith("https://"):
            raise Failure("refusing a non-HTTPS API URL")
        if pace:
            wait = self.last + PACE_SECONDS - time.monotonic()
            if wait > 0:
                time.sleep(wait)
        data = None
        headers = {"Accept": "application/json", "User-Agent": "ccid-remote-ci", **headers}
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(url, data=data, method=method, headers=headers)
        try:
            with self.opener.open(request, timeout=HTTP_TIMEOUT) as response:
                status, raw = response.status, response.read()
        except urllib.error.HTTPError as error:
            status, raw = error.code, error.read()
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            raise Transient(f"{method} {redact(url)}: {type(error).__name__}") from None
        finally:
            self.last = time.monotonic()
        if status >= 500 or status == 429:
            raise Transient(f"{method} {redact(url)}: HTTP {status}")
        try:
            payload = json.loads(raw) if raw else None
        except ValueError:
            payload = None
        return status, payload


def redact(url: str) -> str:
    parts = urllib.parse.urlsplit(url)
    return urllib.parse.urlunsplit((parts.scheme, parts.hostname or "", parts.path, "", ""))


def expect(status: int, payload, ok: tuple[int, ...], what: str):
    if status not in ok:
        detail = ""
        if isinstance(payload, dict):
            detail = str(payload.get("message") or payload.get("error") or "")[:120]
        raise Failure(f"{what}: HTTP {status} {detail}".strip())
    return payload


# ---------------------------------------------------------------- source of truth: the local git copy


class GitCopy:
    """Bare copies of the forge's repositories on this host (kept current by the forge's push mirror)."""

    def __init__(self, template: str) -> None:
        if "{repo}" not in template:
            raise Failure("git_copy must contain {repo}")
        self.template = template

    def _git(self, repo: str, *args: str) -> str:
        directory = self.template.format(repo=repo)
        result = subprocess.run(
            ["git", "-c", "core.hooksPath=/dev/null", "--git-dir", directory, *args],
            capture_output=True, text=True, timeout=120, env={"PATH": os.environ.get("PATH", ""), "LC_ALL": "C"},
        )
        if result.returncode:
            raise Transient(f"git {args[0]} in the copy of {repo} failed")
        return result.stdout

    def head(self, repo: str, branch: str) -> str | None:
        try:
            sha = self._git(repo, "rev-parse", "--verify", "-q", f"refs/heads/{branch}^{{commit}}").strip()
        except Transient:
            return None
        return sha if SHA.match(sha) else None

    def jobs(self, repo: str, sha: str) -> dict:
        try:
            inventory = json.loads(self._git(repo, "show", f"{sha}:.ci/jobs.json"))
        except (Transient, ValueError):
            return {}
        return {name: plan for name, plan in (inventory.get("jobs") or {}).items() if plan.get("scheduler") == "remote"}

    def tags(self, repo: str, pattern: str) -> dict[str, str]:
        """Release tags matching `pattern`, each with the commit it points to (annotated tags peeled)."""
        result = {}
        try:
            listing = self._git(repo, "for-each-ref", "--format=%(refname:strip=2) %(objectname) %(*objectname)", "refs/tags")
        except Transient:
            return result
        for line in listing.splitlines():
            parts = line.split()
            if len(parts) >= 2 and fnmatch.fnmatchcase(parts[0], pattern) and NAME.match(parts[0]):
                sha = parts[2] if len(parts) > 2 else parts[1]
                if SHA.match(sha):
                    result[parts[0]] = sha
        return result

    def contains(self, repo: str, ancestor: str, sha: str) -> bool:
        directory = self.template.format(repo=repo)
        return subprocess.run(["git", "--git-dir", directory, "merge-base", "--is-ancestor", ancestor, sha],
                              capture_output=True, timeout=120).returncode == 0

    def changed(self, repo: str, base: str, sha: str) -> list[str]:
        return [line for line in self._git(repo, "diff", "--name-only", base, sha).splitlines() if line]


def matches(paths: list[str], changed: list[str]) -> bool:
    return any(fnmatch.fnmatchcase(file, glob) or fnmatch.fnmatchcase(file, glob.replace("/**", "/*"))
               or (glob.endswith("/**") and file.startswith(glob[:-2])) for glob in paths for file in changed)


# ---------------------------------------------------------------- the primary forge: commit statuses


class Forgejo:
    def __init__(self, http: Http, config: dict) -> None:
        self.http, self.api, self.token_env = http, config["api"].rstrip("/"), config["token_env"]

    def status(self, repo: str, sha: str, state: str, context: str, description: str, target: str | None) -> None:
        body = {"state": state, "context": context, "description": description[:DESCRIPTION_MAX]}
        if target:
            body["target_url"] = target
        status, payload = self.http("POST", f"{self.api}/repos/{repo}/statuses/{sha}",
                                    {"Authorization": f"token {secret(self.token_env)}"}, body, pace=False)
        expect(status, payload, (200, 201), f"forge status for {repo}@{sha[:12]}")


# ---------------------------------------------------------------- providers


class Run(dict):
    """Provider run handle stored in state: provider, id, url (+ provider specifics)."""


class GitLab:
    name = "gitlab"
    RUNNING = {"created", "waiting_for_resource", "preparing", "pending", "running", "scheduled", "manual"}
    RESULT = {"success": "success", "failed": "failure", "canceled": "error", "skipped": "error"}

    def __init__(self, http: Http, config: dict) -> None:
        self.http, self.api, self.token_env = http, config.get("api", "https://gitlab.com/api/v4").rstrip("/"), config["token_env"]

    def _call(self, method: str, path: str, body=None):
        return self.http(method, f"{self.api}{path}", {"PRIVATE-TOKEN": secret(self.token_env)}, body)

    @staticmethod
    def _project(mirror: str) -> str:
        return urllib.parse.quote(mirror, safe="")

    def ensure(self, mirror: str) -> None:
        """CI on, instance runners on, pipeline variables allowed for maintainers (the broker passes the job and
        the expected commit as variables; GitLab's newer default `no_one_allowed` refuses them): the project's own
        settings, applied idempotently (GET first, one PUT only for what differs)."""
        project = expect(*self._call("GET", f"/projects/{self._project(mirror)}"), (200,), f"GitLab project {mirror}")
        wanted = {"builds_access_level": "enabled", "shared_runners_enabled": True,
                  "ci_pipeline_variables_minimum_override_role": "maintainer"}
        change = {key: value for key, value in wanted.items() if project.get(key) != value}
        if not change:
            return
        expect(*self._call("PUT", f"/projects/{self._project(mirror)}", change), (200,), f"enable CI on GitLab {mirror}")
        log(f"gitlab {mirror}: set {', '.join(sorted(change))}")

    def head(self, mirror: str, branch: str, tag: bool = False) -> str | None:
        kind = "tags" if tag else "branches"
        status, payload = self._call("GET", f"/projects/{self._project(mirror)}/repository/{kind}/{urllib.parse.quote(branch, safe='')}")
        if status == 404:
            return None
        return (expect(status, payload, (200,), f"GitLab {kind} {mirror}:{branch}").get("commit") or {}).get("id")

    def dispatch(self, mirror: str, branch: str, sha: str, job: str, plan: dict, request: str) -> Run:
        variables = [{"key": "CCID_REMOTE_JOB", "value": job}, {"key": "CCID_EXPECTED_SHA", "value": sha},
                     {"key": "CCID_REMOTE_REQUEST", "value": request}]
        pipeline = expect(*self._call("POST", f"/projects/{self._project(mirror)}/pipeline",
                                      {"ref": branch, "variables": variables}), (201,), f"GitLab pipeline on {mirror}")
        run = Run(provider=self.name, id=pipeline["id"], url=pipeline.get("web_url"), mirror=mirror)
        if pipeline.get("sha") != sha:
            self.cancel(run)
            raise Transient(f"GitLab {mirror} moved to another commit while dispatching")
        return run

    def poll(self, run: Run) -> str:
        pipeline = expect(*self._call("GET", f"/projects/{self._project(run['mirror'])}/pipelines/{int(run['id'])}"),
                          (200,), "GitLab pipeline")
        state = pipeline.get("status", "")
        if state in self.RUNNING:
            return "running"
        return self.RESULT.get(state, "error")

    def cancel(self, run: Run) -> None:
        self._call("POST", f"/projects/{self._project(run['mirror'])}/pipelines/{int(run['id'])}/cancel")


class GitHub:
    """Reads with a read token; every write (Actions on, dispatch) goes through the gate command."""

    name = "github"
    RESULT = {"success": "success", "failure": "failure", "timed_out": "failure", "startup_failure": "error",
              "cancelled": "error", "skipped": "error", "neutral": "error", "action_required": "error", "stale": "error"}

    def __init__(self, http: Http, config: dict) -> None:
        self.http = http
        self.api = config.get("api", "https://api.github.com").rstrip("/")
        self.read_env = config.get("read_token_env")
        self.gate = config.get("gate_dispatch") or []
        if not self.gate:
            raise Failure("the GitHub provider needs gate_dispatch (writes go through the GitHub write gate)")

    def _get(self, path: str):
        headers = {"Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28"}
        if self.read_env and os.environ.get(self.read_env):
            headers["Authorization"] = f"Bearer {secret(self.read_env)}"
        return self.http("GET", f"{self.api}{path}", headers)

    def ensure(self, mirror: str) -> None:
        """Actions must be on; switching them on is a gate write, so it is part of the gate's dispatch."""

    def head(self, mirror: str, branch: str, tag: bool = False) -> str | None:
        if tag:  # the commit a tag points to (annotated tags peeled)
            status, payload = self._get(f"/repos/{mirror}/commits/{urllib.parse.quote(branch, safe='')}")
            return None if status in (404, 422) else expect(status, payload, (200,), f"GitHub tag {mirror}:{branch}").get("sha")
        status, payload = self._get(f"/repos/{mirror}/branches/{urllib.parse.quote(branch, safe='')}")
        if status == 404:
            return None
        return (expect(status, payload, (200,), f"GitHub branch {mirror}:{branch}").get("commit") or {}).get("sha")

    def dispatch(self, mirror: str, branch: str, sha: str, job: str, plan: dict, request: str) -> Run:
        workflow = f"{plan['workflow']}.yml"
        argv = [*self.gate, mirror, workflow, branch, "--input", f"job={job}", "--input", f"sha={sha}",
                "--input", f"request={request}"]
        result = subprocess.run(argv, capture_output=True, text=True, timeout=180)
        if result.returncode:
            reason = (result.stdout.strip().splitlines() or result.stderr.strip().splitlines() or ["refused"])[-1][:120]
            raise Transient(f"GitHub gate did not dispatch {mirror}: {reason}")
        since = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() - 120))
        for _ in range(12):  # the run appears a few seconds after the dispatch
            time.sleep(5)
            payload = expect(*self._get(f"/repos/{mirror}/actions/workflows/{workflow}/runs"
                                        f"?event=workflow_dispatch&head_sha={sha}&created=%3E%3D{since}"),
                             (200,), f"GitHub runs of {mirror}")
            for candidate in payload.get("workflow_runs", []):
                if request in (candidate.get("display_title") or "") and candidate.get("head_sha") == sha:
                    return Run(provider=self.name, id=candidate["id"], url=candidate.get("html_url"), mirror=mirror)
        raise Failure(f"GitHub accepted the dispatch on {mirror} but no run with request {request} appeared")

    def poll(self, run: Run) -> str:
        payload = expect(*self._get(f"/repos/{run['mirror']}/actions/runs/{int(run['id'])}"), (200,), "GitHub run")
        if payload.get("head_sha") and payload["head_sha"] != run.get("sha", payload["head_sha"]):
            return "error"
        if payload.get("status") != "completed":
            return "running"
        return self.RESULT.get(payload.get("conclusion") or "", "error")

    def cancel(self, run: Run) -> None:
        """Cancelling is a GitHub write; the run ends at its own timeout-minutes instead."""


class CircleCI:
    name = "circleci"
    RESULT = {"success": "success", "failed": "failure", "error": "error", "canceled": "error",
              "unauthorized": "error", "not_run": "error"}

    def __init__(self, http: Http, config: dict) -> None:
        self.http, self.api, self.token_env = http, config.get("api", "https://circleci.com/api/v2").rstrip("/"), config["token_env"]

    def _call(self, method: str, path: str, body=None):
        return self.http(method, f"{self.api}{path}", {"Circle-Token": secret(self.token_env)}, body)

    def ensure(self, mirror: str) -> None:
        """Project and pipeline definition are declared on the CircleCI side; nothing to switch here."""

    def head(self, mirror: str, branch: str, tag: bool = False) -> str | None:
        return None  # CircleCI reads the VCS itself; the pipeline's revision is verified after the trigger

    def dispatch(self, mirror: str, branch: str, sha: str, job: str, plan: dict, request: str) -> Run:
        body = {"tag" if plan.get("tag") else "branch": branch,
                "parameters": {"ccid_job": job, "ccid_sha": sha, "ccid_request": request}}
        pipeline = expect(*self._call("POST", f"/project/{mirror}/pipeline", body), (201, 200), f"CircleCI pipeline on {mirror}")
        run = Run(provider=self.name, id=pipeline["id"], url=None, mirror=mirror, number=pipeline.get("number"))
        for _ in range(12):
            time.sleep(5)
            detail = expect(*self._call("GET", f"/pipeline/{pipeline['id']}"), (200,), "CircleCI pipeline")
            revision = (detail.get("vcs") or {}).get("revision")
            if revision:
                if revision != sha:
                    raise Transient(f"CircleCI {mirror} built another commit")
                run["url"] = f"https://app.circleci.com/pipelines/{mirror}/{pipeline.get('number')}"
                return run
        raise Failure("CircleCI pipeline has no revision")

    def poll(self, run: Run) -> str:
        workflows = expect(*self._call("GET", f"/pipeline/{run['id']}/workflow"), (200,), "CircleCI workflows").get("items", [])
        if not workflows:
            return "running"
        states = [w.get("status") for w in workflows]
        if any(s in ("running", "on_hold", "failing") for s in states):
            return "running"
        for verdict in ("failure", "error"):
            if any(self.RESULT.get(s) == verdict for s in states):
                return verdict
        return "success" if all(s == "success" for s in states) else "error"

    def cancel(self, run: Run) -> None:
        for workflow in (self._call("GET", f"/pipeline/{run['id']}/workflow")[1] or {}).get("items", []):
            self._call("POST", f"/workflow/{workflow['id']}/cancel")


class Bitbucket:
    name = "bitbucket"
    RESULT = {"SUCCESSFUL": "success", "FAILED": "failure", "ERROR": "error", "STOPPED": "error", "EXPIRED": "error"}

    def __init__(self, http: Http, config: dict) -> None:
        self.http, self.api, self.token_env = http, config.get("api", "https://api.bitbucket.org/2.0").rstrip("/"), config["token_env"]

    def _call(self, method: str, path: str, body=None):
        return self.http(method, f"{self.api}{path}", {"Authorization": f"Bearer {secret(self.token_env)}"}, body)

    def ensure(self, mirror: str) -> None:
        status, payload = self._call("GET", f"/repositories/{mirror}/pipelines_config")
        if status == 200 and payload.get("enabled"):
            return
        expect(*self._call("PUT", f"/repositories/{mirror}/pipelines_config", {"enabled": True}), (200,),
               f"enable Pipelines on Bitbucket {mirror}")
        log(f"bitbucket {mirror}: Pipelines enabled")

    def head(self, mirror: str, branch: str, tag: bool = False) -> str | None:
        kind = "tags" if tag else "branches"
        status, payload = self._call("GET", f"/repositories/{mirror}/refs/{kind}/{urllib.parse.quote(branch, safe='')}")
        if status == 404:
            return None
        return (expect(status, payload, (200,), f"Bitbucket {kind} {mirror}:{branch}").get("target") or {}).get("hash")

    def dispatch(self, mirror: str, branch: str, sha: str, job: str, plan: dict, request: str) -> Run:
        body = {"target": {"type": "pipeline_commit_target", "commit": {"type": "commit", "hash": sha},
                           "selector": {"type": "custom", "pattern": plan["workflow"]}},
                "variables": [{"key": "CCID_REMOTE_JOB", "value": job}, {"key": "CCID_REMOTE_REQUEST", "value": request}]}
        pipeline = expect(*self._call("POST", f"/repositories/{mirror}/pipelines/", body), (201,), f"Bitbucket pipeline on {mirror}")
        workspace, slug = mirror.split("/", 1)
        url = f"https://bitbucket.org/{workspace}/{slug}/pipelines/results/{pipeline.get('build_number')}"
        return Run(provider=self.name, id=pipeline["uuid"], url=url, mirror=mirror)

    def poll(self, run: Run) -> str:
        pipeline = expect(*self._call("GET", f"/repositories/{run['mirror']}/pipelines/{urllib.parse.quote(run['id'], safe='')}"),
                          (200,), "Bitbucket pipeline")
        state = pipeline.get("state") or {}
        if state.get("name") != "COMPLETED":
            return "running"
        return self.RESULT.get((state.get("result") or {}).get("name", ""), "error")

    def cancel(self, run: Run) -> None:
        self._call("POST", f"/repositories/{run['mirror']}/pipelines/{urllib.parse.quote(run['id'], safe='')}/stopPipeline")


PROVIDER_CLASSES = {"gitlab": GitLab, "github": GitHub, "circleci": CircleCI, "bitbucket": Bitbucket}


# ---------------------------------------------------------------- state


class State:
    def __init__(self, directory: Path) -> None:
        self.directory = directory
        directory.mkdir(parents=True, exist_ok=True)

    def path(self, repo: str, job: str) -> Path:
        return self.directory / f"{repo.replace('/', '__')}__{job}.json"

    def load(self, repo: str, job: str) -> dict:
        try:
            return json.loads(self.path(repo, job).read_text())
        except (OSError, ValueError):
            return {}

    def save(self, repo: str, job: str, value: dict) -> None:
        target = self.path(repo, job)
        temporary = target.with_suffix(".tmp")
        temporary.write_text(json.dumps(value, indent=1, sort_keys=True) + "\n")
        os.replace(temporary, target)

    def all(self) -> dict:
        result = {}
        for file in sorted(self.directory.glob("*.json")):
            try:
                result[file.stem] = json.loads(file.read_text())
            except ValueError:
                result[file.stem] = {"unreadable": True}
        return result


# ---------------------------------------------------------------- the broker


def context(job: str) -> str:
    return f"remote / {job}"


class Broker:
    def __init__(self, config: dict, http: Http | None = None, git: GitCopy | None = None) -> None:
        self.config = config
        self.http = http or Http()
        self.git = git or GitCopy(config["git_copy"])
        self.forge = Forgejo(self.http, config["forgejo"])
        self.state = State(Path(config["state_dir"]))
        self.interval = float(config.get("min_interval_hours", DEFAULT_INTERVAL_HOURS)) * 3600
        self.release_tags = config.get("release_tags", DEFAULT_RELEASE_TAGS)
        self.platforms = config.get("platforms") or {}
        self.providers: dict[str, object] = {}
        for repo, mirrors in config.get("repos", {}).items():
            if not REPO.match(repo) or not isinstance(mirrors, dict):
                raise Failure(f"bad repository entry {repo!r}")
            for provider, mirror in mirrors.items():
                if provider not in PROVIDERS or not isinstance(mirror, str) or not mirror:
                    raise Failure(f"bad mirror {provider!r} for {repo}")

    def provider(self, name: str):
        if name not in self.providers:
            settings = (self.config.get("providers") or {}).get(name)
            if settings is None:
                raise Failure(f"provider {name} is not configured on this host")
            self.providers[name] = PROVIDER_CLASSES[name](self.http, settings)
        return self.providers[name]

    def mirror(self, repo: str, provider: str) -> str:
        mirror = (self.config.get("repos", {}).get(repo) or {}).get(provider)
        if not mirror:
            raise Failure(f"{repo} has no {provider} mirror configured")
        return mirror

    def describe(self, plan: dict, text: str) -> str:
        return f"{plan['provider']} {plan['platform']}: {text}"

    # one job, one commit ------------------------------------------------------

    def start(self, repo: str, branch: str, sha: str, job: str, plan: dict, record: dict) -> bool:
        """Dispatch when the mirror carries `sha`. True when a run started; False when it must wait."""
        family = str(plan.get("platform") or "").split("-")[0]
        if self.platforms.get(family) is not True:
            raise Refused(f"{plan.get('platform')} needs Julian's yes (platform switch '{family}' is off on this host)")
        provider = self.provider(plan["provider"])
        mirror = self.mirror(repo, plan["provider"])
        provider.ensure(mirror)
        head = provider.head(mirror, branch, bool(plan.get("tag")))
        if head is not None and head != sha:
            log(f"{repo} {job}: the {plan['provider']} mirror is at {head[:12]}, waiting for {sha[:12]}")
            return False
        request = secrets.token_hex(6)
        self.forge.status(repo, sha, "pending", context(job), self.describe(plan, "dispatching"), None)
        run = provider.dispatch(mirror, branch, sha, job, plan, request)
        run["sha"] = sha
        now = time.time()
        record.update(inflight={"commit": sha, "branch": branch, "request": request, "run": dict(run), "plan": plan,
                                "dispatched": now, "deadline": now + 60 * int(plan.get("timeout_minutes") or 60)},
                      last_dispatched={"commit": sha, "at": now}, transient=0)
        self.forge.status(repo, sha, "pending", context(job), self.describe(plan, "running"), run.get("url"))
        log(f"{repo} {job}: dispatched {sha[:12]} ({'tag' if plan.get('tag') else 'branch'} {branch}) to {plan['provider']} ({run.get('url')})")
        return True

    def follow(self, repo: str, job: str, record: dict) -> None:
        """Poll the job's run in flight; report its result or its timeout."""
        inflight = record["inflight"]
        plan = inflight.get("plan") or {}
        provider = self.provider(inflight["run"]["provider"])
        run = Run(inflight["run"])
        sha = inflight["commit"]
        verdict = provider.poll(run)
        elapsed = int(time.time() - inflight["dispatched"])
        if verdict == "running":
            if time.time() < inflight["deadline"]:
                return
            provider.cancel(run)
            verdict, text = "error", f"timed out after {elapsed // 60} min"
        else:
            outcome = {"success": "passed", "failure": "failed", "error": "ended without a result"}[verdict]
            text = f"{outcome} in {elapsed // 60} min"
        description = f"{run['provider']} {plan.get('platform') or '?'}: {text}"
        self.forge.status(repo, sha, verdict, context(job), description, run.get("url"))
        record["last_result"] = {"commit": sha, "state": verdict, "url": run.get("url"), "at": time.time(),
                                 "seconds": elapsed}
        record.pop("inflight", None)
        log(f"{repo} {job}: {sha[:12]} {verdict} ({text})")

    def due(self, repo: str, sha: str, plan: dict, record: dict) -> bool:
        last = record.get("last_dispatched") or {}
        if last.get("commit") == sha:
            return False
        if time.time() - float(last.get("at", 0)) < self.interval:
            return False
        paths = plan.get("paths") or []
        base = last.get("commit")
        if paths and base and SHA.match(base) and self.git.contains(repo, base, sha):
            if not matches(paths, self.git.changed(repo, base, sha)):
                return False
        return True

    # commands -----------------------------------------------------------------

    def sweep(self) -> int:
        failures = 0
        for repo in sorted(self.config.get("repos", {})):
            for job, plan, branch, sha in self.declared(repo):
                record = self.state.load(repo, job)
                try:
                    if record.get("inflight"):
                        self.follow(repo, job, record)
                    elif not self.release(repo, job, plan, record) and self.due(repo, sha, plan, record):
                        self.start(repo, branch, sha, job, self.with_plan(plan), record)
                except Refused as error:
                    if record.get("refused") != sha:  # once per commit, not every sweep
                        log(f"{repo} {job}: not dispatched: {error}")
                        record["refused"] = sha
                except Transient as error:
                    log(f"{repo} {job}: provider unavailable, retrying later ({error})")
                    record["transient"] = int(record.get("transient", 0)) + 1
                    if record["transient"] >= MAX_TRANSIENT and not record.get("inflight"):
                        self.report_error(repo, sha, job, plan, "provider unavailable", record)
                except Failure as error:
                    failures += 1
                    log(f"{repo} {job}: {error}")
                    if not record.get("inflight"):
                        self.report_error(repo, sha, job, plan, str(error), record)
                self.state.save(repo, job, record)
        return 1 if failures else 0

    def release(self, repo: str, job: str, plan: dict, record: dict) -> bool:
        """Run the job once for the oldest new release tag. True when a tag was handled in this sweep."""
        if not self.release_tags:
            return False
        tags = self.git.tags(repo, self.release_tags)
        if "tags_seen" not in record:  # first sight: the existing tags are the baseline, never run
            record["tags_seen"] = sorted(tags)
            return False
        for name in sorted(set(tags) - set(record["tags_seen"])):
            sha = tags[name]
            tagged = self.git.jobs(repo, sha).get(job)
            last = record.get("last_result") or {}
            if not tagged or tagged.get("provider") != plan.get("provider") or (
                    last.get("commit") == sha and last.get("state") in ("success", "failure")):
                record["tags_seen"] = sorted({*record["tags_seen"], name})  # nothing to run, or already proven
                continue
            tag_plan = {**self.with_plan(tagged), "tag": True}
            try:
                started = self.start(repo, name, sha, job, tag_plan, record)
            except Failure as error:
                self.report_error(repo, sha, job, tag_plan, str(error), record)
                started = True
            if started:
                record["tags_seen"] = sorted({*record["tags_seen"], name})
            return True  # one dispatch per job and sweep; a mirror without the tag yet waits for the next sweep
        return False

    def report_error(self, repo: str, sha: str, job: str, plan: dict, text: str, record: dict) -> None:
        try:
            self.forge.status(repo, sha, "error", context(job), self.describe(plan, text), None)
        except (Failure, Transient) as error:
            log(f"{repo} {job}: could not post the error status ({error})")
        record.update(last_dispatched={"commit": sha, "at": time.time()}, transient=0,
                      last_result={"commit": sha, "state": "error", "reason": text[:200], "at": time.time()})

    @staticmethod
    def with_plan(plan: dict) -> dict:
        return {key: plan.get(key) for key in ("provider", "platform", "workflow", "timeout_minutes", "checks")}

    def declared(self, repo: str):
        """(job, plan, branch, head) for every remote job of every followed branch, newest head only."""
        seen = set()
        branches = set()
        # The inventory on the default branches names the branches to follow.
        for branch in ("main", "master"):
            sha = self.git.head(repo, branch)
            if sha:
                for plan in self.git.jobs(repo, sha).values():
                    branches.update(plan.get("push_branches") or [])
        for branch in sorted(branches):
            if not NAME.match(branch):
                continue
            sha = self.git.head(repo, branch)
            if not sha:
                continue
            for job, plan in sorted(self.git.jobs(repo, sha).items()):
                if branch in (plan.get("push_branches") or []) and job not in seen and NAME.match(job):
                    if plan.get("provider") in (self.config.get("repos", {}).get(repo) or {}):
                        seen.add(job)
                        yield job, plan, branch, sha

    def dispatch_now(self, repo: str, job: str, sha: str | None, branch: str) -> int:
        if repo not in self.config.get("repos", {}):
            raise Failure(f"{repo} is not followed on this host")
        sha = sha or self.git.head(repo, branch)
        if not sha or not SHA.match(sha):
            raise Failure("no exact commit to dispatch")
        plans = self.git.jobs(repo, sha)
        if job not in plans:
            raise Failure(f"{repo}@{sha[:12]} declares no remote job {job}")
        record = self.state.load(repo, job)
        if record.get("inflight"):
            raise Failure(f"{repo} {job} already has a run in flight ({record['inflight']['run'].get('url')})")
        plan = self.with_plan(plans[job])
        try:
            started = self.start(repo, branch, sha, job, plan, record)
        except Failure as error:
            self.report_error(repo, sha, job, plan, str(error), record)
            self.state.save(repo, job, record)
            raise
        if not started:
            raise Transient("the mirror does not carry the commit yet; try again after the push mirror ran")
        self.state.save(repo, job, record)
        return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--config", required=True, type=Path, help="host configuration (JSON)")
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("sweep")
    sub.add_parser("status")
    one = sub.add_parser("dispatch")
    one.add_argument("--repo", required=True)
    one.add_argument("--job", required=True)
    one.add_argument("--commit")
    one.add_argument("--branch", default="main")
    args = parser.parse_args(argv)
    config = json.loads(args.config.read_text())
    broker = Broker(config)
    lock = open(Path(config["state_dir"]) / ".lock", "w")  # noqa: SIM115 - held for the process lifetime
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        log("another sweep holds the lock")
        return 0
    try:
        if args.command == "status":
            print(json.dumps(broker.state.all(), indent=1, sort_keys=True))
            return 0
        if args.command == "dispatch":
            return broker.dispatch_now(args.repo, args.job, args.commit, args.branch)
        return broker.sweep()
    except (Failure, Transient, Refused) as error:
        log(str(error))
        return 2


if __name__ == "__main__":
    sys.exit(main())
