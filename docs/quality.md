# Forge quality checks

`ccid quality` collects read-only forge facts and evaluates them through the
separate [cqlt](https://github.com/corbet-foss/cqlt) Rust library. The same policy
works for GitHub and Forgejo. Network collection is separate from deterministic
evaluation; saved evidence can be reviewed and checked offline.

```sh
ccid quality collect --forge github --output github.json
ccid quality collect --forge github --org example --output example.json
ccid quality check --snapshot github.json
ccid quality check --snapshot github.json --json > report.json
ccid quality check --snapshot github.json --policy quality-policy.json --fail-on warning
```

GitHub collection uses the existing `gh` login, or a `CQLT_TOKEN` environment
variable. Other instances require `CQLT_TOKEN` and an explicit HTTPS API root:

```sh
# Supply CQLT_TOKEN through your existing secret manager, never a command argument.
ccid quality collect --forge forgejo --api-url https://forge.example/api/v1 \
  --output forgejo.json
ccid quality collect --forge github --api-url https://github.example/api/v3 \
  --org example --output enterprise.json
```

Collection requires an existing curl 8.3+ (environment variable expansion) and,
when using its login, `gh`. Credentials stay in process memory/environment;
curl expands them internally, keeping them out of command arguments and files.
No redirect is followed. API responses and snapshots may contain private
metadata: keep them in private storage and CI. Snapshot files are written
atomically with private permissions. Every request has a 30-second timeout and
16-MiB response limit; `--timeout` bounds the overall collection (default 900s).
No tool is installed, repository cloned, content executed or forge data modified.

With no `--org`, collection paginates membership organizations. Explicit
organizations are also supported. This is the credential's accessible scope;
it cannot prove the absence of repositories hidden by permissions. Personal
account repositories are outside the organization scan. Pagination continues
to an empty page even when a forge caps page size; repeated identities fail
closed. Failed organization reads remain visible in the requested scope.
Failed content reads become unknown, not missing or passing.

Both adapters resolve the default branch and inspect contents at that commit.
GitHub's native README API handles supported README locations. Forgejo checks
the root README. Root license filenames accept LICENSE, LICENCE or COPYING,
including dot, hyphen and underscore suffixes; license suitability is not
inferred. Organization profiles use `.github/profile/README.md` on GitHub and
`.profile/README.md` on Forgejo. Accessible private profiles are recorded as
documentation, not proof of a public profile. The first rule set checks presence
and nonzero byte size, not prose quality or public rendering.

## Policy and CI

The [cqlt rule catalogue](https://github.com/corbet-foss/cqlt#presentation-rules-v1)
defines presentation rules, default severities and applicability. Reports contain
every check, source and policy hashes, rule IDs, evidence and remedies. The
same snapshot and policy produce identical JSON. Exit statuses are:

| Code | Meaning |
|---|---|
| 0 | No failures at the selected threshold |
| 1 | Quality violations |
| 2 | Collection unknowns, invalid inputs or execution failure |

Collection exit 0 means collection completed, not that quality passed. Check
the snapshot separately. A policy can adjust known rule severities and record
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

An existing private scheduled job can collect fresh evidence, retain the
snapshot and JSON report, and fail on the check's exit status. Repository CI
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
