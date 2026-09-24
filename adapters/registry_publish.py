#!/usr/bin/env python3
"""Publish inspected archives; never build, change registry policy, or retry uncertain uploads.

The caller supplies one reviewed release bundle and its external SHA-256. Existing
version/tag identity and exact public downloads are reconciled before publication.
"""
from __future__ import annotations

import argparse
import base64
import csv
from email.parser import Parser
from email.utils import parsedate_to_datetime
import fcntl
import hashlib
import gzip
import io
import itertools
import json
import lzma
import os
from pathlib import Path, PurePosixPath
import re
import secrets
import struct
import tarfile
import tempfile
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request
import zipfile

MAX_BYTES = 128 * 1024 * 1024
CHANNELS = {"cargo", "npm", "jsr", "pypi"}
TOKEN_NAMES = {"cargo": "CARGO_REGISTRY_TOKEN", "npm": "NPM_TOKEN", "jsr": "JSR_TOKEN", "pypi": "PYPI_TOKEN"}
# Each selector is `token` (the default: a long-lived registry token from
# TOKEN_NAMES) or `trusted` (a short-lived credential derived from the GitHub
# Actions OIDC ID token of the running workflow).
AUTH_MODES = {"cargo": "RELEASE_CARGO_AUTH", "npm": "RELEASE_NPM_AUTH", "jsr": "RELEASE_JSR_AUTH", "pypi": "RELEASE_PYPI_AUTH"}
# npm derives its audience from the registry hostname; PyPI publishes its
# audience at https://pypi.org/_/oidc/audience. JSR's audience is per upload.
NPM_OIDC_AUDIENCE = "npm:registry.npmjs.org"
PYPI_OIDC_AUDIENCE = "pypi"
OIDC_RESPONSE_LIMIT = 64 * 1024
LABELS = {"cargo": "Cargo", "npm": "npm", "jsr": "JSR", "pypi": "PyPI"}
RECOVERY_COOLDOWN = 3600  # Client policy, not a claimed PyPI rate-limit window.
USER_AGENT = "ccid-registry-publisher/1; https://github.com/corbet-labs/ccid"


class Failure(Exception):
    """A publication requirement is missing, conflicting, or uncertain."""


class HttpFailure(Failure):
    """Safe response metadata; never preserve headers, bodies or credentials."""

    def __init__(self, method, origin, status, retry_after=None):
        super().__init__(f"{method} {urllib.parse.urlsplit(origin).hostname} returned HTTP {status}; no request was retried")
        observed = int(time.time())
        delay = None
        if retry_after is not None:
            try:
                delay = int(retry_after) if re.fullmatch(r"[0-9]+", retry_after) else max(0, int(parsedate_to_datetime(retry_after).timestamp()) - observed)
            except (TypeError, ValueError, OverflowError):
                pass
        self.outcome = {"kind": "http-response", "method": method, "origin": origin,
                        "status": status, "observed_at": observed, "retry_after_seconds": delay}


def require(condition, message):
    if not condition:
        raise Failure(message)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def json_bytes(value):
    return (json.dumps(value, sort_keys=True, indent=2) + "\n").encode()


class GithubOidcToken(str):
    """A GitHub Actions ID token that JSR accepts directly with the `githuboidc` scheme.

    The distinct type selects the authorization scheme; the value itself is never
    logged, journaled or included in failure messages.
    """


def jsr_body(data):
    """Return the one deterministic gzip tarball that is both hashed and uploaded.

    JSR binds an OIDC publish permission to the SHA-256 of the exact request body,
    so the body is built once and cached on the inspected channel data.
    """
    if "jsr_body" not in data:
        raw = io.BytesIO()
        with tarfile.open(fileobj=raw, mode="w") as archive:
            for path, payload in sorted(data["jsr_files"].items()):
                info = tarfile.TarInfo(path)
                info.size, info.mode, info.mtime = len(payload), 0o644, 0
                archive.addfile(info, io.BytesIO(payload))
        data["jsr_body"] = gzip.compress(raw.getvalue(), mtime=0)
    return data["jsr_body"]


def plain_path(value):
    require(isinstance(value, str) and value and "\\" not in value, "Invalid archive path")
    path = PurePosixPath(value)
    require(not path.is_absolute() and not any(part in {"", ".", ".."} for part in value.split("/")), "Unsafe archive path")
    require(all(ord(char) >= 32 for char in value), "Control character in archive path")
    return path


def archive_files(payload, *, source=False):
    require(len(payload) <= MAX_BYTES, "Archive exceeds the publication size bound")
    files, seen, total = {}, set(), 0
    with tarfile.open(fileobj=io.BytesIO(payload), mode="r:*") as archive:
        commit = archive.pax_headers.get("comment", "").strip()
        if source:
            require(re.fullmatch(r"[0-9a-f]{40}", commit), "Source archive lacks its exact Git commit")
        for member in archive:
            path = str(plain_path(member.name.rstrip("/")))
            require(path not in seen, "Duplicate archive path")
            seen.add(path)
            require(member.isfile() or member.isdir(), "Links and special archive members are forbidden")
            total += member.size
            require(total <= MAX_BYTES and len(seen) <= 10000, "Archive contents exceed publication limits")
            if member.isfile():
                files[path] = archive.extractfile(member).read()
    return files, commit


def durable_write(path, value):
    path = Path(path)
    require(not path.is_symlink(), "Journal must not be a symlink")
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=path.parent, prefix=".journal-", delete=False) as stream:
        temporary = Path(stream.name)
        stream.write(json_bytes(value))
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)
    descriptor = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def crate_dependencies(manifest):
    result = []
    for target, table in [(None, manifest), *manifest.get("target", {}).items()]:
        for section, kind in [("dependencies", "normal"), ("dev-dependencies", "dev"), ("build-dependencies", "build")]:
            for alias, value in table.get(section, {}).items():
                spec = {"version": value} if isinstance(value, str) else value
                require(isinstance(spec.get("version"), str), "Cargo dependency lacks a registry version")
                require(not set(spec) & {"path", "git", "registry", "registry-index", "workspace"}, "Cargo dependency is not crates.io resolved")
                result.append({"name": spec.get("package", alias), "version_req": spec["version"],
                               "features": spec.get("features", []), "optional": spec.get("optional", False),
                               "default_features": spec.get("default-features", spec.get("default_features", True)),
                               "target": target, "kind": kind, "registry": None,
                               "explicit_name_in_toml": alias if spec.get("package") else None})
    return sorted(result, key=lambda item: (item["target"] or "", item["kind"], item["name"]))


def cargo_lock_graph(payload):
    """Resolve Cargo's optional version/source qualifiers without guessing."""
    document = tomllib.loads(payload.decode())
    require(document.get("version") in {3, 4} and isinstance(document.get("package"), list),
            "Unsupported normalized Cargo lock format")
    packages = {}
    for package in document["package"]:
        require(isinstance(package, dict) and all(isinstance(package.get(key), str) and package[key]
                                                for key in ("name", "version"))
                and (package.get("source") is None or isinstance(package["source"], str)),
                "Invalid Cargo lock package identity")
        identity = (package["name"], package["version"], package.get("source"))
        require(identity not in packages, "Duplicate Cargo lock package identity")
        packages[identity] = package
    graph = {}
    for identity, package in packages.items():
        dependencies = package.get("dependencies", [])
        require(isinstance(dependencies, list), "Invalid Cargo lock dependency list")
        edges = set()
        for dependency in dependencies:
            match = re.fullmatch(r"([A-Za-z0-9_-]+)(?: ([^ ()]+))?(?: \(([^()]+)\))?", dependency) if isinstance(dependency, str) else None
            require(match is not None, "Unsupported Cargo lock dependency reference")
            candidates = [key for key in packages if key[0] == match[1]
                          and (match[2] is None or key[1] == match[2])
                          and (match[3] is None or key[2] == match[3])]
            require(len(candidates) == 1, "Missing or ambiguous Cargo lock dependency reference")
            require(candidates[0] not in edges, "Duplicate Cargo lock dependency edge")
            edges.add(candidates[0])
        graph[identity] = edges
    metadata = {key: value for key, value in document.items() if key != "package"}
    return metadata, packages, graph


def inspect_packaged_lock(source, packaged, name, version):
    """Allow only the producing package's exact reachable locked graph."""
    source_meta, source_packages, source_graph = cargo_lock_graph(source)
    package_meta, packages, graph = cargo_lock_graph(packaged)
    require(package_meta == source_meta, "Normalized Cargo lock metadata differs")
    root = (name, version, None)
    require(root in source_packages, "Producing package is absent from source lock")
    reachable, pending = set(), [root]
    while pending:
        identity = pending.pop()
        if identity not in reachable:
            reachable.add(identity)
            pending.extend(source_graph[identity])
    require(set(packages) == reachable, "Normalized Cargo lock differs from the required dependency closure")
    for identity, package in packages.items():
        metadata = {key: value for key, value in package.items() if key != "dependencies"}
        expected = {key: value for key, value in source_packages[identity].items() if key != "dependencies"}
        require(metadata == expected, "Normalized Cargo dependency metadata/checksum differs")
        require(graph[identity] == source_graph[identity], "Normalized Cargo dependency edges differ")


