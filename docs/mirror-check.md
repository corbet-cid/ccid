# Mirror checks: the pull request and the merge request (`render_schema = 3`)

The primary forge is the only place a repository's CI runs on every commit. A copy of the repository on
GitHub or GitLab is a mirror: nothing of ours pushes there and starts a run. What a mirror platform does run
is a check of an *outsider's* pull request (GitHub) or merge request (GitLab), on that platform's own compute,
so a contribution is judged by the same declared checks without anyone being let into our infrastructure.

`ccid render` writes the check into every `render_schema = 3` repository that has a mirrored verdict job.
Nothing is declared for it besides the verdict job itself (`.ci/ccid.toml` stays the only hand-edited file).

| Platform | File | Starts on | Does not start on |
|---|---|---|---|
| GitHub | `.github/workflows/pr-check.yml` | `pull_request` | a push, a tag, a schedule, `pull_request_target`, a manual run |
| GitLab | `.gitlab-ci.yml`, job `mr-check` | a merge request pipeline | a pushed branch (`workflow: rules` let a pipeline exist only for a merge request, a `v*` tag, and the release mirror's own run) |

The same `.gitlab-ci.yml` carries the [release mirror](release-mirror.md) job; its rules are unchanged.
Schemas 1 and 2 render the release mirror alone, byte for byte as before.

## Which verdict jobs are mirrored

A verdict job (`[verdict] jobs`, else `verify`) is mirrored when **all** hold; render decides, at render time,
and a run never asks again. The check never goes red for infrastructure an outsider does not have, and it
never goes green for a check it did not run: a job that does not qualify is *not emitted at all*.

| Condition | Why |
|---|---|
| the command is `ccid:run-declared-checks` | any other command is the repository's own script, which the mirror cannot know to be self-contained (`.ci/run-cached.sh` is the common case until `[runtime]`) |
| the label is `linux` (the default) | the platforms' runners are Linux x86_64; there is no Windows, macOS or ARM check (B6) |
| the job names no `secrets` | a mirror holds none of ours |
| the job declares no `siblings` | whether a sibling repository is public cannot be known when rendering, and a private one cannot be read there |
| no `mirror = false` | the repository's own opt-out |
| the repository lists no `[surface] upstream` | a fork keeps its upstream's CI; its mirror CI stays off |

```toml
[jobs.verify]
scheduler = "forgejo"
checks = ["test"]
workflow = "ci"
command = ["ccid:run-declared-checks"]
mirror = false                       # optional: keep this verdict job on the forge
mirror_reason = "needs the office network"   # required with it; plain text, at most 120 characters
```

Every verdict job that stays on the forge is reported in the output of `ccid render` and `ccid render --check`
(and in the `forge_only` list of the JSON report): `verify runs on the forge only: command bash
.ci/run-cached.sh`. When the GitHub check exists, the same lines are written to the run's step summary.
`mirror` and `mirror_reason` exist at `render_schema = 3` only.

When no verdict job qualifies, `pr-check.yml` is not rendered (an owned one is deleted), and `.gitlab-ci.yml`
carries the pipeline rules and the release mirror but no `mr-check`.

## What a check does

Both platforms run one script (`src/render/mirror-check-core.sh`, identical on both) in the image
`nixos/nix:latest` (no digest pin: the ccid binary below is verified instead). On GitHub the pull request is
checked out on the runner with the job's read-only token and the script runs in the container
(`docker run`); on GitLab the platform's own checkout is the script's working directory.

1. Reads the release channel (`channel.json`) anonymously from the forge that serves ccid's repository (the
   `repository` of ccid's own package metadata, one redirect hop at most). It takes the release the channel
   names for the `linux` class, else its default, so the check runs the same ccid the forge's own jobs run.
2. Downloads that release's binary anonymously and checks its sha256 against the channel. A digest that
   differs ends the check; nothing runs.
3. The binary is linked against a Nix glibc. If the image does not have it, the script fetches exactly that
   store path from the public Nix cache (`nix-store --realise`).
4. For each mirrored verdict job, in name order, runs `ccid job-run --mirror --job <job>` and ends with the
   status of the last failing job, after running them all.

A forge or cache that cannot be reached fails the check (red), never passes it. The script needs only what
the image has: `sh`, `curl`, `git`, `grep`, `tr`, `cut`, `head`, `sha256sum`, `mktemp`, `nix-store`.

## `ccid job-run --mirror`

```sh
ccid job-run --mirror --job verify [--source <checkout>]   # the checkout defaults to the current directory
```

The platform has checked the commit out. `--mirror` takes no `--repository`, `--sha`, `--server-url` or
`--owner`, and no token: the commit is `HEAD`, and the repository's identity is the checkout's `origin` without
its credentials (`CI_REPOSITORY_URL`; none when there is no origin, which a `[runtime]` job refuses).

It runs the job's declared checks like the forge does, with these differences:

- It refuses, with exit 2 and the reason, a job that render would not mirror (the table above) or one that is
  not a verdict job. The script only passes the jobs render emitted, so this is a guard against use by hand.
- No result cache (`CCID_RESULT_CACHE`), no remote or compile cache (`CCID_REMOTE_CACHE`,
  `CCID_COMPILE_CACHE_CONFIG`, `RUSTC_WRAPPER`, `SCCACHE_*`, `KACHE_*`), no forge token, LFS store, flake
  archive target, tool root or channel location, and no job token of the platform: the job's command and every
  process it starts see none of them.
- Fleet data comes from the channel file the script saved (`CCID_CHANNEL_FILE`), so `[runtime]` and the
  first-party checks work. The persistent state of `[runtime]` is a directory of the container
  (`CI_CACHE_ROOT`), cold on every run.
- It does not run `render --check` on the checkout: the checkout's `origin` is the platform's, not the forge's,
  and the forge's own per-commit verify already holds every commit to its rendered files.

## The aarch64 pattern for nix repositories (evaluation only)

A nix repository used to ask a hosted ARM runner for a native `nix flake check` (a remote job). No runner
is ever ARM (B6); aarch64 is covered by evaluating every system without building, on `linux`:

```toml
[checks.native]            # the build, on x86_64
kind = "nix"
mode = "native"

[checks.eval]              # every system, evaluated: aarch64 included, nothing built
kind = "nix"
mode = "eval"              # nix flake check --all-systems --no-build

[jobs.verify]
scheduler = "forgejo"
checks = ["native", "eval"]
workflow = "ci"
command = ["ccid:run-declared-checks"]
```

The verify job runs on every commit and is mirrored on both platforms. `ccid surface plan` makes the edit:
for a remote job on an `arm64`/`aarch64` platform in a repository with `kind = "nix"` checks, it drops the remote
job, adds `[checks.eval]` when none has `mode = "eval"` and lists it in the first verdict job; it leaves a
repository without nix checks as a decision.

## Not covered here

- Switching a mirror's CI on (GitHub Actions, GitLab `builds_access_level`, `"ci": true` in the placement
  data) is the enrollment (platform events and the provider switches), after the copy carries the rendered files.
- 197 verdict jobs still start `.ci/run-cached.sh`; they are mirrored once `[runtime]` replaces it (job-runtime.md).
- A merge request from a fork runs in the fork's project, on the fork owner's minutes; we open no merge
  requests on GitLab ourselves.
