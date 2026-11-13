# Generated job adapters

Repositories declare checks and jobs in `.ci/ccid.toml`. Add an immutable tool
pin to generate the manual execution adapter used by the existing staged-job
dispatcher:

```toml
[render]
tool_revision = "1234567890abcdef1234567890abcdef12345678" # replace with a verified ccid commit

[jobs.verify]
checks = ["test"]
workflow = "repository-jobs"
command = ["bash", ".ci/run.sh"]
```

```sh
ccid render --repo .
ccid render --repo . --check
```

`scheduler` defaults to `crow`; set `scheduler = "argo"` on an individual job
to select Argo. The generated `.ci/jobs.json` inventory retains each validated
job's scheduler, command, exact check selection, tool pin and manifest digest.
It contains no credentials, machine paths, timestamps or mutable branch pins.
The committed manifest remains authoritative.

The renderer emits one `.crow/<workflow>.yaml` per distinct workflow name.
Jobs may share a workflow. Each adapter accepts the staged `CCID_JOB_REQUEST`,
verifies the supplied tool's binary digest and source revision, and invokes
`ccid execute-job` with Crow's expected source commit. The request carries the
source archive and Git bundle with their hashes. `execute-job` validates those
inputs and selects the declared command and checks from the verified source.

The Crow adapter requires the existing local worker contract: provisioned
shell, Git, checksum tools, accessible staged files and the verified ccid
binary. The dispatcher must supply the request and tool variables. Generation
does not build tools, stage sources, submit jobs or provision a service.
Argo continues to use the installed shared job WorkflowTemplate through its
existing dispatcher; the repository inventory does not create a Kubernetes
template. Native forge push/PR dispatch and result reporting are separate
integration work and are not supplied by these manual adapters.

## Secret environment

A job that needs a credential (for example a release token) declares it in two
places. `[render].secret_environment` maps an environment variable to the name of
a Crow repository secret; the renderer adds one `from_secret` reference per entry
to the `repository-job` step, never to a status step, and never a value. A job
lists the variables it may see in its own `secrets`:

```toml
[render]
tool_revision = "1234567890abcdef1234567890abcdef12345678"
secret_environment = { PUBLISH_TOKEN = "publish_token" }

[jobs.release]
checks = ["publish"]
workflow = "repository-jobs"
command = ["bash", ".ci/run.sh"]
secrets = ["PUBLISH_TOKEN"]
```

`execute-job` strips every declared variable a job does not list, so `verify`
and every other job on the same workflow never see the value. Names are plain
uppercase identifiers that cannot shadow scheduler, reporter or resolver
variables (`CI_*`, `CCID_*`, `CFRG_STATUS_*`, `CFRG_RESOLVER_*` and the reserved
job keys), a job's own `environment` may not set a declared variable, and a job
with `secrets` must run on Crow. The inventory records each job's `secrets`. The
Crow secret itself, restricted to the manual event, is provisioned by the
operator; ccid never distributes tokens.

## The one check-shell

A repository opts in with an empty table:

```toml
[check_shell]
```

Rendering then writes `.ci/check-shell.nix`, one generated file with one native
C/C++ toolchain family (nix gcc, with unwrapped clang for wasm32 targets and
libclang for bindgen), one nixpkgs pin and the native libraries the build
scripts discover through pkg-config. `ccid check`, `ccid cached` and the
declared command of `ccid job-run` run inside exactly that file whenever it
carries the render marker; a lane that evaluates the same file builds with the
same toolchain and libraries. A hand-written `.ci/check-shell.nix` is foreign
and is never entered or overwritten; a repository that removes `[check_shell]`
loses only the renderer-owned file. The generated file's content is the
release's template, so the pin and the library set cannot drift between
repositories.

