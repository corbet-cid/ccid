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
`--metadata FILE` adds the resolved-graph rules below after the scan (see [The resolved graph](#the-resolved-graph)).

Success prints
`First-party declarations follow main; locked crate revisions are unique`.

## The resolved graph

`--metadata FILE` also checks the document `cargo metadata --locked --format-version 1`
prints for the repository (`FILE` is read relative to the working directory). It replaces the
copied `check-pins.mjs` variants and the `--metadata` modes of the `check-first-party.py`
variants, and runs after the manifest and lock scan of the same call (the scan's refusal is
reported first; an unreadable `FILE` is exit 2 whatever the scan finds). `--metadata` and
`--manifest-only` are exclusive (exit 2). One strictness level serves every repository: the
full validator.

The rules run in this order, the first refusal ends the check. A message is the text after
`check-first-party: `; the path in parentheses is `FILE`.

| Rule | Refusal |
|---|---|
| `version` is 1 and `packages` is not empty | `Missing original Cargo metadata graph` |
| `resolve.nodes` is an array | `Missing resolved dependency graph` |
| every package has a string `id` (unique) and a string `name` | `Missing or duplicated package ID`, `Missing package name` |
| every node `id` names one package, once | `Unknown or duplicated resolved package ID` |
| nodes and packages are the same set | `Incomplete resolved package graph` |
| `resolve.root` is `null` or a node (an absent root is refused) | `Unknown resolved root package` |
| `workspace_members` is an array of nodes, not empty | `Unknown or missing resolved workspace members`, `Missing actual workspace package` |
| every node has `deps` and `dependencies` arrays, naming the same packages | `Missing resolved dependency edges`, `Inconsistent resolved dependency edges` |
| every package has a `dependencies` array | `Missing package dependency declarations` |
| every edge selects a known package with a non-empty `dep_kinds` | `Unknown or incomplete resolved dependency edge` |
| every dep kind is `null`, `"dev"` or `"build"` with a `null` or string target (both keys present) | `Invalid resolved dependency kind` |
| every selected edge has a declaration of the same package, kind and target, for the edge's name (the declaration's `rename`, else the library target name, `-` read as `_`) | `Missing selected dependency declaration` |
| a first-party declaration's source is the selected package's source without its fragment (and the other way round) | `Mismatched selected dependency source` |
| every first-party package source and selected declaration source follows `branch=main`; a package also has a complete 40-hex fragment | `Expected main and a complete resolved revision: <source>` |
| every `[first_party.graph] required` name and every first-party package name is exactly one package with a first-party source | `Missing, invalid or duplicated first-party dependency: <name>` |

A source is first-party in the graph when it is `git+` followed by a fleet prefix (or a legacy
alias, refused with `canonical_only` as `First-party source must use its canonical owner URL:
<source>`). Retained issuers are judged first (below); then, in the order of `resolve.nodes`, each
package source before its declarations; the names come last. Only the declarations of **selected** edges are
judged: an unused dev, optional or target-specific declaration stays under its owner's policy,
and cargo prints a `#<revision>` fragment on a declaration of an unselected edge, so a
declaration's fragment is never judged.

- `rev_selectors = true` also accepts `rev=<40-hex>` as a package or declaration query, a
  package's fragment being that same revision; the refusal then reads `Expected main or a
  matching transitive revision pin: <source>`.
- Retained entries (`[[first_party.retained]]`) are the issuers. A retained package named by a
  package or a selected declaration must exist once (else `Missing, invalid or duplicated
  first-party dependency: <name>`) and pass the lock rule above (`Missing resolved revision
  for <name>`, `Invalid first-party source selector for <name>`). When it matches its entry
  exactly (`git+<repository>?branch=<branch>#<40-hex>`, version when named), its package source
  and the declaration `git+<repository>?branch=<branch>` are accepted as they are; every other
  selected declaration of it must be a first-party `branch=main` source, else `Invalid
  first-party source selector for <name>`.
- Nothing is read from a script: no `node`, no `python3`, no helper process.

On success the command prints the scan line, then
`First-party dependencies resolve once each to a complete source revision`.

### Rules deliberately not ported

The retired scripts also carried owner tables: original owners on a `v01` line that must not use
`main`, a package-name-equals-repository-name rule with a sibling table, a `.git` suffix rule, a
canonical-host rule and a one-revision-per-repository rule. When this command was written no
lock or graph of the repositories that carried them needed them (no source anywhere named a
retained line, and the scripts that carry them already refused their own repository's current
lock), so they are not data in this command. A line that is still retained is a
`[[first_party.retained]]` entry, which accepts the exact retained source or `main`.

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
| `[first_party.graph] required` | First-party packages every graph must contain exactly once, for `--metadata` only (`[]`) |

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

[first_party.graph]
required = ["shared-core"]
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
| 2 | Operational error, including an unreadable or unparseable file (also the `--metadata` file), a git failure, a bad `[first_party]` or `--metadata` with `--manifest-only`: stderr is `ccid first-party: <error>` |

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
