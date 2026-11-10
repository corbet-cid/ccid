# Advisory exposure checks

`ccid advisories` ports the repository policy in
`.github/check-advisory-exposure.mjs` (cblc, cpsd, cssr) into shared tooling.
`cargo deny --locked check advisories` stays the advisory gate; this command
checks what cargo-deny cannot express: every exception in `deny.toml` is a
reviewed id that still resolves, and the repaired transport has no exception
at all.

```sh
cargo metadata --locked --format-version 1 > dependency-metadata.json
ccid advisories
ccid advisories --repo . --manifest .ci/ccid.toml
```

The command is offline: it runs no network fetch and no cargo command. The
caller generates `dependency-metadata.json` first, exactly as `.ci/verify.sh`
does before invoking the script.

## Data

```toml
[advisories]
metadata = "dependency-metadata.json" # the cargo metadata file the policy reads (default)
forbid = ["native-tls", "rustls-pemfile"]
floors = { hyper = "1.0.0", h2 = "0.4.16", rustls = "0.23.0", rustls-webpki = "0.103.13" }

[[advisories.reviewed]]
id = "RUSTSEC-2024-0388"
package = "derivative"
version = "2.2.0"
```

A missing `[advisories]` table means no exceptions, never a skipped check. A
malformed table (unknown keys) is refused.

## Rules

Every match of `id = "(RUSTSEC-[0-9-]+)"` anywhere in `<repo>/deny.toml` must
be a reviewed id whose package and version occur among the metadata packages;
every resolved package must avoid the forbid list and meet its version floor.
Each floor compares `(major, minor, patch)` of the resolved version against
the floor, where each part is the leading decimal digits of the first three
dot-separated parts (a missing or digit-less part is 0), so a pre-release
suffix is ignored. The script's refusal stops at the first violation; so does
this command.

| Refusal | Meaning |
|---|---|
| `Unreviewed advisory exception: <id>` | `deny.toml` excepts an id with no review entry |
| `Remove or reassess obsolete exception: <id>` | the reviewed package and version no longer resolve |
| `Legacy TLS dependency returned: <name>` | a forbidden crate resolved |
| `Version floor violated: <name> <version> < <floor>` | a transport crate resolved below its floor |

## Exit statuses

| Code | Meaning |
|---|---|
| 0 | `Repaired transport and remaining reviewed advisory versions checked` |
| 1 | Policy violation, reported as `check-advisory-exposure: <message>` on stderr |
| 2 | Missing or unparseable manifest, `deny.toml` or metadata file |
