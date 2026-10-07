# Native submission

`ccid crow-ci` replaces the Crow submission helper. `ccid ci-job` submits the
repository-owned jobs already planned by `ccid job`. Symlinks named `crow-ci`
and `ci-job` select the corresponding subcommand and retain the existing arguments.

```
ccid crow-ci plan --repo . --branch main --workflow verify
ccid crow-ci run --repo . --branch main --workflow verify
ccid crow-ci status REPOSITORY_ID RUN_NUMBER
ccid ci-job plan --repo . --job verify
ccid ci-job run --repo . --job verify
```

Configuration is JSON selected by `--submission-config PATH` or
`CCID_SUBMISSION_CONFIG`. Compatibility entry points also look for a sibling
`submission.json`; the ordinary fallback is `$XDG_CONFIG_HOME/ccid/submission.json`
(or `$HOME/.config/ccid/submission.json`). The operator declares these fields:

| Field | Meaning |
|---|---|
| `api` | HTTPS Crow API prefix, including its version path |
| `token_command` | Argument vector returning only the Crow token |
| `ssh` | SSH executable and connection arguments |
| `state_root` | Existing helper scratch root, with absolute path |
| `host_sources`, `worker_sources` | Host and worker views of the same source store |
| `host_tools`, `worker_tools` | Host and worker views of the same immutable tool store |
| `remote_binary` | Verified ccid executable on the SSH host |
| `tool_repo` | Existing local Git checkout containing pinned ccid revisions |
| `tool_origins` | Allowed canonical HTTPS identities for that checkout |
| `origin_aliases` | Exact hostname aliases; ports are not silently aliased |
| `argo_namespace`, `argo_template` | Existing declarative Argo namespace/template |

No credentials or fleet paths are built into ccid. The helper uses the declared
credential command without logging its output. Native Git chooses committed
exports, locally available gitlinks and bundle ancestry. Local Git LFS objects
are checked by size and SHA-256; no source fetch or smudge happens during staging.
The declared remote binary receives objects with `crow-ci receive` and checks
tool receipts with `crow-ci binary-receipt`. Both are Rust operations.

The source archive writer preserves the former Python PAX encoding, including
record padding, because archive digests identify existing work. Request JSON
keeps sorted keys and ASCII escaping. Existing `crow-ci-locks`,
`ci-provider-requests`, `ci-job-state`, and `ci-job-tools` paths and JSON fields
are retained. Locks are advisory `flock` locks shared with running legacy
workers. Intent writes are atomic and synchronized before network mutations.
Cached restarts require exact stored transport paths and workflow configuration.
Unknown outcomes never trigger an automatic retry.

`crow-ci archive` exposes offline source staging for integrity checks.
`crow-ci validate-adapters` verifies a digest-bound adapter corpus with its
exact pinned executable in plan mode; it never executes repository checks.

Validation belongs on the worker. `cargo test --release submission::tests`
contains the ported helper regression cases, including source closure,
submission/retry, historical provider state, and receipt boundaries. Build
artifacts, downloaded planners and source objects use persistent declared stores.

## Reading results and the post-landing check

`crow-ci digest REPO_ID RUN [--max-lines 120] [--tail 30]` is the default way to
read a result. A green run prints one line. A red run prints, per failed step,
the step, state and exit code, then only what is needed to act: compiler errors
with code, location and snippet (repeats collapsed with a count), failed test
names with their panics, Nix evaluation traces, Python and Node exceptions, the
last traced shell command, known infrastructure signatures (forge rate limits,
timeouts, a full disk: rerun, not a code failure) and the last informative log
lines, capped at `--max-lines`, ending with the exact `crow-ci logs` command for
the full log. Log text is redacted before extraction; a log that Crow no longer
retains degrades to the step's recorded exit code. The extractors are tested on
real, sanitized Crow logs (`src/submission/tests/fixtures`).

