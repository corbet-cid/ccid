# `ccid resolve-lock`

Retention of a freshly resolved npm lock as a commit artifact. This is the
`resolve-npm` maintenance job the repositories used to run as
`.ci/resolve-npm.sh`: an explicit resolution, never a verification verdict.

## Contract

```
ccid resolve-lock [--repo .] [--manifest .ci/ccid.toml] --npm
```

- Requires `ARTIFACT_ROOT` (absolute, outside `--repo`) and `CI_COMMIT_SHA`
  (40 lowercase hex characters) in the environment, else exit 2.
- Exports the commit with `git -C <repo> archive <CI_COMMIT_SHA>` into a
  scratch directory and runs exactly
  `npm install --package-lock-only --ignore-scripts --no-audit --no-fund`
  there: the byte-identical command the retired scripts ran.
- Retains `package-lock.json` under
  `$ARTIFACT_ROOT/$CI_COMMIT_SHA/resolve-npm/`, written through a temp file
  plus rename so a failure leaves no partial lock behind.
- Prints one `sha256sum`-style line per retained file
  (`<hex>  package-lock.json`), the lock between `---LOCK-BEGIN---` and
  `---LOCK-END---` when `print_lock` is set, and finally
  `Resolved npm lock only; retain it in a commit and run the full verify job.`
- Exit 0 on success; exit 1 when `npm install` fails (stderr passes
  through, then `ccid resolve-lock: npm install failed`); exit 2 on bad
  inputs. The checks stay `cache = false` in the repositories (their
  `cache_outputs = ["package-lock.json"]` goes away in the rollout: the
  retained lock is an artifact under `ARTIFACT_ROOT`, not a cached check
  output).

## HR1: resolution outside the declared source

Everything happens in the exported copy; `--repo` is only read (`git
archive`). A test asserts the checkout's content digest (sha256 over the
sorted `git ls-files --cached --others -z` paths and contents) and its
`package-lock.json` bytes are identical before and after `ccid resolve-lock`.

## HR2: no lock drift

There is no selection to drift: npm mode runs the one fixed argv above, and
a test asserts the recorded argv byte-for-byte. No `cargo update`, no `-p`
of any package, no bare update anywhere in ccid.

## The dropped bare `cargo update`

The 10 base `resolve-dependencies.sh` variants (and every other full-update
variant) ran a bare `cargo update`, which moves third-party pins away from
the integration lock and destroys the compile sharing the two-way
third-party sync protects (lanes share only ~22 % of CI compile work
today). Per the cmtymeet answer Q9 the Rust `resolve-dependencies` jobs are
retired (`xtask lock-refresh` is the one mover of Rust pins) and only the
npm pins are ported here, because npm pins have no other mover. Invoking
`ccid resolve-lock` without `--npm` is refused (exit 2).

## Manifest data

```toml
[resolve]
print_lock = true   # echo the retained lock between ---LOCK-BEGIN---/---LOCK-END---
```

A missing table means defaults. Unknown keys are refused (exit 2).
