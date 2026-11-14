# Converting verify scripts to declared checks

A repository's `.ci/verify.sh` is an ordered list of gates behind
`[checks.verify] commands = [["bash", ".ci/verify.sh"]]`. The conversion
moves that list into the manifest itself, one `commands` entry per stage, in
the script's order:

```toml
[checks.verify]
kind = "commands"
fleet = true
commands = [
  ["ccid", "first-party"],
  ["cargo", "fmt", "--all", "--check"],
  ["cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings"],
  ["cargo", "test", "--locked"],
  ["ccid", "coverage", "run"],
]
cache_outputs = ["coverage.json", "coverage.lcov", "coverage.txt"]
```

`native.sh`, `check.sh` and `native-check.sh` follow the same rules as their
own `[checks.<phase>]`. A phase dispatcher `.ci/check.sh` that nothing calls
is deleted by the rollout, not converted; `acceptance.py` stays the land
tool's.

## Hard rules

- No converted command writes a tracked file or a declared input. Every file
  a command writes into the tree is a `cache_outputs` entry; scratch lives
  under `TMPDIR`. The proof of a conversion is the checkout's content digest
  (sorted `git ls-files --cached --others` paths and contents, declared
  outputs left out), unchanged after `ccid check --check verify`.
- No lock drift: no command runs `cargo update` in any form. The owned-crate
  float is not a check command: a run with a non-empty float resolves before
  any key, in a copy, and runs every check in the staged copy with the
  resolved locks (see "Resolution before keying" in [verdicts.md](verdicts.md)); declared outputs are copied back
  to the checkout. A script whose only `cargo update` is the shared
  `update-our-crates.sh` call converts; any other `cargo update` line stops
  the conversion of that repository.

## Stage rules

| verify.sh stage | `commands` entry |
|---|---|
| `set -euo pipefail`, `exec 2>&1`, `CARGO_BUILD_JOBS` / `CARGO_TERM_COLOR` / `RUST_TEST_THREADS` exports, `rustc --version` and nightly version echoes | dropped: ccid sets jobs and test threads from its budget, the colour is in the shared profile, and `cache_tools` records the tool versions |
| `python3 .github/check-first-party.py [args]` | `["ccid", "first-party", args...]` and `fleet = true` on the check; the script's exception maps become `[first_party]` data, keeping only entries that match the current lock ([first-party.md](first-party.md)) |
| `bash .ci/update-our-crates.sh` | dropped, with no replacement inside the check |
| `cargo fmt --check`, `clippy`, `test`, `check`, `build` | the script's exact cargo argv, one entry each |
| `cargo fmt --all` followed by `git diff --exit-code -- '*.rs' > format.patch` | `["cargo", "fmt", "--all", "--check"]`; no `format.patch` (rustfmt prints the diff into the check's log). A writing `cargo fmt` rewrites tracked sources on an unformatted tree, which the first hard rule forbids also on a failing run |
| `cargo check --target wasm32-unknown-unknown ...` | the exact argv |
| `cargo metadata ... > dependency-metadata.json` | `["bash", "-c", "<the line>"]`; `dependency-metadata.json` into `cache_outputs`. A tracked `dependency-metadata.json` stops the conversion |
| `bash scripts/wasm-runner.sh` and the `GITHUB_PATH` reload | merged into the wasm test entry: `["bash", "-c", "PATH=\"$(ccid toolchain wasm-bindgen):$PATH\" <the test line>"]` |
| the llvm-cov block and the coverage gate call (and a `test-*coverage*.py` call) | `["ccid", "coverage", "run"]`; the block's arguments become `[coverage]` data; the Python gate tests are dropped (they are ccid's tests now); a second llvm-cov run for a totals gate merges into the one run: `gates = ["totals", "source"]` |
| a block that only calls tools (`cd sub && ...`, an environment prefix, exports a later stage reads) | `["bash", "-c", "set -euo pipefail\n<the lines>"]`; `set -euo pipefail` keeps the script's failure semantics for a multi-line block |
| repository-specific gates (record-fixture import, node, npm, pins, auto-merge tests, Tor, nix, valkey, native SDK, fixtures, browser coverage, advisories, `python3 .ci/<own script>.py`) | the unchanged command line |

