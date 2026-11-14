# The job runtime (`[runtime]`)

A Forgejo job whose command is `ccid:run-declared-checks` runs `ccid check` over the job's checks.
Before `[runtime]`, a repository that needed persistent CI state, tools from a nixpkgs pin and the
shared toolchain lock gave its jobs a script instead (`.ci/run-cached.sh`, with
`.ci/toolchain-lock.sh`). Every such script is a variant of one skeleton. With
`[runtime]` declared, `ccid job-run` does what the skeleton did, and the script goes.

```toml
[jobs.verify]
scheduler = "forgejo"
runs_on = "linux"
checks = ["verify", "advisories"]
workflow = "ci"
command = ["ccid:run-declared-checks"]

[checks.advisories]
kind = "commands"
commands = [["cargo", "deny", "--locked", "check", "advisories"]]
cache_pure = false              # advisories change without the tree changing

[runtime]
tools = ["python3", "cargo-deny", "cargo-llvm-cov"]   # nixpkgs attribute paths
install_root = false            # true: CARGO_INSTALL_ROOT = <state>/install-bin, its bin on PATH
state_env = { YARN_CACHE_FOLDER = "yarn-cache" }      # further persistent directories, by variable

[toolchain]                     # only with [runtime]
nightly = true                  # nightly with llvm-tools (coverage)
targets = ["stable:wasm32-unknown-unknown"]
```

The table names are working names.

## What `job-run` does with `[runtime]`

After the source is staged and `render --check` passed, before the job command starts:

1. **State.** The root is `CI_CACHE_ROOT`, else `CARGO_HOME`; it must be absolute. The repository's
   state is `<root>/ccid-cached/<namespace>`, where the namespace is the first 24 hex characters of
   `sha256(CI_REPOSITORY_URL)`. Toolchains are shared: `CI_TOOLCHAIN_ROOT` (absolute), else
   `<root>/ccid-cached/toolchains`. These are the script's paths, so the state of a repository that
   moves from the script to the runtime carries over.
2. **State lock.** `<state>/lock`, exclusive, for the whole job: the script's `flock 9`. A job that
   does not get it within `CI_TIMEOUT` seconds (default 2700) ends with exit 1.
3. **Tools.** `nix build --no-link --print-out-paths` of moon, proto and every `tools` entry at the
   nixpkgs revision and NAR hash of the rendered check-shell (`src/render/check-shell.nix`; a test keeps
   the two pins equal). Each output's `bin` goes in front of `PATH`, the last one first. moon and
   proto are always built: `ccid cached` runs checks through moon, and the toolchain lock provisions
   Rust through proto. A failed build stops the job (exit 2).
4. **Environment.** `PROTO_HOME=<toolchains>/proto`, `MOON_HOME=<state>/moon-home`,
   `PROTO_AUTO_CLEAN=false`, `PROTO_TELEMETRY=false`, `PROTO_YES=true`, and `PATH` =
   `<this ccid's directory>:<proto>/shims:<proto>/bin:<tools>:<PATH>`. With `[toolchain]` also
   `RUSTUP_INIT_SKIP_PATH_CHECK=yes`, `RUSTUP_TOOLCHAIN=stable`, `CI_JOBS` (default 2),
   `CARGO_BUILD_JOBS=$CI_JOBS`, `RUST_TEST_THREADS=${CI_TEST_THREADS:-2}` and
   `CARGO_NET_GIT_FETCH_WITH_CLI=true`. `ccid check` derives the parallelism of what it runs from
   `CI_JOBS` itself (see `execution.md`).
5. **Git aliases.** Every legacy owner prefix of the release channel's fleet data
   (`Fleet::insteadof_pairs`) becomes a `url.<canonical>.insteadOf` rule, appended as
   `GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n` after the `GIT_CONFIG_COUNT` entries the environment
   already has. Without fleet data the job refuses to start (exit 2): it would otherwise fetch first-party
   dependencies through the legacy URLs.
6. **Toolchain** (`[toolchain]`). The shared-first toolchain lock of `ccid toolchain ensure`
   (`<toolchains>/install.lock`, same protocol and lock file as `toolchain-lock.sh`) with the job's
   environment and the Rust homes `ccid check` uses (`CCID_RUSTUP_HOME`, `CCID_CARGO_HOME`). Then
   `rustup which --toolchain stable cargo` and its directory in front of `PATH`. A toolchain that cannot be
   provisioned in time ends the job with exit 1.
