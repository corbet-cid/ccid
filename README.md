# ccid

Reusable check commands invoked by Crow on its existing build workers.
Repository manifests share the
same Cargo, Nix, JavaScript, source-verification, cache, deadline and resource
rules across providers. ccid does not install tools, host a service, schedule
jobs or pretend that one platform proves another. The Rust executor does not
publish releases. The optional [registry publisher resource](adapters/registry-publish.md)
provides explicit, separately invoked uploads of already-verified archives.

The Rust command library forbids unsafe code in this crate and requires Rust
1.89+ to build. Git and the selected check's existing tools are required at runtime.
The checked Linux binary is built once per source revision and reused by Crow
consumers. The existing Python entry point remains for pinned consumers and
bootstrapping the tool build; it requires Python 3.12+.
Crow remains the CI system and owns scheduling, status, admission and artifacts.

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

## Worker allocation

`CI_JOBS` is the operator's numeric CPU allocation. Without it, ccid preserves
the worker's `CARGO_BUILD_JOBS`, then reads its affinity/cgroup CPU quota;
without a readable quota it uses at most four
CPUs. `CI_TEST_THREADS` defaults to that allocation. `CI_NIX_JOBS` defaults to
one concurrent derivation using the allocated cores, and can divide the budget
among more derivations. These variables do not change container or host limits.

An explicit `CI_MEMORY_MB` records the scheduler's memory allocation. An
optional `CI_MEMORY_PER_JOB_MB` reduces parallel jobs to fit it. These are
admission inputs, not a replacement for the worker's enclosing memory limit.
`CI_TIMEOUT` is the total selected-check deadline in seconds, default 2700.
Expired commands receive TERM then KILL as a process group.

Package settings including `CARGO_HOME`, `UV_CACHE_DIR`, `BUN_INSTALL_CACHE_DIR`,
and `npm_config_cache` are inherited unchanged. An explicit `CARGO_TARGET_DIR`
(or `CARGO_BUILD_TARGET_DIR`) selects existing compiled outputs. Otherwise targets
live beneath explicitly configured `CI_CACHE_ROOT/targets`, or `CARGO_HOME/targets`
(default `$HOME/.cargo/targets`). The namespace includes canonical forge/owner/repo
identity, not the manifest label. Cargo retains responsibility for compiler,
profile, feature and dependency compatibility; source/tool revisions do not
create new cache namespaces. Existing target trees, downloaded dependencies and
compatible dependency artifacts are never moved or cleared. Workspace outputs
are rebuilt when an archive is extracted at a different canonical source path.

Crow supplies `CI_REPOSITORY_URL`, a canonical forge URL without credentials.
Local checks derive identity from Git origin, or use their canonical local path
when no origin exists. The actual target owns `.ccid/lock`, including through
path aliases. That lock covers freshness reconciliation and the selected sequence;
another request waits at most 60 seconds before failing. A distinct explicit
`CARGO_BUILD_BUILD_DIR` is rejected because its intermediate outputs would bypass
that lock. It is cooperative:
shared writable caches and a shared worker UID are not a security boundary.
Repository scripts that internally change target paths need their own review.

Each invocation owns temporary source/command directories beneath inherited
`TMPDIR` (or the platform temporary directory), removing only those directories
on handled exit. Operators choose a disk-backed parent and its orphan lifecycle.
ccid does not clear pre-existing scratch. Unix outer-process SIGKILL is covered
by the supervisor described below; killing that supervisor or losing the host
can still leave scratch behind.

Optional `CI_MIN_AVAILABLE_MB` enables Linux admission using `MemAvailable` and
cgroup-v2 headroom. It reserves that amount and caps `CI_MEMORY_MB` to the remainder;
`CI_MEMORY_PER_JOB_MB` then bounds jobs. Insufficient headroom fails before checks.
The supplied Linux Crow adapter defaults the reserve to 8192 MiB.
Swap occupancy alone is never a gate. This is a point-in-time check, not a memory
reservation or replacement for Crow's concurrent-job policy.

## Pinned source and bootstrap

Crow adapters pin an exact ccid Git commit in a literal `CCID_REVISION` step
environment value, never a branch. The dispatcher
stages a Git archive for that commit plus its SHA-256, separately from the
repository source archive. The adapter checks both the tool archive SHA-256
and `git get-tar-commit-id` against its literal pin before extracting the tool.
The dispatcher also supplies `CI_TOOL_BINARY` and `CI_TOOL_BINARY_SHA256`.
The adapter verifies the binary digest and requires `source-revision` to equal
its literal pin before running the combined verified archive check:

```sh
"$CI_TOOL_BINARY" check \
  --archive "$SOURCE_ARCHIVE" --sha256 "$SOURCE_SHA256" \
  --commit "$CI_COMMIT_SHA" --check "$CHECKS"
```

Archive checks require `CI_REPOSITORY_URL` and an explicit inherited Cargo home or
target-cache root; they never fall back to a disposable job HOME. They own their source directory,
verify the archive digest and embedded Git commit using a private archive
snapshot, and reject escaping links, cycles and special files before extraction.
The standalone `verify-source` interface remains available without cache reuse.

