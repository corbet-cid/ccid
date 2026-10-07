# One verdict per commit

Crow posts one commit status per workflow (`ci/crow/manual/repository-jobs`).
Every repository job shares that workflow, so a failed side job (release,
publish) overwrites the result of the checks that gate landing, and a landing
procedure that waits on the forge status cannot tell the verdict from the side
jobs. Generated adapters therefore report through `ccid verdict`, which posts
through the operator's status reporter (cfrg's status procedure; ccid never
talks to a forge itself):

| Context | Meaning | Gates landing |
|---|---|---|
| `ccid/verdict` | The commit's aggregated verdict. `success` only when every gating job has succeeded for this exact commit, `failure` as soon as one failed, `pending` otherwise | yes |
| `ccid/<job>` | The result of that job alone (`ccid/verify`, `ccid/release`, `ccid/publish`, ...) | no |

Crow's native `ci/crow/*` context stays as it is and must not be used as the
landing gate once a repository reports the verdict.

## Declaring the gating jobs

```toml
[verdict]
jobs = ["verify"]        # default when omitted: ["verify"] if that job exists
```

Names must be declared jobs and unique. A repository without a `verify` job and
without `[verdict]` gets an empty list: only `ccid/<job>` contexts are posted
and no verdict exists. `ccid render` bakes the list into the generated
adapter's status steps, so changing it needs `ccid render` and a commit like any
other manifest change.

## How it works

The generated `native-status-pending` and `native-status-complete` steps run
`"$CI_TOOL_BINARY" verdict --commit SHA --job JOB --gating A,B --state S ...`
(only when the operator configured `CCID_STATUS_CONFIG`; unconfigured adapters
behave as before). The command verifies the reporter (`CCID_STATUS_BINARY`
against `CCID_STATUS_BINARY_SHA256`), posts `ccid/<job>`, and, for a gating job,
records the job's state per repository and commit under
`$CFRG_STATUS_STATE_DIR/ccid-verdict/<owner>/<name>/<commit>.json` (locked,
pruned after 30 days) and posts `ccid/verdict` from all recorded gating states.
Jobs running in separate pipelines therefore contribute to one verdict; a rerun
reopens it (`pending`), a rerun that passes closes it again. Without a shared
state directory only a lone gating job can decide; with several the verdict stays
`pending` rather than guessing. A reporter failure fails the status step, so a
missing verdict is visible.

## Rolling it out

1. Provision the status reporter on the Crow agent (`CCID_STATUS_BINARY`,
   `CCID_STATUS_BINARY_SHA256`, `CCID_STATUS_CONFIG`, `CFRG_STATUS_STATE_DIR` on
   persistent storage) as declared infrastructure.
2. Move the repository's adapter pin to a ccid revision that contains
   `ccid verdict` and run `ccid render`.
3. Only then change the repository's land policy `contexts` from `ci/crow/*` to
   `ccid/verdict`. Doing it earlier would make landing wait for a context that
   nothing posts.

Release and publish jobs stay separate jobs with their own `ccid/<job>`
context; they never appear in the verdict.
