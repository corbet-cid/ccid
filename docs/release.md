# The release flow (`render_schema = 3`, `[release]`)

A repository that declares `[release]` gets one workflow, `.forgejo/workflows/release.yml`, and the
command `ccid release`. The flow is the same for every repository: the version comes from the
repository's own version file, the platform set follows from the version, one draft collects every
file, and a green run publishes it. There are no knobs besides the table below.

```toml
[release]
kind = "binary"                      # binary | library
version = "Cargo.toml"               # Cargo.toml, package.json or pyproject.toml (a path inside the repository)
platforms = ["linux-x86_64", "linux-aarch64", "macos-x86_64", "macos-arm64", "windows-x86_64", "windows-arm64"]
build = ["release-build"]            # checks the release job runs; they leave the release's files in $CCID_RELEASE_DIR
legs = { windows = "release-windows", macos = "release-macos" }   # the check each leg runs on its label
seal = ["release-seal"]              # checks promotion runs on the complete draft to derive the manifests
manifests = ["manifest.json"]        # the only files promotion may replace (besides SHA256SUMS)
registries = ["crates"]              # crates | npm | pypi | aur
notes = "CHANGELOG.md"               # optional: the text of the release (default: one generated line)
```

`seal` and `manifests` go together (or neither). A library (`kind = "library"`) has no `platforms`, no
`legs`; its release is the registry packages the build checks leave in `$CCID_RELEASE_DIR`.

The jobs are not declared: `[jobs.release]`, `release-windows`, `release-macos`, `release-promote` and
the workflow name `release` belong to the table (a declared job with one of those names is refused).

## What a version releases

| version | class | platforms |
|---|---|---|
| `X.Y.Z`, `Z > 0` | patch | `linux-x86_64` alone |
| `X.Y.0`, `Y > 0` | minor | every platform in `platforms` |
| `X.0.0` | major | every platform in `platforms` |

A suffix (`-rc.1`, `+build`), a leading zero, two numbers and `0.0.0` name no release and are refused.
`ccid release version` prints `0.1.0 minor`; `ccid release platforms` prints the set.

## The run

```text
dispatch release on main
  release (linux)          preflight, third-party lock refresh, the build checks, ONE draft build-<commit>
  release-windows (windows) only if the version holds a Windows platform
  release-macos (macos)     only if it holds a macOS platform; after the Windows leg when both exist
  release-promote (linux)   after every leg that ran: seal, SHA256SUMS, the draft becomes v<version>
```

The jobs are ordered by `needs`; the release job tells the legs whether they run (its outputs `legs` and
`released`, written to `$GITHUB_OUTPUT`). A failed leg stops the promotion. Nothing polls and nothing is
dispatched by a finishing step except the lock refresh below. A newer dispatch waits for the run that
holds the draft (`cancel-in-progress: false`).

**`ccid release build`** (the release job)

1. Preflight: the source is the tip of `main`; neither a release nor a tag `v<version>` exists (a version
   is released once); the commit's `ccid/verdict` status is not red (an absent one is noted, not refused).
2. The third-party lock refresh (below).
3. The `build` checks run (`ccid check`) with the credentials removed from their environment; they leave the
   release's files in `$CCID_RELEASE_DIR`: one flat directory, plain file names, no `SHA256SUMS`.
4. The draft `build-<commit>` is created (target: the commit) and every file is uploaded, checked by size on
   the release itself. The forge keeps the part of an interrupted upload, so a wrong-size asset of the draft
   is removed and uploaded again (four attempts, each followed by a look at the draft); an asset of the right
   size is never touched. A rerun for the same commit resumes: every asset the draft already holds must have
   exactly this run's bytes, else the run stops and names the way out (`ccid release discard`).

**A leg** (`ccid release leg <os>`) runs its declared check on its label with the credentials, the draft of
the commit must exist and be a draft. The check fetches what it needs and leaves its evidence in the draft:

```sh
"$CCID_BIN" release fetch ccvl-windows-x86_64.exe --into "$CCID_RELEASE_DIR"   # the read credential reads a draft
# ... test it, write the receipt ...
"$CCID_BIN" release put "$CCID_RELEASE_DIR/windows.receipt"                     # add, replace or leave identical
```

