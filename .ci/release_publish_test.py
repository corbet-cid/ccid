#!/usr/bin/env python3
"""Execute the bootstrap publisher shell against deterministic fake APIs."""

from __future__ import annotations

import base64
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest
import zipfile


SOURCE = "a" * 40
WORKFLOW = "b" * 40
RUN_ID = "123"
WORKFLOW_BYTES = b"name: synthetic bootstrap\n"
WORKFLOW_SHA = hashlib.sha256(WORKFLOW_BYTES).hexdigest()


def publisher_script() -> str:
    workflow = Path(__file__).parents[1] / ".github/workflows/bootstrap.yml"
    text = workflow.read_text()
    marker = "      - name: Publish or reuse the exact immutable release asset\n"
    section = text[text.index(marker) :]
    start = section.index("        run: |\n") + len("        run: |\n")
    return textwrap.dedent(section[start:])


def make_archive(path: Path, *, duplicate: bool = False, oversized: bool = False) -> None:
    binary = b"tool" if not oversized else b"x" * (32 * 1024 * 1024 + 1)
    receipt = {
        "source_revision": SOURCE,
        "release_tag": "ccid-" + SOURCE,
        "target": "x86_64-unknown-linux-gnu",
        "binary_sha256": hashlib.sha256(binary).hexdigest(),
        "workflow_revision": WORKFLOW,
        "workflow_sha256": WORKFLOW_SHA,
        "workflow_run_id": int(RUN_ID),
    }
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as bundle:
        for name, content in (("ccid", binary), ("receipt.json", json.dumps(receipt).encode())):
            info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = (0o100700 if name == "ccid" else 0o100600) << 16
            bundle.writestr(info, content)
        if duplicate:
            info = zipfile.ZipInfo("ccid", date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o100700 << 16
            bundle.writestr(info, binary)


def fake_tools(root: Path, archive: Path, *, release: dict | None,
               tag: bool = True, git_failure: bool = False,
               release_status: int = 200, download_failure: bool = False) -> Path:
    fake = root / "bin"
    fake.mkdir()
    calls = root / "calls"
    (fake / "git").write_text(
        "#!/usr/bin/env python3\n"
        "import sys\n"
        f"if {git_failure!r}: raise SystemExit(7)\n"
        "if sys.argv[1] == 'ls-remote' and "
        f"{tag!r}:\n"
        f" print('{SOURCE}\\trefs/tags/ccid-{SOURCE}')\n"
        f" print('{SOURCE}\\trefs/tags/ccid-{SOURCE}^{{}}')\n"
    )
    (fake / "curl").write_text(
        "#!/usr/bin/env python3\n"
        "import json, pathlib, shutil, sys\n"
        "args=sys.argv[1:]\n"
        "out=pathlib.Path(args[args.index('--output')+1])\n"
        "url=args[-1]\n"
        f"if '/releases/tags/' in url:\n out.write_text(json.dumps({release!r}))\n print({release_status!r})\n"
        f"elif {download_failure!r}: raise SystemExit(8)\n"
        f"else: shutil.copyfile({str(archive)!r}, out)\n"
    )
    workflow_json = {"content": base64.b64encode(WORKFLOW_BYTES).decode()}
    (fake / "gh").write_text(
        "#!/usr/bin/env python3\n"
        "import json, pathlib, sys\n"
        f"pathlib.Path({str(calls)!r}).write_text(' '.join(sys.argv[1:]))\n"
        "if sys.argv[1:2] == ['api']:\n"
        f" print(json.dumps({workflow_json!r}))\n"
        "elif sys.argv[1:2] == ['release']:\n"
        " pass\n"
    )
    for command in fake.iterdir():
        command.chmod(0o755)
    return calls


@contextmanager
def run_publisher(*, release: dict | None, tag: bool = True,
                  git_failure: bool = False, release_status: int = 200,
                  download_failure: bool = False, duplicate: bool = False,
                  oversized: bool = False):
  with tempfile.TemporaryDirectory(prefix="ccid-publisher-test-") as directory:
    root = Path(directory)
    archive_root = root / "release"
    archive_root.mkdir()
    archive = archive_root / ("ccid-" + SOURCE + "-linux-x86_64.zip")
    make_archive(archive, duplicate=duplicate, oversized=oversized)
    (archive_root / "archive.sha256").write_text(
        hashlib.sha256(archive.read_bytes()).hexdigest() + "  " + archive.name + "\n"
    )
    calls = fake_tools(root, archive, release=release, tag=tag,
                       git_failure=git_failure, release_status=release_status,
                       download_failure=download_failure)
    env = os.environ | {
        "PATH": str(calls.parent / "bin") + os.pathsep + os.environ["PATH"],
        "GH_TOKEN": "test-token", "SOURCE_REVISION": SOURCE,
        "WORKFLOW_REVISION": WORKFLOW, "WORKFLOW_RUN_ID": RUN_ID,
        "ARCHIVE_ROOT": str(archive_root), "GITHUB_REPOSITORY": "corbet-labs/ccid",
    }
    result = subprocess.run(
        ["bash", "-euo", "pipefail", "-c", publisher_script()],
        env=env, text=True, capture_output=True,
    )
    yield result.returncode, result.stdout + result.stderr, calls


def release_with_asset() -> dict:
    with tempfile.TemporaryDirectory() as directory:
        archive = Path(directory) / "asset.zip"
        make_archive(archive)
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    return {"tag_name": "ccid-" + SOURCE, "assets": [{
        "name": "ccid-" + SOURCE + "-linux-x86_64.zip",
        "browser_download_url": "https://assets.example/ccid.zip",
        "digest": "sha256:" + digest,
    }]}


class PublisherShellTests(unittest.TestCase):
    def test_git_transport_failure_fails_closed(self):
        with run_publisher(release=None, git_failure=True) as (code, _, calls):
            self.assertNotEqual(code, 0)
            self.assertNotIn("release upload", calls.read_text())
            self.assertNotIn("release create", calls.read_text())

    def test_http_500_fails_without_upload(self):
        with run_publisher(release=None, tag=False, release_status=500) as (code, _, calls):
            self.assertNotEqual(code, 0)
            self.assertNotIn("release upload", calls.read_text())
            self.assertNotIn("release create", calls.read_text())

    def test_duplicate_asset_fails_without_download_or_upload(self):
        release = release_with_asset()
        release["assets"].append(dict(release["assets"][0]))
        with run_publisher(release=release) as (code, _, calls):
            self.assertNotEqual(code, 0)
            self.assertNotIn("release upload", calls.read_text())
            self.assertNotIn("release create", calls.read_text())

    def test_failed_existing_asset_download_fails_closed(self):
        with run_publisher(release=release_with_asset(), download_failure=True) as (code, _, calls):
            self.assertNotEqual(code, 0)
            self.assertNotIn("release upload", calls.read_text())
            self.assertNotIn("release create", calls.read_text())

    def test_duplicate_and_oversized_archives_are_rejected(self):
        for kwargs in ({"duplicate": True}, {"oversized": True}):
            with run_publisher(release=None, tag=False, **kwargs) as (code, _, calls):
                self.assertNotEqual(code, 0)
                self.assertFalse(calls.exists())

    def test_missing_release_and_tag_create(self):
        with run_publisher(release=None, tag=False, release_status=404) as (code, _, calls):
            self.assertEqual(code, 0)
            self.assertIn("release create", calls.read_text())

    def test_exact_existing_asset_is_reused(self):
        with run_publisher(release=release_with_asset()) as (code, _, calls):
            self.assertEqual(code, 0)
            self.assertNotIn("release upload", calls.read_text())
            self.assertNotIn("release create", calls.read_text())


if __name__ == "__main__":
    unittest.main()
