# Execution and worker contracts

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
compatible dependency artifacts are never moved or cleared. Verified archives
execute at the locked target's stable `.ccid/source-v1` path; its disposable
contents are replaced for each run and removed on exit.

Crow supplies `CI_REPOSITORY_URL`, a canonical forge URL without credentials.
Local checks derive identity from Git origin, or use their canonical local path
when no origin exists. The actual target owns `.ccid/lock`, including through
path aliases. That lock covers freshness reconciliation and the selected sequence;
another request waits at most 60 seconds before failing. A distinct explicit
`CARGO_BUILD_BUILD_DIR` is rejected because its intermediate outputs would bypass
that lock. It is cooperative:
shared writable caches and a shared worker UID are not a security boundary.
Repository scripts that internally change target paths need their own review.

Each invocation owns temporary verification/command directories beneath inherited
`TMPDIR` (or the platform temporary directory), plus the marked stable source
directory under its target. It removes only its own contents on handled exit.
Operators choose disk-backed parents and their orphan lifecycle.
Checked programs create Unix sockets below `TMPDIR`, and a socket path holds at
most 107 bytes, so a nested worker or `nix-shell` `TMPDIR` can make a program
fail at launch. The per-invocation scratch directory stays beneath the inherited
parent only while it leaves 64 bytes for the program's own paths. Otherwise it is
one level below a short per-user base, `/tmp/c<uid>` (created owner-only; an
existing path is used only as a real directory owned by the same user without
group or other access). An unusable base degrades to the inherited parent; it
never fails the run. `TMPDIR` is never a cache key input, so the location does
not change any result key.
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
canonical source root. Stable-path execution preserves compiled paths such as
`CARGO_MANIFEST_DIR` and allows unchanged workspace artifacts to stay fresh.
Moving the target or using a resolver candidate still invalidates those paths.
Changed inputs get fresh mtimes,
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

Crow builds the tool as an ordinary repository job: `compile` builds the release
binary from the exact commit on the worker, `verify` runs formatting, Clippy with
warnings denied and the test suite on the same release profile. The binary of a
commit is published once to the forge's generic package registry (`cfrg release`),
immutable per commit and fetched by URL and sha256; its `receipt.json` binds the
source revision, target triple and binary digest. A consumer verifies the receipt
and `source-revision` before it uses the binary, and only for the exact source,
checks and Linux environment it records. This is a Linux lane, not native
macOS/Windows evidence.

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

See the [code quality baseline](docs/code-quality.md) for enforced checks and review principles.