PYPI_FIELDS = {
    "metadata-version": "metadata_version", "name": "name", "version": "version",
    "summary": "summary", "description-content-type": "description_content_type",
    "project-url": "project_urls", "license-expression": "license_expression",
    "license-file": "license_file", "license": "license", "keywords": "keywords",
    "classifier": "classifiers", "requires-python": "requires_python", "requires-dist": "requires_dist",
    "provides-extra": "provides_extra", "author": "author", "author-email": "author_email",
    "maintainer": "maintainer", "maintainer-email": "maintainer_email", "home-page": "home_page",
    "download-url": "download_url", "platform": "platform", "supported-platform": "supported_platform",
    "provides-dist": "provides_dist", "obsoletes-dist": "obsoletes_dist",
    "requires-external": "requires_external", "dynamic": "dynamic",
}
PYPI_MULTIPLE = {"project-url", "license-file", "classifier", "requires-dist", "provides-extra",
                 "platform", "supported-platform", "provides-dist", "obsoletes-dist", "requires-external", "dynamic"}


def python_metadata(payload):
    parsed = Parser().parsestr(payload.decode("utf-8"))
    require(not parsed.defects and not parsed.is_multipart(), "Malformed Python package metadata")
    fields = []
    for key in dict.fromkeys(key.lower() for key in parsed.keys()):
        require(key in PYPI_FIELDS, "Unreviewed Python metadata field")
        values = parsed.get_all(key)
        require(key in PYPI_MULTIPLE or len(values) == 1, "Duplicate Python metadata field")
        fields.extend((PYPI_FIELDS[key], value) for value in values)
    return [*fields, ("description", parsed.get_payload())]


def compressed_source_digest(payload, opener):
    """Compare large corresponding-source archives without unbounded expansion."""
    digest, total = hashlib.sha256(), 0
    try:
        with opener(io.BytesIO(payload), "rb") as stream:
            while chunk := stream.read(65536):
                total += len(chunk)
                require(total <= 512 * 1024 * 1024, "Corresponding source exceeds decompression bound")
                digest.update(chunk)
    except (OSError, EOFError, lzma.LZMAError) as error:
        raise Failure("Invalid compressed corresponding source") from error
    return digest.hexdigest()


def rewrite_imports(payload, dependencies):
    """Rewrite supported static declarations; never touch comments/string contents."""
    if not dependencies:
        return payload
    text = payload.decode("utf-8")
    expression = re.compile(r"//[^\n]*|/\*[\s\S]*?\*/|'(?:\\.|[^'\\])*'|\"(?:\\.|[^\"\\])*\"|`(?:\\.|[^`\\])*`|/(?:\\.|\[(?:\\.|[^\]\\])*\]|[^/\\\n])+/[dgimsuvy]*|[A-Za-z_$][A-Za-z0-9_$]*|[^\s]")
    tokens = [match for match in expression.finditer(text) if not match.group().startswith(("//", "/*"))]
    for index, token in enumerate(tokens):
        value = token.group()
        if value.startswith("/"):
            # Regex versus division needs a full parser in other contexts. Keep
            # this adapter's supported transformation deliberately narrower.
            previous = tokens[index - 1].group() if index else ""
            require(len(value) > 1 and previous in {"", "(", "=", ",", "[", ":"},
                    "Ambiguous slash syntax requires a reviewed JSR source change")
        if value.startswith("`") and "${" in value:
            # Current family interpolation uses identifiers/operators only.
            # Reject strings, nesting and executable imports instead of
            # pretending opaque template contents have been parsed.
            interpolations = re.findall(r"\$\{([^{}]*)\}", value)
            require(len(interpolations) == value.count("${") and all(
                re.fullmatch(r"[A-Za-z0-9_$ .?:()+*!<>=,\-]+", item)
                and not re.search(r"\b(?:import|export|from)\b", item)
                for item in interpolations), "Unsupported JSR template interpolation requires a source change")
    replacements = []
    for index, token in enumerate(tokens):
        if token.group() not in {"import", "export"}:
            continue
        position = index + 1
        if position < len(tokens) and tokens[position].group() == "type":
            position += 1
        if position >= len(tokens):
            continue
        if tokens[position].group() == "{":
            position += 1
            while position < len(tokens) and re.fullmatch(r"[A-Za-z_$][A-Za-z0-9_$]*|,", tokens[position].group()):
                position += 1
            if position >= len(tokens) or tokens[position].group() != "}":
                continue
            position += 1
        elif tokens[position].group() == "*":
            position += 1
            if position + 1 < len(tokens) and tokens[position].group() == "as":
                position += 2
        else:
            continue
        if position + 1 >= len(tokens) or tokens[position].group() != "from":
            continue
        target = tokens[position + 1]
        specifier = target.group()
        if specifier[:1] not in {"'", '"'} or specifier[-1:] != specifier[:1]:
            continue
        name = specifier[1:-1]
        if name not in dependencies:
            continue
        version = dependencies[name]
        require(re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version), "JSR npm dependencies require exact stable versions")
        replacements.append((target.start(), target.end(), specifier[0] + "npm:" + name + "@" + version + specifier[0]))
    covered = {start for start, _, _ in replacements}
    for index, token in enumerate(tokens):
        if token.group() == "from" and index + 1 < len(tokens):
            target = tokens[index + 1]
            value = target.group()
            if value[:1] in {"'", '"'} and value[1:-1] in dependencies:
                require(target.start() in covered, "Unsupported JSR import/export declaration; prepare an explicit supported source change first")
        if token.group() == "import" and index + 1 < len(tokens):
            target_index = index + 2 if tokens[index + 1].group() == "(" else index + 1
            if target_index < len(tokens):
                value = tokens[target_index].group()
                require(not (value[:1] in {"'", '"'} and value[1:-1] in dependencies),
                        "Side-effect/dynamic JSR dependency imports require a source change before publication")
    for start, end, replacement in reversed(replacements):
        text = text[:start] + replacement + text[end:]
    return text.encode("utf-8")