Everything linked inside the shell carries a RUNPATH to each of the shell's
runtime library directories, so it starts without `LD_LIBRARY_PATH`. The
same directory list also reaches Cargo as host-target rustflags
(`-C link-arg=-Wl,-rpath-link,<dir>`), because Cargo's freshness check never
reads the linker environment: a changed shell therefore relinks an existing
target directory instead of keeping binaries linked against the previous
shell. Cargo puts rustflags into artifact file names, so the previous shell's
artifacts stay in a persistent target until something prunes them, as after
a toolchain change. A job that exports `RUSTFLAGS` replaces target rustflags
(Cargo's precedence) and loses this; append to
`CARGO_TARGET_<TRIPLE>_RUSTFLAGS` instead. `.ci/check-shell-probe.sh` holds
the contract for a fresh and for a persistent target.

`--check` fails on missing, changed or obsolete generated files without writing
anything. Normal rendering updates only files marked as renderer-owned and
removes obsolete generated Crow adapters after a workflow rename. Foreign
files and symlink destinations are refused before any changes. Migrate an
existing hand-maintained adapter by reviewing its differences and choosing a
new workflow name; the renderer does not silently take ownership of it.
Native platform and release workflows stay separately maintained.

Commit the manifest, inventory and generated adapters together. Rendered files
are deterministic for the manifest and renderer version; the tool pin should
refer to a worker-validated build that supports this adapter contract.

### Declared template transitions

A changed template (`src/render/*`) changes the committed files of every
repository that renders it, and `job-run` checks them byte for byte, so a
channel bump would otherwise fail every such repository until each one is
re-rendered. A template change therefore declares a transition in
`render.rs` (`TRANSITIONS`): the template, the release whose renders carry the
previous body, and that body, kept under `src/render/previous/`. `--check`
accepts a file this release renders with a declared previous body, lists it
under `transitional` in its report and says so on stderr; `render` writes
the current body, so a repository moves at its next normal render and a bump
re-stales nothing. Any other difference still fails.

The window is bounded by generation, not by date: one previous body per
template (the one the channel's release renders) and at most four
transitions. A later change of the same template replaces the previous body
with the then-released one, so a render two changes behind drifts. Remove a
transition once the fleet has re-rendered.

## The whole CI surface (`render_schema = 3`)

At `render_schema = 3` ccid owns every CI file of the repository: verdict jobs run per commit, other jobs by
hand, labels are `linux`, `windows` or `macos`, retired CI files go once their replacement is live, and
unknown ones are refused. `ccid surface report` and `ccid surface plan` show what a render does with each
file. See [surface.md](surface.md). Schemas 1 and 2 render byte for byte as before.

## The release flow

A manifest with a `[release]` table (`render_schema = 3`) gets `.forgejo/workflows/release.yml`: the release
job, the platform legs and the promotion in one run, ordered by `needs`. The jobs are planned from the table,
never declared. See [release.md](release.md).

## Forgejo Actions

A job with `scheduler = "forgejo"` renders to a native Forgejo Actions workflow,
`.forgejo/workflows/<workflow>.yml`, instead of a Crow adapter. Jobs that share a
workflow name share one file; each job becomes a job of that workflow.

```toml
[jobs.verify]
scheduler = "forgejo"
runs_on = "linux"                       # runner label; default "light" (schema 3: linux, windows or macos; default linux)
checks = ["test"]
workflow = "ci"
command = ["ccid:run-declared-checks"]  # the only command a Forgejo job takes
paths = ["src/**", "Cargo.toml"]        # optional push path filter (all jobs of the workflow)
events = ["push", "pull_request"]       # optional; default push, pull_request and manual
push_branches = ["main"]                # optional; default main
siblings = { cdate = "main", cink = "<40-hex commit>" }  # optional; see below
```

The workflow runs on push, pull request and manual dispatch, cancels a
superseded run of the same ref, and fetches the exact commit with the job token.
It then runs `ccid check` from the pinned `tool_revision`. A manifest with
`[runtime]` gets the persistent job environment around that `ccid check`: state
root and lock, tools from the check-shell's nixpkgs pin, the toolchain lock (see
[job-runtime.md](job-runtime.md)); no job script is needed. The CI speed profile
is part of the workflow: no incremental state and no debug information.

The runner, not the repository, owns every operator fact: `CCID_TOOL_ROOT` names
the directory that holds the pinned tools as
`<root>/ccid/<revision>/<triple>/ccid` (`CCID_TOOL_TRIPLE` overrides the host
triple), and cache endpoints arrive through the runner environment. A missing or
mismatched tool fails the job; a check never installs a tool. The shim of a central (`render_schema` 2 and 3) workflow checks the sha256 of the binary against the channel entry before every run, a copy that is already on the runner included: a runner's own tool cache is a place other jobs can write, so a digest checked only at download proves nothing about the file that runs later. An entry without a sha256 is refused. Both fetches of the step (the channel file, the tool) run `curl -q`: no `~/.curlrc`, a file every job of the runner can write, can fail the channel fetch over to the last copy of the channel or read the token.

The digest is only as good as the entry: when the forge does not answer, the entry comes from the runner's last copy of the channel, a file the jobs can write, and a job running at the same time under the same account is not covered (it could replace the file between the check and the start, and can read the other job's environment anyway). A runner that must exclude both keeps its tools in a root only it writes and gives each job its own account. Jobs of one
workflow must agree on `events`. Forgejo jobs take no secrets, environment or
refresh scope; switching a job from Crow removes the owned Crow adapter.

