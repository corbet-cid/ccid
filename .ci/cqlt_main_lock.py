#!/usr/bin/env python3
"""One diagnostic cqlt main-transition lock; not a build or acceptance gate."""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import tomllib
from datetime import datetime, timezone

URL = "https://github.com/corbet-foss/cqlt"
HISTORICAL = "e6275c5373954ca7f3b4fc680ffaccc149dc32f6"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def file_digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def clean_environment():
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("GH_", "GITHUB_", "GIT_", "SSH_"))}
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null",
               GIT_TERMINAL_PROMPT="0", GIT_NO_REPLACE_OBJECTS="1", GIT_NO_LAZY_FETCH="1")
    return env


def git(repo, *args):
    result = subprocess.run(["git", "--no-replace-objects", "--no-lazy-fetch", "-C", str(repo),
                             "-c", "credential.helper=", "-c", "core.hooksPath=/dev/null", *args],
                            env=clean_environment(), capture_output=True, text=True)
    require(result.returncode == 0, "required Git source operation failed")
    return result.stdout.strip()


def read_archive(path, expected):
    require(re.fullmatch("[0-9a-f]{64}", expected) and file_digest(path) == expected, "archive digest mismatch")
    require(path.stat().st_size <= 64 * 1024 * 1024, "archive exceeds diagnostic bound")
    members = {}
    with tarfile.open(path, "r:") as archive:
        for member in archive:
            require(not member.name.startswith("/") and ".." not in member.name.split("/")
                    and "\\" not in member.name, "unsafe archive member")
            if member.isdir():
                continue
            require(member.isfile() and member.name not in members, "linked/special/duplicate archive member")
            require(member.size <= 64 * 1024 * 1024, "oversized archive member")
            members[member.name] = archive.extractfile(member).read()
    return members


def mirror(payload, receipt, root, historical=HISTORICAL):
    require(receipt["schema"] == 1 and receipt["purpose"] == "ccid-cqlt-main-transition"
            and receipt["repository"] == URL and receipt["historical_commit"] == historical,
            "foreign transition receipt")
    main = receipt["main_commit"]
    require(re.fullmatch("[0-9a-f]{40}", main), "invalid main identity")
    datetime.fromisoformat(receipt["main_observed_at"])
    require(set(receipt["commits"]) == {main, historical}, "unexpected commit inventory")
    require(all(re.fullmatch("[0-9a-f]{40}", tree) for tree in receipt["commits"].values()), "invalid tree identity")
    require(digest(payload["cqlt.bundle"]) == receipt["bundle_sha256"], "bundle digest mismatch")
    bundle = root / "cqlt.bundle"; bundle.write_bytes(payload["cqlt.bundle"])
    repo = root / "cqlt.git"; repo.mkdir()
    git(repo, "init", "--bare", "--template=", "--quiet")
    git(repo, "bundle", "verify", str(bundle))
    require(git(repo, "bundle", "list-heads", str(bundle)) == main + " refs/heads/main", "unexpected bundle refs")
    git(repo, "fetch", "--no-tags", str(bundle), "refs/heads/main:refs/heads/main")
    git(repo, "fsck", "--full", "--strict", "--no-reflogs")
    git(repo, "merge-base", "--is-ancestor", historical, main)
    for oid, tree in receipt["commits"].items():
        require(git(repo, "rev-parse", oid + "^{commit}") == oid
                and git(repo, "rev-parse", oid + "^{tree}") == tree, "commit/tree mismatch")
    git(repo, "symbolic-ref", "HEAD", "refs/heads/main")
    require(git(repo, "rev-parse", "HEAD^{commit}") == main, "HEAD differs from verified original main")
    return repo


def scoped_environment(repo):
    env = clean_environment()
    settings = [("credential.helper", ""), ("core.hooksPath", "/dev/null"),
                ("protocol.ext.allow", "never"), ("protocol.file.allow", "always"),
                ("url." + repo.as_uri() + ".insteadOf", URL)]
    env["GIT_CONFIG_COUNT"] = str(len(settings))
    for index, (key, value) in enumerate(settings):
        env["GIT_CONFIG_KEY_" + str(index)] = key
        env["GIT_CONFIG_VALUE_" + str(index)] = value
    env["CARGO_NET_GIT_FETCH_WITH_CLI"] = "true"
    return env


def resolve(root, env):
    subprocess.run(["cargo", "--config", "net.offline=false", "update"], cwd=root, env=env, check=True)
    return subprocess.check_output(["cargo", "metadata", "--locked", "--format-version", "1"], cwd=root, env=env)


