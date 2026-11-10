# First-party gate

`ccid first-party` replaces the copied `.github/check-first-party.py` scripts
(63 repositories). Every first-party git dependency must declare
`branch = "main"` (or the branch of a matching retained entry) and every
first-party lock entry must resolve to exactly one full 40-hex revision.

```sh
ccid first-party [--repo DIR] [--manifest .ci/ccid.toml] [--archive] [--manifest-only] [--self-test] [--metadata FILE]
```

URL parts use plain string operations, no URL parser: strip one `git+`, the
fragment is the text after the first `#`, the query the text between the first
`?` and the `#`, the repository the text before the `?`.

## Rules

A source is first-party when `fleet.classify(source).is_some()`: under a
canonical prefix, or under a legacy alias prefix (accepted by default). With
`canonical_only = true` an alias is refused:

`First-party source must use its canonical owner URL: <source>`

Lock rule, per locked package name (a lock without `package` is an empty
list):

- names are the locked names whose source is first-party, plus every retained
  package name (even with a non-first-party source);
- one entry per name, else `Duplicate first-party crate: <name>`;
- a retained entry matches when the source equals
  `git+<repository>?branch=<branch>#<fragment>` exactly (fragment
  `^[0-9a-f]{40}$`, and `version` matches when the entry names one), or when
  it passes the main rule below;
- otherwise the raw query must equal `branch=main` exactly (with
  `rev_selectors = true` also `rev=<fragment>`), else
  `Invalid first-party source selector for <name>`;
- the fragment must be `^[0-9a-f]{40}$`, else
  `Missing resolved revision for <name>`.

Manifest rule, recursively over every table (dependency key is the table's
`package` key, else its key) and list:

- a table whose `git` equals a retained `repository` exactly with the same
  package name must declare `branch = "<branch>"` (unversioned retained
  entries only; versioned entries are lock-only), else
  `First-party dependency must declare branch = "<branch>"`;
- else a first-party `git` must declare `branch = "main"` with no `rev` or
  `tag`, else `First-party dependency must declare branch = "main"`.

`--manifest-only` checks manifests only. A path package and a git package of
the same name refuse as `Duplicate first-party crate`.

Paths: git mode when `<repo>/.git` exists and `--archive` is absent
(`git ls-files -z` in `<repo>`), else a walk skipping `.git`, `target`,
`node_modules` and `skip`, keeping files named exactly `Cargo.toml` /
`Cargo.lock`. Checks run in sorted path order. `--self-test` prints
`first-party: the self-test is ccid's own test suite` and exits 0.
`--metadata` exits 2: `--metadata is not supported yet (first-party graph lane)`.

Success prints
`First-party declarations follow main; locked crate revisions are unique`.

## Data

`[first_party]` in `.ci/ccid.toml`, all optional; absent table means defaults.
`--manifest .ci/ccid.toml` names the file; unknown fields are refused.

| Field | Meaning |
|---|---|
| `rev_selectors` | Accept `rev=<fragment>` as a lock query (`false`) |
| `canonical_only` | Refuse sources under a fleet alias prefix (`false`) |
| `require_root_lock` | A root `Cargo.toml` needs a root `Cargo.lock`: `Missing committed root manifest or lock` (`false`) |
| `lock_per_manifest` | Every tracked `Cargo.toml` needs a sibling `Cargo.lock`, tracked or not, and it is checked: `Missing resolved snapshot: <path>` (`false`) |
| `skip` | Extra directory names skipped by the archive walk (`[]`) |
| `[[first_party.retained]]` | Retained original owners (`[]`) |

```toml
[first_party]
rev_selectors = false
canonical_only = false
require_root_lock = false
lock_per_manifest = false
skip = []

[[first_party.retained]]
package = "cmbl"
repository = "https://forge.example/org-a/cmbl.git"
branch = "v01"
version = "0.1.0"
```

| Retained field | Meaning |
|---|---|
| `package` | Locked package name, never empty |
| `repository` | Exact spelling as written, with or without `.git`, never empty |
| `branch` | Required branch, for example `v01`, never empty |
| `version` | Exact locked version, when given; versioned entries are lock-only |

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Gate passed |
| 1 | Gate refusal: stderr is `check-first-party: <message> (<path>)` |
| 2 | Operational error, including an unreadable file, a git failure or a bad `[first_party]`: stderr is `ccid first-party: <error>` |

## Fleet requirement

A check that calls this command declares it in `.ci/ccid.toml`:

```toml
[checks.first-party]
kind = "commands"
fleet = true
commands = [["ccid", "first-party"]]
```

Without `CCID_FLEET_FILE`, or inside a ccid check without `fleet = true`,
the command exits 2.