`cache_tools` and `cache_env` of `[checks.verify]` stay as they were;
`cache_outputs` stays and gains every file a converted command writes into
the tree.

### The llvm-cov block as data

| Script | `[coverage]` |
|---|---|
| `"$NIGHTLY_CARGO" llvm-cov --locked <A> --json --output-path coverage.json` | `llvm_cov = [<A>]` |
| `... llvm-cov report [--locked] <R> --lcov --output-path coverage.lcov` | `report = [<R>]` |
| `... llvm-cov report [--locked] <R> <T> --text --output-path coverage.txt` | `text = [<T>]` |
| `check-source-coverage.py` (emitted-source gate) | `gates = ["source"]` (default) |
| `check-coverage.py <json>` (totals) on the same JSON | `"totals"` in `gates` |
| `--require-sources src` style listing | `require_sources = "src"` |
| an exit trap copying the reports to `$ARTIFACT_ROOT` | `artifacts = true` |
| a legacy exclusions JSON | `[[coverage.exclusions]]` from `ccid coverage convert-exclusions --from <file>` |

Keys equal to the defaults can be left out (see [coverage.md](coverage.md)).
The run always gives the source gate all three reports of the one execution,
so a gate called with the wrong number of reports cannot exist any more.

## Worked examples

### Source gate with an annotated report and artifacts

The script (stage order `upd meta covgate fmt fp clippy test llvmcov`):

```sh
trap '... cp coverage.json coverage.lcov coverage.txt "$ARTIFACT_ROOT/" ...' EXIT
bash .ci/update-our-crates.sh
cargo metadata --locked --format-version 1 > dependency-metadata.json
python3 .github/test-check-source-coverage.py
python3 .ci/test-provider-controls.py
rustc --version
cargo fmt --all --check
python3 .github/check-first-party.py
cargo clippy --locked --all-targets -- -D warnings
python3 .ci/provider-controls.py
cargo test --locked --no-fail-fast
# nightly block, private target, then:
"$NIGHTLY_CARGO" llvm-cov --locked --branch --ignore-filename-regex '(^|/)(tests|scripts)/' --json --output-path coverage.json
"$NIGHTLY_CARGO" llvm-cov report --branch --ignore-filename-regex '(^|/)(tests|scripts)/' --lcov --output-path coverage.lcov
"$NIGHTLY_CARGO" llvm-cov report --branch --ignore-filename-regex '(^|/)(tests|scripts)/' --text --show-instantiations --output-path coverage.txt
python3 .github/check-source-coverage.py coverage.lcov coverage.json coverage.txt
```

Converted:

```toml
[checks.verify]
kind = "commands"
fleet = true
commands = [
  ["bash", "-c", "cargo metadata --locked --format-version 1 > dependency-metadata.json"],
  ["python3", ".ci/test-provider-controls.py"],
  ["cargo", "fmt", "--all", "--check"],
  ["ccid", "first-party"],
  ["cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings"],
  ["python3", ".ci/provider-controls.py"],
  ["cargo", "test", "--locked", "--no-fail-fast"],
  ["ccid", "coverage", "run"],
]
cache_tools = [...]
cache_outputs = ["coverage.json", "coverage.lcov", "coverage.txt", "dependency-metadata.json"]

[coverage]
llvm_cov = ["--branch", "--ignore-filename-regex", "(^|/)(tests|scripts)/"]
report = ["--branch", "--ignore-filename-regex", "(^|/)(tests|scripts)/"]
text = ["--show-instantiations"]
artifacts = true

# the entries of docs/coverage-exclusions.json, from
# ccid coverage convert-exclusions --from docs/coverage-exclusions.json
[[coverage.exclusions]]
file = "src/..."
line = 0
context = ["...", "...", "..."]
reason = "..."
targets = ["native"]
```

The script's `ARTIFACT_ROOT` check moves into the run (`artifacts = true`
refuses an unset, relative or in-tree `ARTIFACT_ROOT` with exit 2, when the
run starts).