class Bundle:
    def __init__(self, payload, expected_sha256, repository, *, create_jsr_receipt=False):
        require(re.fullmatch(r"[0-9a-f]{64}", expected_sha256 or ""), "Supply the reviewed release bundle SHA-256")
        require(sha(payload) == expected_sha256, "Release bundle SHA-256 mismatch")
        self.files, _ = archive_files(payload)
        self.digest = expected_sha256
        self.manifest = json.loads(self.files["release.json"])
        manifest = self.manifest
        require(manifest.get("schema") == 1 and manifest.get("repository") == repository, "Release bundle repository/schema mismatch")
        require(re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository), "Invalid repository identity")
        self.repository = repository
        self.create_jsr_receipt = create_jsr_receipt
        self.name = manifest["package"]
        self.version = manifest["version"]
        require(re.fullmatch(r"[a-z][a-z0-9_-]*", self.name), "Invalid package name")
        require(re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?", self.version), "Invalid release version")
        require(manifest.get("tag") == "v" + self.version and re.fullmatch(r"[0-9a-f]{40}", manifest.get("tag_commit", "")), "Release requires an exact existing tag commit")
        require(isinstance(manifest.get("channels"), dict) and manifest["channels"] and set(manifest["channels"]) <= CHANNELS, "Invalid registry selection")
        self.channels = {name: self.inspect_channel(name, entry) for name, entry in manifest["channels"].items()}

    def entry(self, path, digest):
        value = self.files[str(plain_path(path))]
        require(re.fullmatch(r"[0-9a-f]{64}", digest or "") and sha(value) == digest, "Bundle entry checksum mismatch")
        return value

    def inspect_channel(self, registry, entry):
        source_payload = self.entry(entry["source_archive"], entry["source_sha256"])
        source, commit = archive_files(source_payload, source=True)
        require(commit == entry["source_commit"], "Producing source commit mismatch")
        package = tomllib.loads(source["Cargo.toml"].decode())["package"]
        require(package["name"] == self.name and package["version"] == self.version, "Producing source package/version mismatch")
        require(package.get("repository", "").removesuffix(".git") == "https://github.com/" + self.repository, "Producing repository mismatch")
        receipt = json.loads(self.entry(entry["receipt"], entry["receipt_sha256"]))
        check = {"cargo": "rust-package", "npm": "js-package", "jsr": "js-package", "pypi": "python-package"}[registry]
        if registry == "jsr" and receipt.get("check") == "jsr-package":
            check = "jsr-package"
        require(all(receipt.get(key) == value for key, value in {
            "schema": 1, "package": self.name, "version": self.version,
            "commit": commit, "source_sha256": entry["source_sha256"], "check": check,
        }.items()), "Preparation receipt does not match producing inputs")
        if receipt.get("tool_revision"):
            pins = re.findall(r"^\s+CCID_REVISION:\s*['\"]?([0-9a-f]{40})['\"]?\s*$", source[".crow/ccid.yaml"].decode(), re.M)
            require(pins == [receipt["tool_revision"]], "Preparation tool differs from committed workflow")
        else:
            hosted = json.loads(self.entry(entry["hosted_receipt"], entry["hosted_receipt_sha256"]))
            require(hosted.get("provider") == "github-actions" and hosted.get("status") == "success"
                    and hosted.get("commit") == commit and hosted.get("source_sha256") == entry["source_sha256"]
                    and check in hosted.get("checks", []) and bool(hosted.get("tools")), "Hosted preparation evidence is incomplete")
            require(hosted.get("workflow_sha256") == sha(source[".github/workflows/ci.yml"]), "Hosted preparation workflow mismatch")
        artifacts = {name: self.entry(item["path"], item["sha256"]) for name, item in entry["artifacts"].items()}
        require(all(str(plain_path(name)) == name and "/" not in name for name in artifacts), "Artifact names must be plain filenames")
        require(receipt.get("artifacts") == {name: sha(value) for name, value in artifacts.items()}, "Artifact set differs from verified receipt")
        identity = {"repository": self.repository, "package": self.name, "version": self.version,
                    "registry": registry, "producing_commit": commit, "source_sha256": entry["source_sha256"],
                    "tag_commit": self.manifest["tag_commit"], "artifacts": {name: sha(value) for name, value in artifacts.items()}}
        data = {"identity": identity, "artifacts": artifacts, "source": source, "receipt": receipt, "entry": entry}
        if registry == "cargo":
            self.inspect_cargo(data)
        elif registry in {"npm", "jsr"}:
            self.inspect_javascript(data, registry)
        else:
            self.inspect_python(data)
        return data

    def inspect_cargo(self, data):
        filename = f"{self.name}-{self.version}.crate"
        require(set(data["artifacts"]) == {filename}, "Expected one verified crate")
        files, _ = archive_files(data["artifacts"][filename])
        prefix = self.name + "-" + self.version + "/"
        require(all(name.startswith(prefix) for name in files), "Invalid crate package root")
        files = {name[len(prefix):]: value for name, value in files.items()}
        source = data["source"]
        require(files.get("Cargo.toml.orig") == source["Cargo.toml"], "Crate original manifest differs from producing source")
        original = tomllib.loads(source["Cargo.toml"].decode())
        normalized = tomllib.loads(files["Cargo.toml"].decode())
        package = normalized["package"]
        for key in ("name", "version", "authors", "description", "documentation", "homepage", "repository", "license", "license-file", "keywords", "categories", "links", "rust-version", "edition"):
            require(package.get(key) == original["package"].get(key), "Normalized Cargo metadata differs: " + key)
        require(crate_dependencies(normalized) == crate_dependencies(original) and normalized.get("features", {}) == original.get("features", {}), "Normalized Cargo dependencies/features differ")
        require(not normalized.get("patch") and not normalized.get("replace"), "Crate contains dependency overrides")
        for name, value in files.items():
            if name not in {"Cargo.toml", "Cargo.toml.orig", ".cargo_vcs_info.json"}:
                if name == "Cargo.lock" and source.get(name) != value:
                    require(name in source, "Crate lock is absent from producing source")
                    inspect_packaged_lock(source[name], value, self.name, self.version)
                else:
                    require(source.get(name) == value, "Crate payload differs from producing source: " + name)
        if ".cargo_vcs_info.json" in files:
            require(json.loads(files[".cargo_vcs_info.json"])["git"]["sha1"] == data["identity"]["producing_commit"], "Crate Git provenance mismatch")
        publication = package.get("publish")
        require(publication is None or publication is True or isinstance(publication, list) and "crates-io" in publication,
                "Cargo manifest disables crates.io publication")
        readme = package.get("readme")
        readme_file = readme if isinstance(readme, str) else None
        metadata = {"name": self.name, "vers": self.version, "deps": crate_dependencies(normalized),
                    "features": normalized.get("features", {}), "authors": package.get("authors", []),
                    "readme": files[readme_file].decode() if readme_file else None, "readme_file": readme_file,
                    "rust_version": package.get("rust-version"), "badges": normalized.get("badges", {})}
        for key in ("description", "documentation", "homepage", "license", "repository", "links"):
            metadata[key] = package.get(key)
        metadata.update(license_file=package.get("license-file"), keywords=package.get("keywords", []), categories=package.get("categories", []))
        data["cargo_metadata"] = metadata

    def javascript_scope(self, source):
        """Return the committed npm/JSR scope; registry names never follow the GitHub owner."""
        pattern = r"js/(@[a-z0-9][a-z0-9._-]*)/" + re.escape(self.name) + r"/package\.json"
        scopes = {match[1] for match in (re.fullmatch(pattern, path) for path in source) if match}
        require(len(scopes) <= 1, "Ambiguous JavaScript package scope")
        if scopes:
            return scopes.pop()
        if "web/package.json" in source:
            name = json.loads(source["web/package.json"]).get("name", "")
            if re.fullmatch(r"@[a-z0-9][a-z0-9._-]*/" + re.escape(self.name), name):
                return name.split("/")[0]
        return "@" + self.repository.split("/")[0]

    def inspect_javascript(self, data, registry):
        if registry == "jsr" and data["receipt"].get("layout") == "web-wasm-v1":
            self.inspect_generated_jsr(data)
            return
        source = data["source"]
        scope = self.javascript_scope(source)
        prefix = "js/" + scope + "/" + self.name + "/"
        if registry == "npm":
            candidates = [path for path in (prefix, "web/") if path + "package.json" in source]
            require(bool(candidates), "JavaScript source manifest is missing")
            manifests = [json.loads(source[path + "package.json"]) for path in candidates]
            require(all(manifest == manifests[0] for manifest in manifests),
                    "Discovered JavaScript source manifests are conflicting")
            prefix = candidates[0]
        manifest = json.loads(source[prefix + "package.json"])
        require(manifest["name"] == scope + "/" + self.name and manifest["version"] == self.version, "JavaScript package identity mismatch")
        require(not manifest.get("private") and "packageExtensions" not in manifest, "JavaScript manifest forbids publication")
        data["js_name"] = manifest["name"]
        data["js_manifest"] = manifest
        require(len(data["artifacts"]) == 1, "Expected one verified JavaScript archive")
        filename, payload = next(iter(data["artifacts"].items()))
        packaged, _ = archive_files(payload)
        if data["receipt"]["check"] == "js-package":
            require(all(name.startswith("package/") for name in packaged), "Invalid npm archive root")
            packaged = {name.removeprefix("package/"): value for name, value in packaged.items()}
        packaged_manifest = json.loads(packaged["package.json"])
        comparable = packaged_manifest
        if registry == "npm" and "gitHead" in packaged_manifest:
            require(packaged_manifest["gitHead"] == data["identity"]["producing_commit"],
                    "Generated npm gitHead differs from producing source commit")
            if "gitHead" not in manifest:
                comparable = {key: value for key, value in packaged_manifest.items() if key != "gitHead"}
        require(comparable == manifest, "JavaScript archive manifest differs from source")
        if registry == "npm":
            require(filename == f"{manifest['name'].replace('@', '').replace('/', '-')}-{self.version}.tgz", "npm archive filename mismatch")
            data["js_manifest"] = packaged_manifest
            return
        expected = {name.removeprefix(prefix): value for name, value in source.items()
                    if name.startswith(prefix + "src/") or name in {prefix + "package.json", prefix + "jsr.json"}}
        expected["README.md"] = source["README.md"]
        licenses = {name: value for name, value in source.items() if name.startswith("LICENSES/")}
        for dependency in manifest.get("dependencies", {}):
            component = dependency.removeprefix(scope + "/")
            prefix_license = "typst/vendor/" + component + "/LICENSES/"
            for name, value in source.items():
                if name.startswith(prefix_license):
                    licenses["LICENSES/vendor/" + component + "/" + name.removeprefix(prefix_license)] = value
        actual_licenses = {name: value for name, value in packaged.items() if name.startswith("LICENSES/")}
        require(licenses and actual_licenses == licenses, "JSR license inventory/bytes differ from producing source and declared sibling vendors")
        expected.update(licenses)
        if data["receipt"]["check"] == "jsr-package":
            require(packaged == expected, "Verified JSR source archive differs from producing source")
        config = json.loads(expected["jsr.json"])
        require(config["name"] == manifest["name"] and config["version"] == self.version, "JSR manifest identity mismatch")
        transformations = {}
        for path, payload in list(expected.items()):
            if path.endswith(".ts"):
                original = payload
                payload = rewrite_imports(payload, manifest.get("dependencies", {}))
                if payload != original:
                    transformations[path] = {"kind": "explicit npm import specifiers", "original_sha256": sha(original), "published_sha256": sha(payload)}
                expected[path] = payload
        if self.create_jsr_receipt:
            data["entry"]["published_files"] = {path: sha(payload) for path, payload in expected.items()}
            data["entry"]["source_transformations"] = transformations
        require(data["entry"].get("published_files") == {path: sha(payload) for path, payload in expected.items()},
                "JSR requires the reviewed complete published-file hash inventory")
        require(data["entry"].get("source_transformations", {}) == transformations,
                "JSR source-to-published transformations are not explicitly recorded in the reviewed receipt")
        data["identity"].update(published_files={path: sha(payload) for path, payload in expected.items()},
                                source_transformations=transformations)
        exports = config["exports"]
        data.update(jsr_files=expected, jsr_exports={".": exports} if isinstance(exports, str) else exports, transformations=transformations)

    def inspect_generated_jsr(self, data):
        source, receipt = data["source"], data["receipt"]
        require(receipt["check"] == "jsr-package", "Generated JSR requires its own preparation check")
        origin = json.loads(source[".ci/jsr-input.json"])
        name = self.javascript_scope(source) + "/" + self.name
        require(receipt.get("original_npm") == origin and origin.get("schema") == 1
                and origin.get("package") == name and origin.get("version") == self.version
                and re.fullmatch(r"[0-9a-f]{40}", origin.get("source_commit", ""))
                and re.fullmatch(r"[0-9a-f]{64}", origin.get("source_sha256", "")),
                "Generated JSR original npm provenance differs from committed inputs")
        npm_name = f"{name.replace('@', '').replace('/', '-')}-{self.version}.tgz"
        jsr_name = npm_name.removesuffix(".tgz") + "-jsr.tar.gz"
        require(receipt.get("original_npm_artifact") == npm_name and set(data["artifacts"]) == {npm_name, jsr_name},
                "Generated JSR requires exact prepared and original npm archives")
        require(sha(data["artifacts"][npm_name]) == origin.get("archive_sha256"), "Original npm archive hash differs")
        original, _ = archive_files(data["artifacts"][npm_name])
        require(all(path.startswith("package/") for path in original), "Invalid original npm archive root")
        original = {path.removeprefix("package/"): value for path, value in original.items()}
        manifest = json.loads(original["package.json"])
        require(manifest.get("name") == name and manifest.get("version") == self.version
                and manifest.get("gitHead") == origin["source_commit"] and not manifest.get("private"),
                "Original npm manifest identity differs")
        require({key: value for key, value in manifest.items() if key != "gitHead"}
                == json.loads(source["web/package.json"]), "Generated JSR source manifest differs from original npm")
        require(all(original[path] == source["web/" + path] for path in ("index.js", "index.d.ts")),
                "Generated JSR entrypoint differs from committed source")
        published, _ = archive_files(data["artifacts"][jsr_name])
        require(sum(map(len, published.values())) < 20_000_000
                and len({path.casefold() for path in published}) == len(published), "JSR size or path casing limit exceeded")
        expected = {**original, "jsr.json": source["web/jsr.json"]}
        old_path, new_path = "source/dependencies.tar.gz", "source/dependencies.tar.xz"
        original_compressed = expected.pop(old_path)
        vendor_digest = compressed_source_digest(original_compressed, gzip.open)
        require(new_path in published and compressed_source_digest(published[new_path], lzma.open) == vendor_digest,
                "Generated JSR corresponding source changed during recompression")
        expected[new_path] = published[new_path]
        transformations = {new_path: {"kind": "lossless gzip-to-xz recompression", "original_path": old_path,
                           "original_sha256": sha(original_compressed), "published_sha256": sha(expected[new_path]),
                           "uncompressed_sha256": vendor_digest}}
        replacements = json.loads(source[".ci/jsr-readme-replacements.json"])
        require(isinstance(replacements, list) and 0 < len(replacements) <= 16
                and all(isinstance(item, dict) and set(item) == {"original", "replacement"}
                        and all(isinstance(value, str) and 0 < len(value.encode()) <= 4096 for value in item.values())
                        for item in replacements), "Invalid committed JSR README replacements")
        for path in ("README.md", "source/README.md"):
            original_readme = original[path]
            require(b"tar -xzf dependencies.tar.gz" in original_readme, "Original source extraction instruction is missing")
            expected[path] = original_readme.replace(b"dependencies.tar.gz", b"dependencies.tar.xz").replace(
                b"tar -xzf dependencies.tar.xz", b"tar -xJf dependencies.tar.xz")
            for item in replacements:
                require(expected[path].count(item["original"].encode()) == 1, "JSR README replacement is absent or ambiguous")
                expected[path] = expected[path].replace(item["original"].encode(), item["replacement"].encode())
            transformations[path] = {"kind": "source archive extraction and Wasm initialization instructions",
                                     "original_sha256": sha(original_readme), "published_sha256": sha(expected[path])}
            if path == "README.md":
                expected[path] = source[".ci/jsr-readme.md"] + b"\n" + expected[path]
                transformations[path].update(kind="JSR usage and source archive extraction instructions",
                                             published_sha256=sha(expected[path]))
        require(published == expected, "Generated JSR runtime/source/license bytes differ from reviewed transformations")
        inventory = {path: sha(value) for path, value in expected.items()}
        require(receipt.get("published_files") == inventory and receipt.get("source_transformations") == transformations,
                "Generated JSR preparation inventory or transformations differ")
        config = json.loads(expected["jsr.json"])
        require(config.get("name") == name and config.get("version") == self.version, "JSR manifest identity mismatch")
        if self.create_jsr_receipt:
            data["entry"].update(published_files=inventory, source_transformations=transformations)
        require(data["entry"].get("published_files") == inventory
                and data["entry"].get("source_transformations") == transformations, "Generated JSR reviewed inventory differs")
        data["identity"].update(published_files=inventory, source_transformations=transformations, original_npm=origin)
        exports = config["exports"]
        data.update(js_name=name, js_manifest=manifest, jsr_files=expected,
                    jsr_exports={".": exports} if isinstance(exports, str) else exports, transformations=transformations)

    def inspect_python(self, data):
        document = tomllib.loads(data["source"]["py/pyproject.toml"].decode())
        project = document["project"]
        require(project["name"] == self.name and project["version"] == self.version, "Python source identity mismatch")
        if document.get("build-system", {}).get("build-backend") == "maturin":
            self.inspect_native_python(data, project, document)
            return
        require(set(data["artifacts"]) == {f"{self.name}-{self.version}-py3-none-any.whl", f"{self.name}-{self.version}.tar.gz"},
                "Expected exact pure-Python wheel and sdist filenames")
        data["python"] = {}
        for filename, payload in data["artifacts"].items():
            if filename.endswith(".whl"):
                with zipfile.ZipFile(io.BytesIO(payload)) as archive:
                    names = archive.namelist()
                    require(len(names) == len(set(names)) and sum(item.file_size for item in archive.infolist()) <= MAX_BYTES, "Invalid wheel archive")
                    for name in names:
                        plain_path(name.rstrip("/"))
                    paths = [name for name in names if name.endswith(".dist-info/METADATA")]
                    require(len(paths) == 1, "Expected one wheel metadata file")
                    fields = python_metadata(archive.read(paths[0]))
                kind, pyversion = "bdist_wheel", "py3"
            elif filename == f"{self.name}-{self.version}.tar.gz":
                files, _ = archive_files(payload)
                fields = python_metadata(files[f"{self.name}-{self.version}/PKG-INFO"])
                kind, pyversion = "sdist", "source"
            else:
                raise Failure("Unexpected Python distribution filename")
            values = dict(fields)
            require(values.get("name") == self.name and values.get("version") == self.version
                    and values.get("requires_python") == project.get("requires-python")
                    and values.get("license_expression") == project.get("license"), "Python archive metadata differs from source")
            data["python"][filename] = {"fields": fields, "filetype": kind, "pyversion": pyversion}
        require(len(data["python"]) == 2 and sum(name.endswith(".whl") for name in data["python"]) == 1, "Expected the verified pure-Python wheel and sdist")
        require(len({json.dumps(item["fields"]) for item in data["python"].values()}) == 1, "Wheel and sdist metadata differ")

    def inspect_native_python(self, data, project, document):
        receipt = data["receipt"]
        require(receipt.get("layout") == "maturin-v1" and document.get("tool", {}).get("maturin", {}).get("bindings") == "pyo3",
                "Native Python requires reviewed maturin/PyO3 preparation")
        native = receipt["native_wheel"]
        normalized = re.sub(r"[-_.]+", "_", self.name).lower()
        filename, sdist_name = native["filename"], f"{normalized}-{self.version}.tar.gz"
        match = re.fullmatch(re.escape(normalized + "-" + self.version) + r"-(cp3[0-9]+)-(abi3|cp3[0-9]+t?)-([A-Za-z0-9_]+(?:\.[A-Za-z0-9_]+)*)\.whl", filename)
        require(match is not None and match[3] != "any" and set(data["artifacts"]) == {filename, sdist_name},
                "Expected exact native wheel and sdist filenames")
        tags = {"-".join(parts) for parts in itertools.product(*[part.split(".") for part in match.groups()])}
        require(isinstance(native.get("tags"), list) and len(native["tags"]) == len(tags) and set(native["tags"]) == tags,
                "Native wheel receipt tags differ from filename")
        with zipfile.ZipFile(io.BytesIO(data["artifacts"][filename])) as archive:
            entries = archive.infolist()
            require(len(entries) <= 10000 and len({item.filename for item in entries}) == len(entries)
                    and sum(item.file_size for item in entries) <= MAX_BYTES, "Invalid native wheel archive")
            for item in entries:
                plain_path(item.filename.rstrip("/"))
                require(not item.is_dir() and (item.external_attr >> 16 & 0o170000) in {0, 0o100000},
                        "Native wheel contains special archive members")
            files = {item.filename: archive.read(item) for item in entries}
        info = normalized + "-" + self.version + ".dist-info/"
        require({path.split("/")[0] for path in files if ".dist-info/" in path} == {info.rstrip("/")},
                "Native wheel metadata directory identity differs")
        require(receipt.get("wheel_files") == {path: sha(value) for path, value in files.items()}, "Native wheel inventory differs")
        wheel = Parser().parsestr(files[info + "WHEEL"].decode())
        require(not wheel.defects and wheel.get_all("Wheel-Version") == ["1.0"]
                and wheel.get_all("Root-Is-Purelib") == ["false"] and not wheel.get_all("Build")
                and len(wheel.get_all("Tag", [])) == len(tags) and set(wheel.get_all("Tag", [])) == tags,
                "Native WHEEL metadata differs from filename or platform layout")
        records = list(csv.reader(io.StringIO(files[info + "RECORD"].decode(), newline="")))
        require(all(len(row) == 3 for row in records) and len({row[0] for row in records}) == len(records)
                and {row[0] for row in records} == set(files), "Native wheel RECORD inventory differs")
        for path, digest, size in records:
            expected_hash = "sha256=" + base64.urlsafe_b64encode(bytes.fromhex(sha(files[path]))).decode().rstrip("=")
            require((digest, size) == ("", "") if path == info + "RECORD" else (digest, size) == (expected_hash, str(len(files[path]))),
                    "Native wheel RECORD hash or size differs")
        extension = native["extension"]
        module = document["tool"]["maturin"]["module-name"].replace(".", "/")
        require(extension in files and extension.startswith(module + ".") and extension.endswith((".so", ".pyd"))
                and sha(files[extension]) == native.get("extension_sha256"), "Native extension identity/hash differs")
        sdist, _ = archive_files(data["artifacts"][sdist_name])
        prefix = normalized + "-" + self.version + "/"
        require(all(path.startswith(prefix) for path in sdist), "Native sdist root differs")
        sdist = {path.removeprefix(prefix): value for path, value in sdist.items()}
        require(receipt.get("sdist_files") == {path: sha(value) for path, value in sdist.items()}, "Native sdist inventory differs")
        source_map = receipt.get("sdist_source_files", {})
        required = {path for path in data["source"] if path.startswith(("py/", "LICENSES/"))}
        require(required <= set(source_map.values()) and source_map.get("pyproject.toml") == "py/pyproject.toml",
                "Native sdist omits committed Python source")
        for target, source in source_map.items():
            plain_path(target)
            require(source in data["source"] and sdist.get(target) == data["source"][source], "Native sdist source bytes differ")
        for path, value in data["source"].items():
            if path.startswith("py/" + normalized + "/") and path.endswith((".py", ".pyi", "/py.typed")):
                require(files.get(path.removeprefix("py/")) == value, "Native wheel Python source bytes differ")
        require(files.get(normalized + "/source/" + sdist_name) == data["artifacts"][sdist_name],
                "Native wheel corresponding source differs from verified sdist")
        fields = python_metadata(files[info + "METADATA"])
        require(fields == python_metadata(sdist["PKG-INFO"]), "Wheel and sdist metadata differ")
        values = dict(fields)
        require(values.get("name") == self.name and values.get("version") == self.version
                and values.get("requires_python") == project.get("requires-python")
                and values.get("license_expression") == project.get("license"), "Python archive metadata differs from source")
        license_files = [value for key, value in fields if key == "license_file"]
        require(license_files and len(license_files) == len(set(license_files)), "Native Python license inventory is missing/duplicate")
        for path in license_files:
            plain_path(path)
            require(path in sdist and files.get(info + "licenses/" + path) == sdist[path], "Native wheel/sdist license bytes differ")
        data["python"] = {filename: {"fields": fields, "filetype": "bdist_wheel", "pyversion": match[1]},
                          sdist_name: {"fields": fields, "filetype": "sdist", "pyversion": "source"}}


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, response, code, message, headers, new_url):
        return None


