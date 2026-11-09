# Coverage gates

`ccid coverage` evaluates LLVM coverage reports that the repository produced
with `cargo llvm-cov`. It never runs llvm-cov itself: the repository keeps its
own nightly selection, private target directory and report commands, and calls
these subcommands where its scripts used to run.

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
  per line;
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
| 2 | Operational error, including an unparseable manifest: stderr is `ccid coverage: <error>` |
