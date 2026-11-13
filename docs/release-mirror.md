# Release mirror (pull model)

A release is built and published once, on the primary forge. Every downstream forge
then copies it by itself when the release tag reaches it. Nothing pushes releases
downstream, and no downstream builds anything.

`ccid render` writes the provider files into every repository whose forge location it
can tell (the manifest's `repository`, else the `origin` remote; a directory without
either gets no files). There is nothing to declare:

| Provider | File | Trigger | Writes |
|---|---|---|---|
| GitHub | `.github/workflows/release-mirror.yml` | a pushed `v*` tag (or a manual run naming one) | a GitHub release with the same name, notes and pre-release flag, and every asset, with the job's own `GITHUB_TOKEN` |
| GitLab | `.gitlab-ci.yml` (only when absent or generated) | a pushed `v*` tag (or a manual or API pipeline with `RELEASE_TAG`) | the assets as generic package files and a GitLab release linking them, with `CI_JOB_TOKEN` |
| Bitbucket | `bitbucket-pipelines.yml` | a pushed `v*` tag | the assets as Downloads named `<tag>-<asset>`. Not rendered yet: Downloads need a paid Bitbucket plan |

A hand-written `.gitlab-ci.yml` is never overwritten; that repository has no GitLab mirror
until it includes the job itself.

## What a job does

1. Asks the baked identity URL where the forge is (one redirect hop) and reads
   `/api/v1/repos/<owner>/<repo>/releases/tags/<tag>`. **No published release for the tag
   ends the job with success after seconds.**
2. Downloads every asset, checks each against its size and against the release's checksum
   files, and stops before publishing anything if one does not hold.
3. Publishes the same bytes downstream. An existing asset is never replaced; an interrupted
   earlier run is completed; a repeated run writes nothing.

Checksum contract of a release: either one `SHA256SUMS` file (coreutils format, also
`SHA256SUMS.txt`, `sha256sums.txt`, `checksums.txt`, `checksums.sha256`, `CHECKSUMS`) that
lists every other asset, or one `<asset>.sha256` file per asset. An asset without an entry,
and a release with assets but no checksum file, fail the job.

## Private repositories

A public repository is read anonymously. A private one needs the downstream secret
`FORGE_RELEASE_TOKEN` (GitHub Actions secret, GitLab masked CI variable): a read-only
forge token limited to that repository. Without it the job finds no release and says so.
Bitbucket additionally needs the secured variable `BITBUCKET_DOWNLOADS_TOKEN`.

## Rollout

The files are *introduced*: `render --check` accepts their absence and reports them as
transitional, so a channel bump re-stales no repository; `render` writes them. Remove the
entries from `INTRODUCED` in `src/render/mirror.rs` once the fleet has re-rendered. The host
a repository's workflow records is kept by later renders whatever the renderer's `origin`
spells (a developer's and a runner's differ); a changed `<owner>/<repo>` is re-rendered.
