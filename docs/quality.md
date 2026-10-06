# Forge quality checks

`ccid quality` evaluates saved forge evidence through the separate
[cqlt](https://github.com/corbet-foss/cqlt) Rust library. The same policy
works for GitHub and Forgejo. Evidence is collected with
[`cfrg collect`](https://git.corbet.ch/corbet-libs/cfrg); network collection
is separate from deterministic evaluation, so saved evidence can be reviewed
and checked offline.

```sh
cfrg collect --forge github --output github.json
ccid quality check --snapshot github.json
ccid quality check --snapshot github.json --json > report.json
ccid quality check --snapshot github.json --policy quality-policy.json --fail-on warning
```

## Policy and CI

The [cqlt rule catalogue](https://github.com/corbet-foss/cqlt#presentation-rules-v1)
defines presentation rules, default severities and applicability. Reports contain
every check, source and policy hashes, rule IDs, evidence and remedies. The
same snapshot and policy produce identical JSON. Exit statuses are:

| Code | Meaning |
|---|---|
| 0 | No failures at the selected threshold |
| 1 | Quality violations |
| 2 | Unknown evidence, invalid inputs or execution failure |

A policy can adjust known rule severities and record
exact, reasoned exceptions; no glob exemptions or unknown-evidence waivers.

```json
{
  "schema": 1,
  "severities": { "repo.topics": "error" },
  "exceptions": [
    {
      "subject": "example/archive",
      "rule": "repo.readme",
      "reason": "Intentionally preserved upstream snapshot; tracked in the archive catalogue."
    }
  ]
}
```

An existing private scheduled job can collect fresh evidence with
`cfrg collect`, retain the snapshot and JSON report, and fail on the check's
exit status. Repository CI
can invoke the same command through an ordinary ccid `commands` check. A
committed snapshot is historical evidence; it does not establish current forge
health. Scheduling and audit storage belong to the caller, not this command.

## Reuse boundary

### Writing quality

`ccid quality prose` delegates to cqlt's subordinate Vale backend. Install Vale
through your normal tool provisioning first; the command never installs it.
Vale 3.23.0 is tested. cqlt supplies its own isolated English writing rules and
does not load the repository's or user's Vale configuration.

```sh
ccid quality prose --snapshot github.json --json
ccid quality prose --snapshot forgejo.json --json
ccid quality prose --file README.md --file docs/guide.md
ccid quality prose --input texts.json --fail-on error --timeout 120
```

`texts.json` is an array of `{ "subject": "example#description", "format":
"text", "text": "Tools for checking repository descriptions." }` objects.
Use `markdown` for Markdown. Inputs can be combined; subjects must be unique.
Files must be explicit UTF-8 `.md`, `.markdown` or `.txt` files. No implicit
directory traversal or remote README download takes place. Saved forge evidence
contains descriptions but only README paths/sizes; supply README bodies as files
or explicit text inputs. Incomplete forge evidence is rejected.

cqlt checks repeated words, selected vague claims, wordiness and empty text.
The default failure threshold is warning; `--fail-on error` keeps stylistic
warnings advisory while empty text fails. Exit 2 means execution/input failure,
including missing Vale, timeout or malformed backend output. Input is limited to
4,096 texts and 16 MiB; captured output is limited to 16 MiB. The default total
subprocess timeout is 120 seconds. The private temporary workspace is removed
after execution. Reports can contain private text excerpts: retain them privately.

See [cqlt's prose contract](https://github.com/corbet-foss/cqlt/blob/main/docs/prose.md)
for deterministic replay, rule meanings and their limits. Prose findings assess
style; factual consistency and audience fit require separate editorial review.

### Semantic review with Jev

`ccid quality semantic` uses cqlt's optional, version-pinned Jev policy. The
input is an explicit JSON array of cases. Each has `subject` and `state` with
`audience`, `description`, and `facts`. Facts may contain `purpose`, `kind`,
`maturity`, `capabilities`, and `limitations`. No forge data is collected by
this command, and no claim is marked true merely because a field is missing.

```sh
ccid quality semantic --input cases.json
ccid quality semantic --input cases.json --replay recorded.json
```

The first command is a dry run: it prints the request hash and byte count for
each case without network access. Replay verifies exact request hashes and
prints the report. For an explicitly approved hosted run, supply the key through
`TYPESAFE_API_KEY` and hard caps for that invocation:

```sh
ccid quality semantic --input cases.json --live --record recorded.json \
  --max-requests 8 --max-request-bytes 131072
```

Live mode sends one HTTPS POST per case to TypeSafe's `/v1/systemone` endpoint.
It requires an existing curl 8.3+ and writes each successful raw response to
the private record file before proceeding. The key is expanded inside curl;
it is absent from command arguments and reports. Each call has a timeout and
1-MiB response cap. The command makes no automatic retries. A failed HTTP call
or invalid response is an error; successful earlier responses remain recorded.
The record path must not already exist and is reserved before the first call.
The byte cap limits request volume, not the provider's dollar billing; the
report records the returned token usage. No paid schedule is installed by this
command. Keep input, records, and reports private
when they contain nonpublic material. Hosted calls incur provider charges and
send the explicit case state to TypeSafe.

The report presents separate purpose, claim support, and maturity judgments,
each with its full probability distribution. Missing evidence yields `unknown`;
other outcomes require `review`. There is
no combined score or automatic failure threshold. These model judgments are
unverified until evaluated against reviewed, held-out examples. See
[cqlt's semantic contract](https://github.com/corbet-foss/cqlt/blob/main/docs/semantic.md).

### Other checks

Use existing tools for deeper checks: [alint](https://github.com/asamarts/alint)
for repository-file policy, [OpenSSF Scorecard](https://github.com/ossf/scorecard)
for its supported security checks, and the repository's native linters and tests.
These can already run through ccid's declarative command checks. This command
does not reimplement their rule engines, install them, or claim that their checks
ran. GitHub's community-profile API is useful supplementary evidence, including
inherited community files, but is GitHub-specific and excludes forks; it is not
the portable policy model. No overall security score is inferred from metadata.

The linked cqlt library is licensed under LGPL-3.0-only with the linking
exception. Its source, license texts and modification rights remain available
at the exact dependency revision in Cargo.lock.
