# Upstream sync

A downstream repository follows its upstream. Every time the upstream's `ci` run on its
default branch is green, the downstream merges that exact commit, checks the merge and
pushes it, and mirrors the upstream's release tags and releases. Nothing polls: the forge
tells the receiver (a system webhook on the upstream's successful runs), the receiver
dispatches the downstream's `upstream-sync` workflow once per declaring repository, and the
run does the rest.

`ccid render` writes the workflow into every repository that carries a
`ccvl-downstream.json` with an `upstream.url`:

```json
{ "schema_version": 1, "upstream": { "remote": "upstream", "branch": "main", "url": "https://forge.example/team/up.git" } }
```

Nothing else is declared. The repository's manifest only needs the job that checks the
merge, named `downstream-check` (a Forgejo job); a declaration without that job is refused
by `render`, because an unchecked merge must never be pushed.

## The workflow

`.forgejo/workflows/upstream-sync.yml` (dispatch only, input `upstream_commit`, one run at a
time per repository, the job runs on the label of `downstream-check`):

1. **Merge, locally.** The upstream commit must have a successful `ci.yml` run (read from
   the Actions API), be on the upstream branch's history, and not be part of the downstream
   branch yet; a repeated or superseded event finds it contained and changes nothing. The
   upstream is read from the forge that runs the job (the declared URL only identifies it),
   anonymously first because the job token reaches only its own repository, and a fetch that
   the runner's route to a local copy cannot serve is asked of the forge itself. The merge is
   made in a work tree of the runner (stable per repository when the runner names
   `CCID_SOURCE_ROOT`) with the identity of the branch tip. **A conflict ends the run red and
   pushes nothing**; the conflicting paths are in the log. Then git alone (no program of the
   merged tree, which could vouch for itself) compares the merge with the upstream commit:
   every path outside the branch's own `allowed_paths` must be identical, or the run is red.
   A changed, added, deleted or mode-changed file (a tampered `.agent/` file, say) is named and refused; a file moved
   out of the upstream's paths into the downstream's own is refused too (the diff is taken without rename detection,
   which would list only the owned destination).
2. **Check.** The checks run on a shallow, detached, single-commit export of the merge, the
   layout of every other job (Git LFS files materialised from the shared store, the upstream
   and the downstream, in that order). The central `ccid job-run --local` runs the declared
   `downstream-check` job there (the land gate's entry point), so ccid's result ledger and
   the compile caches are reused: only what the downstream's own tree changes runs again. A
   red check ends the run red.
3. **Push.** Only when both were clean: the merge is pushed to the branch the run was
   dispatched on with the job's own token, a plain push (a branch that moved meanwhile
   refuses it, the run is red, the next event merges again). Then:
   * every upstream tag shaped `v<major>.<minor>.<patch>` whose commit the downstream now
     contains is put on **the first commit of the downstream branch that contains it**
     (binary search along the first-parent chain), never moved, never replaced. A tag the
     merge does not contain yet waits for the sync that merges it.
   * for each such tag with a published upstream release and no downstream release, a
     release of the same tag is created whose notes name the upstream release and list its
     assets. **No build and no asset copy**: the program is the upstream's, so there is
     nothing to build or upload. A draft upstream release waits.

The job token goes only to the forge that runs the job and reaches only the repository the
job runs for; the upstream's API and git remote are read anonymously first.

## Receiver contract

The receiver (operator infrastructure, not part of ccid) dispatches
`POST /repos/{owner}/{repo}/actions/workflows/upstream-sync.yml/dispatches` with
`{"ref": "<branch>", "inputs": {"upstream_commit": "<40 hex>"}}` for every repository whose
`ccvl-downstream.json` names the event's repository as `upstream.url`.

## Rollout

The file is *introduced*: `render --check` accepts its absence (transitional); `render`
writes it. Its first line differs from the job adapters' marker on purpose, so a ccid that
does not know the file does not take it for an obsolete adapter and a channel bump that
precedes the render cannot turn a repository red. The downstream must list the file among
its own paths in `ccvl-downstream.json` (`allowed_paths`), since the upstream does not
carry it.

## Failure modes

| Situation | Result |
|---|---|
| Upstream commit without a green `ci` run | run red, nothing merged |
| Merge conflict | run red, nothing pushed, paths in the log |
| Check red | run red, nothing pushed |
| Branch moved before the push | push refused, run red; the next event merges again |
| Event lost (receiver down) | repaired by the next green upstream commit or a manual dispatch |
| Tag or release not there yet | skipped; the next sync carries it |
| Token cannot push to the branch | the push is refused and the run says so |

Merge commits are made by the job and are not signed.
