# Publishing existing verified packages

`registry_publish.py` is an optional Python 3.12+ command resource shipped with
ccid source. It does not compile packages, install tools, schedule jobs, change
registry account policy, overwrite packages, or retry uncertain authenticated uploads.
The selected family repository invokes it from its verified `CI_TOOL_ARCHIVE`;
the Rust executor still supplies source verification, supervision and resource
bounds. Publication currently runs on Linux, where journal locking uses `flock`.

The publisher supports prepared Cargo, npm, JSR and Python wheel/sdist
artifacts. Each destination uses its existing protocol and verifies actual public
downloads. It does not manufacture build provenance. JSR's explicit npm import
specifier transformation is recorded separately from the tested input archive.

## Import the existing evidence

Create one import specification naming existing source archives and prepared
artifact directories. These are paths on the machine doing the import; they are
not stored as paths in the resulting portable bundle.

```json
{
  "schema": 1,
  "repository": "example/component",
  "tag_commit": "the-existing-tag-target-as-40-lowercase-hex-digits",
  "channels": {
    "cargo": {
      "source_archive": "/absolute/prepared/cargo-source.tar",
      "artifacts": "/absolute/prepared/cargo"
    },
    "npm": {
      "source_archive": "/absolute/prepared/javascript-source.tar",
      "artifacts": "/absolute/prepared/javascript"
    },
    "jsr": {
      "source_archive": "/absolute/prepared/javascript-source.tar",
      "artifacts": "/absolute/prepared/javascript"
    },
    "pypi": {
      "source_archive": "/absolute/prepared/python-source.tar",
      "artifacts": "/absolute/prepared/python"
    }
  }
}
```

Each directory supplies its existing `SOURCE_COMMIT`, `SHA256SUMS`, preparation
receipt and exact archives. Cargo uses `rust-package.json`; npm uses
`js-package.json`; JSR uses `jsr-package.json` when present, otherwise the
verified npm source/license payload; Python uses `python-package.json`.
A source archive must be the exact producing Git archive recorded by that
receipt, including its Git commit PAX header. A new publisher/CI commit is not the
producing commit. Different channels may deliberately use different tested
commits of the same package version, and the existing tag may differ; all those
identities are preserved and the live tag must match the declared tag target.

For npm, the source manifest may be at `js/@<owner>/<package>/package.json`
or `web/package.json`. If both exist they must describe the same manifest.
The archived manifest must equal that source manifest, except that npm may add
`gitHead` equal to the exact producing commit. This verified value is retained
in the registry metadata; other generated manifest changes are rejected.

Generated Rust/Wasm JSR packages use an explicit `web-wasm-v1` preparation
receipt. The source commits `.ci/jsr-input.json`, pinning the original npm
release's identity and archive hash. The artifact set includes that unchanged
npm archive as evidence and one flat `*-jsr.tar.gz` archive for JSR. The publisher
compares every generated file with the original npm bytes. The only permitted
changes are the committed `web/jsr.json`, lossless gzip-to-XZ recompression of
`source/dependencies.tar.gz`, the matching extraction instructions in both
READMEs, exact bounded substitutions from committed
`.ci/jsr-readme-replacements.json` on those two README files, and the exact
committed `.ci/jsr-readme.md` preface. Each original paragraph must occur exactly
once. No replacement rule can target runtime, license or dependency files. Decompressed source
hashes must match within a bounded streaming comparison. The check receipt and
reviewed bundle both record the full published inventory and each transformation.
The original npm manifest retains its original `gitHead`; current committed
entrypoints, types and source manifest must match it. The original npm archive
is evidence only; only the reconstructed JSR file set is uploaded.

Native Python uses an explicit `maturin-v1` preparation receipt and a committed
Maturin/PyO3 manifest. It adds one CPython native wheel alongside the verified
sdist without changing the existing pure-Python route. The native wheel's exact
filename, expanded platform tags, `WHEEL` metadata, extension hash and complete
`RECORD` inventory must agree. Wheel and sdist metadata and license files must
match; committed Python/Rust source files must appear byte-identically in the
sdist. The wheel must embed that exact sdist under `<package>/source/`, and both
archives require complete preparation inventories. These checks preserve the
tested platform identity; they do not claim an untested platform or create a
wheel during publication.