7. **Directories.** `install_root`: `CARGO_INSTALL_ROOT=<state>/install-bin`, its `bin` on `PATH`.
   `ARTIFACT_ROOT`: the runner's (absolute), else `<state>/release-artifacts`. Each `state_env`
   variable: `<state>/<directory>`. All are created.
8. **`SOURCE_SHA256`.** In a git checkout, the sha256 of `git archive --format=tar HEAD`, the value
   the scripts exported for their receipts. A plain source export gets none.
9. **The command.** `ccid check --repo . --check <each declared check>`, inside the rendered
   check-shell when the repository has one, with the job's stderr joined to its stdout (the scripts'
   `exec 2>&1`). The process is replaced (`exec`); both locks stay held by the job command and
   whatever it starts, until the last of them exits, exactly as with the script.

## What the runtime does not do

- It creates nothing in the checkout. In particular it never makes `.moon/cache/hashes` or
  `.moon/cache/outputs`: `ccid cached` links those into the result cache itself, and a directory or link
  made by anyone else makes the cached executor refuse its cache ("Moon artifact cache belongs to a
  different coordinator"), so the checks run uncached and no verdict is recorded. The scripts that
  linked them into `<state>/moon-cache` lost the result cache that way.
- It never resolves or moves dependencies and runs no `cargo update` (the owned-crate float runs
  inside `ccid check`, outside the checkout).
- It prints no tool versions and guards no check selection: a check keys its tools through
  `cache_tools` and fails without them, and the job's declared checks are the selection.
- It installs nothing besides the toolchain protocol's own guarded installs.

## Rules

- `[runtime]` serves `ccid:run-declared-checks` jobs only: a Forgejo job with its own command in a
  manifest with `[runtime]` is refused when the manifest is planned or rendered.
- `[toolchain]` without `[runtime]` is refused (nothing else reads it).
- `tools` are nixpkgs attribute paths (`[A-Za-z0-9_+-]` segments joined by `.`), each once, without
  moon and proto.
- `state_env` names an uppercase variable that neither the runtime nor the runner owns, and one plain
  directory name that the runtime does not use itself (`lock`, `moon-home`, `install-bin`, ...).
- Both tables are new keys: an older ccid refuses a manifest that has them (`deny_unknown_fields`), so a
  repository uses them only after the release that knows them is on the channel and in its land gates.

## Moving a repository

1. In `.ci/ccid.toml`: every job's `command` becomes `["ccid:run-declared-checks"]`; `[runtime]`
   gets the script's `nix build` attributes without moon and proto, `install_root = true` if the
   script set `CARGO_INSTALL_ROOT`, and its `$state`-relative exports as `state_env`; `[toolchain]` gets
   `TOOLCHAIN_WANT_NIGHTLY` and `TOOLCHAIN_WASM_TARGETS`; a check the script ran with `ccid check`
   instead of `ccid cached` (advisories, registry reads) gets `cache_pure = false`; static exports of the
   script become the job's `environment`; a pre-check line (`nix flake metadata --no-update-lock-file`)
   becomes a declared check.
2. Delete `.ci/run-cached.sh` and `.ci/toolchain-lock.sh`; `ccid render`.
3. The commit's own verify run is the proof.

Blocks the runtime does not cover stay in the repository's script until they are ported: tools found
by path (`CHROME_BIN=$(command -v chromium)`), interpreter environments built from a nix expression,
a second nixpkgs pin, exit traps that copy reports, services started for a test (valkey, PostgreSQL),
per-tool proto homes and fixture clones.

## Replacing a hand-written `.ci/run-cached.sh`

`ccid` renders no `pip`, `google-chrome` or `cargo llvm-cov` probe: those lines live in a repository's own
hand-written `.ci/run-cached.sh`, which `ccid job-run` with `[runtime]` replaces (surface class `job-script`).
Nothing is provisioned for a repository that does not declare it. A Rust repository with browser or wasm
tests and coverage declares its tools instead, from the same nixpkgs pin:

```toml
[runtime]
tools = ["chromium", "chromedriver", "cargo-llvm-cov", "wasm-pack"]
```

`chromium` is free software and is what `wasm-bindgen-test-runner` drives with `CHROME_BIN`; use
`google-chrome` (unfree, not in the pin) only when a test needs Chrome itself. A Python tool such as
`python3 -m pip` has no place in a Rust repository's CI: delete the probe with the script.
