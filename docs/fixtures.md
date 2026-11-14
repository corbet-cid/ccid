# Declared native fixtures

A check whose tests need native inputs (an authenticated proof directory, a prebuilt worker binary, a
started service) declares them instead of building them by hand inside its command. ccid provides them in
the check environment and removes them afterwards.

```toml
[fixtures.proof]
setup    = ["bash", ".ci/fixture-proof.sh"]      # required
teardown = ["bash", ".ci/fixture-proof-down.sh"] # optional
env      = ["CBLC_PROOF_DIRECTORY", "CBLC_BB"]   # required, the names setup must export

[checks.coverage]
kind     = "commands"
fixtures = ["proof"]
commands = [["ccid", "coverage", "run"]]
```

## Lifetime

For each check that lists `fixtures`, in the order listed and only when the check actually runs (a cache
hit starts nothing):

1. ccid creates a private directory and runs `setup` in the repository root with
   `CCID_FIXTURE_DIR` (that directory) and `CCID_FIXTURE_ENV` (a file inside it);
2. `setup` writes `NAME=VALUE` lines to `CCID_FIXTURE_ENV`: exactly the names of `env`, each once, with a
   non-empty value. A missing, repeated, empty or undeclared name stops the check before its commands;
3. the check's commands run with those variables added to their environment. For `ccid coverage run`
   called from the check, the fixture therefore lives for the one coverage run;
4. `teardown` runs in reverse order, with the fixture's variables, also when the check failed; every
   teardown runs, the first failure is reported; the private directory is removed.

The variables are removed again before the next check, so a fixture reaches only the checks that list it.

## Rules

- `env` names are uppercase `[A-Z_][A-Z0-9_]*`; `PATH`, `HOME`, `TMPDIR`, `RUSTC`, `CARGO`, `RUSTFLAGS`,
  `LD_*`, `CARGO_*`, `CI_*` and `CCID_*` are refused, so a fixture cannot change the toolchain.
- Two fixtures of one check may not export the same name. A check may not list a fixture twice or one
  that is not declared.
- The manifest bytes already key a check's result, so changing a fixture's definition re-keys every check
  that uses it; the values `setup` produces are private to the run and never keyed.
- A manifest with `[fixtures]` or `fixtures = [...]` is refused by a ccid release that predates the key.
