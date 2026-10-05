# ccid

Reusable check commands invoked by Crow or Argo on existing build workers.
Repository manifests share the
same Cargo, Nix, JavaScript, source-verification, cache, deadline and resource
rules across providers. ccid does not install tools, host a service, schedule
jobs or pretend that one platform proves another. The Rust executor does not
publish releases. The optional [registry publisher resource](adapters/registry-publish.md)
provides explicit, separately invoked uploads of already-verified archives.

[`ccid forge`](docs/repository-policy.md) validates multi-forge placement, derives
the primary forge from CI, clones exact commits through declared read fallbacks,
and supplies a deterministic execution-failover decision to provider adapters.

[`ccid quality`](docs/quality.md) audits organization and repository presentation
on GitHub and Forgejo through the separate, deterministic cqlt policy library.
It collects read-only evidence and produces reproducible offline quality gates.
`ccid quality prose` checks descriptions and documentation through cqlt's
subordinate Vale backend, with a versioned writing policy and stable reports.

The Rust command library forbids unsafe code in this crate and requires Rust
1.89+ to build. Git and the selected check's existing tools are required at runtime.
The checked Linux binary is built once per source revision and reused by scheduler
adapters. The existing Python entry point remains for pinned consumers and
bootstrapping the tool build; it requires Python 3.12+.
Scheduler adapters own submission and status; ccid owns verification, check
execution, resource admission and receipts. Crow is the default scheduler for
declared jobs, with Argo available per job.

```sh
ccid check --repo /path/to/repository --check native
ccid check --repo /path/to/repository --check fmt,test --plan
```

`--plan` validates every selected check's command semantics without executing
checks or creating a target cache. Execution uses the same validation before
starting the first check, so an invalid later check cannot waste an earlier
build. Plans validate configuration; installed tools, Nix inventories, external
dependencies and product behavior still require runtime checks.

Select only checks whose inputs changed or whose results are missing. The
dispatcher owns reuse of prior matching results; ccid reports exact source,
manifest, commands' durations and compiler identity without re-running a suite
to manufacture a new green badge.

## Cached execution