Cargo may normalize a workspace lockfile when packaging one member. If the
archived lock differs from the source lock, the publisher requires its package
set to equal the source graph reachable from the producing package. Every
retained package identity, checksum, metadata field and resolved dependency edge
must match. Shortened unambiguous dependency references are allowed; new or
upgraded dependencies, ambiguous references, and missing required edges are not.
This gate does not resolve dependencies or replace the required package-check
receipt. Other crate payload files remain identical to the producing source.

For an artifact produced by the manual hosted check route, add `hosted_receipt`
to that channel with the absolute path to its `hosted-run.json`. Import checks its
successful outcome, source/workflow identity, selected check and recorded tools.
Do not relabel hosted execution as ccid execution.

After the selected repository adapter has the verified shared source available:

```sh
python3 .ci/publish.py bundle --input import.json --output publication-bundle.tar
```

The import validates source, receipt and archive identities, then copies their
unchanged bytes into one deterministic tar archive. It creates `release.json`
with all relative paths/hashes. For JSR, it derives licenses from producing
`LICENSES/` and declared sibling `typst/vendor/.../LICENSES/` files, records the
complete published-file hash inventory and any source-to-published import
transformations, and then validates that explicit inventory again. Supported
rewrites are static named or namespace import/export declarations; text inside
comments, ordinary strings and template strings is retained. Unsupported bare
dependency declaration forms fail before publication.

Review the printed import receipt, especially producing/tag identities and JSR
transformations, and retain its external bundle SHA-256. Existing output files
are reused only when byte-identical. Attach the reviewed bundle to the matching
existing GitHub release when it needs a public hosted download, or stage the exact
file on the Crow worker using the existing artifact transport. The import performs
no registry requests and needs no publication credential. Workstations may stage
exact artifacts; repository tests/builds still run on CI.

## Inspect, reconcile and publish

The repository adapter accepts:

```sh
python3 .ci/publish.py inspect --bundle publication-bundle.tar --sha256 REVIEWED_SHA256
python3 .ci/publish.py status --bundle publication-bundle.tar --sha256 REVIEWED_SHA256 \
  --journal-root /absolute/persistent/publication
python3 .ci/publish.py publish --bundle publication-bundle.tar --sha256 REVIEWED_SHA256 \
  --channels cargo --journal-root /absolute/persistent/publication
```

Environment equivalents are `RELEASE_BUNDLE`, `RELEASE_BUNDLE_SHA256`,
`RELEASE_CHANNELS` (`all` or an explicit comma-separated selection), and
`RELEASE_JOURNAL_ROOT`. `inspect` is offline. `status` verifies the existing live
tag/release and exact registry bytes without uploading packages or remote journals.
Already-present exact packages do not require registry publication credentials.
Conflicting bytes, unexpected files, yanked versions and unknown HTTP outcomes
remain failures. Missing versions are reported explicitly.

`publish` requires `GH_TOKEN` for durable publication journaling and a
credential for each selected missing destination. Missing credentials are named
without reading secret stores or printing values. The default token mode uses
`CARGO_REGISTRY_TOKEN`, `NPM_TOKEN`, `JSR_TOKEN`, or `PYPI_TOKEN`; token-backed
publication stays on Crow. Its Cargo guard requires Cargo's live `trustpub_only`
setting to be false. The publisher never changes that setting or creates a
trusted publisher. A missing crate permits its first token publication; other
HTTP failures remain failures and are not retried.

### Trusted (OIDC) publication

Each registry has its own selector. Unset or `token` keeps the token mode above
unchanged; `trusted` uses the running GitHub Actions job's OIDC identity and
needs no long-lived registry token. Any other value is refused.

