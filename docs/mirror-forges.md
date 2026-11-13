# Mirror forges run no CI

A repository lives on one primary forge. Its copies on other forges (GitHub, GitLab, Bitbucket) are mirrors.
The operating model (the ccid wiki page `Operating-model`) binds ccid to three rules about them:

| ID | Rule |
|---|---|
| R6 | No CI runs on any mirror forge, whoever triggers it. CI is switched off on every copy. An outsider checks a change locally with the project's own documented command; it reaches our CI only after we took it into the primary. |
| R7 | None of our actions (pushes, tags, mirroring, releases, automation) may start a mirror CI run. |
| R46 | A registry that a forge offers only through its CI (JSR, through GitHub's OIDC) is published from our side with a registry key that needs no renewal, or not at all. |

So `ccid render` writes no file for the CI of a mirror forge, for any class and any `render_schema`. A release
reaches a copy by an upload made from the primary's side (R45), not by a job on the copy.

## The retired files

Earlier releases rendered these files. They are retired:

| File | What it was |
|---|---|
| `.github/workflows/release-mirror.yml` | the GitHub release mirror, started by a pushed `v*` tag |
| `.gitlab-ci.yml` (a file with the render marker) | the GitLab release mirror and, at `render_schema = 3`, the pipeline rules and the merge-request check |
| `.github/workflows/pr-check.yml` | the GitHub pull-request check (`render_schema = 3`) |
| `.github/workflows/jsr-publish.yml` | the GitHub publisher for JSR, started by a pushed `v*` tag |
| `bitbucket-pipelines.yml` (a file with the render marker) | the Bitbucket release mirror (never rendered in practice: Downloads need a paid plan) |

`ccid render` removes each of them from a repository, at every `render_schema`. A repository gets rid of them with
one normal render and its commit. What `ccid render --check` says depends on the schema:

| `render_schema` | `render --check` with a retired file present |
|---|---|
| 1 and 2 | accepts it as a declared transition: a notice (`<file> is retired (a declared transition ...); the next render removes it`) and the file in the report's `transitional` list. A channel bump therefore re-stales no repository that still carries the files. |
| 3 | red: `<file> (retired, render removes it: nothing: ...)` |

The tolerance at schemas 1 and 2 is temporary. It is dropped in a later release, once the fleet has been swept and
no repository carries the files any more; from then on a check at every schema names them as drift.

- Only a file that carries the render marker is ours. A hand-written file at one of these paths is left alone at
  `render_schema` 1 and 2; at `render_schema = 3` it is an unknown file ([surface.md](surface.md)) and the person
  decides. A hand-written `bitbucket-pipelines.yml` is a legacy file there, as before.
- A symlink is never followed or removed.
- At `render_schema = 3` the removal follows the surface rules: git must be able to restore the file (tracked and
  unmodified), otherwise the render refuses and writes nothing.

## What goes with them

- `ccid job-run --mirror`, the entry point of the pull-request and merge-request check, and the manifest keys
  `mirror` and `mirror_reason` of a verdict job are gone. A manifest that still names the keys is refused.
- `registries = ["jsr"]` in `[release]` is refused: nothing publishes to JSR until it can be done from our side
  (R46). The other registries are unchanged ([release.md](release.md)).
- The report of `ccid render` no longer has a `forge_only` list.
- Remote jobs (`scheduler = "remote"`) are retired the same way: nothing dispatches a provider workflow on a copy any
  more. At schemas 1 and 2 `render` does not record them in `.ci/jobs.json` and `render --check` accepts the declaration
  and an inventory that still records them (a notice, and `.ci/jobs.json` in `transitional`), until a later release drops
  the tolerance; `render_schema = 3` refuses them ([surface.md](surface.md), [rendering.md](rendering.md)).
