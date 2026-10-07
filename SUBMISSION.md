# Native submission

`ccid crow-ci` replaces the Crow submission helper. `ccid ci-job` submits the
repository-owned jobs already planned by `ccid job`. Symlinks named `crow-ci`
and `ci-job` select the corresponding subcommand and retain the existing arguments.

```
ccid crow-ci plan --repo . --branch main --workflow verify --provider crow
ccid crow-ci run --repo . --branch main --workflow verify --provider crow
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
| `github_tool_repository` | Historical public tool repository for receipt reconciliation, or null |

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

GitHub is frozen. Explicit GitHub requests, dispatch and cancellation writes
are refused before transport. Existing reviewed mappings and receipts remain
readable; unresolved hosted work still blocks Crow fallback. An owned queued
hosted run is never cancelled to make room for Crow during the freeze.

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
[--admission-wait SECONDS] [--wait SECONDS] [--plan]` runs after a repository's
main moved. Dependents are read from the forge: every active Crow repository
whose root `Cargo.lock`, `Cargo.toml`, `flake.lock` or `flake.nix` references the
repository's canonical identity (references are cached by git blob id, so a warm
scan costs one tree request per repository; hosts listed in `legacy_hosts` match
by repository name only). Each dependent's repository job runs on its current
main through managed checkouts, so unaffected dependents hit the result cache.
At most `--slots` (never more than eight) runs are in flight, workers wait for
host admission (`CI_ADMISSION_WAIT_SECONDS`), a dependent is submitted at most
once per landing (the record survives restarts; a second call resumes and reports)
and one line per dependent is printed, then a summary with the result-cache hit
rate and wall time. Optional configuration fields: `forge_token_command` (private
repositories) and `legacy_hosts`.

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
