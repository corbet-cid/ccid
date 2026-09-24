#!/usr/bin/env python3
"""Publication protocol/identity tests using temporary files and fake HTTP only."""
import copy
import base64
import csv
import gzip
import importlib.util
import io
import json
import lzma
from pathlib import Path
import tarfile
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock
import zipfile

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("registry_publish", ROOT / "adapters/registry_publish.py")
pub = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pub)
COMMIT, TOOL = "a" * 40, "b" * 40
REPOSITORY = "example/widget"


def tar(files, commit=None):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w", format=tarfile.PAX_FORMAT,
                      pax_headers={"comment": commit} if commit else {}) as archive:
        for path, payload in files.items():
            info = tarfile.TarInfo(path)
            info.size = len(payload)
            archive.addfile(info, io.BytesIO(payload))
    return output.getvalue()


def tar_for_jsr(files):
    """The publisher's documented JSR tar layout, rebuilt independently."""
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w") as archive:
        for path, payload in sorted(files.items()):
            info = tarfile.TarInfo(path)
            info.size, info.mode, info.mtime = len(payload), 0o644, 0
            archive.addfile(info, io.BytesIO(payload))
    return output.getvalue()


def cargo_fixture(*, source_lock=b"version = 4\n", packaged_lock=None):
    manifest = b'[package]\nname = "widget"\nversion = "1.2.3"\nrepository = "https://github.com/example/widget"\nlicense = "MIT"\nrust-version = "1.94"\n'
    source = {"Cargo.toml": manifest, "Cargo.lock": source_lock, "src/lib.rs": b"pub fn value() {}\n",
              ".crow/ccid.yaml": f"    CCID_REVISION: '{TOOL}'\n".encode()}
    source_payload = tar(source, COMMIT)
    archive = tar({**{"widget-1.2.3/" + name: value for name, value in source.items() if not name.startswith(".crow/")},
                   "widget-1.2.3/Cargo.lock": source_lock if packaged_lock is None else packaged_lock,
                   "widget-1.2.3/Cargo.toml.orig": manifest})
    filename = "widget-1.2.3.crate"
    receipt = {"schema": 1, "package": "widget", "version": "1.2.3", "commit": COMMIT,
               "source_sha256": pub.sha(source_payload), "tool_revision": TOOL,
               "check": "rust-package", "artifacts": {filename: pub.sha(archive)}}
    receipt_bytes = pub.json_bytes(receipt)
    entry = {"source_commit": COMMIT, "source_archive": "cargo/source.tar", "source_sha256": pub.sha(source_payload),
             "receipt": "cargo/rust-package.json", "receipt_sha256": pub.sha(receipt_bytes),
             "artifacts": {filename: {"path": "cargo/artifacts/" + filename, "sha256": pub.sha(archive)}}}
    manifest = {"schema": 1, "repository": REPOSITORY, "package": "widget", "version": "1.2.3",
                "tag": "v1.2.3", "tag_commit": "c" * 40, "channels": {"cargo": entry}}
    files = {"release.json": pub.json_bytes(manifest), entry["source_archive"]: source_payload,
             entry["receipt"]: receipt_bytes, "cargo/artifacts/" + filename: archive}
    payload = tar(files)
    return pub.Bundle(payload, pub.sha(payload), REPOSITORY), files


def workspace_locks():
    registry = "registry+https://github.com/rust-lang/crates.io-index"
    source = [
        {"name": "widget", "version": "1.2.3", "dependencies": ["bridge"]},
        {"name": "bridge", "version": "1.0.0", "source": registry, "checksum": "1" * 64,
         "dependencies": ["encoding 2.0.0 (" + registry + ")"]},
        {"name": "encoding", "version": "2.0.0", "source": registry, "checksum": "2" * 64},
        {"name": "build-helper", "version": "0.0.0", "dependencies": ["encoding 1.0.0"]},
        {"name": "encoding", "version": "1.0.0", "source": registry, "checksum": "3" * 64},
    ]
    packaged = copy.deepcopy(source[:3])
    packaged[1]["dependencies"] = ["encoding"]
    return source, packaged


def lock_bytes(packages):
    # JSON strings/arrays are also valid TOML values for these lock fields.
    text = "version = 4\n"
    for package in packages:
        text += "\n[[package]]\n"
        text += "".join(key + " = " + json.dumps(value) + "\n" for key, value in package.items())
    return text.encode()


def npm_fixture(prefix="web/", *, packaged_changes=None, extra_source=None):
    bundle = pub.Bundle.__new__(pub.Bundle)
    bundle.name, bundle.version, bundle.repository = "widget", "1.2.3", REPOSITORY
    manifest = {"name": "@example/widget", "version": "1.2.3", "license": "MIT"}
    packaged = {**manifest, **(packaged_changes or {})}
    data = {"source": {prefix + "package.json": pub.json_bytes(manifest), **(extra_source or {})},
            "receipt": {"check": "js-package"}, "identity": {"producing_commit": COMMIT},
            "artifacts": {"example-widget-1.2.3.tgz": tar({"package/package.json": pub.json_bytes(packaged)})}}
    bundle.inspect_javascript(data, "npm")
    return data


def generated_jsr_fixture():
    bundle = pub.Bundle.__new__(pub.Bundle)
    bundle.name, bundle.version, bundle.repository = "widget", "1.2.3", REPOSITORY
    bundle.create_jsr_receipt = True
    manifest = {"name": "@example/widget", "version": "1.2.3", "license": "LGPL-3.0-only"}
    readme = b"Rebuild: tar -xzf dependencies.tar.gz\nOriginal initialization instructions.\n"
    vendor = tar({"vendor/dependency/LICENSE": b"license", "vendor/dependency/lib.rs": b"source"})
    original = {"package.json": pub.json_bytes({**manifest, "gitHead": COMMIT}),
                "index.js": b"export const answer = 42;\n", "index.d.ts": b"export const answer: number;\n",
                "wasm/widget_bg.wasm": b"\0asmfixture", "LICENSES/LGPL.txt": b"license",
                "README.md": readme, "source/README.md": readme,
                "source/dependencies.tar.gz": gzip.compress(vendor, mtime=0)}
    npm_name, jsr_name = "example-widget-1.2.3.tgz", "example-widget-1.2.3-jsr.tar.gz"
    npm = tar({"package/" + path: value for path, value in original.items()})
    origin = {"schema": 1, "package": manifest["name"], "version": "1.2.3", "source_commit": COMMIT,
              "source_sha256": "d" * 64, "archive_url": "https://registry.npmjs.org/@example/widget/-/widget-1.2.3.tgz",
              "archive_sha256": pub.sha(npm)}
    source = {"web/package.json": pub.json_bytes(manifest), "web/index.js": original["index.js"],
              "web/index.d.ts": original["index.d.ts"], ".ci/jsr-input.json": pub.json_bytes(origin),
              ".ci/jsr-readme.md": b"JSR usage\n",
              ".ci/jsr-readme-replacements.json": pub.json_bytes([{"original": "Original initialization instructions.", "replacement": "Explicit initialization instructions."}]),
              "web/jsr.json": pub.json_bytes({"name": manifest["name"], "version": "1.2.3", "exports": "./index.js"})}
    published = {**original, "jsr.json": source["web/jsr.json"]}
    published.pop("source/dependencies.tar.gz")
    published["source/dependencies.tar.xz"] = lzma.compress(vendor)
    transformations = {"source/dependencies.tar.xz": {
        "kind": "lossless gzip-to-xz recompression", "original_path": "source/dependencies.tar.gz",
        "original_sha256": pub.sha(original["source/dependencies.tar.gz"]),
        "published_sha256": pub.sha(published["source/dependencies.tar.xz"]), "uncompressed_sha256": pub.sha(vendor)}}
    for path in ("README.md", "source/README.md"):
        published[path] = original[path].replace(b"dependencies.tar.gz", b"dependencies.tar.xz").replace(b"tar -xzf dependencies.tar.xz", b"tar -xJf dependencies.tar.xz")
        published[path] = published[path].replace(b"Original initialization instructions.", b"Explicit initialization instructions.")
        transformations[path] = {"kind": "source archive extraction and Wasm initialization instructions", "original_sha256": pub.sha(original[path]), "published_sha256": pub.sha(published[path])}
        if path == "README.md":
            published[path] = source[".ci/jsr-readme.md"] + b"\n" + published[path]
            transformations[path].update(kind="JSR usage and source archive extraction instructions", published_sha256=pub.sha(published[path]))
    receipt = {"check": "jsr-package", "layout": "web-wasm-v1", "original_npm": origin,
               "original_npm_artifact": npm_name, "published_files": {path: pub.sha(value) for path, value in published.items()},
               "source_transformations": transformations}
    data = {"source": source, "receipt": receipt, "entry": {}, "identity": {"producing_commit": "f" * 40},
            "artifacts": {npm_name: npm, jsr_name: tar(published)}}
    return bundle, data, published


