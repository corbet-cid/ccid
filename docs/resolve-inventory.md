# Fixed multi-lock Cargo resolution

`ccid cargo-resolve --inventory --repo EMPTY_ANCHOR --output-dir ARTIFACTS`
is an additive mode. It cannot be combined with caller `--check` or
`--generate-lockfile`. The existing single-root interface is unchanged.
This mode requires the usual explicit resolver context, source archive identity,
Crow source/repository identity, cache and resource budget. `--repo` is an output
overlap guard; it never selects a manifest. No local developer tree is changed.

The verified source's `.ci/resolve.toml` is the only graph and check inventory:

```toml
schema = 1
checks = ["acceptance"]

[[graphs]]
id = "root"
kind = "cargo"
manifest = "Cargo.toml"
lock = "Cargo.lock"

[[graphs]]
id = "browser"
kind = "cargo"
manifest = "tests/browser/Cargo.toml"
lock = "tests/browser/Cargo.lock"

[[graphs]]
id = "typescript"
kind = "npm"
manifest = "clients/ts/package.json"
lock = "clients/ts/package-lock.json"

[[graphs]]
id = "toy"
kind = "cargo"
manifest = "toy/.build/Cargo.toml"
lock = "toy/.build/Cargo.lock"
seed = "root"
prepare = ["python3", "toy/prepare.py"]
```

Each ordinary Cargo entry must be an independent workspace with its adjacent
committed lock. It receives locked baseline metadata, one `cargo update
--manifest-path ...`, then locked candidate metadata. Compatibility uses the
existing resolver's direct dependency and lock-format rules. Nested workspace
members sharing a parent lock refuse rather than masquerade as independent roots.

Each npm entry requires an adjacent committed v2/v3 lock. It runs the original
`npm update --prefix ... --package-lock-only --ignore-scripts`; direct resolved
compatibility and format are compared. npm workspace/link graphs are explicitly
unsupported in this version. Package-manager tool versions are retained.

A derived Cargo entry references an earlier Cargo candidate. The original source
must not contain its generated manifest or lock. Its committed preparation
command may add only those two files and missing parent directories. It must
copy the exact seed lock. Unlocked **metadata**, never a build, resolves the new
harness root; locked metadata then verifies it. Every seed `(name, version,
source)` package tuple must survive. The receipt honestly records a derived graph
and seed digest, with no fabricated original lock. All compilation must later
use this retained derived lock with `--locked`.

Resolution refuses undeclared file/permission/link mutations after each graph.
After all resolutions it retains all baseline/candidate locks, a complete
candidate source tar, the inventory and a deterministic path-to-lock-digest map.
It freezes the entire tree before calling the existing check runner on every
committed selector in order. After **each selector**, all locks and the entire
tree must be unchanged. Selector implementations must separately retain/check
their own internal phases. Reports, targets, node_modules, browser fixtures and
code-generation comparison scratch belong outside source. Tool acquisition
matching candidate locks happens after resolution, before owner compilation.

Checks receive `CCID_LOCK_SET` and `CCID_LOCK_SET_SHA256`; no graph/filter selection
is accepted from them. Only the existing runner performs admission, target-cache
ownership, timeout and process supervision. No new scheduler or credential path.

## Receipts

Each Cargo graph uses the existing schema-3 receipt, extended with `graph_id`,
`manifest_path`, `lock_path`, `inventory_sha256`, `lock_set_sha256` and
`seed_lock_sha256`. Normal mode is `refresh` / `checked-against-baseline`.
Derived mode is `derive-from-candidate` / `checked-seed-package-subset`.
The npm receipt is explicitly schema 1, kind `npm-resolution`; it is never
represented as Cargo schema 3.

Aggregate `receipt.json` is schema 1, kind `fixed-lock-inventory-resolution`.
It binds original source archive/commit/repository, reviewed tool revision,
optional supplied dependency closure digest, inventory/check-manifest digests,
original and frozen candidate tree hashes, candidate archive hash, lock-set hash,
every graph receipt hash, exact requested/completed selector lists and failure.
Acceptance requires compatibility, every fixed selector and all integrity checks.
A resolver failure never produces an accepted aggregate; already retained locks
may remain for diagnosis. Never infer success from those partial files.

This receipt certifies execution of the committed selectors, not their adequacy.
Reviewers still compare the committed inventory/scripts to the original full
acceptance union, source/head/base, exact tools, report digests and actual Crow
status. A reduced committed inventory is a source change requiring review, not a
caller runtime knob.

Qualification must run the standard ccid build-worker tests and then a real
multi-graph owner workflow. The new unit tests use synthetic package-manager
subprocesses only to exercise file/integrity boundaries; they are not evidence
of actual Cargo/npm resolution or product acceptance.

## Tool dependency transition prerequisite

The tool's original cqlt dependency now follows its original Git URL's `main`
branch. The historical revision-based lock is retained until an explicit
`cqlt-main-lock` diagnostic creates a real candidate on Crow. That selector
requires `CQLT_SOURCE_ARCHIVE` and `CQLT_SOURCE_SHA256`: an exact source-bound
receipt plus complete original Git bundle, with only the genuinely observed
`refs/heads/main` and the historical commit in its ancestry. The worker validates
source/archive/commit/tree/ref hashes, imports into owned scratch, disables Git
credential/config inheritance, and rewrites only the original cqlt URL within
its Cargo process environment. It runs one Cargo update and locked metadata,
retaining both locks and compiler/source receipts. This is not a build or any
acceptance result. No new ref is invented and no lock is hand-edited.

After independent review, commit that exact worker lock; bind the resulting
source to the normal main-only dependency closure. Existing build/test workflows
now accept optional `DEPENDENCY_SOURCE_ARCHIVE`/`DEPENDENCY_SOURCE_SHA256` as a
pair. The verified-source bootstrap checks the immutable canonical consumer
digest and delegates through it to the unchanged standard build routine.
Existing workflow archive/commit checks and binary publication receipts remain.
The diagnostic transition is not a substitute for all standard tool tests or
real owner multi-graph qualification.
