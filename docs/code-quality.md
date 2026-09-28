# Code quality baseline

The baseline is a small set of checks with clear failure reasons. It is not a
single quality score: excellent prose cannot cancel an incomplete audit or a
failing test.

## Enforced in this repository

The bootstrap runs formatting, Clippy with warnings denied, and the existing
behavioral tests before publishing an immutable binary receipt. All targets
means Rust targets in the current native build; it does not claim coverage of
other operating systems. Experimental Windows execution remains unverified.

Cargo lints forbid unsafe code, deny ignored `must_use` results, and reject
`dbg!`, `todo!`, and `unimplemented!`. These apply to the library, executable,
and tests. Handle expected unsupported operations with explicit errors. Avoid
blanket lint suppression; any necessary exception should explain its invariant
at the narrowest useful scope.

## Review principles

- Keep policy pure and execution explicit. cqlt evaluates supplied evidence;
  ccid owns collection, process supervision, credentials, and deadlines.
- Preserve unknown states. Missing tools, incomplete inventories, malformed
  responses, and unavailable checks must never become a quality pass.
- Bound external work. Give subprocesses and network requests deadlines, byte
  limits, explicit inputs, and useful failure messages. Retain enough evidence
  to reproduce a decision without publishing private input.
- Make contracts precise. Validate untrusted inputs before side effects, use
  types for meaningful domain distinctions, and bind receipts to exact source,
  tool, ruleset, and configuration identities.
- Keep modules cohesive. Command execution, archive verification, and check
  builders have separate modules. Split by responsibility when navigation or
  changes become difficult; an arbitrary line count is not a correctness gate.
- Test behavior and failure boundaries. Protect cancellation, incomplete data,
  escaping archives, output limits, and deterministic replay. Do not add tests
  that only repeat implementation details or chase a coverage percentage.
- Keep dependencies purposeful. Prefer established formatters, compilers,
  linters, and prose tools over duplicating their analysis. Pin identities where
  reproducibility requires them; schedule dependency review separately.
- Write for the intended reader. Descriptions explain purpose and useful scope;
  factual and maturity claims need evidence. Vale findings and semantic review
  remain separate from build correctness.

Existing `ccid check` manifests can compose Cargo, JavaScript, Nix, and repository
commands. Add a repository-specific gate only when its failure is actionable.
Broad lint families, complexity thresholds, dependency-security gates, and
coverage targets need a pilot and a noise review before wider enforcement.

For the lint definitions, see the [Clippy lint catalogue](https://rust-lang.github.io/rust-clippy/stable/index.html).
