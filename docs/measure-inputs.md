# Measured check inputs (`ccid measure-inputs`)

A check whose key covers the whole tree reruns for every commit: a change to a README, a licence or
a CI file reruns the test battery. `cache_inputs` narrows a check's key to the files it depends
on. A guessed list is a false skip waiting to happen (a test that asserts a directory exists, a
build script probing for an optional file, a linter that looks for its configuration), and a false
skip is worse than a slow run. So the list is measured:

```
ccid measure-inputs --check verify [--repo .] [--depth 1] [--strace strace] [--no-verify]
```

## Method

1. **Trace.** The check runs uncached (`ccid check`, without `CCID_RESULT_CACHE`) under
   `strace -ff -y -v`, with compile-cache wrappers removed from the environment (a cache hit would
   hide the compiler's reads). The check must pass on the full tree.
2. **Only the check's processes.** A process counts from the `execve` whose environment carries
   `CCID_TARGET_LOCK_HELD` (set by `ccid check` for every command it runs), and so do its
   descendants. ccid's own work around the check (keying, staging the tree, the float) is not an
   input of the check. Paths in the staged copy `ccid check` may run in
   (`$CCID_TARGET_LOCK_HELD/.ccid/source-v1`) map back to the checkout.
3. **Observations.** Every path inside the checkout that such a process opened, stat'ed, listed,
   executed, read as a link, changed into, or probed and did not find. `-y` names each descriptor's
   path, including the working directory of a relative call, so relative paths resolve exactly.
   Tool state (`.git`, `.ccid`, `.moon`) and the check's declared `cache_outputs` are not inputs.
4. **Generalisation** (`--depth 1`): each observation becomes the top-level entry it lies under. A
   top-level file is itself, anything below a directory is `<dir>/**` (so a new file in a directory
   the check already reads is covered), and a top-level name looked for and not found is both
   `<name>` and `<name>/**`. A check that lists the repository root depends on every top-level entry:
   then there is no narrowing. `.ci/**` is always included.
5. **Verification.** The check runs again in a copy of the checkout that holds only the files the
   globs select (git's own pathspec matcher, the one the key uses). A failure there means the trace
   missed something (for example a read by a daemon that was already running) and the result is
   refused (exit 1).

The command prints one JSON line with the `cache_inputs` line to put on the check; it writes
nothing to the repository. The key mechanism adds every lock and ecosystem manifest to a narrowed
declaration on its own.

## Limits

- `cache_inputs` narrows the moon result cache. The shared verdict store still keys the whole tree,
  so a narrowed check skips through the moon cache, not through a verdict.
- The measurement holds for the tree it was taken on. A change that makes the check read a new
  top-level entry is covered only if that entry was already looked for (cargo, for example, probes
  for `build.rs`, `benches` and `examples`). Measure again after a structural change.
- A `kind = "nix"` check keys every file anyway.
- strace slows the check down considerably: measure on a workstation, not in a job.
