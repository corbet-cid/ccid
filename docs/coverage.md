# Coverage gates

`ccid coverage run` produces the three reports of one `cargo llvm-cov`
execution and runs the declared gates on them. `ccid coverage gate` and
`ccid coverage totals` evaluate reports a repository produced itself.

## Run

```sh
ccid coverage run [--target native] [--repo .] [--manifest .ci/ccid.toml]
```

One run, in `--repo`:

1. the nightly toolchain: `rustup which --toolchain nightly rustc` and
   `... cargo`. Every child gets `RUSTC`, `CARGO`, `RUSTUP_TOOLCHAIN=nightly`
   and the nightly bin directory first on `PATH`, so cargo-llvm-cov's own
   compiler calls stay on nightly. `<cargo> llvm-cov --version` must succeed;
2. a private `CARGO_TARGET_DIR` created under `TMPDIR` (else the platform
   default), refused as `unsafe coverage target` when that directory is inside
   `--repo` (the staged copy when the run floated), and removed afterwards in
   every case; `CARGO_BUILD_TARGET_DIR` and `CARGO_LLVM_COV_TARGET_DIR` are
   removed from the children's environment;
3. one instrumented execution and two reports of it:

   ```sh
   <cargo> llvm-cov --locked <llvm_cov> --json --output-path coverage.json
   <cargo> llvm-cov report --locked <report> --lcov --output-path coverage.lcov
   <cargo> llvm-cov report --locked <report> <text> --text --output-path coverage.txt
   ```

   Reports left by an earlier run are removed first, so a gate never reads a
   stale one. They are the check's `cache_outputs`;
4. the gates in `gates` order, in-process, each reading the same three
   reports, with the manifest's exclusions and `--target`.

`[coverage]` keys of the run, all optional; the defaults are the arguments of
the shared verify scripts:

| Key | Default | Meaning |
|---|---|---|
| `llvm_cov` | `["--workspace", "--all-features", "--branch", "--no-cfg-coverage", "--no-cfg-coverage-nightly", "--ignore-filename-regex", "/tests/"]` | Arguments of the execution |
| `report` | `["--branch", "--ignore-filename-regex", "/tests/"]` | Arguments of both reports |
| `text` | `[]` | Extra arguments of the text report only (for example `["--show-instantiations"]`) |
| `gates` | `["source"]` | Ordered subset of `"totals"`, `"source"`; empty, unknown or repeated names are refused |
| `require_sources` | none | The totals gate's `--require-sources`, relative to `--repo`; needs `"totals"` in `gates` |
| `artifacts` | `false` | Copy the reports that exist to `$ARTIFACT_ROOT` after the run, also on failure |

Arguments the run sets itself (`--json`, `--lcov`, `--text`, `--html`,
`--cobertura`, `--codecov`, `--output-path`, `--output-dir`, `--no-report`,
`--locked`, `--target-dir`) are refused in `llvm_cov`, `report` and `text`.
Both gates of one repository read the reports of the one execution; there is
never a second build for the second gate. A manifest without these keys keys
and renders exactly as before.

Outcomes: gate output on stdout as below; a failing tool prints
`ccid coverage run: <command> failed (exit N)` and exits 1, the first gate
refusal prints `Coverage gate: <message>` and exits 1. An invalid `[coverage]`,
a missing nightly or cargo-llvm-cov, an unsafe target directory, and
`artifacts = true` with `ARTIFACT_ROOT` unset, relative or inside `--repo`
exit 2 before anything runs. A signal stops the run after the current tool and
still removes the target directory.

## Source gate

```sh
ccid coverage gate <LCOV> <JSON> <ANNOTATED> [TARGET]
```

`LCOV` comes from `cargo llvm-cov report --lcov`, `JSON` from
`cargo llvm-cov --json`, `ANNOTATED` from `cargo llvm-cov report --text`; all
three must describe the same execution. `TARGET` defaults to `native`.

The gate cross-checks the three reports exactly:

- every raw JSON file maps to one repository-relative production Rust source
  (`.rs`, outside `tests/` and `target/`);
- every LCOV record belongs to a known file, with duplicate, negative and
  out-of-range counters refused;
- each file's `LF`/`LH`/`BRF`/`BRH` summaries match the raw export, except
  that a file without emitted branches and a zero branch summary needs no
  `BRF`/`BRH` records (LLVM counts generic copies the merged records omit);
