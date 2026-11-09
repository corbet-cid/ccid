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
| `schema` | Key schema (`2`). Unknown schemas are refused. |
| `tree` | SHA-256 of the tree's content: path, kind (regular/executable/symlink/submodule) and bytes of every committed file. A `git archive` export of a commit and a checkout of the same commit produce the same value; generated directories (`.git`, `.moon`, `.ccid`, `target`, `node_modules`) are not part of it. |
| `git_dependencies` | Every git-sourced package of every `Cargo.lock` (root and nested), keyed `<lock path>:<package>:<url>`, mapped to the commit the build resolves it to. Read **after** the producer's dependency resolution, so it is the resolved commit, not the committed pin. |
| `toolchain` | Stable role → SHA-256 of the tool's **semantic identity** (its release and target), never its installed bytes. See [Tool identity](#tool-identity). |
| `profile` | SHA-256 of the shared build profile (`profiles/dev.toml`) compiled into the producing binary. |
| `epoch` | `VERDICT_EPOCH`: what a verdict *means*. A release that checks the same things the same way keeps every verdict; only a semantic change raises it (`2`). |
| `check` | The selected check's name. |
| `check_identity` | SHA-256 of the check declaration plus the **declared** environment: the values of `SEMANTIC_ENV` and the check's `cache_env`, with producer-local path entries normalised away (see [Declared environment](#declared-environment)). An absent variable and an empty one differ. Ambient variables never enter the key. |

No path, URL, commit id, run id, timestamp or host name is part of the key. Two producers on
different hosts therefore compute the same key for the same content.

### Tool identity

The toolchain is identified by what a tool **is**, not by how it was installed:

| Tool | Identity |
|---|---|
| `rustc`, `cargo`, `rustdoc` (stable and nightly) | `<tool> -vV`: release, commit hash, commit date, host and LLVM version. |
| C/C++ compilers (`cc`, `cxx`, `clang`, `gcc`, …) | `--version` (installation-directory and configure-prefix lines removed) plus `-dumpmachine`. |
| `git`, `bash`, `python3`, `pkg-config`, `node`, `npm`, `deno`, `nix`, `cargo-llvm-cov`, other Cargo subcommands | the tool's `--version` output. |
| `rustup` | **not part of the key.** It only dispatches to a selected toolchain and compiles nothing. The compiler rustup selects is keyed by its own semantic identity, so a rustup update that changes no compiler release changes nothing a build sees. A declared `rustup` probe is likewise not a build input. |

An upstream rustup install, a distribution package and a Nix package whose binaries are
patchelf'd therefore agree when they are the same release with the same target. A different
release, commit or target still changes the key.

### Declared environment

`SEMANTIC_ENV` and `cache_env` are hashed by value, except that path lists
(`LD_LIBRARY_PATH`, `LIBRARY_PATH`, `PKG_CONFIG_PATH`, `PYTHONPATH`, `NODE_PATH`, `CPATH`,
`C_INCLUDE_PATH`, `CPLUS_INCLUDE_PATH`, `CMAKE_PREFIX_PATH`) are normalised first:

* entries inside the checkout are build state; the tree digest already names the source,
* entries inside the producer's Cargo target or any `target/<profile>` directory are build
  outputs,
* entries under the selected Rust toolchain's `lib` trees are locators for libraries whose
  release the semantic toolchain identity already names.

Empty and duplicate entries are removed; order is preserved, so a genuinely different library
search path still changes the key. This is what lets a land gate that inherited its own xtask
`LD_LIBRARY_PATH` and a CI runner that did not agree when the actual build inputs are equal.