Under the target lock, archive checks compare the canonical source root plus source
bytes, type, mode and directory membership with `.ccid/source-state.json`.
Unchanged regular files/directories reuse their recorded mtimes only at the same
canonical source root. A new disposable extraction path gives workspace inputs
fresh mtimes, forcing Cargo to rebuild outputs that may embed absolute source
paths such as `CARGO_MANIFEST_DIR`, while retaining the shared dependency cache
and compatible dependency artifacts. Changed inputs also get fresh mtimes,
including rollback to an older Git revision. Symlink mtimes remain freshly
extracted, which can cause extra work. Metadata written before source-root tracking
is treated as nonmatching rather than failing the check. Local `check --repo` never
changes source timestamps and invalidates archive freshness metadata before using
the same target.

Old freshness metadata is removed before reuse. Only successful checks whose
source remains unchanged publish new metadata atomically. Failure, interruption,
or source mutation leaves compiled outputs intact but no reusable freshness state.
The Cargo fixture covers a runtime input found through compile-time
`CARGO_MANIFEST_DIR` across repeated disposable extractions, changed content,
older revision rollback, directory membership/deletion, build-script inputs,
failure and source mutation. A unit fixture preserves same-root reuse coverage.
This does not compensate for undeclared inputs in repository build scripts.
The caller must stage
submodule/LFS/dependency source closures separately; a root Git archive does
not magically contain them. Workflow configuration may still come from a forge.
Native macOS/Windows/ARM, physical accelerator and release-trust gates retain
their own identities and evidence.


## Building and platform evidence

The manual GitHub Actions `ccid portable Linux bootstrap` workflow accepts only an
explicit full `source_revision`, fetches that commit, and runs the existing
bootstrap with stable Rust on `ubuntu-24.04` using a bounded four-job/test
thread budget. It uploads one immutable `ccid-<source-revision>-linux-x86_64`
artifact containing the binary and `receipt.json`; the receipt binds the source,
binary SHA, target, rustc output, workflow/config/manifest digests and locked
Cargo dependency snapshot. It is a free public Linux build lane, not native
macOS/Windows evidence, and Crow remains the fallback provider. Consumers must
verify the receipt and use the artifact only for the exact source, checks and
Linux environment it records.

The reusable `ccid reusable Linux check` workflow accepts a caller's exact
source commit, checks, request id, dependency snapshot, tool revision/run and
artifact IDs, binary digest, manifest digest and provider-config digest. It
requires committed `.ci/ccid.toml` and `.ci/providers.toml`; the provider
configuration's `[dependencies].files` list defines the canonical sorted
path-to-file-digest snapshot supplied by the caller. It guards the detached
source checkout and dependency files before and after the check, verifies the
artifact and `source-revision`, runs the existing Rust binary, and uploads a
result receipt containing the actual Rust, Node and Bun environment plus the
selected exit code. The caller's thin wrapper owns `run-name:
ccid/<request_id>`; the reusable workflow never retries, masks failures or
silently falls back.
Callers may set positive `ci_jobs`, `ci_test_threads`, `ci_min_available_mb`,
and `ci_timeout` inputs; their defaults remain 4 jobs, 4 test threads, 4096 MiB
reserved, and 1800 seconds. A caller using one job must pass one explicitly.
Successful receipts must contain the matching allocation emitted by ccid,
including its actual budget and Python version. `setup_rust: false` avoids a
toolchain refresh for checks requiring only already-provisioned non-Rust tools;
its default remains true and selects current stable. `.ci/test_hosted_budget.py`
exercises the workflow's actual validation and receipt code without hosted work.
Cross-repository artifact access uses the caller's builtin token and must be
proven before enabling a provider mapping. Any hosted mapping published before
that proof is experimental and must retain Crow as the operational fallback;
the reusable workflow does not claim hosted success from a missing artifact or
an unverified token boundary.

The manual Crow `build` workflow runs formatting and release-profile tests,
then builds the binary with the same profile, target and persistent cache.
`CCID_SOURCE_REVISION` embeds the exact source identity exposed by
`ccid source-revision`. A successful build atomically publishes a native binary
and `receipt.json` containing `source_revision`, `target`, and `binary_sha256`.
A conflicting existing artifact is never overwritten. Consumers verify and
reuse this artifact; they do not compile ccid per repository.

Linux uses owned process groups for handled cancellation and deadlines. The
host-native GNU binary may depend on its build host's runtime paths; it is not
advertised as a portable musl release. macOS shares the Unix implementation but
requires a native build and validation before a platform success claim.

Unix check execution uses a short-lived supervisor in a separate process group.
Its parent holds a pipe open; EOF triggers the existing command-group cleanup,
including the ten-second TERM grace period before KILL. This allows cleanup
after the enclosing executor kills the outer CLI process with SIGKILL, as Crow
6.4's local backend does. Commands receive null stdin and cannot retain the
parent's liveness pipe. Plans do not spawn a supervisor.

Killing the supervisor itself is uncatchable, and a command that deliberately
escapes its owned process group requires an external job isolation boundary.
Verify cancellation through the actual executor; a `killed` pipeline status
alone does not prove that its descendants stopped.

Default Windows builds support inspection (`source-revision` and `check --plan`)
and refuse check execution. The `windows-experimental` Cargo feature selects a
Windows-only Tokio backend with a suspended child assigned to an owned Job
Object and kill-on-close enabled before it resumes. Native tests must prove
normal exit, timeout, Ctrl-C, parent-exits-first and forced runner termination
before this backend becomes the default. This is an implementation available
for validation, not a Windows success claim. Nix checks explicitly require a
supported Nix host. No macOS or Windows hosted build is triggered by this repo.