class Http:
    def request(self, method, url, *, data=None, headers=None, missing=False, download=False, limit=MAX_BYTES):
        parsed = urllib.parse.urlsplit(url)
        require(parsed.scheme == "https" and not parsed.username and not parsed.password, "Registry requests require an HTTPS origin")
        request_headers = {"User-Agent": USER_AGENT, "Cache-Control": "no-cache", **(headers or {})}
        request = urllib.request.Request(url, data=data, headers=request_headers, method=method)
        try:
            with urllib.request.build_opener(NoRedirect()).open(request, timeout=60) as response:
                payload = response.read(limit + 1)
                require(len(payload) <= limit, "Remote payload exceeds expected bounds")
                return payload
        except urllib.error.HTTPError as error:
            if missing and error.code == 404:
                return None
            if download and method == "GET" and error.code in {301, 302, 303, 307, 308}:
                target = error.headers.get("Location", "")
                host = urllib.parse.urlsplit(target).hostname
                require(host in {"release-assets.githubusercontent.com", "objects.githubusercontent.com"}, "Unexpected artifact redirect origin")
                # Signed artifact URLs carry their own authorization. Never forward headers.
                return self.request("GET", target, limit=limit)
            raise HttpFailure(method, parsed.scheme + "://" + parsed.netloc, error.code, error.headers.get("Retry-After")) from None
        except (urllib.error.URLError, TimeoutError, OSError):
            raise Failure(f"{method} {parsed.hostname} response is unavailable; no request was retried") from None

    def json(self, method, url, **kwargs):
        payload = self.request(method, url, **kwargs)
        if payload is None:
            return None
        value = json.loads(payload)
        require(value is not None or not kwargs.get("missing"),
                "A null JSON response is not confirmed HTTP 404 absence")
        return value


