# The CI surface (`render_schema = 3`)

At `render_schema = 3` ccid owns a repository's whole CI surface. `.ci/ccid.toml` is the only CI file a
person edits; `ccid render` writes the rest and accounts for every other file under the CI roots, so a
hand-written workflow cannot quietly appear next to the rendered ones.

```toml
[render]
render_schema = 3      # channel-following like 2: no tool_revision

[jobs.verify]          # the verdict job: every push, every pull request, by hand
scheduler = "forgejo"
runs_on = "linux"      # linux (the default), windows or macos
checks = ["test"]
workflow = "ci"
command = ["ccid:run-declared-checks"]

[jobs.deep]            # any other job: by hand, unless it names its events
scheduler = "forgejo"
checks = ["deep"]
workflow = "deep"
command = ["ccid:run-declared-checks"]

[surface]              # only what the renderer cannot know
keep = { ".ci/publish.py" = "the packaging helper the release build calls" }
upstream = [".gitlab-ci.yml"]   # a fork's own CI files: never touched
```

Schemas 1 and 2 render byte for byte as before (`tests/it/render_golden.rs`); nothing in this document
applies to them.

## Jobs

- The verdict jobs (`[verdict] jobs`, else `verify`) run on every push, every pull request and by hand.
  Declaring `events` on one is refused: the per-commit verdict is not repository data.
- Every other job runs by hand unless it names its `events`. A workflow holds verdict jobs or other jobs,
  never both.
- Labels are `linux`, `windows` and `macos`; a job that names none runs on `linux`. Any other label is
  refused.
- Only Forgejo jobs: Crow and Argo jobs, `allow_crow`, `push_consumer` and remote jobs (`scheduler =
  "remote"`) are refused. What a remote job did becomes a Forgejo job on the `windows` or `macos` label, an
  evaluation of the aarch64 outputs on `linux` (plus a cross build where a build is truly needed), or goes.
- A schema-3 repository must declare a verdict job.

## Files

Every file under `.ci/`, `.circleci/`, `.crow/`, `.forgejo/`, `.github/` and `.gitlab/`, and the root files
`.gitlab-ci.yml` and `bitbucket-pipelines.yml`, has one class. In a git work tree the surface is what git sees:
tracked files and new files git does not ignore.

| Class | Meaning | `render` | `render --check` |
|---|---|---|---|
| rendered | carries the render marker (or is `jobs.json`) | writes it; deletes it when it is no longer rendered | drift is red |
| data | `.ci/ccid.toml`, `.ci/archives.toml`, `.ci/live-inputs.toml` | reads | must parse |
| script | named by the manifest (a check or job argv, `cache_inputs`, a job environment value), used by such a script, or listed in `[surface] keep` with a reason | leaves | - |
| community | issue and pull-request templates, CODEOWNERS, FUNDING, SECURITY, CONTRIBUTING, `.ci/README.md`, ... | leaves | - |
| upstream | matches `[surface] upstream` | never touches | - |
| legacy | a retired family (table in `src/render/surface.rs`) | see below | red |
| unknown | anything else | refuses the render | red |

The rendered files of a schema-3 repository include the [mirror checks](mirror-check.md):
`.github/workflows/pr-check.yml` (a pull request) and, in `.gitlab-ci.yml` next to the release mirror, the
pipeline rules and the `mr-check` job (a merge request). They replace the hand-written
`.github/workflows/ci.yml`, a legacy family that render now deletes (unless a declared check reads it).

A legacy family has a replacement. When the replacement is live:

- render deletes the file if nothing that stays uses it;
- it keeps the file (and says which file uses it) while the manifest, a script, a rendered file or any other
  file of the repository still uses it. Prose (Markdown and text files) mentions files and uses none.

When the replacement is not live yet, render refuses the file unless the manifest or a script it runs uses
it, so a repository with that family stays on `render_schema = 2` until the replacement lands. Each such
family names what it waits for. A hand-written remote-scheduler pipeline in `.gitlab-ci.yml` is replaced by
the rendered release mirror.

Render deletes or overwrites a hand-written file only when git can restore it: tracked and unmodified in the
work tree. An untracked or changed file is refused with that reason. A symlink is never followed, read or
deleted; one in a legacy or unknown path is refused. A refused render writes and deletes nothing.

"Uses" is found by text: a file uses another when it names the other's path (or a directory of it below a
surface root), or, in the same directory, its file name, as a whole word. The search reads tracked text
files up to 1 MiB; the manifest counts through its parsed strings only.

## The release families

The hand-written release files (`.forgejo/workflows/release.yml`, `release-macos.yml`,
`.github/workflows/release.yml`, `.ci/release-check.py`) are legacy files whose replacement is the rendered
release flow ([release.md](release.md)). They are live legacy files (render replaces or deletes them) in a
repository whose manifest declares `[release]`; in one that does not they are refused, so a repository keeps
its release until it has the new one.

## Report and plan

```sh
ccid surface report --repo .                 # every CI file: class, action, family, replacement, note
ccid surface plan --repo A --repo B ...      # the move to render_schema = 3, reads only
ccid surface plan --summary --repo ...       # only the totals
```

`report` classifies the files of a repository on any schema against the files its own schema renders.

`plan` applies the mechanical data edits to each manifest in memory and renders the result at schema 3
without writing: `render_schema = 3`; drop an own `tool_revision`/`tool_sha256` and `allow_crow`; dead
labels to `linux`; `events` dropped on verdict jobs and `events = ["manual"]` dropped elsewhere; a separate
pull-request job with the verdict's checks and command folded into the verdict job; a non-verdict job that
shares the verdict's workflow moved to its own; the native aarch64 job of a nix repository replaced by the
evaluation-only `mode = "eval"` check ([the pattern](mirror-check.md#the-aarch64-pattern-for-nix-repositories-evaluation-only)).
A remote, Crow or Argo job and `push_consumer` are decisions for a person; the plan applies the conservative
default (the job goes, or moves to Forgejo) and says so. For each repository the plan lists the edits, the decisions, the files render writes, deletes,
keeps and refuses, and whether it is ready; the totals count repositories per edit kind and files per family.