- the expected branch locations are twice the distinct JSON branch locations
  per line. When the export carries function records (`cargo llvm-cov --json`
  always does), the locations are those of `functions[].branches` naming the
  file: LLVM's JSON exporter lists every branch region of a file in
  `files[].branches`, including regions whose two counters are both folded to
  the constant zero, while the per-function lists and the LCOV exporter drop
  exactly those regions with the same filter (`!TrueFolded || !FalseFolded`).
  rustc emits such regions for every branch of a function it instrumented but
  never generated code for, such as an uninstantiated generic. Every
  per-function location must also be a per-file location, and every per-file
  location the function records omit must be unexecuted (both counts zero)
  and start on a line LCOV reports with count zero, else `Missing emitted
  branch locations`. The line condition leaves the omitted code to the line
  gate: an uninstantiated function's lines are uncovered and refused unless a
  reviewed exclusion names them, and a folded region on an executed line (a
  never-called closure beside executed code) still refuses. An export without
  function records keeps the per-file locations;
- the annotated report names the same files and lines with consistent hit
  counts; a heading-free annotated report binds only when all three reports
  agree on exactly one production source.

Then every emitted line and branch must be covered, except for exclusions.

Success prints one `Excluded line: <file>:<line> <reason>` or
`Excluded branch: <file>:<line>:<block>:<id> <reason>` line per applied entry,
then `lines: X/Y` (`lines: X/Y; exclusions: N` when the manifest declares at
least one exclusion, with `N` the applied entries) and `branches: X/Y`, and
exits 0. Any refusal, unreadable report or invalid exclusion entry prints
`Coverage gate: <message>` and exits 1.

## Exclusions

Exclusions live only as `[[coverage.exclusions]]` in `.ci/ccid.toml`; a
missing manifest means no exclusions. Every entry is validated on every run,
whatever its targets; then only entries whose `targets` contain the run's
target apply.

| Field | Meaning |
|---|---|
| `file` | Repository-relative production Rust source path |
| `line` | Excluded 1-based line, at least 1 |
| `source` | The stripped source line; exactly one of `source` or `context` |
| `context` | Three unstripped lines: `line - 1`, `line`, `line + 1`; requires `line >= 2` |
| `reason` | Why the exclusion exists; never empty |
| `evidence` | Optional pointer at the reviewed justification |
| `targets` | Targets the entry applies to (`native`, `wasm`, `browser`); default `["native"]` |
| `branch` | Optional exact arm `{ block, id }` with non-negative integers |
| `evidence_sources` | Optional map of repository-relative path to sha256 hex, re-hashed every run |
| `evidence_packages` | Optional map of package name to `{ version, source }`, matched against `Cargo.lock` |

A line exclusion on a line that has branches is allowed only when every
emitted arm of that line has its own branch exclusion. A branch exclusion
needs its file listed in `evidence_sources`.

## Totals gate

```sh
ccid coverage totals <JSON> [--require-sources <DIR>]
```

Checks exact LLVM line and branch counts per unit: each unit prints
`lines: <covered>/<count>` and `branches: <covered>/<count>`, and any
shortfall refuses. With `--require-sources`, every `**/*.rs` below `DIR`
(resolved against the current directory) must appear among the report's file
names, else `Missing production sources: <sorted absolute paths>`. Refusals
print `Coverage gate: <message>` and exit 1.

## Converter

```sh
ccid coverage convert-exclusions --from <legacy.json> [--default-targets native[,wasm]]
```

Converts a legacy exclusion JSON list to `[[coverage.exclusions]]` TOML on
stdout, for a mechanical move into `.ci/ccid.toml`:

- entries with `file` keep `file`, `line`, `source`, `reason`, `evidence`,
  `branch`, `evidence_sources`, `evidence_packages`; `target = "x"` becomes
  `targets = ["x"]`, `targets` is kept, otherwise the default targets apply;
- entries with `path` + `context` become `file` + `context`;
- entries with `path` + `source` become `file` + `source`;
- anything else is an error naming the entry index.

Entries keep their input order. The output parses back into
`[[coverage.exclusions]]`.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Gate passed |
| 1 | Gate refusal: stderr is `Coverage gate: <message>` |
| 1 | `coverage run`: a tool failed: stderr is `ccid coverage run: <command> failed (exit N)` |
| 2 | Operational error, including an unparseable manifest: stderr is `ccid coverage: <error>` |

Converting a repository's verify script into declared checks that call `ccid coverage run`: [verify.md](verify.md).
