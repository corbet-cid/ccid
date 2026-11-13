# Contributed projects

A **contributed project** is someone else's project that we contribute to. ccid adapts to it
instead of imposing itself (operating model R10, R11):

| Rule | What ccid does |
|---|---|
| The project's own CI and conventions apply. Nothing of ours is imposed. | `ccid render` refuses: it writes no file into the clone, and `--check` is refused too. |
| Nothing of our tooling enters their repository. | `lock-refresh`, every `release` command that builds, publishes or discards, and the plain `ccid check` and `ccid cached` (the result cache and the check-shell write beside the sources) refuse. Our own checks run from a manifest **outside** the clone and write nothing into it. |
| Their CI never runs on our compute. | `job-run` (a forge job, or a source export), `execute-job`, `push-run`, `verdict`, `crow-ci` and `ci-job` refuse. `ccid contributed check` also refuses when a forge's CI job is what runs it (`GITHUB_ACTIONS`, `GITEA_ACTIONS`, `FORGEJO_ACTIONS`, `GITLAB_CI`, `CIRCLECI`, `BITBUCKET_BUILD_NUMBER`). |
| Our tools work in their environment without assuming our forge, runners or caches. | `ccid contributed check` needs no result cache, remote cache, fleet data, tool root, forge credential or runner. None of ours is passed on to the project's commands. |
| We may use our own compute and caches for our own work. | The compile caches (`CCACHE_DIR`, the native caches) serve the run when the machine provides them. |
| Their releases and issues are not mirrored or relayed. | cfrg: no destination, no release upload, no relay (see the cfrg documentation). |

Every refusal exits with status 2 and says `<operation> refused: <name> is a contributed project`.

## The declaration

The class is declared **outside the project**, in the fleet's one placement document
(`lib/placement.json`), in a `contributed` list of its `native` view. ccid reads a copy and never
writes it:

```json
"contributed": [
  {"name": "seaweedfs",
   "upstream": "https://github.com/seaweedfs/seaweedfs",
   "forks": ["https://github.com/example-user/seaweedfs"]}
]
```

| Field | Meaning |
|---|---|
| `name` | A plain lowercase name, unique; the list is in strictly ascending order. |
| `upstream` | The project's own repository. |
| `forks` | Optional. Our copies of it on any forge, ours included. |

Every location is spelled `https://<host>/<owner>/<repo>` exactly: lowercase host, no user name,
port, query, trailing slash or `.git`; a location belongs to one project only, and owner and
repository match case-insensitively. A malformed entry is refused with the entry named, and so is a
list that is not in order. A location on our forge is known under every spelling of the forge: the host
of the placement's `source.origin`, the `forge_hosts` of the fleet data and this machine's
`CCID_FORGE_HOSTS` (an in-cluster name). (`default` names the default primary, which can be another
forge, and is not a spelling of ours.) Where a spelling is in none of those, list the location under it in
`forks`: a clone through `https://git.example/...` of a fork whose entry says `https://forge.example/...`
is only recognised when the machine knows both names.

`CCID_PLACEMENT_FILE` names the copy (the whole document or its native view; a symbolic link is
followed). Without the variable nothing is classified, so a runner that has no copy behaves as before.
A variable that is set while the file is missing, unreadable, empty, not a placement (no `repositories`
list) or invalid is an error, and so is a `CCID_FLEET_FILE` that is set but unusable: the guard never
switches itself off silently.

## How a project is recognised

* A working copy: any of its remotes, fetch and push, **as git resolves them** (`url.<base>.insteadOf` and
  `pushInsteadOf` applied) in any spelling git accepts (https, ssh, scp-like, with or without `.git`), or
  the job's own name for its repository (`CI_REPOSITORY_URL`, or `GITHUB_SERVER_URL` with
  `GITHUB_REPOSITORY`). A repository git refuses to read (a foreign owner, a corrupt config) is an error,
  never "no remotes". Not recognised: a remote that is a local path or `file://` URL, an ssh host alias
  (`git@work-github:...`), and a source export without a `.git` directory.
* A forge job: the forge's URL and `owner/name` (`job-run`), or `owner/name` on our forge (`verdict`).
* A URL: the consumer of `push-run`, the repository of a staged job.

A location that is merely a *dependency* of one of our own jobs (a fork of ours that a build fetches) is
not the job's repository, and its job is ours.

## Commands

| Command | Result |
|---|---|
| `ccid contributed show` | One JSON line: `digest`, `forge_hosts`, `projects`. Exit 2 without a placement copy. |
| `ccid contributed which [--repo DIR]` | One JSON line. Exit 0: a contributed project; 1: it is not; 2: cannot tell. |
| `ccid contributed check --repo DIR --manifest FILE --check NAME[,NAME] [--plan]` | Our own checks on their working copy, from the overlay manifest `FILE`. |

`contributed check` requires that the working copy is a declared contributed project (use `ccid check`
for an owned one) and that `FILE` lies outside it (outside the working copy's top level and, for a linked
worktree, outside the main checkout). The overlay is an ordinary ccid manifest; its checks should be the
project's own documented commands. Only `commands` and `cargo` checks run: `javascript` and `nix` checks
install and write into the working copy by what they are, so a selection that contains one is refused
whole. It has the same cancellation and parent supervision as `ccid check`. It prints a
`{"event":"contributed",...}` line, then the usual events. What runs is the direct route of `ccid check`
with a fixed environment:

* The result cache (`CCID_RESULT_CACHE`) and the moon cache (`CCID_REMOTE_CACHE`) are **not** used. A
  result is keyed by committed content including the manifest, and it is stored beside the sources; an
  overlay manifest is neither committed nor in their repository.
* These variables are removed before anything runs, so no project command sees them (a list, not a rule:
  the rest of the environment is the developer's own, as for any command run by hand):
  `CCID_RESULT_CACHE`, `CCID_REMOTE_CACHE`, `CCID_FORGE_TOKEN`, `CCID_FLEET_FILE`, `CCID_FLEET_DIGEST`,
  `CCID_FLEET_KEYED`, `CCID_FORGE_HOSTS`, `CCID_CHANNEL_FILE`, `CCID_CHANNEL_URL`, `CCID_TOOL_ROOT`,
  `CCID_TOOL_TRIPLE`, `CCID_PLACEMENT_FILE`, `CCID_STATUS_BINARY`, `CCID_STATUS_BINARY_SHA256`,
  `CCID_SUBMISSION_CONFIG`, `CI_REPOSITORY_URL`, every `CFRG_*`, and the forge and registry tokens
  `GITHUB_TOKEN`, `GH_TOKEN`, `GITEA_TOKEN`, `FORGEJO_TOKEN`, `GITLAB_TOKEN`, `BITBUCKET_TOKEN`,
  `CARGO_REGISTRY_TOKEN`, `CARGO_REGISTRIES_*_TOKEN`, `NPM_TOKEN` and `NODE_AUTH_TOKEN`.
* A failing command fails the check, and a missing tool is not a pass.

## What this does not cover

* A fork's own workflow files never call ccid, so ccid cannot see their CI start: what keeps it off our
  runners is the forge's Actions switch, which cfrg keeps off on contributed repositories of our forge
  (cfrg `docs/contributed.md`).
* A remote that git cannot resolve to a URL we know (see above) is not recognised, and an owned working
  copy that keeps a contributed project's URL as a remote is treated as that project.
* `execute-job` and `verdict` recognise a repository only when the request or `--repo` names it; the Crow
  commands `retry-workflow` and `receive` are not guarded (Crow is retired).

Nothing about the project, its overlay or its name is kept in this repository.
