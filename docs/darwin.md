# ccid on macOS

A job of the label `macos` runs inside the macOS build clone, and a rendered job starts by finding a ccid on
the release channel. Until the channel carried a macOS binary, every macOS job had to be a hand-written
workflow. This is how the binary is built, tested and put on the channel, and what the runner must provide.

## The binary

`x86_64-apple-darwin` is built natively in the clone (the clone is x86_64). It is not cross-built and not
built anywhere else: the Command Line Tools and the macOS SDK exist only there.

```sh
.forgejo/workflows/release-darwin.yml   # dispatch it on the commit that is being released (label macos)
.ci/release-darwin.sh                   # what it runs: build, test, leave the binary in the draft darwin-<commit>
.ci/release-darwin-publish.sh COMMIT    # where the registry credential lives: publish it, delete the draft
```

The job holds the job token only. Publishing to the registry the tool channel reads takes a package-write
credential that never enters a job, so the job leaves `ccid-x86_64-apple-darwin` and its `.sha256` in the draft
release `darwin-<commit>` of this repository (the job token may write releases) and the publisher, on the host
that holds `CFRG_RELEASE_TOKEN`, reads the draft (the runner's read credential), checks the digest and runs
`cfrg release publish --name ccid-x86_64-apple-darwin --commit COMMIT`, which puts the file next to the Linux
binary of the same commit in the generic package registry. Registry files are immutable per commit.

`.ci/release-darwin.sh` is for bash 3.2 and the BSD userland (the clone has no flock, timeout, node or
git-lfs, and umask 077). It sets `CARGO_NET_GIT_FETCH_WITH_CLI=true`: cargo's own git client cannot verify the
forge's certificate chain on macOS (libgit2 with Secure Transport, error -9836), the `git` program can.

## On the channel

`channel.json` selects a release per runner label: `classes.macos = {revision, sha256}` with the commit and the
digest `release-darwin-publish.sh` prints. The shim of a rendered workflow does the rest: it takes the `macos`
class (else the default), downloads `$CCID_TOOL_URL/<revision>/ccid-<triple>` and checks the digest.

A rendered job on the clone needs these variables in the clone's runner environment (the `envs` of the macOS
runner template, next to `CI_CACHE_ROOT`), none of which is a secret:

| variable | value |
|---|---|
| `CCID_CHANNEL_URL` | the same file the Linux runners name |
| `CCID_TOOL_URL` | the same generic package registry the Linux runners name |
| `CCID_TOOL_ROOT` | a directory on the clone's persistent cache disk |
| `CCID_TOOL_TRIPLE` | `x86_64-apple-darwin` (the shim's default is the Linux triple) |
| `CCID_SOURCE_ROOT` | the stable source path on the cache disk (it is part of every compile-cache key) |

## What is tested on macOS

`.ci/release-darwin.sh` runs the test suite in the clone and refuses a binary whose suite fails, except the tests
named in `.ci/darwin-skipped-tests.txt`, each group with its reason: what asserts Linux paths (`/proc`, the
owner-only scratch root layout, `/var` against `/private/var`), what needs a GNU `sha256sum` or git-lfs, and a few
that failed on the first run and are not investigated yet. A new failing test is a red leg. First run (2026-11-13,
commit `579b52e`): the build took 2 m 42 s warm; 650 of 659 library tests and 201 of 218 integration tests passed;
every `release::` test passed, so the release flow (curl transport, drafts, legs, promotion) runs on macOS.