def native_python_fixture():
    bundle = pub.Bundle.__new__(pub.Bundle)
    bundle.name, bundle.version = "widget", "1.2.3"
    metadata = b"Metadata-Version: 2.4\nName: widget\nVersion: 1.2.3\nRequires-Python: >=3.10\nLicense-Expression: LGPL-3.0-only\nLicense-File: LICENSES/LGPL.txt\n\nDescription\n"
    project = b'[build-system]\nrequires=["maturin==1.15.0"]\nbuild-backend="maturin"\n[project]\nname="widget"\nversion="1.2.3"\nrequires-python=">=3.10"\nlicense="LGPL-3.0-only"\n[tool.maturin]\nbindings="pyo3"\nmodule-name="widget._native"\n'
    source = {"py/pyproject.toml": project, "py/Cargo.toml": b"[package]\nname='widget-python'\nversion='1.2.3'\n",
              "py/Cargo.lock": b"version=4\n", "py/src/lib.rs": b"// Rust binding\n",
              "py/widget/__init__.py": b"from ._native import Conversation\n", "LICENSES/LGPL.txt": b"LGPL license"}
    sdist_files = {path.removeprefix("py/"): value for path, value in source.items()}
    sdist_files["PKG-INFO"] = metadata
    sdist = tar({"widget-1.2.3/" + path: value for path, value in sdist_files.items()})
    extension = "widget/_native.abi3.so"
    info = "widget-1.2.3.dist-info/"
    wheel_files = {extension: b"\x7fELFfixture", "widget/__init__.py": source["py/widget/__init__.py"],
                   "widget/source/widget-1.2.3.tar.gz": sdist, info + "METADATA": metadata,
                   info + "licenses/LICENSES/LGPL.txt": source["LICENSES/LGPL.txt"],
                   info + "WHEEL": b"Wheel-Version: 1.0\nGenerator: maturin 1.15.0\nRoot-Is-Purelib: false\nTag: cp310-abi3-manylinux_2_34_x86_64\nTag: cp310-abi3-linux_x86_64\n"}
    record = io.StringIO(newline="")
    writer = csv.writer(record)
    for path, value in sorted(wheel_files.items()):
        writer.writerow((path, "sha256=" + base64.urlsafe_b64encode(bytes.fromhex(pub.sha(value))).decode().rstrip("="), str(len(value))))
    writer.writerow((info + "RECORD", "", ""))
    wheel_files[info + "RECORD"] = record.getvalue().encode()
    output = io.BytesIO()
    with zipfile.ZipFile(output, "w") as archive:
        for path, value in wheel_files.items():
            archive.writestr(path, value)
    filename = "widget-1.2.3-cp310-abi3-manylinux_2_34_x86_64.linux_x86_64.whl"
    receipt = {"layout": "maturin-v1", "native_wheel": {"filename": filename,
               "tags": ["cp310-abi3-manylinux_2_34_x86_64", "cp310-abi3-linux_x86_64"],
               "extension": extension, "extension_sha256": pub.sha(wheel_files[extension])},
               "wheel_files": {path: pub.sha(value) for path, value in wheel_files.items()},
               "sdist_files": {path: pub.sha(value) for path, value in sdist_files.items()},
               "sdist_source_files": {path.removeprefix("py/"): path for path in source}}
    data = {"source": source, "receipt": receipt, "artifacts": {filename: output.getvalue(), "widget-1.2.3.tar.gz": sdist}}
    return bundle, data, wheel_files, sdist_files


def recovery_fixture(*, legacy=False):
    artifact = "widget-1.2.3-py3-none-any.whl"
    identity = {"registry": "pypi", "repository": REPOSITORY, "package": "widget", "version": "1.2.3",
                "producing_commit": COMMIT, "source_sha256": "d" * 64, "tag_commit": "c" * 40,
                "artifacts": {artifact: pub.sha(b"tested wheel")}}
    bundle = SimpleNamespace(repository=REPOSITORY, version="1.2.3", digest="e" * 64,
                             channels={"pypi": {"identity": identity, "artifacts": {artifact: b"tested wheel"}}})
    stem = "publication-pypi-" + artifact
    intent = {"schema": 1, "status": "attempted", "identity": {**identity, "unit": artifact}, "owner": "original-owner"}
    outcome = {"kind": "http-response", "method": "POST", "origin": "https://upload.pypi.org",
               "status": 429, "observed_at": 1000, "retry_after_seconds": None}
    if legacy:
        outcome = {"kind": "prior-session-report", "method": "POST", "origin": "https://upload.pypi.org",
                   "status": 429, "observed_not_after": 1000, "raw_response_retained": False, "retry_after_seconds": None,
                   "report": {"commit": "1" * 40, "path": "task/RECEIPT.md", "blob": "2" * 40,
                              "sha256": "3" * 64, "lines": [10, 12]}}
    error = {"schema": 1, "status": "upload-failed", "identity": intent["identity"],
             "bundle_sha256": bundle.digest, "attempt": 0, "intent_sha256": pub.sha(pub.json_bytes(intent)), "outcome": outcome}
    remote = FakeRemote()
    remote.assets[stem + "-intent.json"] = copy.deepcopy(intent)
    if not legacy:
        remote.assets[stem + "-error.json"] = copy.deepcopy(error)
    return bundle, remote, stem, intent, error


class FakeRemote:
    def __init__(self):
        self.assets, self.visible, self.uploads, self.events = {}, set(), [], []
        self.lose_response = False
        self.fail_upload = False
        self.race = False

    def verify_tag(self):
        self.events.append("tag")

    def present(self, registry, data):
        self.events.append("get")
        return set(self.visible)

    def asset(self, name):
        return self.assets.get(name)

    def credentials(self, registry, data=None):
        return "fixture-token"

    def persist_asset(self, name, value, *, claim=False):
        if self.race and claim:
            self.assets[name] = {**value, "owner": "another-attempt"}
        if name in self.assets:
            pub.require(not claim, "Another attempt owns intent")
            pub.require(self.assets[name] == value, "Asset conflict")
        self.assets[name] = copy.deepcopy(value)
        self.events.append("intent" if claim else "complete")

    def upload(self, registry, unit, data, token):
        self.events.append("upload")
        self.uploads.append(unit)
        if self.fail_upload:
            raise pub.Failure("lost response before visibility")
        self.visible.add(unit)
        if self.lose_response:
            raise pub.Failure("lost accepted response")

    def wait_for_visibility(self, registry, unit, data, response):
        return self.present(registry, data)


class FakeHttp:
    def __init__(self, response=None):
        self.calls = []
        self.response = response

    def request(self, method, url, **kwargs):
        self.calls.append((method, url, kwargs))
        return pub.json_bytes(self.response or {"ok": True})

    def json(self, method, url, **kwargs):
        return json.loads(self.request(method, url, **kwargs))


class RoutingHttp(FakeHttp):
    """Answer requests by method and URL prefix; any unexpected request fails the test."""

    def __init__(self, routes):
        super().__init__()
        self.routes = routes

    def request(self, method, url, **kwargs):
        self.calls.append((method, url, kwargs))
        for (route_method, prefix), response in self.routes.items():
            if method == route_method and url.startswith(prefix):
                if isinstance(response, Exception):
                    raise response
                return pub.json_bytes(response)
        raise AssertionError("Unexpected fixture request: " + method + " " + url)


ID_TOKEN_URL = "https://token.actions.example.invalid/fixture-token-request?api-version=2.0"
ID_TOKEN = "fixture-header.fixture-claims.fixture-signature"


def trusted_environment(registry, **changes):
    environment = {"GH_TOKEN": "fixture-gh", pub.AUTH_MODES[registry]: "trusted", "GITHUB_ACTIONS": "true",
                   "ACTIONS_ID_TOKEN_REQUEST_URL": ID_TOKEN_URL, "ACTIONS_ID_TOKEN_REQUEST_TOKEN": "fixture-request-token"}
    environment.update(changes)
    return {key: value for key, value in environment.items() if value is not None}