def main():
    require(os.environ.get("CI") == "crow", "Crow context required")
    root = Path.cwd().resolve()
    archive = Path(os.environ["SOURCE_ARCHIVE"])
    source = read_archive(archive, os.environ["SOURCE_SHA256"])
    commit = os.environ["CI_COMMIT_SHA"]
    require(re.fullmatch("[0-9a-f]{40}", commit), "exact consumer commit required")
    with archive.open("rb") as stream:
        archived_commit = subprocess.check_output(["git", "get-tar-commit-id"], stdin=stream, text=True).strip()
    require(archived_commit == commit, "source commit mismatch")
    for name in ["Cargo.toml", "Cargo.lock", ".ci/cqlt_main_lock.py"]:
        require(source[name] == (root / name).read_bytes(), "verified source input differs")
    manifest = tomllib.loads(source["Cargo.toml"].decode())
    require(manifest["dependencies"]["cqlt"] == {"git": URL, "branch": "main"}, "transition manifest must declare original main")
    old = [p for p in tomllib.loads(source["Cargo.lock"].decode())["package"] if p["name"] == "cqlt"]
    require(len(old) == 1 and old[0]["source"] == "git+" + URL + "?rev=" + HISTORICAL + "#" + HISTORICAL,
            "only the explicit historical baseline transition is supported")
    supplied = Path(os.environ["CQLT_SOURCE_ARCHIVE"])
    payload = read_archive(supplied, os.environ["CQLT_SOURCE_SHA256"])
    require(set(payload) == {"manifest.json", "cqlt.bundle"}, "unexpected source supply members")
    receipt = json.loads(payload["manifest.json"])
    require(receipt["consumer"] == {"commit": commit, "repository": os.environ["CI_REPOSITORY_URL"],
            "source_archive_sha256": os.environ["SOURCE_SHA256"], "manifest_sha256": digest(source["Cargo.toml"]),
            "original_lock_sha256": digest(source["Cargo.lock"])}, "consumer source binding differs")
    output_base = Path(os.environ["CARGO_HOME"]) / "ccid-artifacts" / commit / "cqlt-main-lock"
    output_base.mkdir(parents=True, exist_ok=True)
    output = Path(tempfile.mkdtemp(prefix="candidate-", dir=output_base))
    (output / "Cargo.lock.original").write_bytes(source["Cargo.lock"])
    (output / "supply-manifest.json").write_bytes(payload["manifest.json"])
    # Outer verified ccid runner owns deadline/process-tree supervision and target lock.
    with tempfile.TemporaryDirectory(prefix="cqlt-main-", dir=os.environ["TMPDIR"]) as scratch:
        repo = mirror(payload, receipt, Path(scratch))
        env = scoped_environment(repo)
        cargo = subprocess.check_output(["cargo", "--version"], env=env, text=True).strip()
        rustc = subprocess.check_output(["rustc", "--version"], env=env, text=True).strip()
        metadata = resolve(root, env)
        new = (root / "Cargo.lock").read_bytes()
        (output / "Cargo.lock").write_bytes(new)
        (output / "metadata.json").write_bytes(metadata)
        expected = "git+" + URL + "?branch=main#" + receipt["main_commit"]
        selected = [p for p in tomllib.loads(new.decode())["package"] if p["name"] == "cqlt"]
        require(len(selected) == 1 and selected[0]["source"] == expected, "resolved cqlt is not the observed genuine main")
        current_files = {path.relative_to(root).as_posix() for path in root.rglob("*") if not path.is_dir()}
        require(current_files == set(source), "source file inventory changed during update")
        for name, data in source.items():
            if name != "Cargo.lock":
                require((root / name).read_bytes() == data, "non-lock source changed during update")
        result = {"schema": 1, "kind": "ccid-cqlt-main-lock", "acceptance": False, "build_performed": False,
                  "source_commit": commit, "source_archive_sha256": os.environ["SOURCE_SHA256"],
                  "repository": os.environ["CI_REPOSITORY_URL"], "original_lock_sha256": digest(source["Cargo.lock"]),
                  "candidate_lock_sha256": digest(new), "metadata_sha256": digest(metadata),
                  "source_supply_sha256": os.environ["CQLT_SOURCE_SHA256"], "cqlt_main": receipt["main_commit"],
                  "main_observed_at": receipt["main_observed_at"], "resolved_at": datetime.now(timezone.utc).isoformat(),
                  "cargo": cargo, "rustc": rustc, "mode": "explicit-source-identity-transition",
                  "compatibility": "not-compared-against-incoherent-rev-baseline"}
        (output / "receipt.json").write_text(json.dumps(result, sort_keys=True, indent=2) + "\n")
        print(json.dumps({"event": "cqlt-main-lock-retained", "directory": str(output), "receipt": result}, sort_keys=True))


if __name__ == "__main__":
    main()