### A gate script with the wrong arity, and environment for later stages

The script calls the three-report emitted-source gate with two reports and
never writes the text report; the record-fixture import exports two variables
that the test and coverage stages read:

```sh
python3 .github/check-first-party.py
<record-fixture import: exports two variables the tests read>
rustc --version
bash .ci/update-our-crates.sh
cargo fmt --all
git diff --exit-code -- '*.rs' > format.patch
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
export CARGO_PROFILE_TEST_OPT_LEVEL='0'
# nightly block, private target, then:
"$NIGHTLY_CARGO" llvm-cov --locked --workspace --all-features --branch --no-cfg-coverage --no-cfg-coverage-nightly --ignore-filename-regex "/tests/" --json --output-path coverage.json
"$NIGHTLY_CARGO" llvm-cov report --locked --branch --ignore-filename-regex '/tests/' --lcov --output-path coverage.lcov
python3 .github/check-coverage.py coverage.lcov coverage.json
```

Converted: the import and every stage that reads its exports form one block,
in the script's order; the writing `cargo fmt --all` and its `git diff`
become `cargo fmt --all --check`; the llvm-cov arguments are the defaults, so
no `[coverage]` table is needed, and the run writes and gates all three
reports:

```toml
[checks.verify]
kind = "commands"
fleet = true
commands = [
  ["ccid", "first-party"],
  ["bash", "-c", "set -euo pipefail\n<record-fixture import, unchanged>\ncargo fmt --all --check\ncargo clippy --locked --all-targets -- -D warnings\ncargo test --locked --all-targets\nexport CARGO_PROFILE_TEST_OPT_LEVEL='0'\nccid coverage run"],
]
cache_tools = [...]
cache_outputs = ["coverage.json", "coverage.lcov", "coverage.txt"]
```

When the record-fixture family moves into ccid, the block splits into one
entry per stage.

### A native phase script

```sh
: "${CI:?CI only}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export RUST_TEST_THREADS="${RUST_TEST_THREADS:-2}"
export CARGO_NET_GIT_FETCH_WITH_CLI=true
rustc -Vv
cargo --version
sha256sum Cargo.lock
python3 .github/check-first-party.py --archive
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
sha256sum Cargo.lock
```

Converted (the lock digests only printed; a check that moves its locks is
refused by the verdict store and reported by the float):

```toml
[checks.native]
kind = "commands"
fleet = true
commands = [
  ["bash", "-c", ": \"${CI:?CI only}\""],
  ["ccid", "first-party", "--archive"],
  ["cargo", "fmt", "--all", "--", "--check"],
  ["bash", "-c", "CARGO_NET_GIT_FETCH_WITH_CLI=true cargo clippy --locked --all-targets --all-features -- -D warnings"],
  ["bash", "-c", "CARGO_NET_GIT_FETCH_WITH_CLI=true cargo test --locked --all-features"],
]
```

## Checking a conversion

1. `ccid check --repo . --check verify --plan` parses the manifest, including
   `[coverage]`.
2. Record the checkout's content digest and the root `Cargo.lock` bytes, run
   `ccid check --repo . --check verify`, and compare both afterwards. A
   changed digest names a command that writes into the tree: declare its
   output or move it to `TMPDIR`.
3. A `--locked` stage that fails after the conversion is a float finding:
   compare `ccid float --repo . --into <dir outside the repository>` with
   what the stage needs (`cargo metadata --locked` in a copy with those
   locks); never add a `cargo update`.

## Checks that read git history (`history = true`)

A check such as a push-range or signature gate judges commits, not the tree. Declare `history = true` on it. It must
also declare `cache_commit = true` or `cache = false` (a range is not a property of the tree; the manifest is refused
otherwise). `ccid job-run` then fetches the full history of the commit instead of the depth-1 checkout, and exports
`CCID_PUSH_BEFORE` (the previous tip of the push, from the event payload `GITHUB_EVENT_PATH`; absent for a new branch,
a manual run or `--local`). The variable is never part of a key. A manifest without `history` renders and keys exactly as before.