Per-run scheduler and systemd state (`FORGEJO_*`, `ACTIONS_*`, `RUNNER_*`, `GITHUB_*`, `CI_*`,
`INVOCATION_ID`, `JOURNAL_STREAM`, `SYSTEMD_EXEC_PID`, `*_PRESSURE_*`, `nix-shell` attributes)
is never keyed. `GIT_CONFIG_*` `url.*.insteadOf` rewrites are forwarded to cached work as plumbing
so a land can serve unpushed commits from mirrors; they are not keyed, because the resolved
commits they lead to are. `RUSTC_WRAPPER`, a plain `CC`/`CXX` selector (its compiler's identity is
keyed through the toolchain; a selector that carries flags is keyed verbatim) and the presentation
locale (`LANG`, `LC_ALL`, `TZ`) are plumbing too — a compiler cache, a cache wrapper in front of
the same compiler, or a locale changes how a result is produced or presented, not whether the check
passes — so a runner with `kache-corbet`/`sccache-corbet cc`/`en_US.UTF-8` and a workstation with
`C.UTF-8` still share verdicts. A check that really depends on one declares it in `cache_env`.

`fleet = true` on a check adds `CCID_FLEET_DIGEST` (the SHA-256 of the release channel's
canonical fleet data) to its `cache_env` when the manifest is loaded, so its verdict and its moon
task are keyed by the fleet data it reads and an edit of that data misses once. The data's
location (`CCID_FLEET_FILE`) and a runner's extra forge host spellings (`CCID_FORGE_HOSTS`) are
never keyed. A check without the field keys exactly as before;
see [fleet data](fleet.md).

## The record

One JSON file per key: `<CCID_RESULT_CACHE>/verdicts/<key>.json`.

| Field | Meaning |
|---|---|
| `schema` | Record schema (`2`). |
| `key` | The key above. |
| `components` | The exact components; they must hash to `key`, or the record is refused. |
| `commit` | The commit the tree came from (provenance only). |
| `repository` | Canonical repository path (`owner/name`), provenance only. Scheme, user, host, port and `.git` are stripped: `git.example`, `forge.example`, `forge.example.internal` and the in-cluster service name are transport aliases of the same repository, and the tree digest names the content. |
| `result` | `success` or `failure`. **Only `success` is ever accepted.** |
| `producer` | `lane` or `ci`, plus free-form identity (host, lane, pipeline URL). Provenance only. |
| `tool` | The producing ccid binary: SHA-256 of its bytes and the source revision compiled into it. |
| `started_at`, `finished_at`, `duration_seconds` | Unix seconds. |

Records are written atomically under a per-key lock. A pass wins over a failure forever: two
producers that agree on the key agree on the result, and a later failure never erases a pass. A
**success from a different producing binary may refresh the record** (same key and components,
new `tool`/provenance/timestamps). Without that, after a channel bump the previous channel's
record would be refused as foreign and the run that recomputes could never leave a record the
new channel accepts — every later run would miss forever. New records are
group-readable/group-writable when the store is group-writable (`0660`, store mode `2775`).

## Acceptance (CI)

A lookup is a **hit** only when all of this holds:

1. the record parses, validates, and its `components` are *exactly* equal to the components
   computed now — never a partial match;
2. `result` is `success`;
3. the producing binary is on the release channel: `CCID_CHANNEL_FILE` names its
   `revision` + `sha256` (any class), or, when no channel file is provisioned, CI accepts only the
   binary it is running — the workflow verified that binary against the channel before exec;