`ccid cached --check test,clippy` runs the selected checks through
[moon](https://moonrepo.dev): each check becomes one moon task whose command is
the ordinary `ccid check` for that check. moon hashes the repository inputs plus a
tool identity (this ccid revision, the linker request and the toolchain versions
of the selected checks) and skips checks whose exact inputs already passed. With
`CCID_REMOTE_CACHE=grpc://host:9092` (or `http(s)://`), results are shared through
a Bazel Remote API cache such as bazel-remote, so another runner or machine reuses
them. Each restored result carries the original receipt
(`.ccid/results/<check>.jsonl`, written via `CCID_RECEIPT`).

The moon configuration (`.moon/workspace.yml`, `moon.yml`) is generated on every
run and refused if a repository already owns one; add `.moon/`, `moon.yml` and
`.ccid/` to `.gitignore`. `moon` and the same ccid revision must be on PATH; ccid
installs neither. `--force` runs every selected check regardless of the cache,
which a schedule should do periodically: a cache cannot detect flaky tests or
undeclared external inputs. `commands` checks bring their own tool pins (for
example `.prototools`), which are hashed as repository inputs.

Declare `cache_tools = [["go", "version"], ["git", "--version"]]` on custom
checks to hash the actual installed tools, and `cache_env = ["CGO_ENABLED"]`
for environment settings that affect their results. Declare produced files with
`cache_outputs = ["app"]`; cache hits restore these along with the receipt.
Output paths are relative to the repository. The remote endpoint is passed to
moon through its environment, so changing the cache address does not rewrite
the workspace configuration or partition otherwise identical work.

## Scheduler selection

Jobs group existing checks and select their scheduler. Crow is the default;
Argo can be selected per job or for one invocation. Both receive the same check
selection and repository-owned entrypoint:

```toml
[jobs.verify]
scheduler = "crow"
checks = ["test", "build"]
workflow = "verify"
command = ["bash", ".ci/run.sh"]
```

```sh
ccid job --job verify
ccid job --job verify --scheduler argo
```

The command emits a validated JSON plan. Submission adapters stage the exact
source/tool inputs and dispatch the plan: `workflow` identifies the Crow
workflow, and `command` is the same entrypoint used by its Argo counterpart.
This explicit choice does not retry failures on another scheduler or duplicate
active work. Queue-aware automatic selection remains a separate policy.

## Manifest

Each repository owns `.ci/ccid.toml`:

```toml
schema = 1
project = "example"

[checks.rust]
kind = "cargo"
actions = ["fmt", "test", "clippy"]
all_features = true
toolchain = "system"
# test_runner = "nextest" # Existing nextest only; doctests still run via Cargo.

[checks.native]
kind = "nix"
mode = "native"
# expected_checks = ["module-evaluation", "package"] # Optional exact inventory.

[checks.small]
kind = "nix"
mode = "named"
checks = ["module-evaluation"]

[checks.typescript]
kind = "javascript"
manager = "bun"
scripts = ["typecheck", "test"]

[checks.documents]
kind = "commands"
commands = [["bash", "scripts/check-documents.sh"]]
```

Cargo actions are `fmt`, `test`, `clippy`, `check`, `build`. Other options are
`workspace`, `all_features`, `all_targets`, `features`, `packages`, `exclude`, and `release`.
All builds/tests use `--locked`. An explicit toolchain uses `rustup run`, which
does not install a missing compiler. `system` records the existing compiler.
Nextest is optional and never silently replaces Cargo when unavailable.
`CI_LINKER=system` is the default; explicit `mold` runs Cargo through an existing
`mold -run` while preserving the warm target cache. An installed Nix linker
wrapper is resolved through its own `orig-bintools` metadata so wrapper flags
cannot precede `-run`. Unchanged executables can
be reused and do not prove a new linker executed; measure a changed leaf for
linker comparisons. No linker is installed and no cache is deleted.

The explicit Crow-only `cargo-resolve` command produces a reviewable dependency
candidate without changing the checked source worktree:

```sh
CCID_RESOLVE_CARGO=1 ccid cargo-resolve --repo . --output-dir /tmp/ccid-cargo-resolution
```

Pass one or more existing manifest selectors with repeated `--check` options
to validate the candidate before it is accepted:

```sh
CCID_RESOLVE_CARGO=1 ccid cargo-resolve --repo . --output-dir /tmp/ccid-cargo-resolution --check linux
```

The command requires Crow's `CI_REPOSITORY_URL`, `CI_COMMIT_SHA`, and an
inherited `CARGO_HOME` and Crow's verified `SOURCE_ARCHIVE`/`SOURCE_SHA256`.
It verifies and re-extracts that archive into Crow's isolated temporary source
tree, preserving every committed Cargo target and path dependency. It runs
`cargo metadata --locked` and `cargo update` with Cargo's registry access
explicitly online under the normal admission, deadline, and owned
process-group/parent-watch rules, and atomically writes a dated candidate `Cargo.lock` plus
a JSON receipt containing both lock digests, the source identity, and the
actual Cargo and rustc versions. When checks are requested, the candidate is
validated in the isolated source with the same runner, cache lock, admission,
deadline, and process supervision as ordinary checks; the receipt records the
selectors and pass/fail outcome. A failed validation rejects the candidate and
does not modify the submitted consumer worktree. The source lock and worktree
remain unchanged. The receipt is rejected if the lock format changes or a
workspace package's direct dependency changes compatibility major (or the
minor compatibility line for a 0.x dependency, including the patch line for
0.0.x); transitive implementation-version changes remain Cargo's declared-
constraint responsibility. Ordinary checks continue to require `--locked`.
The workflow that consumes a candidate must publish that exact snapshot to both
provider paths, preserving one resolved lock identity.

For an initial lock or a manifest change that makes the old lock unusable,
explicitly pass `--generate-lockfile` (Crow variable
`RESOLVE_GENERATE_LOCKFILE=1`). This runs `cargo generate-lockfile` followed by
locked metadata and any selected checks in the same isolated, supervised lane.
The committed manifests define the new graph; no compatibility comparison with
an incoherent baseline is claimed. The receipt records this mode, the original
lock when present, both digests and the verified source archive. Ordinary refresh
still requires coherent locked metadata and rejects direct compatibility breaks.

The manual `resolve` lane may resolve another committed public Cargo consumer
by supplying all four target inputs together: `RESOLVE_SOURCE_ARCHIVE`,
`RESOLVE_SOURCE_SHA256`, `RESOLVE_SOURCE_COMMIT`, and
`RESOLVE_REPOSITORY_URL`. Supplying only some of them fails closed; supplying
none resolves the workflow's own source. The lane reuses a verified ccid
binary/receipt for its exact core revision and bootstraps it only when absent.
The candidate and receipt are written below the configured `CARGO_HOME`
artifact root under the target commit.

Nix modes are `native`, `named`, `eval`, `list`, and `all-systems`. Every mode
first verifies a nonempty native check inventory. `eval` evaluates all systems
without building; it is not foreign runtime evidence. `all-systems` requires
appropriate builders. Put VM, graphical, native-platform and publication
checks in separately named explicit lanes; a generic native check must not
accidentally build a distribution's entire image matrix.

JavaScript supports npm, Bun and pnpm with frozen dependency resolution. npm
installation disables lifecycle scripts by default. Required native preparation
belongs in an explicit repository command. Use `kind="commands"` for existing
authoritative scripts, Nix apps/packages and special integration contracts.

See [execution and worker contracts](docs/execution.md) for resource allocation,
verified source staging, bootstrap artifacts and platform evidence.