### Sibling repositories

A Forgejo job whose checks read other repositories (a family of packages that
verify each other) declares them in `siblings`: a key is a repository (`name`
in the job's own owner, or `owner/name`), a value is `main` (the default-branch
tip at run time) or an exact lowercase 40-hex commit. Before the checks, the
workflow fetches each one read-only with the same discipline as the checkout (once
through the runner's routing, which may send it to a local copy that lags a push
by seconds; if that does not serve it, once from the forge itself, never a wait) into
`<checkout>.siblings/<name>`, and exports `CCID_SIBLINGS_DIR` (that directory)
and `CCID_SIBLINGS_REVISIONS` (`name=sha,...`, the resolved commits). A pinned
commit that does not arrive fails the job. Because a `main` sibling moves
without the repository changing, a cached check that reads siblings lists
`CCID_SIBLINGS_REVISIONS` in `cache_env`. Non-Forgejo jobs take no
`siblings`.

## Crow is retired

From this renderer on, `ccid render` refuses jobs that still select Crow (the default
`scheduler`), Crow push adapters and `[push_consumer.*]` fan-out: set
`scheduler = "forgejo"` on the job. `[render] allow_crow = true` keeps the old rendering for a
repository that has not moved yet; it is the only way to render a Crow adapter and is meant to
disappear. Argo jobs are untouched. There is no Forgejo equivalent of `push_consumer` yet.

## Remote providers (retired)

`scheduler = "remote"` declared an external CI provider's own workflow in the repository's mirror, dispatched by a
host-side broker. That starts CI on a copy of the repository, which the operating model rules out (R6, R7), so the
scheduler is retired and the broker is gone. The Windows and macOS builds run on our own build VMs through the
`windows` and `macos` runner labels (R22).

- `render_schema = 3` refuses the scheduler ([surface.md](surface.md)).
- Schemas 1 and 2 still accept the declaration, as a transition (the shape follows [mirror-forges.md](mirror-forges.md)):
  `render` no longer records the job in `.ci/jobs.json` and no longer requires the provider workflow file, and says so on
  standard error; `render --check` accepts the declaration and an inventory that still records the job (the report lists
  `.ci/jobs.json` as `transitional`), and the next `render` writes the inventory without it. The manifest is the
  repository's own: its remote jobs, their provider workflow files and the checks that tested them are removed by hand,
  or replaced by a Forgejo job on the `windows` or `macos` label. The tolerance is dropped in a later release, once the
  fleet's manifests have moved.

ccid's own Windows coverage is a compile gate, not a leg: the check `windows-compile` of ccid's manifest cross-checks the
library, binary and tests for `x86_64-pc-windows-gnullvm` with `windows-experimental` on a `linux` runner (the standard
library for the target is in the runner's shared toolchain), so the code keeps building where outside contributors work
(R6, U2). Nothing runs on Windows; unix-only test code carries `cfg(unix)`.

## Mirror forges

`render` writes no file for the CI of a mirror forge (GitHub, GitLab, Bitbucket), at any schema: no CI runs on a
copy (operating model R6, R7). The release mirror, the pull-request and merge-request checks and the JSR publisher
that earlier releases rendered are retired; `render` removes them, and `render --check` names them at
`render_schema = 3` (schemas 1 and 2 accept their presence for now). See [mirror-forges.md](mirror-forges.md).

Repositories that declare an upstream in `ccvl-downstream.json` also get the dispatch-only
workflow `.forgejo/workflows/upstream-sync.yml`, which merges, checks and pushes every green
upstream commit and mirrors its release tags. See [upstream-sync.md](upstream-sync.md).