4. when the check declares `cache_outputs`, every declared output is present in the content store
   and restores byte-for-byte into its declared path (see [Outputs](#outputs)). A pass whose
   outputs cannot be restored is a **miss**, never a hit with missing artefacts.

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
did before. It never guesses. A declared output left in the checkout by an earlier run is
evidence the check produced, not an input it reads: it never disables a later lookup. An
undeclared untracked file still refuses the key.

After a successful check, the inputs are re-read; a dependency that moved while the check ran
writes no record.

## Outputs

A hit skips the ordinary cached transaction, so it also skips moon's output hydration. The
declared `cache_outputs` are therefore stored beside the verdict store:

```text
CCID_RESULT_CACHE/
  verdicts/<key>.json              the verdict record
  outputs/
    blobs/<sha256>                 file bytes, or a deterministic tar of a directory
    verdicts/<key>.json            the output index, named by the SAME verdict key
    prune.stamp                    last retention pass
```

* **Content-addressed.** Each declared output becomes one blob named by the SHA-256 of its
  bytes; identical reports across runs, checks, repositories and branches share one blob. A
  directory becomes a deterministic tar (sorted entries, zero mtime/uid/gid, explicit 0644/0755)
  so the same tree always produces the same digest.
* **Complete before the verdict.** A producer stores every blob and the index first, and writes
  the verdict record only after that transaction succeeds. A declared output that is missing,
  a symlink, a special file or larger than the store bound leaves **no verdict**: the check
  itself may still pass, but the next run recomputes instead of hitting an unrestorable pass.
* **Restore is the hit.** A hit reads the index, verifies every blob digest, and installs each
  output into its declared path (files atomically, directories by staged swap). If the index is
  absent (a record from an older release), a blob is missing or corrupt, or the restored bytes
  differ, the lookup becomes a miss and the check runs. After the restore the job sees exactly
  what a real run left: `coverage.json`, `coverage.lcov`, `coverage.txt`, ... are archived or
  uploaded unchanged.
* **Safety.** Outputs must be plain repository-relative paths, never `../`, absolute, a
  generated/operational directory (`.git`, `.ccid`, `.moon`, `target`, `node_modules`) or
  tracked source input. Directories may not contain symlinks or special files. The store is
  bounded at 64 MiB per output and 256 MiB per declared set.
* **Retention.** The newest output set per repository+branch is always kept, plus a bounded
  window of `8` older sets (nine in total). Only `outputs/` is pruned:
  unreferenced blobs are garbage-collected after a one-hour grace period so a producer cannot
  lose a blob between writing it and publishing its index. Verdict records, the moon cache and
  every other cache are never touched. A coverage set is a few megabytes, so the window is tens
  of megabytes per repository+branch.
* **Provenance only.** The branch in the index comes from `CCID_BRANCH`, otherwise the CI branch
  variables, otherwise `HEAD`; it groups retention and is never part of the verdict key.

## Coverage

Coverage is a check like any other. A repository that declares its coverage work in a check (or
in the `verify` check that contains it) gets one verdict per tree, with the same key components as
every other check: the tree content, the resolved dependency commits, the **semantic release of the nightly
compiler and `cargo-llvm-cov`** the check declares in `cache_tools`, the profile and the declared
environment. A nightly toolchain update therefore changes the key exactly once, and an unchanged
tree is never instrumented twice. The declared coverage reports are in the output store above, so
a later nightly run restores the identical reports without a second full build.

## Producing, planning, inspecting

* `ccid cached --check <name>` — look up, then run the ordinary cached transaction on a miss and
  record a success. Events: `verdict-plan`, `verdict-inspect`, `verdict-record`,
  `verdict-skipped`, `verdict-disabled`, `verdict-unavailable`, `verdict-outputs-stored`,
  `verdict-outputs-restored`, `verdict-outputs-prune`. A hit restores the declared outputs and
  emits a `cache-check` with `status: cached` and `computed: 0`, and leaves the declared receipt
  (`<check>.jsonl`).
* `ccid cached --check <name> --plan` — print each check's verdict and its bypass reason (a
  declaration opt-out or a purity-scan finding such as a network tool) without running anything.
  A pass whose stored outputs are absent reports `miss` and the missing set.
* `ccid job-run --local --source <dir> --sha <commit>` — run exactly CI's declared job against an
  existing export of the commit. This is the land gate's producer entry point; it uses the same
  job plan, environment, adapter check and command as `ccid job-run` on a runner.
* `CCID_VERDICT_DEBUG=1` — emit a `verdict-debug` event with the exact `check_identity` input
  (declaration and normalised declared environment) and the toolchain digests beside the key.
  Values are the declared build environment; the switch is off unless a producer asks for it.
  When two producers disagree, this prints what they disagree on instead of guessing.

## Store operation

`CCID_RESULT_CACHE` is provisioned by the runner (CI) or the operator (land gate). The
`verdicts/` and `outputs/` directories are created on first write with mode `2775`; records,
indexes and blobs are `0660` when the store is group-writable. Both producers must be in the
store's group. Records are immutable in practice; a corrupted, symlinked or oversized record or
index is refused, never parsed as a pass, and a corrupted blob is never restored. Retention is
automatic and idempotent; there is no operator step that can delete another cache.
