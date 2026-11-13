#!/usr/bin/env bash
# Build and test ccid ON macOS (the label `macos`) and leave the binary in the DRAFT release `darwin-<commit>`
# of this repository, for the trusted publisher (.ci/release-darwin-publish.sh).
#
# Why a draft: the macOS build clone runs a job with the job token only. Publishing to the registry the tool
# channel reads takes a package-write credential that never enters a job, so the job leaves the binary where the
# job token can write (a draft release) and the publisher, on the host that holds the credential, publishes it.
#
# Bash 3.2 and the BSD userland are enough: the clone has no node, flock or timeout, and umask 077.
#   .ci/release-darwin.sh            the commit checked out here is the one built
set -euo pipefail
export LC_ALL=C
: "${CI_CACHE_ROOT:?CI_CACHE_ROOT must name the persistent build directory of the clone}"
: "${GITHUB_SERVER_URL:?}" "${GITHUB_REPOSITORY:?}"
token=${FORGEJO_TOKEN:-${GITHUB_TOKEN:-}}
[ -n "$token" ] || { echo 'release-darwin: the job token is required' >&2; exit 2; }
for tool in cargo rustc curl jq shasum git; do
  command -v "$tool" > /dev/null || { echo "release-darwin: missing tool: $tool" >&2; exit 2; }
done
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
sha=$(git rev-parse HEAD)
git diff --quiet --ignore-submodules HEAD -- || { echo 'release-darwin: the checkout has changes; the release must be the exact commit' >&2; exit 1; }
triple=$(rustc -vV | sed -n 's/^host: //p')
case $triple in
  *-apple-darwin) ;;
  *) echo "release-darwin: this is $triple, not macOS" >&2; exit 2 ;;
esac
name=ccid-$triple
scratch=$(mktemp -d "${TMPDIR:-/tmp}/ccid-darwin.XXXXXX")
trap 'find "$scratch" -delete' EXIT
printf 'header = "Authorization: token %s"\n' "$token" > "$scratch/auth"
chmod 0600 "$scratch/auth"
api=${GITHUB_API_URL:-${GITHUB_SERVER_URL%/}/api/v1}/repos/$GITHUB_REPOSITORY

# 1. the exact commit, built here, with the commit embedded
export CARGO_TARGET_DIR=$CI_CACHE_ROOT/ccid-release-target
export CCID_SOURCE_REVISION=$sha
cargo build --release --locked
bin=$CARGO_TARGET_DIR/release/ccid
[ "$("$bin" source-revision)" = "$sha" ] || { echo "release-darwin: $bin does not embed $sha" >&2; exit 1; }

# 2. the suite, on the platform it ships for
skips=()
while IFS= read -r line; do
  case $line in '' | '#'*) continue ;; esac
  skips+=(--skip "${line%%[[:space:]]*}")
done < "$root/.ci/darwin-skipped-tests.txt"
set +e
cargo test --release --locked --no-fail-fast -- ${skips[@]+"${skips[@]}"} > "$scratch/test.log" 2>&1
rc=$?
set -e
tail -n 25 "$scratch/test.log"
# A red suite names its failing tests (the tail above only shows the last test target): the names go into .ci/darwin-skipped-tests.txt
# with a reason, or the test gets fixed.
if [ "$rc" != 0 ]; then
  grep -E '^test .+ \.\.\. FAILED$' "$scratch/test.log" | sed -e 's/^test //' -e 's/ \.\.\. FAILED$//' | sort -u | sed 's/^/release-darwin: FAILED /' || true
fi
[ "$rc" = 0 ] || { echo "release-darwin: the test suite failed on macOS (exit $rc)" >&2; exit 1; }

# 3. the binary and its digest, into the draft darwin-<commit> (a rerun starts the draft again)
mkdir "$scratch/out"
cp "$bin" "$scratch/out/$name"
(cd "$scratch/out" && shasum -a 256 "$name" > "$name.sha256")
cat "$scratch/out/$name.sha256"
tag=darwin-$sha
call() { # METHOD URL [curl args]: the body lands in $scratch/response, prints the status
  local method=$1 url=$2
  shift 2
  curl -q --silent --show-error --max-time 600 -K "$scratch/auth" -X "$method" -o "$scratch/response" -w '%{http_code}' "$@" "$url"
}
old=$(call GET "$api/releases/tags/$tag" || true)
if [ "$old" = 200 ]; then
  [ "$(jq -r .draft "$scratch/response")" = true ] || { echo "release-darwin: $tag is public; nothing is replaced" >&2; exit 1; }
  call DELETE "$api/releases/$(jq -r .id "$scratch/response")" > /dev/null
fi
status=$(call POST "$api/releases" -H 'Content-Type: application/json' --data "$(jq -cn --arg tag "$tag" --arg sha "$sha" \
  '{tag_name: $tag, target_commitish: $sha, name: ("ccid for macOS, " + $sha[0:12]), body: "The macOS build of this commit, for the publisher.", draft: true, prerelease: false}')")
[ "$status" = 201 ] || { echo "release-darwin: the forge answered $status creating the draft $tag" >&2; exit 1; }
id=$(jq -r .id "$scratch/response")
for file in "$name" "$name.sha256"; do
  status=$(call POST "$api/releases/$id/assets?name=$file" -H 'Content-Type: application/octet-stream' --data-binary "@$scratch/out/$file")
  [ "$status" = 201 ] || { echo "release-darwin: the forge answered $status uploading $file" >&2; exit 1; }
done
call GET "$api/releases/$id" > /dev/null
want=$(wc -c < "$scratch/out/$name" | tr -d ' ')
have=$(jq -r --arg n "$name" '[.assets[] | select(.name == $n)][0].size' "$scratch/response")
[ "$want" = "$have" ] || { echo "release-darwin: the draft holds $have of $want bytes of $name" >&2; exit 1; }
echo "release-darwin: $name ($want bytes) is in the draft $tag; publish it with .ci/release-darwin-publish.sh $sha"