`put` changes a DRAFT only: a new file is uploaded, a file with other bytes is removed and uploaded again, a
file with the same bytes is left alone. A public release is final.

**`ccid release promote`** (the promotion)

1. The draft of the commit is downloaded (the runner's read credential; the job token cannot read a draft).
2. The `seal` checks run on that directory. They derive the declared `manifests` from the evidence and
   refuse (exit non-zero) a release whose evidence is incomplete. They may change nothing else: a changed,
   removed or added file that is not a declared manifest stops the promotion.
3. The manifests that differ from the draft's are replaced in the draft, then `SHA256SUMS` (coreutils
   format, byte order, over every other asset) is written and replaced if it differs. These are the only
   replacements promotion makes, and only while the release is a draft.
4. The draft must hold exactly the sealed files; the version must still be free and the source must still
   be main. The draft is renamed to `v<version>` (name `<version>`), which creates the tag on the commit,
   and the public release is checked.

A repeat of the promotion finds the public release and verifies its `SHA256SUMS`; it releases nothing again.

## The environment of a check

| variable | meaning |
|---|---|
| `CCID_RELEASE_DIR` | the directory the check works in (build: the files of the release; leg: scratch; seal: the downloaded draft) |
| `CCID_RELEASE_VERSION`, `CCID_RELEASE_CLASS`, `CCID_RELEASE_KIND`, `CCID_RELEASE_COMMIT` | what is released |
| `CCID_RELEASE_PLATFORMS`, `CCID_RELEASE_LEGS` | the platform set and the legs of this version, space separated |
| `CCID_RELEASE_MANIFESTS` | the declared manifests, space separated |
| `CCID_BIN` | this ccid, for `release fetch` and `release put` |

The forge is `GITHUB_SERVER_URL` and `GITHUB_REPOSITORY` (the runner sets both) with the job token
(`CCID_FORGE_TOKEN`). The read credential is the file named by `CCID_FORGE_READ_TOKEN_FILE`, else
`$CREDENTIALS_DIRECTORY/forgejo-ci-read`. Build and seal checks do not see any of these credentials.

## Libraries: the `pack` check

A library's release is its registry packages. They are made ONCE, by the release run, before anything is public
(R23); the registries, the forge release and every copy then carry the same files (R8, R24, U6). A repository declares
one check of `kind = "pack"` and names it in `[release] build`:

```toml
[checks.pack]
kind = "pack"
crates = "."                        # the directory of the published crate's Cargo.toml
npm = "js/@corbet-labs/cgreet"      # the directory of its package.json
pypi = "py"                         # the directory of its pyproject.toml

[release]
kind = "library"
version = "Cargo.toml"
registries = ["crates", "npm", "pypi"]
build = ["pack"]
```

| key | file left in `$CCID_RELEASE_DIR` | how |
|---|---|---|
| `crates` | `<name>-<version>.crate` | `cargo fetch --locked`, then `cargo package --locked --offline` from the clean checkout (never `--allow-dirty`) |
| `npm` | `<name>-<version>.tgz` (`@scope/name` as `scope-name-<version>.tgz`) | the commit's files (`git archive`), `bun install --frozen-lockfile` or `npm ci`, then `npm pack` (so `prepack` builds) |
| `pypi` | `<norm>-<version>.tar.gz` and `<norm>-<version>-py3-none-any.whl` | the same files, `uv build --no-config --build-constraints build-constraints.txt --require-build-hashes` |

The rules, all refused when the manifest is read (`render --check`, `release version`, `preflight`) or before any
tool runs:

- The keys name exactly the registries of `registries` without `aur`: a registry with no key is "published, never
  packed", a key with no registry is "packed, never published". `jsr` is not a registry (R46). A native wheel is a
  binary with platforms (R21), not a library pack: any file other than the two above is refused by name.
- The pack check belongs to `build` and is the only one. It is never a check of a job: a commit's verify does not pack
  (A8), and a pack is never cached, because the release run makes its files from the commit.
- Every package carries the release version (`Cargo.toml` `[package].version`, `package.json` `version`,
  `pyproject.toml` `[project].version`); a workspace-inherited or `dynamic` version cannot be released.