def id_token_request(call):
    method, url, options = call
    parsed = pub.urllib.parse.urlsplit(url)
    query = pub.urllib.parse.parse_qs(parsed.query)
    return method, parsed._replace(query="").geturl(), query, options["headers"]


def trusted_channel(registry):
    identity = {"registry": registry, "repository": REPOSITORY, "package": "widget", "version": "1.2.3"}
    if registry == "npm":
        return {"identity": identity, "js_name": "@example/widget", "artifacts": {"example-widget-1.2.3.tgz": b"npm bytes"}}
    return {"identity": identity, "artifacts": {"widget-1.2.3-py3-none-any.whl": b"wheel bytes"}}


class PublicationTests(unittest.TestCase):
    def test_trusted_jsr_binds_github_id_token_to_the_exact_uploaded_body(self):
        bundle = SimpleNamespace(name="widget", version="1.2.3", repository=REPOSITORY)
        files = {"jsr.json": b'{"name":"@example/widget","version":"1.2.3","exports":"./src/index.ts"}',
                 "src/index.ts": b"export const answer = 42;\n"}
        data = {"js_name": "@example/widget", "jsr_files": files}
        task = {"id": "12345678-1234-1234-1234-123456789abc", "packageScope": "example",
                "packageName": "widget", "packageVersion": "1.2.3", "status": "pending"}
        http = RoutingHttp({("GET", "https://token.actions.example.invalid/"): {"value": ID_TOKEN},
                            ("POST", "https://api.jsr.io/"): task})
        remote = pub.Remote(bundle, http=http, environment=trusted_environment("jsr"))
        token = remote.credentials("jsr", data)
        self.assertIsInstance(token, pub.GithubOidcToken)
        method, base, query, headers = id_token_request(http.calls[0])
        self.assertEqual((method, base), ("GET", ID_TOKEN_URL.split("?")[0]))
        self.assertEqual(query["api-version"], ["2.0"])
        self.assertEqual(headers["Authorization"], "Bearer fixture-request-token")
        body = pub.gzip.compress(tar_for_jsr(files), mtime=0)
        self.assertEqual(json.loads(query["audience"][0]), {"permissions": [{
            "permission": "package/publish", "scope": "example", "package": "widget",
            "version": "1.2.3", "tarballHash": "sha256-" + pub.sha(body)}]})
        self.assertEqual(remote.upload("jsr", "source", data, token)["jsr_task"], task["id"])
        method, url, options = http.calls[1]
        self.assertEqual((method, url), ("POST", "https://api.jsr.io/scopes/example/packages/widget/versions/1.2.3?config=/jsr.json"))
        self.assertEqual(options["headers"]["Authorization"], "githuboidc " + ID_TOKEN)
        self.assertEqual(options["data"], body)
        self.assertEqual(len(http.calls), 2)

    def test_trusted_npm_exchanges_id_token_before_the_single_put(self):
        bundle = SimpleNamespace(name="widget", version="1.2.3", repository=REPOSITORY)
        data = {"js_name": "@example/widget", "artifacts": {"widget.tgz": b"exact npm bytes"},
                "js_manifest": {"name": "@example/widget", "version": "1.2.3"}}
        exchange = "https://registry.npmjs.org/-/npm/v1/oidc/token/exchange/package/@example%2fwidget"
        http = RoutingHttp({("GET", "https://token.actions.example.invalid/"): {"value": ID_TOKEN},
                            ("POST", exchange): {"token": "fixture-npm-short-lived"},
                            ("PUT", "https://registry.npmjs.org/@example%2Fwidget"): {"ok": True}})
        remote = pub.Remote(bundle, http=http, environment=trusted_environment("npm"))
        token = remote.credentials("npm", data)
        self.assertEqual(token, "fixture-npm-short-lived")
        _, _, query, _ = id_token_request(http.calls[0])
        self.assertEqual(query["audience"], ["npm:registry.npmjs.org"])
        method, url, options = http.calls[1]
        self.assertEqual((method, url), ("POST", exchange))
        self.assertEqual(options["headers"]["Authorization"], "Bearer " + ID_TOKEN)
        remote.upload("npm", "widget.tgz", data, token)
        method, url, options = http.calls[2]
        self.assertEqual(method, "PUT")
        self.assertEqual(options["headers"]["Authorization"], "Bearer fixture-npm-short-lived")
        self.assertEqual(len(http.calls), 3)

    def test_trusted_pypi_mints_api_token_used_as_dunder_token(self):
        bundle = SimpleNamespace(name="widget", version="1.2.3", repository=REPOSITORY)
        unit = "widget-1.2.3-py3-none-any.whl"
        data = {"artifacts": {unit: b"wheel bytes"},
                "python": {unit: {"fields": [("name", "widget"), ("version", "1.2.3")], "filetype": "bdist_wheel", "pyversion": "py3"}}}
        http = RoutingHttp({("GET", "https://token.actions.example.invalid/"): {"value": ID_TOKEN},
                            ("POST", "https://pypi.org/_/oidc/mint-token"): {"success": True, "token": "fixture-pypi-minted"},
                            ("POST", "https://upload.pypi.org/legacy/"): {}})
        remote = pub.Remote(bundle, http=http, environment=trusted_environment("pypi"))
        token = remote.credentials("pypi", data)
        self.assertEqual(token, "fixture-pypi-minted")
        _, _, query, _ = id_token_request(http.calls[0])
        self.assertEqual(query["audience"], ["pypi"])
        method, url, options = http.calls[1]
        self.assertEqual((method, url), ("POST", "https://pypi.org/_/oidc/mint-token"))
        self.assertEqual(json.loads(options["data"]), {"token": ID_TOKEN})
        remote.upload("pypi", unit, data, token)
        authorization = http.calls[2][2]["headers"]["Authorization"]
        self.assertEqual(pub.base64.b64decode(authorization.removeprefix("Basic ")), b"__token__:fixture-pypi-minted")

    def test_refused_trusted_exchange_fails_before_any_intent_or_upload(self):
        refusals = {"npm": ("POST", "https://registry.npmjs.org/-/npm/v1/oidc/", "https://registry.npmjs.org"),
                    "pypi": ("POST", "https://pypi.org/_/oidc/mint-token", "https://pypi.org")}
        for registry, (method, prefix, origin) in refusals.items():
            for stage in ("exchange", "id-token"):
                with self.subTest(registry=registry, stage=stage):
                    routes = {("GET", "https://token.actions.example.invalid/"): {"value": ID_TOKEN},
                              (method, prefix): pub.HttpFailure(method, origin, 404)}
                    if stage == "id-token":
                        routes[("GET", "https://token.actions.example.invalid/")] = pub.HttpFailure(
                            "GET", "https://token.actions.example.invalid", 403)
                    bundle = SimpleNamespace(repository=REPOSITORY, version="1.2.3", digest="e" * 64,
                                             channels={registry: trusted_channel(registry)})
                    remote = FakeRemote()
                    remote.credentials = pub.Remote(bundle, http=RoutingHttp(routes),
                                                    environment=trusted_environment(registry)).credentials
                    with tempfile.TemporaryDirectory() as temporary:
                        with self.assertRaisesRegex(pub.Failure, "refused") as raised:
                            pub.Publisher(bundle, remote, temporary).execute([registry], True)
                        self.assertEqual(list(Path(temporary).rglob("*-intent.json")), [])
                    self.assertNotIn("fixture", str(raised.exception))
                    self.assertEqual((remote.assets, remote.uploads), ({}, []))

    def test_trusted_mode_without_github_id_token_variables_fails_closed(self):
        bundle = SimpleNamespace(name="widget", version="1.2.3", repository=REPOSITORY)
        data = {"js_name": "@example/widget", "jsr_files": {"jsr.json": b"{}"}}
        for registry in ("npm", "jsr", "pypi"):
            for missing in ("ACTIONS_ID_TOKEN_REQUEST_URL", "ACTIONS_ID_TOKEN_REQUEST_TOKEN", "GITHUB_ACTIONS"):
                with self.subTest(registry=registry, missing=missing):
                    http = FakeHttp()
                    environment = trusted_environment(registry, **{missing: None, pub.TOKEN_NAMES[registry]: "fixture-static"})
                    with self.assertRaisesRegex(pub.Failure, "id-token: write") as raised:
                        pub.Remote(bundle, http=http, environment=environment).credentials(registry, data)
                    self.assertEqual(http.calls, [])
                    self.assertNotIn("fixture", str(raised.exception))

    def test_token_mode_stays_default_and_unknown_modes_are_refused(self):
        bundle = SimpleNamespace(name="widget", version="1.2.3", repository=REPOSITORY)
        for registry in ("npm", "jsr", "pypi"):
            with self.subTest(registry=registry):
                http = FakeHttp()
                environment = {"GH_TOKEN": "fixture-gh", pub.TOKEN_NAMES[registry]: "fixture-static",
                               "GITHUB_ACTIONS": "true", "ACTIONS_ID_TOKEN_REQUEST_URL": ID_TOKEN_URL,
                               "ACTIONS_ID_TOKEN_REQUEST_TOKEN": "fixture-request-token"}
                token = pub.Remote(bundle, http=http, environment=environment).credentials(registry, {})
                self.assertEqual(token, "fixture-static")
                self.assertNotIsInstance(token, pub.GithubOidcToken)
                explicit = pub.Remote(bundle, http=http, environment={**environment, pub.AUTH_MODES[registry]: "token"})
                self.assertEqual(explicit.credentials(registry, {}), "fixture-static")
                self.assertEqual(http.calls, [])
                with self.assertRaisesRegex(pub.Failure, "Unknown " + pub.AUTH_MODES[registry]):
                    pub.Remote(bundle, http=http, environment={**environment, pub.AUTH_MODES[registry]: "oidc"}).credentials(registry, {})
                with self.assertRaisesRegex(pub.Failure, pub.TOKEN_NAMES[registry] + " is missing"):
                    pub.Remote(bundle, http=http, environment={"GH_TOKEN": "fixture-gh"}).credentials(registry, {})
        http = FakeHttp({"id": "12345678-1234-1234-1234-123456789abc", "packageScope": "example",
                         "packageName": "widget", "packageVersion": "1.2.3"})
        pub.Remote(bundle, http=http, environment={}).upload(
            "jsr", "source", {"js_name": "@example/widget", "jsr_files": {"jsr.json": b"{}"}}, "fixture-static")
        self.assertEqual(http.calls[0][2]["headers"]["Authorization"], "Bearer fixture-static")

    def test_generated_jsr_keeps_original_runtime_and_lossless_source_kit(self):
        bundle, data, published = generated_jsr_fixture()
        bundle.inspect_javascript(data, "jsr")
        self.assertEqual(data["jsr_files"], published)
        self.assertEqual(data["identity"]["source_transformations"], data["receipt"]["source_transformations"])
        bundle.create_jsr_receipt = False
        bundle.inspect_javascript(data, "jsr")

    def test_generated_jsr_rejects_runtime_license_and_recompression_tampering(self):
        for path, value in (("index.js", b"changed runtime"), ("LICENSES/LGPL.txt", b"changed license"),
                            ("source/dependencies.tar.xz", lzma.compress(b"different vendor")),
                            ("README.md", b"changed beyond extraction instructions")):
            with self.subTest(path=path), self.assertRaises(pub.Failure):
                bundle, data, published = generated_jsr_fixture()
                published[path] = value
                data["artifacts"]["example-widget-1.2.3-jsr.tar.gz"] = tar(published)
                data["receipt"]["published_files"] = {name: pub.sha(content) for name, content in published.items()}
                bundle.inspect_javascript(data, "jsr")

    def test_generated_jsr_rejects_origin_inventory_and_transform_changes(self):
        for change in ("origin", "inventory", "transform", "replacement"):
            with self.subTest(change=change), self.assertRaises(pub.Failure):
                bundle, data, _ = generated_jsr_fixture()
                if change == "origin":
                    data["receipt"]["original_npm"]["archive_sha256"] = "0" * 64
                elif change == "inventory":
                    del data["receipt"]["published_files"]["README.md"]
                elif change == "replacement":
                    data["source"][".ci/jsr-readme-replacements.json"] = pub.json_bytes([{"original": "missing paragraph", "replacement": "changed instructions"}])
                else:
                    data["receipt"]["source_transformations"] = {}
                bundle.inspect_javascript(data, "jsr")

    def test_native_python_validates_compressed_tags_record_and_corresponding_source(self):
        bundle, data, _, _ = native_python_fixture()
        bundle.inspect_python(data)
        wheel = data["receipt"]["native_wheel"]["filename"]
        self.assertEqual(data["python"][wheel]["pyversion"], "cp310")

    def test_native_python_rejects_metadata_record_source_and_license_tampering(self):
        for change in ("tag", "record", "source", "license", "extension", "filename", "source-map", "facade", "metadata"):
            with self.subTest(change=change), self.assertRaises(pub.Failure):
                bundle, data, wheel_files, _ = native_python_fixture()
                filename = data["receipt"]["native_wheel"]["filename"]
                if change == "tag":
                    wheel_files["widget-1.2.3.dist-info/WHEEL"] = wheel_files["widget-1.2.3.dist-info/WHEEL"].replace(b"Root-Is-Purelib: false", b"Root-Is-Purelib: true")
                elif change == "record":
                    wheel_files["widget-1.2.3.dist-info/RECORD"] = b""
                elif change == "source":
                    wheel_files["widget/source/widget-1.2.3.tar.gz"] = b"different sdist"
                elif change == "license":
                    wheel_files["widget-1.2.3.dist-info/licenses/LICENSES/LGPL.txt"] = b"changed license"
                elif change == "extension":
                    data["receipt"]["native_wheel"]["extension_sha256"] = "0" * 64
                elif change == "source-map":
                    del data["receipt"]["sdist_source_files"]["src/lib.rs"]
                elif change == "facade":
                    wheel_files["widget/__init__.py"] = b"changed Python facade\n"
                elif change == "metadata":
                    wheel_files["widget-1.2.3.dist-info/METADATA"] = wheel_files["widget-1.2.3.dist-info/METADATA"].replace(b"LGPL-3.0-only", b"MIT")
                else:
                    data["artifacts"][filename.replace("cp310", "cp311")] = data["artifacts"].pop(filename)
                if change not in {"extension", "filename", "source-map"}:
                    if change != "record":
                        record = io.StringIO(newline="")
                        writer = csv.writer(record)
                        for path, value in sorted(wheel_files.items()):
                            if path.endswith(".dist-info/RECORD"):
                                writer.writerow((path, "", ""))
                            else:
                                writer.writerow((path, "sha256=" + base64.urlsafe_b64encode(bytes.fromhex(pub.sha(value))).decode().rstrip("="), str(len(value))))
                        wheel_files["widget-1.2.3.dist-info/RECORD"] = record.getvalue().encode()
                    output = io.BytesIO()
                    with zipfile.ZipFile(output, "w") as archive:
                        for path, value in wheel_files.items():
                            archive.writestr(path, value)
                    data["artifacts"][filename] = output.getvalue()
                    data["receipt"]["wheel_files"] = {path: pub.sha(value) for path, value in wheel_files.items()}
                bundle.inspect_python(data)

    def test_workspace_lock_prunes_unreachable_packages_and_shortens_references(self):
        source, packaged = workspace_locks()
        bundle, _ = cargo_fixture(source_lock=lock_bytes(source), packaged_lock=lock_bytes(packaged))
        self.assertEqual(set(bundle.channels["cargo"]["artifacts"]), {"widget-1.2.3.crate"})

    def test_workspace_lock_rejects_changed_dependency_identity_or_metadata(self):
        source, original = workspace_locks()
        for key, value in (("version", "2.0.1"), ("checksum", "4" * 64),
                           ("source", "registry+https://unreviewed.invalid/index")):
            with self.subTest(key=key), self.assertRaises(pub.Failure):
                packaged = copy.deepcopy(original)
                packaged[2][key] = value
                cargo_fixture(source_lock=lock_bytes(source), packaged_lock=lock_bytes(packaged))

    def test_workspace_lock_rejects_missing_reachable_package_or_edge(self):
        source, original = workspace_locks()
        for remove_package in (False, True):
            with self.subTest(remove_package=remove_package), self.assertRaises(pub.Failure):
                packaged = copy.deepcopy(original)
                packaged[1]["dependencies"] = []
                if remove_package:
                    packaged.pop()
                cargo_fixture(source_lock=lock_bytes(source), packaged_lock=lock_bytes(packaged))

    def test_workspace_lock_rejects_ambiguous_reference_duplicate_and_extra_package(self):
        source, packaged = workspace_locks()
        ambiguous = copy.deepcopy(source)
        ambiguous[1]["dependencies"] = ["encoding"]
        with self.assertRaises(pub.Failure):
            cargo_fixture(source_lock=lock_bytes(ambiguous), packaged_lock=lock_bytes(packaged))
        for extra in (packaged[2], source[4]):
            with self.subTest(extra=extra), self.assertRaises(pub.Failure):
                cargo_fixture(source_lock=lock_bytes(source), packaged_lock=lock_bytes(packaged + [extra]))

    def test_npm_supports_both_source_layouts_and_exact_generated_git_head(self):
        for prefix in ("js/@example/widget/", "web/"):
            for changes in ({}, {"gitHead": COMMIT}):
                with self.subTest(prefix=prefix, changes=changes):
                    data = npm_fixture(prefix, packaged_changes=changes)
                    self.assertEqual(data["js_manifest"].get("gitHead"), changes.get("gitHead"))

    def test_npm_rejects_wrong_git_head_and_other_generated_manifest_changes(self):
        for changes in ({"gitHead": "d" * 40}, {"gitHead": COMMIT, "license": "unreviewed"}):
            with self.subTest(changes=changes), self.assertRaises(pub.Failure):
                npm_fixture(packaged_changes=changes)

    def test_npm_rejects_conflicting_discovered_manifests(self):
        manifest = {"name": "@example/widget", "version": "1.2.3", "license": "MIT"}
        data = npm_fixture(extra_source={"js/@example/widget/package.json": pub.json_bytes(manifest)})
        self.assertEqual(data["js_name"], "@example/widget")
        with self.assertRaisesRegex(pub.Failure, "conflicting"):
            npm_fixture(extra_source={"js/@example/widget/package.json": pub.json_bytes({**manifest, "license": "unreviewed"})})

    def test_npm_scope_follows_committed_manifest_not_repository_owner(self):
        def inspect(source, name):
            bundle = pub.Bundle.__new__(pub.Bundle)
            bundle.name, bundle.version, bundle.repository = "widget", "1.2.3", REPOSITORY
            manifest = {"name": name, "version": "1.2.3", "license": "MIT"}
            filename = name.replace("@", "").replace("/", "-") + "-1.2.3.tgz"
            data = {"source": {path: pub.json_bytes(manifest) for path in source},
                    "receipt": {"check": "js-package"}, "identity": {"producing_commit": COMMIT},
                    "artifacts": {filename: tar({"package/package.json": pub.json_bytes(manifest)})}}
            bundle.inspect_javascript(data, "npm")
            return data["js_name"]
        self.assertEqual(inspect(["js/@labs/widget/package.json"], "@labs/widget"), "@labs/widget")
        self.assertEqual(inspect(["web/package.json"], "@labs/widget"), "@labs/widget")
        with self.assertRaisesRegex(pub.Failure, "Ambiguous"):
            inspect(["js/@labs/widget/package.json", "js/@other/widget/package.json"], "@labs/widget")
        with self.assertRaisesRegex(pub.Failure, "identity mismatch"):
            inspect(["js/@labs/widget/package.json"], "@other/widget")

    def test_import_keeps_old_producer_and_distinct_tag(self):
        bundle, files = cargo_fixture()
        self.assertEqual(bundle.channels["cargo"]["identity"]["producing_commit"], COMMIT)
        self.assertNotEqual(bundle.manifest["tag_commit"], COMMIT)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            artifacts = root / "prepared"
            artifacts.mkdir()
            entry = bundle.manifest["channels"]["cargo"]
            (root / "source.tar").write_bytes(files[entry["source_archive"]])
            (artifacts / "SOURCE_COMMIT").write_text(COMMIT + "\n")
            (artifacts / "rust-package.json").write_bytes(files[entry["receipt"]])
            filename, item = next(iter(entry["artifacts"].items()))
            (artifacts / filename).write_bytes(files[item["path"]])
            (artifacts / "SHA256SUMS").write_text(item["sha256"] + "  " + filename + "\n")
            request = {"schema": 1, "repository": REPOSITORY, "tag_commit": "c" * 40,
                       "channels": {"cargo": {"source_archive": str(root / "source.tar"), "artifacts": str(artifacts)}}}
            (root / "import.json").write_bytes(pub.json_bytes(request))
            result = pub.import_bundle(root / "import.json", root / "release.tar", REPOSITORY)
            imported = pub.Bundle((root / "release.tar").read_bytes(), result["sha256"], REPOSITORY)
            self.assertEqual(imported.channels["cargo"]["artifacts"], bundle.channels["cargo"]["artifacts"])
            self.assertEqual(result, pub.import_bundle(root / "import.json", root / "release.tar", REPOSITORY))
            (root / "release.tar").write_bytes(b"different")
            with self.assertRaises(pub.Failure):
                pub.import_bundle(root / "import.json", root / "release.tar", REPOSITORY)

    def test_archive_traversal_and_links_refused(self):
        with self.assertRaises(pub.Failure):
            pub.archive_files(tar({"../escape": b"bad"}))
        output = io.BytesIO()
        with tarfile.open(fileobj=output, mode="w") as archive:
            member = tarfile.TarInfo("link")
            member.type, member.linkname = tarfile.SYMTYPE, "elsewhere"
            archive.addfile(member)
        with self.assertRaises(pub.Failure):
            pub.archive_files(output.getvalue())

    def test_tampered_bundle_and_wrong_source_refused(self):
        bundle, files = cargo_fixture()
        with self.assertRaises(pub.Failure):
            pub.Bundle(tar(files), "0" * 64, REPOSITORY)
        manifest = json.loads(files["release.json"])
        manifest["channels"]["cargo"]["source_commit"] = "d" * 40
        files["release.json"] = pub.json_bytes(manifest)
        payload = tar(files)
        with self.assertRaises(pub.Failure):
            pub.Bundle(payload, pub.sha(payload), REPOSITORY)

    def test_intent_is_durable_before_upload(self):
        bundle, _ = cargo_fixture()
        remote = FakeRemote()
        with tempfile.TemporaryDirectory() as temporary:
            result = pub.Publisher(bundle, remote, temporary).execute(["cargo"], True)
            self.assertEqual(result["cargo"]["missing"], [])
            self.assertLess(remote.events.index("intent"), remote.events.index("upload"))
            journals = list(Path(temporary).rglob("*-intent.json"))
            self.assertEqual(len(journals), 1)
            self.assertTrue(json.loads(journals[0].read_text())["owner"])

    def test_ambiguous_upload_is_never_retried_locally_or_elsewhere(self):
        bundle, _ = cargo_fixture()
        remote = FakeRemote()
        remote.fail_upload = True
        with tempfile.TemporaryDirectory() as first, tempfile.TemporaryDirectory() as second:
            for root in (first, first, second):
                with self.assertRaises(pub.Failure):
                    pub.Publisher(bundle, remote, root).execute(["cargo"], True)
            self.assertEqual(len(remote.uploads), 1)

    def test_remote_claim_loser_never_uploads(self):
        bundle, _ = cargo_fixture()
        remote = FakeRemote()
        remote.race = True
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaises(pub.Failure):
                pub.Publisher(bundle, remote, temporary).execute(["cargo"], True)
            self.assertEqual(remote.uploads, [])

    def test_real_remote_claim_requires_exclusive_durable_ownership(self):
        bundle, _ = cargo_fixture()
        http = FakeHttp()
        remote = pub.Remote(bundle, http=http, environment={"GH_TOKEN": "fixture-gh"})
        remote.release = {"id": 42}
        intent = {"schema": 1, "owner": "this-attempt", "identity": {"registry": "cargo"}}
        with mock.patch.object(remote, "asset", return_value=intent):
            with self.assertRaisesRegex(pub.Failure, "already owns"):
                remote.persist_asset("intent.json", intent, claim=True)
        self.assertEqual(http.calls, [])
        with mock.patch.object(remote, "asset", side_effect=[None, {**intent, "owner": "other-attempt"}]):
            with self.assertRaisesRegex(pub.Failure, "not durably verified"):
                remote.persist_asset("intent.json", intent, claim=True)
        self.assertEqual([method for method, _, _ in http.calls], ["POST"])
        self.assertEqual(json.loads(http.calls[0][2]["data"]), intent)

    def test_accepted_lost_response_reconciles_exact_bytes(self):
        bundle, _ = cargo_fixture()
        remote = FakeRemote()
        remote.lose_response = True
        with tempfile.TemporaryDirectory() as temporary:
            publisher = pub.Publisher(bundle, remote, temporary)
            self.assertEqual(publisher.execute(["cargo"], True)["cargo"]["missing"], [])
            self.assertEqual(publisher.execute(["cargo"], True)["cargo"]["missing"], [])
            self.assertEqual(len(remote.uploads), 1)

    def test_status_does_not_upload_or_create_remote_journals(self):
        bundle, _ = cargo_fixture()
        remote = FakeRemote()
        with tempfile.TemporaryDirectory() as temporary:
            self.assertTrue(pub.Publisher(bundle, remote, temporary).execute(["cargo"])["cargo"]["missing"])
            self.assertEqual(remote.uploads, [])
            self.assertEqual(remote.assets, {})

    def test_http_error_keeps_safe_status_and_retry_after_without_retry(self):
        class Opener:
            def open(self, request, timeout):
                raise pub.urllib.error.HTTPError(request.full_url, 429, "fixture-private-body",
                                                 {"Retry-After": "7200", "Set-Cookie": "fixture-secret"}, None)
        with mock.patch.object(pub.urllib.request, "build_opener", return_value=Opener()) as opener, mock.patch.object(pub.time, "time", return_value=1000):
            with self.assertRaises(pub.HttpFailure) as raised:
                pub.Http().request("POST", "https://upload.pypi.org/legacy/", headers={"Authorization": "fixture-secret"})
        self.assertEqual(opener.call_count, 1)
        self.assertEqual(raised.exception.outcome, {"kind": "http-response", "method": "POST", "origin": "https://upload.pypi.org",
                                                   "status": 429, "observed_at": 1000, "retry_after_seconds": 7200})
        self.assertNotIn("fixture", str(raised.exception) + json.dumps(raised.exception.outcome))
        with mock.patch.object(pub.time, "time", return_value=1000):
            dated = pub.HttpFailure("POST", "https://upload.pypi.org", 429, "Thu, 01 Jan 1970 02:00:00 GMT")
            unknown = pub.HttpFailure("POST", "https://upload.pypi.org", 429, "unknown")
        self.assertEqual(dated.outcome["retry_after_seconds"], 6200)
        self.assertIsNone(unknown.outcome["retry_after_seconds"])

    def test_429_capture_is_durable_but_normal_publish_never_retries(self):
        bundle, remote, stem, _, _ = recovery_fixture()
        remote.assets.clear()
        def rejected(*args):
            remote.uploads.append(args[1])
            raise pub.HttpFailure("POST", "https://upload.pypi.org", 429)
        remote.upload = rejected
        with tempfile.TemporaryDirectory() as temporary:
            publisher = pub.Publisher(bundle, remote, temporary)
            for _ in range(2):
                with self.assertRaises(pub.Failure):
                    publisher.execute(["pypi"], True)
            error = remote.assets[stem + "-error.json"]
            self.assertEqual(error["outcome"]["status"], 429)
            self.assertEqual(error["intent_sha256"], pub.sha(pub.json_bytes(remote.assets[stem + "-intent.json"])))
            self.assertEqual(len(remote.uploads), 1)
            self.assertEqual(pub.regular_bytes(publisher.root / (stem + "-error.json")), pub.json_bytes(error))

    def test_explicit_recovery_preserves_original_and_uploads_once_across_runners(self):
        for legacy in (False, True):
            with self.subTest(legacy=legacy):
                bundle, remote, stem, intent, error = recovery_fixture(legacy=legacy)
                digest = pub.sha(pub.json_bytes(error))
                with tempfile.TemporaryDirectory() as first, tempfile.TemporaryDirectory() as second, mock.patch.object(pub.time, "time", return_value=10000):
                    for root in (first, second):
                        result = pub.Publisher(bundle, remote, root).recover_rejected(error, digest)
                        self.assertEqual(result["pypi"]["missing"], [])
                    self.assertEqual(len(remote.uploads), 1)
                    self.assertEqual(remote.assets[stem + "-intent.json"], intent)
                    claim = remote.assets[stem + "-attempt-1-intent.json"]
                    self.assertEqual(claim["previous_intent_sha256"], pub.sha(pub.json_bytes(intent)))
                    self.assertEqual(claim["rejection_sha256"], digest)
                    self.assertIn(stem + "-attempt-1-complete.json", remote.assets)
                    remote.visible.clear()
                    with self.assertRaises(pub.Failure):
                        pub.Publisher(bundle, remote, second).recover_rejected(error, digest)
                    self.assertEqual(len(remote.uploads), 1)

    def test_recovery_respects_client_cooldown_and_longer_retry_after(self):
        bundle, remote, _, _, error = recovery_fixture()
        for delay, now in ((None, 4599), (7200, 8199)):
            error["outcome"]["retry_after_seconds"] = delay
            with tempfile.TemporaryDirectory() as temporary, mock.patch.object(pub.time, "time", return_value=now):
                with self.assertRaisesRegex(pub.Failure, "pending until"):
                    pub.Publisher(bundle, remote, temporary).recover_rejected(error, pub.sha(pub.json_bytes(error)))
        self.assertEqual(remote.uploads, [])

    def test_recovery_refuses_uncertain_wrong_source_and_unrecorded_outcomes(self):
        edits = (lambda x: x["outcome"].update(status=503),
                 lambda x: x["outcome"].update(kind="uncertain"),
                 lambda x: x["outcome"].update(method="GET"),
                 lambda x: x["outcome"].update(origin="https://other.invalid"),
                 lambda x: x.update(bundle_sha256="0" * 64),
                 lambda x: x["identity"].update(source_sha256="0" * 64),
                 lambda x: x.update(intent_sha256="0" * 64),
                 lambda x: x["outcome"].update(observed_at=999))
        for edit in edits:
            bundle, remote, _, _, error = recovery_fixture()
            edit(error)
            with tempfile.TemporaryDirectory() as temporary, mock.patch.object(pub.time, "time", return_value=10000):
                with self.assertRaises(pub.Failure):
                    pub.Publisher(bundle, remote, temporary).recover_rejected(error, pub.sha(pub.json_bytes(error)))
            self.assertEqual(remote.uploads, [])

    def test_legacy_report_cannot_supersede_captured_uncertainty_or_acceptance(self):
        for suffix in ("-error.json", "-complete.json", "-response.json"):
            bundle, remote, stem, _, error = recovery_fixture(legacy=True)
            remote.assets[stem + suffix] = {"kind": "uncertain"}
            with tempfile.TemporaryDirectory() as temporary, mock.patch.object(pub.time, "time", return_value=10000):
                with self.assertRaises(pub.Failure):
                    pub.Publisher(bundle, remote, temporary).recover_rejected(error, pub.sha(pub.json_bytes(error)))
            self.assertEqual(remote.uploads, [])

    def test_recovery_race_loser_and_repeated_rejection_never_repeat_upload(self):
        for race in (False, True):
            bundle, remote, _, _, error = recovery_fixture()
            remote.race = race
            if not race:
                def rejected(*args):
                    remote.uploads.append(args[1])
                    raise pub.HttpFailure("POST", "https://upload.pypi.org", 429)
                remote.upload = rejected
            with tempfile.TemporaryDirectory() as first, tempfile.TemporaryDirectory() as second, mock.patch.object(pub.time, "time", return_value=10000):
                for root in (first, second):
                    with self.assertRaises(pub.Failure):
                        pub.Publisher(bundle, remote, root).recover_rejected(error, pub.sha(pub.json_bytes(error)))
            self.assertEqual(len(remote.uploads), 0 if race else 1)


    def test_import_rewrite_preserves_comments_strings_and_templates(self):
        payload = b'''// from 'dep'\n/* export { x } from 'dep'; */\nconst sample = "from 'dep'";\nconst template = `export { z } from 'dep';`;\nexport {\n x, type Y,\n} from 'dep';\n'''
        rewritten = pub.rewrite_imports(payload, {"dep": "1.2.3"})
        self.assertEqual(rewritten.count(b"npm:dep@1.2.3"), 1)
        self.assertIn(b'''const sample = "from 'dep'";''', rewritten)
        self.assertIn(b"/* export { x } from 'dep'; */", rewritten)
        self.assertIn(b"`export { z } from 'dep';`", rewritten)
        with self.assertRaises(pub.Failure):
            pub.rewrite_imports(b"import value from 'dep';", {"dep": "1.2.3"})

    def test_cargo_publish_accepts_documented_response_and_preserves_exact_bytes(self):
        bundle = SimpleNamespace(name="widget", version="1.2.3", repository=REPOSITORY)
        archive = b"exact tested crate bytes"
        metadata = {"name": "widget", "vers": "1.2.3"}
        data = {"artifacts": {"widget-1.2.3.crate": archive}, "cargo_metadata": metadata}
        for response in ({}, {"warnings": {"invalid_categories": [], "invalid_badges": [], "other": []}}, {"ok": True}):
            with self.subTest(response=response):
                http = SimpleNamespace(json=mock.Mock(return_value=response))
                pub.Remote(bundle, http=http, environment={}).upload("cargo", "widget-1.2.3.crate", data, "fixture-token")
                http.json.assert_called_once()
                args, options = http.json.call_args
                self.assertEqual(args, ("PUT", "https://crates.io/api/v1/crates/new"))
                payload = options["data"]
                length = pub.struct.unpack("<I", payload[:4])[0]
                self.assertEqual(json.loads(payload[4:4 + length]), metadata)
                self.assertEqual(pub.struct.unpack("<I", payload[4 + length:8 + length])[0], len(archive))
                self.assertEqual(payload[8 + length:], archive)
        for response in ({"errors": [{"detail": "rejected"}]}, {"ok": False}, {"ok": "true"}, [], None):
            with self.subTest(response=response), self.assertRaises(pub.Failure):
                http = SimpleNamespace(json=mock.Mock(return_value=response))
                pub.Remote(bundle, http=http, environment={}).upload("cargo", "widget-1.2.3.crate", data, "fixture-token")

    def test_npm_upload_is_one_put_with_exact_archive(self):
        bundle = SimpleNamespace(name="widget", version="1.2.3", repository=REPOSITORY)
        http = FakeHttp()
        remote = pub.Remote(bundle, http=http, environment={})
        payload = b"exact tested npm bytes"
        data = {"artifacts": {"widget.tgz": payload}, "js_manifest": {"name": "@example/widget", "version": "1.2.3"}}
        remote.upload("npm", "widget.tgz", data, "fixture-token")
        self.assertEqual(len(http.calls), 1)
        method, url, options = http.calls[0]
        self.assertEqual((method, url), ("PUT", "https://registry.npmjs.org/@example%2Fwidget"))
        document = json.loads(options["data"])
        attachment = next(iter(document["_attachments"].values()))
        self.assertEqual(pub.base64.b64decode(attachment["data"]), payload)
        self.assertNotIn("_nodeVersion", document["versions"]["1.2.3"])

    def test_import_rewrite_protects_regex_literals_and_refuses_ambiguity(self):
        payload = b"const pattern = /export * from 'dep'/;\nexport { value } from 'dep';\n"
        rewritten = pub.rewrite_imports(payload, {"dep": "1.2.3"})
        self.assertIn(b"/export * from 'dep'/", rewritten)
        self.assertEqual(rewritten.count(b"npm:dep@1.2.3"), 1)
        for payload in (b"const ratio = x / y;", b"object.return / import('dep') / 2;", b"value! / import('dep') / 2;"):
            with self.subTest(payload=payload), self.assertRaisesRegex(pub.Failure, "Ambiguous slash"):
                pub.rewrite_imports(payload, {"dep": "1.2.3"})

    def test_import_rewrite_refuses_executable_template_imports(self):
        for payload in (b"const value = `${await import('dep')}`;",
                        b"const value = `${await import /* note */ ('dep')}`;",
                        b"const value = `${`${await import('dep')}`}`;"):
            with self.subTest(payload=payload), self.assertRaisesRegex(pub.Failure, "template interpolation"):
                pub.rewrite_imports(payload, {"dep": "1.2.3"})
        payload = b"const value = `${prefix ?? entry.subject_prefix} ${clean}`;"
        self.assertEqual(pub.rewrite_imports(payload, {"dep": "1.2.3"}), payload)

    def test_python_inspection_requires_exact_pure_wheel_and_sdist_names(self):
        bundle = pub.Bundle.__new__(pub.Bundle)
        bundle.name, bundle.version = "widget", "1.2.3"
        metadata = b"Metadata-Version: 2.4\nName: widget\nVersion: 1.2.3\nRequires-Python: >=3.11\nLicense-Expression: MIT\n\nDescription\n"
        output = io.BytesIO()
        with zipfile.ZipFile(output, "w") as archive:
            archive.writestr("widget-1.2.3.dist-info/METADATA", metadata)
        data = {"source": {"py/pyproject.toml": b'[project]\nname="widget"\nversion="1.2.3"\nrequires-python=">=3.11"\nlicense="MIT"\n'},
                "artifacts": {"widget-1.2.3-py3-none-any.whl": output.getvalue(),
                              "widget-1.2.3.tar.gz": tar({"widget-1.2.3/PKG-INFO": metadata})}}
        bundle.inspect_python(data)
        for filename in ("other-1.2.3-py3-none-any.whl", "widget-1.2.3-cp311-cp311-linux_x86_64.whl"):
            changed = copy.deepcopy(data)
            changed["artifacts"][filename] = changed["artifacts"].pop("widget-1.2.3-py3-none-any.whl")
            with self.subTest(filename=filename), self.assertRaisesRegex(pub.Failure, "exact pure-Python"):
                bundle.inspect_python(changed)

    def test_jsr_upload_uses_gzip_header_and_exact_reviewed_files(self):
        bundle = SimpleNamespace(name="widget", version="1.2.3", repository=REPOSITORY)
        http = FakeHttp({"id": "12345678-1234-1234-1234-123456789abc", "packageScope": "example",
                         "packageName": "widget", "packageVersion": "1.2.3", "status": "pending"})
        remote = pub.Remote(bundle, http=http, environment={})
        files = {"jsr.json": b'{"name":"@example/widget","version":"1.2.3","exports":"./src/index.ts"}',
                 "src/index.ts": b"export const answer = 42;\n"}
        response = remote.upload("jsr", "source", {"js_name": "@example/widget", "jsr_files": files}, "fixture-token")
        self.assertEqual(len(http.calls), 1)
        method, url, options = http.calls[0]
        self.assertEqual((method, url), ("POST", "https://api.jsr.io/scopes/example/packages/widget/versions/1.2.3?config=/jsr.json"))
        self.assertEqual(options["headers"]["Content-Encoding"], "gzip")
        self.assertEqual(pub.archive_files(gzip.decompress(options["data"]))[0], files)
        self.assertEqual(response["jsr_task"], http.response["id"])

    def test_existing_npm_metadata_without_matching_download_is_rejected(self):
        bundle = SimpleNamespace(name="widget", version="1.2.3", repository=REPOSITORY)
        class Http(FakeHttp):
            def request(self, method, url, **kwargs):
                if "/-/" in url:
                    return b"different"
                return pub.json_bytes({"name": "@example/widget", "version": "1.2.3",
                                       "dist": {"tarball": "https://registry.npmjs.org/@example/widget/-/widget.tgz"}})
        remote = pub.Remote(bundle, http=Http(), environment={})
        with self.assertRaisesRegex(pub.Failure, "different bytes"):
            remote.present("npm", {"js_name": "@example/widget", "artifacts": {"widget.tgz": b"expected"}})

    def test_jsr_inventory_requires_all_files_and_producing_vendor_licenses(self):
        bundle = pub.Bundle.__new__(pub.Bundle)
        bundle.name, bundle.version, bundle.repository = "widget", "1.2.3", REPOSITORY
        bundle.create_jsr_receipt = True
        manifest = {"name": "@example/widget", "version": "1.2.3", "dependencies": {"@example/dep": "1.0.0"}}
        prefix = "js/@example/widget/"
        source = {prefix + "package.json": pub.json_bytes(manifest),
                  prefix + "jsr.json": pub.json_bytes({"name": "@example/widget", "version": "1.2.3", "exports": "./src/index.ts"}),
                  prefix + "src/index.ts": b"export { value } from '@example/dep';\n",
                  "README.md": b"readme", "LICENSES/MIT.txt": b"own license",
                  "typst/vendor/dep/LICENSES/MIT.txt": b"dependency license"}
        packaged = {"package/package.json": source[prefix + "package.json"],
                    "package/LICENSES/MIT.txt": source["LICENSES/MIT.txt"],
                    "package/LICENSES/vendor/dep/MIT.txt": source["typst/vendor/dep/LICENSES/MIT.txt"]}
        data = {"source": source, "receipt": {"check": "js-package"}, "entry": {}, "identity": {},
                "artifacts": {"example-widget-1.2.3.tgz": tar(packaged)}}
        bundle.inspect_javascript(data, "jsr")
        self.assertEqual(data["jsr_files"]["LICENSES/vendor/dep/MIT.txt"], b"dependency license")
        self.assertEqual(data["entry"]["published_files"], {name: pub.sha(value) for name, value in data["jsr_files"].items()})
        self.assertIn("src/index.ts", data["entry"]["source_transformations"])
        self.assertEqual(data["identity"]["published_files"], data["entry"]["published_files"])
        self.assertEqual(data["identity"]["source_transformations"], data["entry"]["source_transformations"])
        bundle.create_jsr_receipt = False
        del data["entry"]["published_files"]["README.md"]
        with self.assertRaisesRegex(pub.Failure, "complete published-file"):
            bundle.inspect_javascript(data, "jsr")
        packaged["package/LICENSES/vendor/dep/MIT.txt"] = b"forged license"
        data["artifacts"] = {"example-widget-1.2.3.tgz": tar(packaged)}
        with self.assertRaisesRegex(pub.Failure, "license inventory/bytes differ"):
            bundle.inspect_javascript(data, "jsr")

    def test_pypi_conflicting_or_extra_existing_file_is_rejected(self):
        bundle = SimpleNamespace(name="widget", version="1.2.3", repository=REPOSITORY)
        http = FakeHttp({"urls": [{"filename": "unexpected.whl", "digests": {"sha256": "0" * 64}}]})
        remote = pub.Remote(bundle, http=http, environment={})
        with self.assertRaisesRegex(pub.Failure, "Unexpected/duplicate"):
            remote.present("pypi", {"artifacts": {"widget.whl": b"expected"}})

    def test_public_bundle_fetch_requires_own_release_origin_and_exact_bytes(self):
        payload = b"reviewed bundle bytes"
        class Http(FakeHttp):
            def request(self, method, url, **kwargs):
                self.calls.append((method, url, kwargs))
                return payload
        http = Http()
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "bundle.tar"
            for url in ("http://github.com/example/widget/releases/download/v1.2.3/bundle.tar",
                        "https://github.com/other/widget/releases/download/v1.2.3/bundle.tar",
                        "https://github.com/example/widget/releases/download/../bundle.tar"):
                with self.assertRaises(pub.Failure):
                    pub.fetch_bundle(url, target, pub.sha(payload), REPOSITORY, http)
            self.assertEqual(http.calls, [])
            url = "https://github.com/example/widget/releases/download/v1.2.3/bundle.tar"
            with self.assertRaisesRegex(pub.Failure, "checksum"):
                pub.fetch_bundle(url, target, "0" * 64, REPOSITORY, http)
            self.assertFalse(target.exists())
            pub.fetch_bundle(url, target, pub.sha(payload), REPOSITORY, http)
            self.assertEqual(target.read_bytes(), payload)
            calls = len(http.calls)
            pub.fetch_bundle(url, target, pub.sha(payload), REPOSITORY, http)
            self.assertEqual(len(http.calls), calls)
            self.assertNotIn("headers", http.calls[-1][2])

    def test_authenticated_asset_redirect_drops_authorization(self):
        requests = []
        class Response:
            def __enter__(self):
                return self
            def __exit__(self, *args):
                pass
            def read(self, limit):
                return b"artifact"
        class Opener:
            def open(self, request, timeout):
                requests.append(request)
                if len(requests) == 1:
                    raise pub.urllib.error.HTTPError(request.full_url, 302, "redirect",
                                                     {"Location": "https://release-assets.githubusercontent.com/exact"}, None)
                return Response()
        with mock.patch.object(pub.urllib.request, "build_opener", return_value=Opener()):
            self.assertEqual(pub.Http().request("GET", "https://api.github.com/repos/example/widget/releases/assets/1",
                                              headers={"Authorization": "Bearer fixture"}, download=True), b"artifact")
        self.assertEqual(requests[0].get_header("Authorization"), "Bearer fixture")
        self.assertIsNone(requests[1].get_header("Authorization"))

    def test_trusted_cargo_does_not_need_settings_read(self):
        bundle, _ = cargo_fixture()
        http = FakeHttp()
        environment = {"GH_TOKEN": "fixture-gh", "CARGO_REGISTRY_TOKEN": "fixture-oidc", "RELEASE_CARGO_AUTH": "trusted",
                       "GITHUB_ACTIONS": "true", "ACTIONS_ID_TOKEN_REQUEST_URL": "https://example.invalid/fixture"}
        self.assertEqual(pub.Remote(bundle, http=http, environment=environment).credentials("cargo"), "fixture-oidc")
        self.assertEqual(http.calls, [])

    def test_missing_pypi_token_fails_before_intent(self):
        bundle, _ = cargo_fixture()
        remote = pub.Remote(bundle, http=FakeHttp(), environment={"GH_TOKEN": "fixture-gh"})
        with self.assertRaisesRegex(pub.Failure, "PYPI_TOKEN is missing"):
            remote.credentials("pypi")

    def test_token_cargo_guard_remains_unchanged(self):
        bundle, _ = cargo_fixture()
        environment = {"GH_TOKEN": "fixture-gh", "CARGO_REGISTRY_TOKEN": "fixture-cargo"}
        for settings in ({"trustpub_only": True}, {"trustpub_only": None}, {}):
            with self.subTest(settings=settings):
                http = FakeHttp({"crate": settings})
                remote = pub.Remote(bundle, http=http, environment=environment)
                with self.assertRaisesRegex(pub.Failure, "No registry policy was changed"):
                    remote.credentials("cargo")
                self.assertEqual([method for method, _, _ in http.calls], ["GET"])
        http = FakeHttp({"crate": {"trustpub_only": False}})
        self.assertEqual(pub.Remote(bundle, http=http, environment=environment).credentials("cargo"), "fixture-cargo")

    def test_first_cargo_publication_accepts_confirmed_missing_crate(self):
        bundle, _ = cargo_fixture()
        environment = {"GH_TOKEN": "fixture-gh", "CARGO_REGISTRY_TOKEN": "fixture-cargo"}
        error = pub.urllib.error.HTTPError("https://crates.io/api/v1/crates/widget", 404, "missing", {}, io.BytesIO())
        try:
            with mock.patch.object(pub.urllib.request, "build_opener") as opener:
                opener.return_value.open.side_effect = error
                self.assertEqual(pub.Remote(bundle, environment=environment).credentials("cargo"), "fixture-cargo")
                opener.return_value.open.assert_called_once()
                request = opener.return_value.open.call_args.args[0]
                self.assertEqual((request.method, request.full_url), ("GET", "https://crates.io/api/v1/crates/widget"))
        finally:
            error.close()

    def test_first_cargo_publication_refuses_other_http_failures_without_retry(self):
        bundle, _ = cargo_fixture()
        environment = {"GH_TOKEN": "fixture-gh", "CARGO_REGISTRY_TOKEN": "fixture-cargo"}
        for status in (401, 403, 429, 500):
            with self.subTest(status=status):
                error = pub.urllib.error.HTTPError("https://crates.io/api/v1/crates/widget", status, "unavailable", {}, io.BytesIO())
                try:
                    with mock.patch.object(pub.urllib.request, "build_opener") as opener:
                        opener.return_value.open.side_effect = error
                        with self.assertRaises(pub.HttpFailure):
                            pub.Remote(bundle, environment=environment).credentials("cargo")
                        opener.return_value.open.assert_called_once()
                finally:
                    error.close()

    def test_first_cargo_publication_refuses_successful_null_metadata(self):
        bundle, _ = cargo_fixture()
        environment = {"GH_TOKEN": "fixture-gh", "CARGO_REGISTRY_TOKEN": "fixture-cargo"}
        with mock.patch.object(pub.urllib.request, "build_opener") as opener:
            opener.return_value.open.return_value = io.BytesIO(b" \nnull\n")
            with self.assertRaisesRegex(pub.Failure, "not confirmed HTTP 404"):
                pub.Remote(bundle, environment=environment).credentials("cargo")
            opener.return_value.open.assert_called_once()


if __name__ == "__main__":
    unittest.main()