| Selector | Trusted credential |
|---|---|
| `RELEASE_CARGO_AUTH` | `CARGO_REGISTRY_TOKEN` from `rust-lang/crates-io-auth-action` in the same job; the settings read is skipped. |
| `RELEASE_JSR_AUTH` | A GitHub ID token whose audience is `{"permissions":[{"permission":"package/publish","scope":S,"package":P,"version":V,"tarballHash":"sha256-<hex>"}]}`, the SHA-256 of the exact gzip request body. It is sent as `Authorization: githuboidc <token>`, as `deno publish` does. |
| `RELEASE_NPM_AUTH` | A GitHub ID token with audience `npm:registry.npmjs.org`, exchanged by `POST https://registry.npmjs.org/-/npm/v1/oidc/token/exchange/package/<escaped name>`. The returned short-lived token authorizes the usual single PUT. |
| `RELEASE_PYPI_AUTH` | A GitHub ID token with audience `pypi`, exchanged by `POST https://pypi.org/_/oidc/mint-token` with `{"token": ...}`. The returned short-lived API token uploads as `__token__`. One token is minted per file. |

Trusted JSR, npm and PyPI modes need `GITHUB_ACTIONS=true` and the
`ACTIONS_ID_TOKEN_REQUEST_URL`/`ACTIONS_ID_TOKEN_REQUEST_TOKEN` variables that
GitHub provides only to a job with `permissions: id-token: write`. Without them
the publisher fails closed and never falls back to a token. The ID token request
and exchange happen in the credential step, before any intent is claimed, so a
refused exchange or missing registry rule leaves no journal and uploads nothing.
Failure messages carry only the registry and HTTP status; ID tokens, exchanged
tokens and response bodies are never logged or journaled. JSR accepts the ID
token only when the package is linked to the workflow's GitHub repository; npm
and PyPI require a trusted publisher rule naming the repository and workflow.
Set a trusted selector only in the release workflow whose identity those
registry rules name. GHA can inspect/reconcile all suitable public channels, and
public preparation/checks remain GHA-preferred.

An existing matching GitHub release and immutable tag are prerequisites. This
command does not create a release/tag, submit a Typst Universe package, claim a
provider-native attestation, or substitute Linux evidence for other platforms.
Long-lived registry tokens are never sent to hosted checks/builds.

## Durable publication intent

Before each registry mutation, the publisher obtains its required credential,
fsyncs a local intent, then creates and downloads an immutable intent asset on
the existing GitHub release. Each attempt has a unique owner. Only the creator of
that exact remote claim can upload; a competing caller stops even when its
package/bytes match. The release API's unique asset name supplies cross-runner
exclusion; local `flock` supplies same-directory exclusion. Credentials never
enter intent or completion records.

Normal publication makes at most one authenticated upload request per package
unit: one Cargo PUT, one npm PUT, one JSR POST, or one POST per PyPI filename. JSR's returned
publishing task ID is retained and polled read-only. Registry visibility is polled
for a bounded period after accepted uploads. A lost upload response can be
reconciled only through exact visible bytes. An existing intent with absent or
unknown remote bytes refuses another upload, including on a fresh GHA runner.
Never delete these journals to turn uncertainty into permission to upload again.
A definite PyPI HTTP429 rejection has the separate explicit recovery route below.

After exact public downloads are verified, immutable completion records are
written locally and to the release. If uploading a journal/receipt fails, retain
the failure and reconcile it; the uploader never overwrites an existing asset or
repeats a registry mutation. Existing exact published artifacts remain reusable.

## Explicit recovery after a rejected PyPI upload

The publisher now appends an immutable `-error.json` record when an upload fails.
It preserves the original intent hash, exact bundle/source/artifact identity and
safe outcome metadata. HTTP responses retain only method, origin, status,
observation time and parsed `Retry-After` delay; response bodies, other headers
and credentials are never retained. Missing or invalid `Retry-After` stays null.
Network failures and unknown responses are recorded as uncertain. Ordinary
`publish` continues to refuse every existing intent whose exact bytes are absent.

A separately reviewed, definite `POST https://upload.pypi.org` HTTP429 rejection
can authorize one new attempt with:

```sh
python3 .ci/publish.py recover-rejected --bundle publication-bundle.tar \
  --sha256 REVIEWED_BUNDLE_SHA256 --channels pypi \
  --journal-root /absolute/persistent/publication \
  --rejection-evidence exact-error.json --rejection-sha256 REVIEWED_ERROR_SHA256
```

Review the evidence and authorization before invoking this operation. It checks
the exact original remote intent, error and bundle identities, live release tag,
and fresh registry absence. Existing conflicting files fail the normal download
checks. A one-hour cooldown is client policy, not a claimed PyPI rate-limit
window; a captured longer `Retry-After` takes precedence. Each new immutable
numbered intent links the original intent and rejection hashes. Local locking
and the release asset's unique name prevent competing or repeated claims, also
on a different runner. There is one upload per invocation, with no automatic
retry or renewed recovery after another rejection. A later invocation must name
that later attempt's own reviewed rejection evidence. Network/unknown outcomes,
5xx responses and accepted uploads whose bytes disappeared never permit recovery.

For an old uploader that did not capture the rejection, an explicitly reviewed
prior-session report can supply the first attempt's missing evidence. This is
an operator attestation, not a recaptured or authenticated HTTP response. Its
canonical JSON uses the same error envelope (`schema`, `status`, `identity`,
`bundle_sha256`, `attempt: 0`, `intent_sha256`, `outcome`), with this outcome:

```json
{
  "kind": "prior-session-report",
  "method": "POST",
  "origin": "https://upload.pypi.org",
  "status": 429,
  "observed_not_after": 1770000000,
  "raw_response_retained": false,
  "retry_after_seconds": null,
  "report": {
    "commit": "40-lowercase-hex-characters",
    "path": "task/RECEIPT.md",
    "blob": "40-lowercase-hex-characters",
    "sha256": "64-lowercase-hex-characters",
    "lines": [10, 12]
  }
}
```

Use a documented upper bound on the observation time, not an invented precise
time. The client cooldown runs from that bound. The operator verifies the cited
committed report; the generic publisher validates the citation's shape and
reviewed evidence digest without accessing private records. It writes the report
as a separate immutable `-legacy-rejection.json` and cannot replace any captured
error, accepted response or completion. All previous journals remain unchanged.
Evidence files must use the publisher's canonical JSON encoding. After recovering
one PyPI filename, ordinary `publish --channels pypi` can upload any remaining
filename with no prior intent and skip the already verified file.

## Protocol and test references

The single-request envelopes follow [Cargo's registry API](https://doc.rust-lang.org/cargo/reference/registry-web-api.html),
[npm's publication implementation](https://github.com/npm/cli/blob/latest/workspaces/libnpmpublish/lib/publish.js),
[JSR's management API](https://jsr.io/docs/api) and its
[OpenAPI specification](https://api.jsr.io/.well-known/openapi), and
[PyPI's upload protocol](https://docs.pypi.org/api/upload/).
Trusted modes follow [GitHub's OIDC token request](https://docs.github.com/en/actions/reference/security/oidc),
[Deno's JSR publishing client](https://github.com/denoland/deno/blob/main/cli/tools/publish/mod.rs),
[npm's OIDC exchange](https://github.com/npm/cli/blob/latest/lib/utils/oidc.js) and
[PyPI's trusted publisher exchange](https://docs.pypi.org/trusted-publishers/using-a-publisher/).
Source transformations and each registry's available provenance remain explicit.
Trusted modes authenticate with OIDC, but these custom uploads do not claim a
provenance or build attestation.

The `registry-publisher` ccid selector runs `.ci/registry_publish_test.py` with
fake HTTP and temporary files. It covers exact artifact import, preserved producer
identity, archive safety, download conflicts, JSR license/transformation behavior,
per-request payloads, missing credentials, Cargo policy preservation, trusted
JSR/npm/PyPI ID-token audiences and exchanges, refused exchanges before intent,
missing OIDC variables, unchanged token mode, durable
intent ordering, lost responses, repeated invocations, competing claims,
HTTP429 evidence capture, cooldowns, explicit recovery, legacy report limits and
rejection of uncertain or conflicting recovery inputs.
It performs no build or network publication.