class Remote:
    def __init__(self, bundle, http=None, environment=None):
        self.bundle = bundle
        self.http = http or Http()
        self.environment = os.environ if environment is None else environment
        self.api = "https://api.github.com/repos/" + bundle.repository
        self.release = None

    def github_headers(self):
        token = self.environment.get("GH_TOKEN", "")
        return {"Accept": "application/vnd.github+json", **({"Authorization": "Bearer " + token} if token else {})}

    def verify_tag(self):
        manifest = self.bundle.manifest
        result = self.http.json("GET", self.api + "/git/ref/tags/" + urllib.parse.quote(manifest["tag"], safe=""), headers=self.github_headers())
        target = result["object"]
        for _ in range(5):
            if target["type"] == "commit":
                break
            require(target["type"] == "tag" and re.fullmatch(r"[0-9a-f]{40}", target["sha"]), "Unexpected release tag object")
            target = self.http.json("GET", self.api + "/git/tags/" + target["sha"], headers=self.github_headers())["object"]
        require(target["type"] == "commit" and target["sha"] == manifest["tag_commit"], "Live release tag differs from reviewed tag commit")
        self.release = self.http.json("GET", self.api + "/releases/tags/" + urllib.parse.quote(manifest["tag"], safe=""), headers=self.github_headers())
        require(self.release["tag_name"] == manifest["tag"] and isinstance(self.release["id"], int), "Matching GitHub release is required for durable publication journals")

    def asset(self, name):
        require(self.release is not None, "Release identity must be verified first")
        matches = []
        for page in range(1, 101):
            items = self.http.json("GET", self.api + f"/releases/{self.release['id']}/assets?per_page=100&page={page}", headers=self.github_headers())
            matches.extend(item for item in items if item["name"] == name)
            if len(items) < 100:
                break
        else:
            raise Failure("Release asset inventory exceeds publication bounds")
        require(len(matches) <= 1, "Ambiguous existing publication journal asset")
        if not matches:
            return None
        item = matches[0]
        payload = self.http.request("GET", self.api + f"/releases/assets/{item['id']}",
                                    headers={**self.github_headers(), "Accept": "application/octet-stream"}, download=True)
        require(item.get("digest") == "sha256:" + sha(payload), "GitHub journal asset digest mismatch")
        return json.loads(payload)

    def persist_asset(self, name, value, *, claim=False):
        existing = self.asset(name)
        if existing is not None:
            require(not claim, "Another publication attempt already owns the immutable intent")
            require(existing == value, "Conflicting immutable publication journal")
            return
        require(bool(self.environment.get("GH_TOKEN")), "GH_TOKEN is required to persist publication intent")
        url = f"https://uploads.github.com/repos/{self.bundle.repository}/releases/{self.release['id']}/assets?name=" + urllib.parse.quote(name, safe="")
        self.http.request("POST", url, data=json_bytes(value), headers={**self.github_headers(), "Content-Type": "application/json"})
        require(self.asset(name) == value, "Publication intent was not durably verified; registry upload refused")

    def downloaded(self, url, expected, allowed_hosts):
        require(urllib.parse.urlsplit(url).hostname in allowed_hosts, "Unexpected registry download origin")
        actual = self.http.request("GET", url, limit=len(expected))
        require(actual == expected, "Existing registry version contains different bytes")

    def present(self, registry, data):
        bundle = self.bundle
        name, version = bundle.name, bundle.version
        if registry == "cargo":
            metadata = self.http.json("GET", f"https://crates.io/api/v1/crates/{name}/{version}", missing=True)
            if metadata is None:
                return set()
            require(not metadata["version"].get("yanked"), "Existing Cargo version is yanked")
            filename, payload = next(iter(data["artifacts"].items()))
            require(metadata["version"].get("checksum") == sha(payload), "Existing crate checksum conflict")
            self.downloaded(f"https://static.crates.io/crates/{name}/{filename}", payload, {"static.crates.io"})
            return {filename}
        if registry == "npm":
            encoded = urllib.parse.quote(data["js_name"], safe="@")
            metadata = self.http.json("GET", f"https://registry.npmjs.org/{encoded}/{version}", missing=True)
            if metadata is None:
                return set()
            require(metadata.get("name") == data["js_name"] and metadata.get("version") == version, "Existing npm identity conflict")
            filename, payload = next(iter(data["artifacts"].items()))
            self.downloaded(metadata["dist"]["tarball"], payload, {"registry.npmjs.org"})
            return {filename}
        if registry == "jsr":
            base = "https://jsr.io/" + data["js_name"] + "/" + version
            metadata = self.http.json("GET", base + "_meta.json", missing=True)
            if metadata is None:
                return set()
            package_metadata = self.http.json("GET", "https://jsr.io/" + data["js_name"] + "/meta.json")
            require(version in package_metadata.get("versions", {}) and not package_metadata["versions"][version].get("yanked"),
                    "Existing JSR version is yanked or missing from package metadata")
            require(metadata.get("exports") == data["jsr_exports"], "Existing JSR exports conflict")
            manifest = metadata["manifest"]
            expected = data["jsr_files"]
            require(set(manifest) == {"/" + path for path in expected}, "Existing JSR manifest has missing or unexpected files")
            for path, payload in expected.items():
                require(manifest["/" + path].get("checksum") == "sha256-" + sha(payload), "Existing JSR file checksum conflict")
                self.downloaded(base + "/" + urllib.parse.quote(path, safe="/"), payload, {"jsr.io"})
            return {"source"}
        metadata = self.http.json("GET", f"https://pypi.org/pypi/{name}/{version}/json", missing=True)
        if metadata is None:
            return set()
        present = set()
        for item in metadata["urls"]:
            filename = item["filename"]
            require(filename in data["artifacts"] and filename not in present, "Unexpected/duplicate existing PyPI file")
            require(not item.get("yanked") and item["digests"]["sha256"] == sha(data["artifacts"][filename]), "Existing PyPI file is yanked or conflicts")
            self.downloaded(item["url"], data["artifacts"][filename], {"files.pythonhosted.org"})
            present.add(filename)
        return present

    def auth_mode(self, registry):
        variable = AUTH_MODES[registry]
        mode = self.environment.get(variable, "token")
        require(mode in {"token", "trusted"}, f"Unknown {variable} value; expected token or trusted")
        return mode

    def github_id_token(self, registry, audience):
        """Request one GitHub Actions OIDC ID token; fail closed outside a permitted job."""
        label = LABELS[registry]
        request_url = self.environment.get("ACTIONS_ID_TOKEN_REQUEST_URL", "")
        request_token = self.environment.get("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "")
        require(self.environment.get("GITHUB_ACTIONS") == "true" and request_url and request_token,
                f"{AUTH_MODES[registry]}=trusted requires a GitHub Actions job with `permissions: id-token: write` "
                "(ACTIONS_ID_TOKEN_REQUEST_URL/ACTIONS_ID_TOKEN_REQUEST_TOKEN are absent); no token fallback was attempted")
        separator = "&" if urllib.parse.urlsplit(request_url).query else "?"
        try:
            response = self.http.json("GET", request_url + separator + "audience=" + urllib.parse.quote(audience, safe=""),
                                      headers={"Authorization": "Bearer " + request_token, "Accept": "application/json"},
                                      limit=OIDC_RESPONSE_LIMIT)
        except HttpFailure as error:
            raise Failure(f"GitHub refused the {label} OIDC ID token request (HTTP {error.outcome['status']}); "
                          "no intent was recorded and nothing was uploaded") from None
        value = response.get("value") if isinstance(response, dict) else None
        require(isinstance(value, str) and value.count(".") == 2, "GitHub did not return an OIDC ID token")
        return value

    def exchange(self, registry, url, **kwargs):
        """Exchange an ID token for a short-lived registry token before any intent is claimed."""
        label = LABELS[registry]
        try:
            response = self.http.json("POST", url, limit=OIDC_RESPONSE_LIMIT, **kwargs)
        except HttpFailure as error:
            raise Failure(f"{label} refused the trusted-publishing token exchange (HTTP {error.outcome['status']}); "
                          "check the registry's trusted publisher rule. No intent was recorded and nothing was uploaded") from None
        token = response.get("token") if isinstance(response, dict) else None
        require(isinstance(token, str) and token, f"{label} token exchange returned no publication token")
        return token

    def trusted_credentials(self, registry, data):
        if registry == "jsr":
            # Deno's `deno publish` requests the same audience: a JSON permission
            # list binding scope, package, version and the gzip body's SHA-256.
            scope, name = data["js_name"].removeprefix("@").split("/", 1)
            permission = {"permission": "package/publish", "scope": scope, "package": name,
                          "version": self.bundle.version, "tarballHash": "sha256-" + sha(jsr_body(data))}
            audience = json.dumps({"permissions": [permission]}, separators=(",", ":"))
            return GithubOidcToken(self.github_id_token(registry, audience))
        if registry == "npm":
            identity = self.github_id_token(registry, NPM_OIDC_AUDIENCE)
            # npm-package-arg's escapedName: a scoped name keeps `@` and encodes `/`.
            escaped = urllib.parse.quote(data["js_name"], safe="@").replace("%2F", "%2f")
            return self.exchange("npm", "https://registry.npmjs.org/-/npm/v1/oidc/token/exchange/package/" + escaped,
                                 data=b"", headers={"Authorization": "Bearer " + identity, "Accept": "application/json"})
        identity = self.github_id_token(registry, PYPI_OIDC_AUDIENCE)
        return self.exchange("pypi", "https://pypi.org/_/oidc/mint-token", data=json.dumps({"token": identity}).encode(),
                             headers={"Content-Type": "application/json", "Accept": "application/json"})

    def credentials(self, registry, data=None):
        require(bool(self.environment.get("GH_TOKEN")), "GH_TOKEN is missing for durable publication journaling")
        if registry != "cargo" and self.auth_mode(registry) == "trusted":
            require(data is not None, "Trusted publication requires the inspected channel data")
            return self.trusted_credentials(registry, data)
        variable = TOKEN_NAMES[registry]
        token = self.environment.get(variable, "")
        require(bool(token) and token != "null", variable + " is missing; configure the publication credential before uploading")
        if registry == "cargo":
            mode = self.environment.get("RELEASE_CARGO_AUTH", "token")
            require(mode in {"token", "trusted"}, "Unknown Cargo authentication mode")
            # Cargo's exchange is performed by rust-lang/crates-io-auth-action,
            # which supplies its short-lived result as CARGO_REGISTRY_TOKEN.
            if mode == "token":
                settings = self.http.json("GET", "https://crates.io/api/v1/crates/" + self.bundle.name,
                                          headers={"Authorization": token}, missing=True)
                require(settings is None or settings["crate"].get("trustpub_only") is False,
                        "Cargo requires trusted publishing; token fallback is not enabled. No registry policy was changed")
            else:
                require(self.environment.get("GITHUB_ACTIONS") == "true" and self.environment.get("ACTIONS_ID_TOKEN_REQUEST_URL"), "Trusted Cargo authentication requires the configured GitHub OIDC step")
        return token

    def upload(self, registry, unit, data, token):
        if registry == "cargo":
            metadata = json.dumps(data["cargo_metadata"]).encode()
            archive = data["artifacts"][unit]
            payload = struct.pack("<I", len(metadata)) + metadata + struct.pack("<I", len(archive)) + archive
            result = self.http.json("PUT", "https://crates.io/api/v1/crates/new", data=payload,
                                    headers={"Authorization": token, "Content-Type": "application/octet-stream"})
            # Cargo's publish response contains optional warnings, not the
            # `ok: true` field used by the yank/owner endpoints. Exact registry
            # downloads still establish completion in the publication journal.
            require(isinstance(result, dict) and not result.get("errors")
                    and ("ok" not in result or result["ok"] is True), "Cargo did not confirm publication")
            return
        if registry == "pypi":
            artifact = data["python"][unit]
            payload = data["artifacts"][unit]
            boundary = "ccid-" + secrets.token_hex(24)
            fields = [(":action", "file_upload"), ("protocol_version", "1"), *artifact["fields"],
                      ("filetype", artifact["filetype"]), ("pyversion", artifact["pyversion"]), ("sha256_digest", sha(payload))]
            chunks = [f'--{boundary}\r\nContent-Disposition: form-data; name="{key}"\r\n\r\n{value}\r\n'.encode() for key, value in fields]
            chunks.extend([f'--{boundary}\r\nContent-Disposition: form-data; name="content"; filename="{unit}"\r\nContent-Type: application/octet-stream\r\n\r\n'.encode(), payload, f"\r\n--{boundary}--\r\n".encode()])
            authorization = base64.b64encode(("__token__:" + token).encode()).decode()
            self.http.request("POST", "https://upload.pypi.org/legacy/", data=b"".join(chunks),
                              headers={"Authorization": "Basic " + authorization, "Content-Type": "multipart/form-data; boundary=" + boundary})
            return
        if registry == "npm":
            # Match npm's libnpmpublish metadata envelope, with one HTTP PUT and
            # no client-side retry, lifecycle scripts, or provenance fabrication.
            payload = data["artifacts"][unit]
            manifest = {**data["js_manifest"]}
            require(not manifest.get("private") and "packageExtensions" not in manifest, "npm manifest forbids publication")
            manifest.pop("patchedDependencies", None)
            name, version = manifest["name"], manifest["version"]
            tarball_name = f"{name}-{version}.tgz"
            manifest["_id"] = name + "@" + version
            manifest["dist"] = {"integrity": "sha512-" + base64.b64encode(hashlib.sha512(payload).digest()).decode(),
                                "shasum": hashlib.sha1(payload, usedforsecurity=False).hexdigest(),
                                "tarball": "https://registry.npmjs.org/" + name + "/-/" + tarball_name}
            document = {"_id": name, "name": name, "description": manifest.get("description", ""),
                        "dist-tags": {"latest": version}, "versions": {version: manifest}, "access": "public",
                        "_attachments": {tarball_name: {"content_type": "application/octet-stream",
                                                        "data": base64.b64encode(payload).decode(), "length": len(payload)}}}
            self.http.request("PUT", "https://registry.npmjs.org/" + urllib.parse.quote(name, safe="@"), data=json_bytes(document),
                              headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"})
            return
        # JSR's management API accepts the inspected file inventory in a gzip tar.
        # These are recorded source transformations, not a compilation step.
        # A trusted (OIDC) credential is bound to this exact body's SHA-256.
        scope, name = data["js_name"].removeprefix("@").split("/", 1)
        scheme = "githuboidc " if isinstance(token, GithubOidcToken) else "Bearer "
        response = self.http.json("POST", f"https://api.jsr.io/scopes/{scope}/packages/{name}/versions/{self.bundle.version}?config=/jsr.json",
                                  data=jsr_body(data),
                                  headers={"Authorization": scheme + token, "Content-Type": "application/octet-stream", "Content-Encoding": "gzip"})
        require(re.fullmatch(r"[0-9a-f-]{36}", response.get("id", "")), "JSR did not return a publishing task identity")
        require(response.get("packageScope") == scope and response.get("packageName") == name
                and response.get("packageVersion") == self.bundle.version, "JSR publishing task identity mismatch")
        return {"jsr_task": response["id"]}

    def wait_for_visibility(self, registry, unit, data, response):
        deadline = time.monotonic() + 180
        while True:
            if response and response.get("jsr_task"):
                task = self.http.json("GET", "https://api.jsr.io/publishing_tasks/" + response["jsr_task"])
                require(task.get("id") == response["jsr_task"], "JSR publishing task identity changed")
                require(task.get("status") != "failure", "JSR publishing task failed; no upload was retried")
            present = self.present(registry, data)
            if unit in present:
                return present
            require(time.monotonic() < deadline, "Exact registry bytes remain pending; reconcile the saved intent/response without another upload")
            time.sleep(5)



class Publisher:
    def __init__(self, bundle, remote, journal_root):
        self.bundle, self.remote = bundle, remote
        root = Path(journal_root)
        require(root.is_absolute() and not root.is_symlink(), "Publication journal requires an absolute persistent directory")
        self.root = root / bundle.repository.replace("/", "--") / bundle.version
        self.root.mkdir(parents=True, exist_ok=True)

    def execute(self, registries, publish=False):
        self.remote.verify_tag()
        result = {}
        lock_path = self.root / "publication.lock"
        require(not lock_path.is_symlink(), "Publication lock must not be a symlink")
        with lock_path.open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            for registry in registries:
                data = self.bundle.channels[registry]
                present = self.remote.present(registry, data)
                units = {"source"} if registry == "jsr" else set(data["artifacts"])
                result[registry] = {"verified": sorted(present), "missing": sorted(units - present)}
                if not publish:
                    continue
                for unit in sorted(units):
                    if unit in present:
                        continue
                    identity = {**data["identity"], "unit": unit}
                    intent = {"schema": 1, "status": "attempted", "identity": identity, "owner": secrets.token_hex(24)}
                    stem = "publication-" + registry + "-" + unit
                    require(re.fullmatch(r"[A-Za-z0-9_.-]+", stem), "Unsafe publication journal name")
                    local = self.root / (stem + "-intent.json")
                    remote_name = stem + "-intent.json"
                    existing = self.remote.asset(remote_name)
                    if local.exists() or existing is not None:
                        if local.exists():
                            require(json.loads(local.read_text()).get("identity") == identity, "Local publication journal identity conflict")
                        if existing is not None:
                            require(existing.get("identity") == identity, "Remote publication journal identity conflict")
                        # No upload is allowed after an earlier durable intent, even
                        # when a fresh registry query still returns 404.
                        raise Failure("Previous publication intent exists but exact remote bytes are absent; reconcile read-only, never retry the upload")
                    token = self.remote.credentials(registry, data)
                    durable_write(local, intent)
                    self.remote.persist_asset(remote_name, intent, claim=True)
                    present = self.finish_upload(registry, unit, data, token, stem, intent, 0)
                result[registry] = {"verified": sorted(present), "missing": sorted(units - present)}
        return result

    def immutable_local(self, name, value):
        path = self.root / name
        if path.exists() or path.is_symlink():
            require(regular_bytes(path) == json_bytes(value), "Conflicting immutable local publication journal")
        else:
            durable_write(path, value)

    def finish_upload(self, registry, unit, data, token, stem, intent, attempt):
        failure = None
        response = None
        try:
            response = self.remote.upload(registry, unit, data, token)
        except Failure as error:
            failure = error
            evidence = {"schema": 1, "status": "upload-failed", "identity": intent["identity"],
                        "bundle_sha256": self.bundle.digest, "attempt": attempt,
                        "intent_sha256": sha(json_bytes(intent)),
                        "outcome": getattr(error, "outcome", {"kind": "uncertain"})}
            self.immutable_local(stem + "-error.json", evidence)
            self.remote.persist_asset(stem + "-error.json", evidence)
        if response:
            evidence = {"schema": 1, "identity": intent["identity"], "response": response}
            self.immutable_local(stem + "-response.json", evidence)
            self.remote.persist_asset(stem + "-response.json", evidence)
        present = self.remote.present(registry, data) if failure else self.remote.wait_for_visibility(registry, unit, data, response)
        require(unit in present, str(failure) if failure else "Exact registry bytes are unavailable; do not retry")
        completion = {"schema": 1, "status": "download-verified", "identity": intent["identity"]}
        self.immutable_local(stem + "-complete.json", completion)
        self.remote.persist_asset(stem + "-complete.json", completion)
        return present

    def recover_rejected(self, evidence, expected_sha256):
        """One explicit new claim after a reviewed, definite PyPI 429 rejection."""
        require(sha(json_bytes(evidence)) == expected_sha256, "Rejection evidence SHA-256 differs from the reviewed record")
        require(isinstance(evidence, dict) and set(evidence) == {"schema", "status", "identity", "bundle_sha256", "attempt", "intent_sha256", "outcome"}
                and evidence.get("schema") == 1 and evidence.get("status") == "upload-failed"
                and evidence.get("bundle_sha256") == self.bundle.digest, "Rejection evidence has a different bundle or schema")
        identity = evidence.get("identity", {})
        require(isinstance(identity, dict) and identity.get("registry") == "pypi" and "pypi" in self.bundle.channels, "Only an explicit PyPI rejection can be recovered")
        data = self.bundle.channels["pypi"]
        unit = identity.get("unit")
        require(isinstance(unit, str) and unit in data["artifacts"] and identity == {**data["identity"], "unit": unit}, "Rejected artifact/source identity differs")
        attempt = evidence.get("attempt")
        require(type(attempt) is int and 0 <= attempt < 100, "Invalid rejected attempt number")
        base = "publication-pypi-" + unit
        require(re.fullmatch(r"[A-Za-z0-9_.-]+", base), "Unsafe publication journal name")
        stem = base if attempt == 0 else base + "-attempt-" + str(attempt)
        outcome = evidence.get("outcome", {})
        require(isinstance(outcome, dict) and outcome.get("status") == 429 and outcome.get("method") == "POST"
                and outcome.get("origin") == "https://upload.pypi.org", "Only a definite PyPI POST HTTP429 rejection permits recovery")
        legacy = outcome.get("kind") == "prior-session-report"
        if legacy:
            require(attempt == 0 and set(outcome) == {"kind", "status", "method", "origin", "observed_not_after", "report", "raw_response_retained", "retry_after_seconds"},
                    "Invalid prior-session rejection evidence")
            require(outcome["raw_response_retained"] is False and outcome["retry_after_seconds"] is None, "Prior-session reports cannot claim captured HTTP details")
            report = outcome["report"]
            require(isinstance(report, dict) and set(report) == {"commit", "path", "blob", "sha256", "lines"}, "Expected an exact committed prior-session report citation")
            require(all(isinstance(report[key], str) for key in ("commit", "blob", "sha256"))
                    and re.fullmatch(r"[0-9a-f]{40}", report["commit"]) and re.fullmatch(r"[0-9a-f]{40}", report["blob"])
                    and re.fullmatch(r"[0-9a-f]{64}", report["sha256"]), "Invalid prior-session report hashes")
            plain_path(report["path"])
            require(isinstance(report["lines"], list) and len(report["lines"]) == 2
                    and all(type(n) is int and n > 0 for n in report["lines"]) and report["lines"][0] <= report["lines"][1], "Invalid prior-session report line range")
            observed, delay = outcome["observed_not_after"], None
            evidence_suffix = "-legacy-rejection.json"
        else:
            require(outcome.get("kind") == "http-response"
                    and set(outcome) == {"kind", "status", "method", "origin", "observed_at", "retry_after_seconds"}, "Unknown upload outcomes cannot be recovered")
            observed, delay = outcome["observed_at"], outcome["retry_after_seconds"]
            evidence_suffix = "-error.json"
        require(type(observed) is int and observed > 0 and (delay is None or type(delay) is int and delay >= 0), "Invalid rejection timing evidence")
        not_before = observed + max(RECOVERY_COOLDOWN, delay or 0)
        require(time.time() >= not_before, f"Explicit recovery is pending until Unix time {not_before}; client cooldown and captured Retry-After apply")
        self.remote.verify_tag()
        lock_path = self.root / "publication.lock"
        require(not lock_path.is_symlink(), "Publication lock must not be a symlink")
        with lock_path.open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            original = self.remote.asset(stem + "-intent.json")
            require(original is not None and original.get("identity") == identity
                    and sha(json_bytes(original)) == evidence.get("intent_sha256"), "Original immutable remote intent differs from rejection evidence")
            local_intent = self.root / (stem + "-intent.json")
            if local_intent.exists() or local_intent.is_symlink():
                require(regular_bytes(local_intent) == json_bytes(original), "Original local and remote intents differ")
            present = self.remote.present("pypi", data)
            if unit in present:
                return {"pypi": {"verified": sorted(present), "missing": sorted(set(data["artifacts"]) - present)}}
            for suffix in ("-complete.json", "-response.json"):
                require(not (self.root / (stem + suffix)).exists() and self.remote.asset(stem + suffix) is None,
                        "A prior accepted upload cannot be recovered while its exact bytes are absent")
            if legacy:
                require(not (self.root / (stem + "-error.json")).exists() and self.remote.asset(stem + "-error.json") is None,
                        "A prior-session report cannot replace captured upload outcome evidence")
            else:
                require(self.remote.asset(stem + evidence_suffix) == evidence, "Captured rejection is not durably recorded on the original attempt")
            self.immutable_local(stem + evidence_suffix, evidence)
            self.remote.persist_asset(stem + evidence_suffix, evidence)
            next_stem = base + "-attempt-" + str(attempt + 1)
            require(not (self.root / (next_stem + "-intent.json")).exists()
                    and self.remote.asset(next_stem + "-intent.json") is None, "Recovery already claimed; reconcile its outcome instead of repeating it")
            token = self.remote.credentials("pypi", data)
            intent = {"schema": 1, "status": "attempted", "identity": identity, "owner": secrets.token_hex(24),
                      "attempt": attempt + 1, "previous_intent_sha256": evidence["intent_sha256"],
                      "rejection_sha256": expected_sha256}
            self.immutable_local(next_stem + "-intent.json", intent)
            self.remote.persist_asset(next_stem + "-intent.json", intent, claim=True)
            present = self.finish_upload("pypi", unit, data, token, next_stem, intent, attempt + 1)
            return {"pypi": {"verified": sorted(present), "missing": sorted(set(data["artifacts"]) - present)}}


def fetch_bundle(url, target, expected_sha256, repository, http=None):
    """Download only this repository's public release asset, without credentials."""
    parsed = urllib.parse.urlsplit(url)
    path = urllib.parse.unquote(parsed.path)
    prefix = "/" + repository + "/releases/download/"
    require(parsed.scheme == "https" and parsed.netloc == "github.com" and path.startswith(prefix)
            and not parsed.query and not parsed.fragment and not any(part in {".", ".."} for part in path.split("/")),
            "Bundle URL must be a public release asset in the selected repository")
    require(re.fullmatch(r"[0-9a-f]{64}", expected_sha256 or ""), "Supply the reviewed bundle SHA-256")
    target = Path(target)
    if target.exists():
        require(sha(regular_bytes(target)) == expected_sha256, "Existing bundle differs; no replacement attempted")
        return
    payload = (http or Http()).request("GET", url, download=True)
    require(len(payload) <= MAX_BYTES and sha(payload) == expected_sha256, "Downloaded bundle checksum mismatch")
    target.parent.mkdir(parents=True, exist_ok=True)
    with target.open("xb") as stream:
        stream.write(payload)
        stream.flush()
        os.fsync(stream.fileno())


def regular_bytes(path):
    path = Path(path)
    require(path.is_file() and not path.is_symlink() and path.stat().st_size <= MAX_BYTES, "Expected a bounded regular import file")
    return path.read_bytes()


def make_tar(files):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w", format=tarfile.PAX_FORMAT) as archive:
        for path, payload in sorted(files.items()):
            info = tarfile.TarInfo(str(plain_path(path)))
            info.size, info.mode, info.mtime = len(payload), 0o644, 0
            archive.addfile(info, io.BytesIO(payload))
    return output.getvalue()


def import_bundle(specification, output, repository=None):
    """Copy exact existing source/receipts/archives into a portable reviewed bundle."""
    spec = json.loads(regular_bytes(specification))
    require(spec.get("schema") == 1 and isinstance(spec.get("channels"), dict)
            and spec["channels"] and set(spec["channels"]) <= CHANNELS, "Invalid bundle import specification")
    repository = repository or spec["repository"]
    require(spec["repository"] == repository, "Import repository mismatch")
    manifest = {"schema": 1, "repository": repository, "tag_commit": spec["tag_commit"], "channels": {}}
    files = {}
    for registry, item in spec["channels"].items():
        source_payload = regular_bytes(item["source_archive"])
        source, commit = archive_files(source_payload, source=True)
        package = tomllib.loads(source["Cargo.toml"].decode())["package"]
        if "package" not in manifest:
            manifest.update(package=package["name"], version=package["version"], tag="v" + package["version"])
        require(package["name"] == manifest["package"] and package["version"] == manifest["version"], "Imported channels have different package/version identities")
        directory = Path(item["artifacts"])
        require(directory.is_dir() and not directory.is_symlink(), "Expected the existing artifact directory")
        require(regular_bytes(directory / "SOURCE_COMMIT").decode().strip() == commit, "Existing artifact SOURCE_COMMIT mismatch")
        check = {"cargo": "rust-package", "npm": "js-package", "jsr": "js-package", "pypi": "python-package"}[registry]
        if registry == "jsr" and (directory / "jsr-package.json").is_file():
            check = "jsr-package"
        receipt_payload = regular_bytes(directory / (check + ".json"))
        receipt = json.loads(receipt_payload)
        checksums = {}
        for line in regular_bytes(directory / "SHA256SUMS").decode().splitlines():
            match = re.fullmatch(r"([0-9a-f]{64}) [ *]([A-Za-z0-9_.+-]+)", line)
            require(match and match[2] not in checksums, "Malformed or duplicate prepared checksum entry")
            checksums[match[2]] = match[1]
        entry = {"source_commit": commit, "source_archive": registry + "/source.tar", "source_sha256": sha(source_payload),
                 "receipt": registry + "/" + check + ".json", "receipt_sha256": sha(receipt_payload), "artifacts": {}}
        files[entry["source_archive"]] = source_payload
        files[entry["receipt"]] = receipt_payload
        for name, digest in receipt["artifacts"].items():
            require("/" not in name and str(plain_path(name)) == name, "Unsafe prepared artifact name")
            payload = regular_bytes(directory / name)
            require(sha(payload) == digest and checksums.get(name) == digest, "Prepared archive differs from receipt/checksums")
            path = registry + "/artifacts/" + name
            files[path] = payload
            entry["artifacts"][name] = {"path": path, "sha256": digest}
        if not receipt.get("tool_revision"):
            hosted = regular_bytes(item["hosted_receipt"])
            entry.update(hosted_receipt=registry + "/hosted-run.json", hosted_receipt_sha256=sha(hosted))
            files[entry["hosted_receipt"]] = hosted
        manifest["channels"][registry] = entry
    files["release.json"] = json_bytes(manifest)
    initial = make_tar(files)
    inspected = Bundle(initial, sha(initial), repository, create_jsr_receipt=True)
    # The JSR transformation inventory is now explicit in the hashed bundle that
    # an operator reviews. Publication never adds or invents transformation rules.
    files["release.json"] = json_bytes(inspected.manifest)
    payload = make_tar(files)
    Bundle(payload, sha(payload), repository)
    output = Path(output)
    require(not output.is_symlink(), "Release bundle output must not be a symlink")
    try:
        with output.open("xb") as stream:
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
    except FileExistsError:
        require(regular_bytes(output) == payload, "Existing bundle differs; output was not overwritten")
    return {"bundle": str(output), "sha256": sha(payload), "bytes": len(payload), "release": inspected.manifest}


def main(argv=None, *, repository=None):
    argv = list(__import__("sys").argv[1:] if argv is None else argv)
    if argv[:1] == ["bundle"]:
        parser = argparse.ArgumentParser(description="Import existing verified artifacts without builds or registry writes")
        parser.add_argument("--input", required=True)
        parser.add_argument("--output", required=True)
        args = parser.parse_args(argv[1:])
        try:
            print(json.dumps(import_bundle(args.input, args.output, repository), sort_keys=True))
            return 0
        except (Failure, KeyError, ValueError, OSError, tarfile.TarError, zipfile.BadZipFile) as error:
            print("Bundle import refused: " + str(error), file=__import__("sys").stderr)
            return 1
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=["inspect", "status", "publish", "recover-rejected"])
    parser.add_argument("--bundle", default=os.environ.get("RELEASE_BUNDLE"), required=not os.environ.get("RELEASE_BUNDLE"))
    parser.add_argument("--sha256", default=os.environ.get("RELEASE_BUNDLE_SHA256"), required=not os.environ.get("RELEASE_BUNDLE_SHA256"))
    parser.add_argument("--channels", default=os.environ.get("RELEASE_CHANNELS", "all"))
    parser.add_argument("--journal-root", default=os.environ.get("RELEASE_JOURNAL_ROOT"))
    parser.add_argument("--rejection-evidence")
    parser.add_argument("--rejection-sha256")
    if repository is None:
        parser.add_argument("--repository", required=True)
    args = parser.parse_args(argv)
    try:
        bundle_path = Path(args.bundle)
        if os.environ.get("RELEASE_BUNDLE_URL"):
            fetch_bundle(os.environ["RELEASE_BUNDLE_URL"], bundle_path, args.sha256, repository or args.repository)
        require(bundle_path.is_file() and not bundle_path.is_symlink() and bundle_path.stat().st_size <= MAX_BYTES, "Expected a bounded regular release bundle")
        bundle = Bundle(bundle_path.read_bytes(), args.sha256, repository or args.repository)
        channels = list(bundle.channels) if args.channels == "all" else args.channels.split(",")
        require(channels and len(channels) == len(set(channels)) and set(channels) <= set(bundle.channels), "Selected registry is missing from the inspected bundle")
        summary = {"repository": bundle.repository, "package": bundle.name, "version": bundle.version,
                   "bundle_sha256": bundle.digest, "tag_commit": bundle.manifest["tag_commit"],
                   "channels": {name: bundle.channels[name]["identity"] for name in channels}}
        if args.operation != "inspect":
            require(bool(args.journal_root), "Supply a persistent RELEASE_JOURNAL_ROOT")
            publisher = Publisher(bundle, Remote(bundle), args.journal_root)
            if args.operation == "recover-rejected":
                require(channels == ["pypi"] and args.rejection_evidence and args.rejection_sha256,
                        "Explicit recovery requires --channels pypi and reviewed rejection evidence plus its SHA-256")
                payload = regular_bytes(Path(args.rejection_evidence))
                require(sha(payload) == args.rejection_sha256, "Rejection evidence bytes differ from reviewed SHA-256")
                summary["registries"] = publisher.recover_rejected(json.loads(payload), args.rejection_sha256)
            else:
                require(args.rejection_evidence is None and args.rejection_sha256 is None, "Rejection evidence is only accepted by explicit recover-rejected")
                summary["registries"] = publisher.execute(channels, args.operation == "publish")
        print(json.dumps({"operation": args.operation, **summary}, sort_keys=True))
        return 0
    except (Failure, KeyError, ValueError, OSError, tarfile.TarError, zipfile.BadZipFile) as error:
        print("Publication refused: " + str(error), file=__import__("sys").stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
