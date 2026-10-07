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
command = ["bash", ".ci/run-job.sh"]
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
