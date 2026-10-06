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
