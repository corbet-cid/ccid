# Push execution: newest-head coalescing with a proven graph

A push-enabled job runs automatically when its branch moves. Many deliveries
of the same event share one admitted build; late arrivals attach to the
completed receipt instead of rebuilding. Nothing here installs tools or
provisions services: all execution reuses the existing worker contract
(provisioned shell, Git, checksum tools, the verified ccid binary).

## Manifest keys

```toml
# Top-level repository identity. Required as soon as any job opts into push:
# push adapters bake this URL into trigger provenance.
repository = "https://forge.example.invalid/cpkg/demo.git"

[jobs.official]
checks = ["official"]
workflow = "repository-jobs"
command = ["ccid:run-declared-checks"]
push_branches = ["v01"]
refresh = { branch = "v01", sources = ["forge.example.invalid/cpkg"] }
environment = { CI_JOBS = "2", CI_TEST_THREADS = "2" }

[push_consumer.v01-dep]
consumer = "https://forge.example.invalid/cpkg/demo.git"
branch = "v01"
consumer_branch = "v01"
job = "official"
self_name = "deplib"
```

- `repository`: this repository's canonical forge URL. Push adapters bake it
  into `--trigger-repo` so dependency events carry repository, branch, and
  commit provenance instead of a bare package name.
- `[jobs.<name>] command = ["ccid:run-declared-checks"]`: the only command
  push execution accepts. It runs the job's declared checks through the
  pinned binary. Arbitrary commands are refused for push; manual-only jobs
  (no `push_branches`, no `refresh`) keep any command and render
  byte-identically to before.
- `push_branches`: branches whose pushes may execute this job. Empty means
  manual-only: no adapter, no automatic trigger.
- `refresh`: the newest-head scope. Before the frozen gates, ccid updates
  every first-party git dependency that declares `branch` equal to this
  branch and whose canonical source starts with one of `sources`, runs any
  declared `prepare_commands` (existing checked-in drivers only, default
  none), then prefetches with `cargo fetch --locked`. Selection is the
  transitive lock closure plus direct manifest edges (root, workspace
  members, target/dev/build tables, renamed `package` honoured); `[patch]`
  tables are never read.
- `environment`: manifest-owned worker values applied to prepare, refresh,
  and check execution alike (for example a shared warm `CARGO_TARGET_DIR`,
  build parallelism, fetch policy). Reserved identity, status, and
  job-ownership keys (`CCID_BIN`, `CI_COMMIT_SHA`, `RUNNER_TEMP`,
  `CCID_STATUS_*`, and related prefixes) can never be overridden: the
  entrypoint assigns `RUNNER_TEMP` to the owned scratch directory, and the
  manifest overlay must never undo that. Old manifests without these keys
  plan byte-identically.
- `[push_consumer.<name>]`: dependency fan-out. Pushes to this repository's
  `branch` run the consumer's `job` with the event commit pinned; the
  rendered consumer adapter carries the consumer URL, branch, job, package
  name, and this repository's URL.

## Shared newest-head semantics

All push pipelines for the same consumer URL, branch, and job share one
namespace. Each event records its provenance and a generation, observes a
bounded quiet period, then either attaches to a completed receipt covering
its trigger or becomes the single builder. The admitted generation is frozen
before resolution, so events arriving mid-build stay pending for exactly one
later latest-head run.

Attach requires, in order: the live consumer head equals the receipt head (a
new manifest or source invalidates all prior proof), exact tool, runtime,
and config identity (toolchain fingerprint plus `{head}:{job}`), then graph
coverage — the trigger commit equals the receipt's consumer commit, or a
dependency trigger names a selected package at its recorded sha on its
recorded source branch. Anything unproven builds; failures propagate only to
triggers they actually cover, including admitted triggers of a failure
recorded before graph resolution (failure evidence under full identity,
never a graph claim). While the build lock is busy, waiters poll cheap
local state only: live probes and ancestry run at most once per receipt
generation. Every lock wait is capped by the caller's overall deadline.
Receipts are durable evidence and are never pruned by push execution.

## Gates and delivery

Gates run the declared checks generically under the pinned binary on the
existing warm target, truly `--offline --locked`, with one target lock held
across every gate command and the full lock bytes compared after each one.
The `--offline --locked` flags must precede the first `--` separator
(options after it are test/program arguments, not Cargo flags). Any lock
mutation fails even on exit code 0. The runtime fingerprint is captured
with the exact gate environment before and after the gates; drift refuses a
mixed-toolchain receipt. Only explicit manifest `commands` arrays with
`--offline --locked` run as push gates.

The expected delivery is an immutable binary: publish the ccid revision only
after its own test suite passes on a worker, then re-pin consumers'
`[render].tool_revision` to the landed revision in the same commit that
needs it. A staged manifest naming another revision fails closed until
re-pinned. Generated adapters and inventories are produced by the new binary
itself (`ccid render`); never hand-edit generated files to claim a render.
