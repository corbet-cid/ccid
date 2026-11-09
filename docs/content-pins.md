# Locked content pins

`ccid content-pins` replaces the copied `scripts/check-fixtures.py` and
`.ci/check-native-sdk.py` with one generic subcommand. Each pin is data in
`.ci/ccid.toml`; the check logic lives here, once.

```sh
ccid content-pins --repo . --manifest .ci/ccid.toml
ccid content-pins --pin fixtures --pin native-sdk
```

## Data

```toml
[[content_pins]]
name = "fixtures"
kind = "sha256-manifest"
manifest = "tests/fixtures/provenance.json"   # JSON object; `key` names the map of file -> sha256 (paths relative to the manifest's directory)
key = "sha256"

[[content_pins]]
name = "native-sdk"
kind = "lock-version"
file = ".ci/native-sdk.json"                   # JSON object with `version` and `sha256`
package = "barretenberg-rs"                    # every package of this name in the root Cargo.lock must have exactly that version, and there must be one
```

Pin names must be unique. An unknown `kind`, a duplicate name, an unknown
`--pin` selection, or a repository without any pin is an operational error
(exit 2), never a silent pass.

## Exit statuses

| Code | Meaning |
|---|---|
| 0 | Every selected pin passes |
| 1 | Pinned content refuses (digest mismatch, missing file, escaping path, version or digest refusal) |
| 2 | Unreadable or unparseable inputs, malformed pin declarations, unknown selection |

A path listed in a `sha256-manifest` map must be relative and stay inside
the repository: absolute paths and any `..` component refuse with the same
mismatch message. Refusals print `content-pins: <name>: ...` to stderr;
passes print the success line to stdout.

## Messages

`sha256-manifest` checks every listed file (read as bytes) against its
SHA-256 value:

- pass: `content-pins: <name>: <n> files match <manifest>`
- refuse: `content-pins: <name>: <path> does not match <manifest>`

`lock-version` keeps the three refusal messages of the script it replaces,
prefixed with the pin name:

- `content-pins: <name>: Native artifact needs a stable upstream version`
- `content-pins: <name>: Native artifact needs a SHA-256 digest`
- `content-pins: <name>: Locked <package> must exactly match the authenticated native artifact`
- pass: `content-pins: <name>: <package> <version> matches <file>`

The version must look like `N.N.N` and the digest must be 64 lowercase hex
characters. The lock comparison is exact: the root `Cargo.lock` must contain
exactly one package of that name, with exactly the authenticated version.