`crow-ci check-dependents --repo PATH [--commit SHA] [--job verify] [--slots 8]
[--admission-wait SECONDS] [--wait SECONDS] [--cfrg PATH] [--plan]` runs after a
repository's main moved. Dependents are read from the forge, always through
`cfrg contents` (the executable named by `--cfrg` or `CFRG_BIN`, default `cfrg` on
`PATH`): every active Crow repository whose root `Cargo.lock`, `Cargo.toml`,
`flake.lock` or `flake.nix` references the repository's canonical identity
(references are cached by git blob id and handed to cfrg as known blobs, so a warm
scan costs one tree request per repository; hosts listed in `legacy_hosts` match
by repository name only). Each dependent's repository job runs on its current
main through managed checkouts, so unaffected dependents hit the result cache.
At most `--slots` (never more than eight) runs are in flight, workers wait for
host admission (`CI_ADMISSION_WAIT_SECONDS`), a dependent is submitted at most
once per landing (the record survives restarts; a second call resumes and reports)
and one line per dependent is printed, then a summary with the result-cache hit
rate and wall time. Configuration fields: `forge_token_command` (required: the
command that prints the forge token handed to cfrg in its environment) and the
optional `legacy_hosts`.

## Pod-local mode

A scheduler pod (for example the retest hook of `cfrg serve`) submits without root
ssh and without a host secret store. Configure `token_file` (a mounted secret; an
absolute path, exactly one of `token_file` and `token_command`) and leave `ssh`
empty. Every command that would run on the worker host then runs locally, so the
worker source store must be mounted at `host_sources` and the pinned tools at
`host_tools` (the same paths the host uses), and sources are staged by running the
pinned `remote_binary crow-ci receive` into the mounted store. Admission reads the
local `/proc`. Argo submission (`kubectl`) is not available without ssh.

```json
{
  "api": "https://crow.example.invalid/api/v1",
  "token_file": "/var/run/secrets/crow/api_token",
  "state_root": "/var/lib/ccid",
  "host_sources": "/mnt/workspaces/ci-sources",
  "worker_sources": "/workspaces/ci-sources",
  "host_tools": "/mnt/workspaces/ci-tools",
  "worker_tools": "/workspaces/ci-tools",
  "remote_binary": "/mnt/workspaces/ci-tools/ccid/<revision>/x86_64-unknown-linux-gnu/ccid",
  "tool_repo": "/var/lib/ccid/ccid-source",
  "tool_origins": ["https://forge.example.invalid/org/ccid"],
  "argo_namespace": "ci-argo",
  "argo_template": "ccid-job"
}
```

## Fleet rollout of the commit verdict

`crow-ci rollout-verdict` moves every eligible repository to the aggregated verdict
(`docs/commit-verdict.md`) in one unattended, resumable run:

```
crow-ci rollout-verdict --revision REV --config verdict-rollout.json \
  --land-policy land-policy.json --land-state DIR --lanes LANES.md [--only PATTERN] [--plan]
```

It first blocks until a line of LANES.md begins with `VERDICT-PROVISIONED` (the
operator's statement that the status reporter exists; `--no-gate` skips it). Then, per
repository and `--concurrency` at a time: repin the adapters to `REV` and re-render on
branch `ci/verdict-rollout`, land it with `cfrg land` (exact green heads only), wait for
the first green `ccid/verdict` on the landed commit, and only then switch that
repository's land `contexts` from `ci/crow/*` to `ccid/verdict` (committed through
`brain-commit`; custom contexts are left alone). Repositories without adapters, without
a `verify` job or `[verdict]` declaration, and those matched by the declared skip list
(other lanes) are skipped unless the runtime they already pin contains the verdict
(`supports_from`). A branch that `cfrg land` reports as already contained in main (exit 2,
"already contained in the default branch") counts as landed and the run goes on to the verdict.
When the rollout configuration names a `gate_source` (`repository`, `file`, optional `render`
command run in the checkout, optional `batch`, default 20), every switched gate is also written
to that declared policy: the rollout edits only the lines it needs on branch `ci/verdict-gates`,
runs the render command, pushes and hands the branch to `cfrg land`; this happens when `batch`
gates are waiting, when the run starts (gates a former run left over) and when it ends, and the
run is not complete until the declared source carries every gate. Every forge read of the rollout (a repository's manifest, the head
of main, the statuses of a commit) goes through `cfrg contents` and `cfrg observe`;
`--cfrg` or `CFRG_BIN` names the executable, and it needs `forge_token_command`. State is saved after every repository; a rerun repeats nothing that is
done, and `awaiting` or `failed` repositories are retried. `--plan` lists what would happen
and changes nothing.
