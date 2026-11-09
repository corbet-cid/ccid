# Shared verdicts: the interface between a land gate and CI

`ccid cached` answers one question before it runs anything: **did this check already pass on this
exact content?** Its answer is a *verdict* in the store named by `CCID_RESULT_CACHE` (the
`verdicts/` namespace). A land gate and CI both write to and read from that one store, so CI's
verification of a pushed commit is a lookup for work the land already did instead of a second
build. This document is the interface; the code is `src/cached_verdict.rs`.

## The key

A verdict is keyed by content only. The key is the SHA-256 of the canonical JSON of these
components, in this order:

| Component | Meaning |
|---|---|
| `schema` | Key schema (`1`). Unknown schemas are refused. |
| `tree` | SHA-256 of the tree's content: path, kind (regular/executable/symlink/submodule) and bytes of every committed file. A `git archive` export of a commit and a checkout of the same commit produce the same value; generated directories (`.git`, `.moon`, `.ccid`, `target`, `node_modules`) are not part of it. |
| `git_dependencies` | Every git-sourced package of every `Cargo.lock` (root and nested), keyed `<lock path>:<package>:<url>`, mapped to the commit the build resolves it to. Read **after** the producer's dependency resolution, so it is the resolved commit, not the committed pin. |
| `toolchain` | Stable role → SHA-256 of the executable's bytes (for example `rustc`, `cargo`, `cargo-llvm-cov`, `rustup`, `git`, the declared `cache_tools`, `cc`, `cxx`, `linker`, `rustc-wrapper`). |
| `profile` | SHA-256 of the shared build profile (`profiles/dev.toml`) compiled into the producing binary. |
| `epoch` | `VERDICT_EPOCH`: what a verdict *means*. A release that checks the same things the same way keeps every verdict; only a semantic change raises it. |
| `check` | The selected check's name. |
| `check_identity` | SHA-256 of the check declaration plus the **declared** environment: the values of `SEMANTIC_ENV` and the check's `cache_env`. An absent variable and an empty one differ. Ambient variables never enter the key. |

No path, URL, commit id, run id, timestamp or host name is part of the key. Two producers on
different hosts therefore compute the same key for the same content.

Per-run scheduler and systemd state (`FORGEJO_*`, `ACTIONS_*`, `RUNNER_*`, `GITHUB_*`, `CI_*`,
`INVOCATION_ID`, `JOURNAL_STREAM`, `SYSTEMD_EXEC_PID`, `*_PRESSURE_*`, `nix-shell` attributes)
is never keyed. `GIT_CONFIG_*` `url.*.insteadOf` rewrites are forwarded to cached work as plumbing
so a land can serve unpushed commits from mirrors; they are not keyed, because the resolved
commits they lead to are. `RUSTC_WRAPPER`, a plain `CC`/`CXX` selector (its compiler's bytes are
keyed through the toolchain; a selector that carries flags is keyed verbatim) and the presentation
locale (`LANG`, `LC_ALL`, `TZ`) are plumbing too — a compiler cache, a cache wrapper in front of
the same compiler, or a locale changes how a result is produced or presented, not whether the check
passes — so a runner with `kache-corbet`/`sccache-corbet cc`/`en_US.UTF-8` and a workstation with
`C.UTF-8` still share verdicts. A check that really depends on one declares it in `cache_env`.

## The record

One JSON file per key: `<CCID_RESULT_CACHE>/verdicts/<key>.json`.

| Field | Meaning |
|---|---|
| `schema` | Record schema (`1`). |
| `key` | The key above. |
| `components` | The exact components; they must hash to `key`, or the record is refused. |
| `commit` | The commit the tree came from (provenance only). |
| `repository` | Canonical repository identity (provenance only). |
| `result` | `success` or `failure`. **Only `success` is ever accepted.** |
| `producer` | `lane` or `ci`, plus free-form identity (host, lane, pipeline URL). Provenance only. |
| `tool` | The producing ccid binary: SHA-256 of its bytes and the source revision compiled into it. |
| `started_at`, `finished_at`, `duration_seconds` | Unix seconds. |

Records are written atomically under a per-key lock. The first success wins: two producers that
agree on the key agree on the result, and a later failure never erases a pass. New records are
group-readable/group-writable when the store is group-writable (`0660`, store mode `2775`).

## Acceptance (CI)

A lookup is a **hit** only when all of this holds:

1. the record parses, validates, and its `components` are *exactly* equal to the components
   computed now — never a partial match;
2. `result` is `success`;
3. the producing binary is on the release channel: `CCID_CHANNEL_FILE` names its
   `revision` + `sha256` (any class), or, when no channel file is provisioned, CI accepts only the
   binary it is running — the workflow verified that binary against the channel before exec.

A lane accepts a fully matching record from the channel release it fetched. Anything else is a
**miss** (recompute) or **invalid** (a tampered/foreign record; never a pass). The store alone
never decides what a channel runs.

## Resolution before keying

CI's job floats its owned crates to the newest main inside the check script, so the committed lock
is read before that float and is not what gets built. Before computing a key, `ccid cached`
resolves every branch-tracking git dependency whose URL host is this repository's own forge
(`CI_REPOSITORY_URL`, else `origin`) with `cargo update -p <url>#<name>@<version>`. The land gate
serves the commits it is about to push from mirrors through `GIT_CONFIG_*` `url.*.insteadOf`, so it
resolves to the landed heads; CI resolves to the newest main after the push. Both then key and
build the same commits.

When resolution or keying is impossible — no forge identity, missing tools, a working tree
modified outside `Cargo.lock`, an untracked file the check could read — the verdict layer is
disabled for that run (`verdict-unavailable` / `verdict-disabled`), and CI behaves exactly as it
did before. It never guesses.

After a successful check, the inputs are re-read; a dependency that moved while the check ran
writes no record.

## Coverage

Coverage is a check like any other. A repository that declares its coverage work in a check (or
in the `verify` check that contains it) gets one verdict per tree. A later nightly coverage run
that selects the same check finds the record and builds nothing, so an unchanged tree is never
built twice.

## Producing, planning, inspecting

* `ccid cached --check <name>` — look up, then run the ordinary cached transaction on a miss and
  record a success. Events: `verdict-plan`, `verdict-inspect`, `verdict-record`,
  `verdict-skipped`, `verdict-disabled`, `verdict-unavailable`. A hit emits a `cache-check` with
  `status: cached` and `computed: 0`, and leaves the declared receipt (`<check>.jsonl`).
* `ccid cached --check <name> --plan` — print each check's verdict and its bypass reason (a
  declaration opt-out or a purity-scan finding such as a network tool) without running anything.
* `ccid job-run --local --source <dir> --sha <commit>` — run exactly CI's declared job against an
  existing export of the commit. This is the land gate's producer entry point; it uses the same
  job plan, environment, adapter check and command as `ccid job-run` on a runner.

## Store operation

`CCID_RESULT_CACHE` is provisioned by the runner (CI) or the operator (land gate). The
`verdicts/` directory is created on first write with mode `2775`; records are `0660` when the
store is group-writable. Both producers must be in the store's group. Records are immutable in
practice; a corrupted, symlinked or oversized record is refused, never parsed as a pass.
