#!/usr/bin/env bash
# Build the ccid release binary IN THIS ENVIRONMENT and publish it (or rehearse the publication).
#
# Run it inside the release environment: the CI job image or a CI check-shell. `cfrg release publish`
# refuses a Linux binary that links a host loader instead of a Nix one, so a binary built here is the
# only kind that can be published; a local `cargo build` on a plain host never becomes a release.
#
#   CI_COMMIT_SHA       the exact commit being released (the CI job names it; the build embeds it)
#   CFRG_RELEASE_TOKEN  the registry token (not needed with CCID_RELEASE_DRY_RUN=1)
#   CCID_RELEASE_DRY_RUN=1  print URL, sha256 and size; upload nothing
#
# The release is immutable per commit: a second publication of the same commit must have the same
# bytes (a rebuild with a different toolchain is a new key, so build it in the same environment).
set -euo pipefail
: "${CI_COMMIT_SHA:?CI_COMMIT_SHA must name the exact commit being released}"
dry_run=${CCID_RELEASE_DRY_RUN:-0}
[ "$dry_run" = 1 ] || : "${CFRG_RELEASE_TOKEN:?CFRG_RELEASE_TOKEN must hold the registry token (or set CCID_RELEASE_DRY_RUN=1)}"

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
test "$(git rev-parse HEAD)" = "$CI_COMMIT_SHA" || {
  echo "release: HEAD is not CI_COMMIT_SHA ($CI_COMMIT_SHA)" >&2
  exit 1
}
git diff --quiet --ignore-submodules HEAD -- || {
  echo "release: the checkout has unstaged changes; the release must be the exact commit" >&2
  exit 1
}

export CCID_SOURCE_REVISION="$CI_COMMIT_SHA"
cargo build --release --locked
binary=target/release/ccid
test -x "$binary"
test "$("$binary" source-revision)" = "$CI_COMMIT_SHA" || {
  echo "release: $binary does not embed $CI_COMMIT_SHA" >&2
  exit 1
}

args=(release --config .ci/release.json publish --file "$binary" --commit "$CI_COMMIT_SHA" --name ccid-x86_64-unknown-linux-gnu)
if [ "$dry_run" = 1 ]; then
  args+=(--dry-run)
fi
cfrg "${args[@]}"