- The Python build backend is pinned by hashes in `<pypi>/build-constraints.txt`, a lock file (`uv pip compile
  --universal --generate-hashes`) refreshed with the other locks (R48), so no build input is resolved at release time.
  `SOURCE_DATE_EPOCH` is the committer time of the commit: packing the same commit twice gives the same bytes.
- A pack runs with the release build's environment, which has no credentials; the only network use is the fetch of what
  a lock names (the crates, the npm packages, the pinned build backend).
- bun, uv and npm work from caches of the pack's own (empty at the start, gone with its scratch directory), never from
  a cache the runner shares with other jobs. uv fetches no Python interpreter (`UV_PYTHON_DOWNLOADS=never`) and uses none
  of its own managed ones (`UV_PYTHON_PREFERENCE=only-system`): the Python is one the runner provides. The cargo home
  and the tools' configuration files (`~/.npmrc`, a global bunfig) are not made private: they stay the runner's own.

A pack that fails stops the release run before the draft exists, so nothing becomes public. The publisher on the
datacenter side downloads the public release `v<version>` and uploads exactly these files, after checking them against
its `SHA256SUMS`.

## The third-party lock refresh

Every release run starts with `ccid lock-refresh --third-party` (when the repository tracks a `Cargo.lock`).
A lock that moves must be part of the commit that is released, so the run does not release: it commits the
moved locks as one lock-only commit (trailer `CCID-Lock-Refresh: third-party`), pushes it to `main` with the
job token (to the forge itself, not to a local copy a runner may route to), dispatches `release.yml` on
`main` and ends green with `released=false`. The new run does the release; its own per-commit verify is the
proof of the lock commit. Two refreshes in a row are refused (the lock would move on every run), and so is a
refresh with no git identity (`GIT_AUTHOR_NAME` and `GIT_AUTHOR_EMAIL`, or `user.name` and `user.email`) or a
push the forge refuses (a protected `main`): the message says to commit the lock through the repository's
flow and release again.

## JSR

`jsr` is not a registry of the table. JSR accepts a publish only from a GitHub workflow (OIDC), which would be CI on
a mirror forge (operating model R6, R7); R46 has such a registry published from our side with a key that needs no
renewal, or not at all. Nothing publishes to JSR until that key exists, so `registries = ["jsr"]` is refused. The
workflow `.github/workflows/jsr-publish.yml` that earlier releases rendered is retired
([mirror-forges.md](mirror-forges.md)).

## Copies on other forges

The release is built and published once, on the primary forge. Its files reach every copy by an upload made from
the primary's side (R8, R45); no job runs on a copy and ccid renders nothing for one ([mirror-forges.md](mirror-forges.md)).

## Commands

```
ccid release version | platforms [--repo .]
ccid release preflight [--repo .] [--commit SHA]
ccid release build | promote [--repo .]            (the jobs; started by ccid job-run)
ccid release leg windows|macos [--repo .]          (the jobs; started by ccid job-run)
ccid release put FILE... | fetch [NAME...] --into DIR | discard | sums DIR
```

`discard` deletes the draft of the commit (never a public release); use it when a failed run left a draft
whose bytes the next run will not match (a receipt that carries the run's identity).

## The macOS leg needs a macOS ccid

A job of the label `macos` starts by finding a ccid on the release channel. How the macOS binary is built, tested and
put on the channel, and what the clone's runner environment must provide: [darwin.md](darwin.md).

## Moving a repository onto it

`ccid surface plan` shows the files the table replaces. With `[release]` declared, the hand-written
`.forgejo/workflows/release.yml`, `release-macos.yml`, `.github/workflows/release.yml` and
`.ci/release-check.py` are legacy files that render replaces or deletes (without the table they are refused,
so a repository never loses its release before it has a new one). Check that `registries` names every
registry the old workflows published to. The GitHub release workflow of a JSR library published both Cargo and
JSR; JSR is now left out until it can be published from our side (see JSR above).
A library's own packing code (a `release-check.py`, a `publish.py`, the packing steps of a GitHub release workflow)
goes with them: `kind = "pack"` makes the files, the publisher uploads them.

At `render_schema = 3` the notes end with a "Security and dependencies" section ([security.md](security.md)); a fork gets none.
