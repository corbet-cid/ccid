#!/usr/bin/env bash
# Test the macOS binary that .ci/release-darwin-cross.sh built on Linux, ON macOS (the label `macos`), and attest it:
# the test executables run in the clone, and when the suite passes the job adds `ccid-<triple>.tested` (the binary's
# digest line) to the DRAFT release `darwin-<commit>`. Nothing is built here: the clone only tests.
#
# Bash 3.2 and the BSD userland are enough: the clone has no node, flock or timeout, and umask 077.
# RUST_TEST_THREADS=4 is the ceiling: 8 threads rebooted the guest.
#   .ci/release-darwin.sh            the commit checked out here is the one tested
set -euo pipefail
export LC_ALL=C
: "${GITHUB_SERVER_URL:?}" "${GITHUB_REPOSITORY:?}"
token=${FORGEJO_TOKEN:-${GITHUB_TOKEN:-}}
[ -n "$token" ] || { echo 'release-darwin: the job token is required' >&2; exit 2; }
for tool in curl jq shasum git tar; do
  command -v "$tool" > /dev/null || { echo "release-darwin: missing tool: $tool" >&2; exit 2; }
done
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
sha=$(git rev-parse HEAD)
git diff --quiet --ignore-submodules HEAD -- || { echo 'release-darwin: the checkout has changes; the release must be the exact commit' >&2; exit 1; }
[ "$(uname -s)" = Darwin ] || { echo "release-darwin: this is $(uname -s), not macOS" >&2; exit 2; }
triple=x86_64-apple-darwin
name=ccid-$triple
bundle=macos-tests-x86_64.tar.gz
xroot=/var/tmp/ccid-xtest
scratch=$(mktemp -d "${TMPDIR:-/tmp}/ccid-darwin.XXXXXX")
trap 'find "$scratch" -delete; find "$xroot" -delete 2> /dev/null || true' EXIT
printf 'header = "Authorization: token %s"\n' "$token" > "$scratch/auth"
chmod 0600 "$scratch/auth"
api=${GITHUB_API_URL:-${GITHUB_SERVER_URL%/}/api/v1}/repos/$GITHUB_REPOSITORY
tag=darwin-$sha
call() { # METHOD URL [curl args]: the body lands in $scratch/response, prints the status
  local method=$1 url=$2
  shift 2
  curl -q --silent --show-error --max-time 900 -K "$scratch/auth" -X "$method" -o "$scratch/response" -w '%{http_code}' "$@" "$url"
}

# 1. the draft the Linux job left, and the digests it names
[ "$(call GET "$api/releases/tags/$tag")" = 200 ] || { echo "release-darwin: there is no release $tag; run the Linux job first" >&2; exit 1; }
[ "$(jq -r .draft "$scratch/response")" = true ] || { echo "release-darwin: $tag is public; nothing is tested" >&2; exit 1; }
cp "$scratch/response" "$scratch/release.json"
id=$(jq -r .id "$scratch/release.json")
mkdir "$scratch/in"
# A draft is read with the runner's read credential only (the job token cannot): the file CCID_FORGE_READ_TOKEN_FILE, else
# $CREDENTIALS_DIRECTORY/forgejo-ci-read, as `ccid release fetch` finds it.
readfile=${CCID_FORGE_READ_TOKEN_FILE:-${CREDENTIALS_DIRECTORY:+$CREDENTIALS_DIRECTORY/forgejo-ci-read}}
[ -n "$readfile" ] && [ -r "$readfile" ] || { echo 'release-darwin: this runner has no read credential for a draft (CCID_FORGE_READ_TOKEN_FILE)' >&2; exit 2; }
printf 'header = "Authorization: token %s"\n' "$(tr -d '\n' < "$readfile")" > "$scratch/readauth"
chmod 0600 "$scratch/readauth"
for file in "$name" "$bundle" "$name.sha256"; do
  url=$(jq -er --arg n "$file" '.assets[] | select(.name == $n) | .browser_download_url' "$scratch/release.json")
  curl -q --silent --show-error --fail --location --max-time 900 -K "$scratch/readauth" -o "$scratch/in/$file" "$url"
done
(cd "$scratch/in" && shasum -a 256 -c "$name.sha256")

# 2. the bundle at the path it was built in; the binary must be this commit's
find "$xroot" -delete 2> /dev/null || true
mkdir -p "$xroot"
tar -xzf "$scratch/in/$bundle" -C "$xroot"
mkdir -p "$xroot/target/tmp"
cmp "$scratch/in/$name" "$xroot/target/$triple/release/ccid" || { echo 'release-darwin: the bundle holds another binary than the draft' >&2; exit 1; }
chmod +x "$xroot/target/$triple/release/ccid"
test "$("$scratch/in/$name" source-revision 2> /dev/null || { chmod +x "$scratch/in/$name"; "$scratch/in/$name" source-revision; })" = "$sha" || { echo "release-darwin: the binary does not embed $sha" >&2; exit 1; }

# 3. the suite: every test executable, with the skip list (.ci/darwin-skipped-tests.txt, the one list both read)
skips=()
while IFS= read -r line; do
  case $line in '' | '#'*) continue ;; esac
  skips+=(--skip "${line%%[[:space:]]*}")
done < "$root/.ci/darwin-skipped-tests.txt"
export RUST_TEST_THREADS=4 CARGO_MANIFEST_DIR=$xroot/src
failed=0
cd "$xroot/src"
while IFS= read -r exe; do
  chmod +x "$xroot/$exe"
  echo "release-darwin: running $exe"
  if ! "$xroot/$exe" --test-threads 4 ${skips[@]+"${skips[@]}"} > "$scratch/test.log" 2>&1; then
    failed=1
    tail -n 25 "$scratch/test.log"
    grep -E '^test .+ \.\.\. FAILED$' "$scratch/test.log" | sed -e 's/^test //' -e 's/ \.\.\. FAILED$//' | sort -u | sed 's/^/release-darwin: FAILED /' || true
  else
    grep -E '^test result' "$scratch/test.log" || true
  fi
done < "$xroot/test-executables.txt"
[ "$failed" = 0 ] || { echo 'release-darwin: the test suite failed on macOS' >&2; exit 1; }

# 4. the attestation: the digest line of the tested binary, next to it in the draft (a rerun replaces it)
grep " $name\$" "$scratch/in/$name.sha256" > "$scratch/$name.tested"
old=$(jq -r --arg n "$name.tested" '.assets[] | select(.name == $n) | .id' "$scratch/release.json")
if [ -n "$old" ]; then
  call DELETE "$api/releases/$id/assets/$old" > /dev/null
fi
status=$(call POST "$api/releases/$id/assets?name=$name.tested" -H 'Content-Type: application/octet-stream' --data-binary "@$scratch/$name.tested")
[ "$status" = 201 ] || { echo "release-darwin: the forge answered $status uploading $name.tested" >&2; exit 1; }
echo "release-darwin: $name passed on macOS; attested in the draft $tag"
