#!/usr/bin/env bash
# Cross-build ccid for macOS ON LINUX (the label `linux`) and leave binary + test bundle in the DRAFT release
# `darwin-<commit>` of this repository. The macOS build clone only TESTS what is built here
# (.ci/release-darwin.sh), the publisher (.ci/release-darwin-publish.sh) publishes the tested bytes.
#
# Needs: cargo, cargo-zigbuild, zig, the rustup target, curl, jq, tar, git, sha256sum, and CI_MACOS_SDK (the macOS SDK
# directory; SDKROOT for the linker). Nothing is installed here: a missing tool is a red job.
#
# The test executables bake absolute paths in (CARGO_MANIFEST_DIR, CARGO_BIN_EXE_ccid, CARGO_TARGET_TMPDIR), so the
# build runs in ONE fixed root and the clone unpacks the bundle at the same root.
#   .ci/release-darwin-cross.sh            the commit checked out here is the one built
set -euo pipefail
: "${GITHUB_SERVER_URL:?}" "${GITHUB_REPOSITORY:?}"
token=${FORGEJO_TOKEN:-${GITHUB_TOKEN:-}}
[ -n "$token" ] || { echo 'release-darwin-cross: the job token is required' >&2; exit 2; }
sdk=${CI_MACOS_SDK:-/solid/apps/cache/macos-sdk/current}
[ -d "$sdk" ] || { echo "release-darwin-cross: the macOS SDK $sdk is not readable here" >&2; exit 2; }
for tool in cargo cargo-zigbuild zig curl jq tar git sha256sum; do
  command -v "$tool" > /dev/null || { echo "release-darwin-cross: missing tool: $tool" >&2; exit 2; }
done
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
sha=$(git rev-parse HEAD)
git diff --quiet --ignore-submodules HEAD -- || { echo 'release-darwin-cross: the checkout has changes; the release must be the exact commit' >&2; exit 1; }
triple=x86_64-apple-darwin
name=ccid-$triple
bundle=macos-tests-x86_64.tar.gz
xroot=/var/tmp/ccid-xtest
scratch=$(mktemp -d "${TMPDIR:-/tmp}/ccid-darwin-cross.XXXXXX")
trap 'find "$scratch" -delete; find "$xroot" -delete 2> /dev/null || true' EXIT
find "$xroot" -delete 2> /dev/null || true
mkdir -p "$xroot/src"
git archive "$sha" | tar -x -C "$xroot/src"
cd "$xroot/src"

export SDKROOT=$sdk CARGO_TARGET_DIR=$xroot/target CCID_SOURCE_REVISION=$sha
cargo zigbuild --release --locked --target "$triple" --bin ccid
bin=$xroot/target/$triple/release/ccid
test "$(head -c 8 "$bin" | od -An -tx1 | tr -d ' \n')" = cffaedfe07000001 || { echo "release-darwin-cross: $bin is not an x86_64 Mach-O" >&2; exit 1; }
cargo zigbuild --release --locked --target "$triple" --tests --message-format=json > "$scratch/tests.json"
jq -r 'select(.reason == "compiler-artifact" and .profile.test == true and .executable != null) | .executable' "$scratch/tests.json" | sort -u > "$scratch/exes"
[ -s "$scratch/exes" ] || { echo 'release-darwin-cross: no test executable was built' >&2; exit 1; }

# The bundle: the source tree, the binary and the test executables at their build paths, and the list of executables.
mkdir -p "$xroot/target/tmp"
sed "s|^$xroot/||" "$scratch/exes" > "$xroot/test-executables.txt"
mkdir "$scratch/out"
(cd "$xroot" && tar -czf "$scratch/out/$bundle" src test-executables.txt "target/$triple/release/ccid" $(cat test-executables.txt))
cp "$bin" "$scratch/out/$name"
(cd "$scratch/out" && sha256sum "$name" "$bundle" > "$name.sha256")
cat "$scratch/out/$name.sha256"

printf 'header = "Authorization: token %s"\n' "$token" > "$scratch/auth"
chmod 0600 "$scratch/auth"
api=${GITHUB_API_URL:-${GITHUB_SERVER_URL%/}/api/v1}/repos/$GITHUB_REPOSITORY
tag=darwin-$sha
call() { # METHOD URL [curl args]: the body lands in $scratch/response, prints the status
  local method=$1 url=$2
  shift 2
  curl -q --silent --show-error --max-time 600 -K "$scratch/auth" -X "$method" -o "$scratch/response" -w '%{http_code}' "$@" "$url"
}
old=$(call GET "$api/releases/tags/$tag" || true)
if [ "$old" = 200 ]; then
  [ "$(jq -r .draft "$scratch/response")" = true ] || { echo "release-darwin-cross: $tag is public; nothing is replaced" >&2; exit 1; }
  call DELETE "$api/releases/$(jq -r .id "$scratch/response")" > /dev/null
fi
status=$(call POST "$api/releases" -H 'Content-Type: application/json' --data "$(jq -cn --arg tag "$tag" --arg sha "$sha" \
  '{tag_name: $tag, target_commitish: $sha, name: ("ccid for macOS, " + $sha[0:12]), body: "The macOS build of this commit (cross-built on Linux), for the macOS test leg and the publisher.", draft: true, prerelease: false}')")
[ "$status" = 201 ] || { echo "release-darwin-cross: the forge answered $status creating the draft $tag" >&2; exit 1; }
id=$(jq -r .id "$scratch/response")
for file in "$name" "$bundle" "$name.sha256"; do
  status=$(call POST "$api/releases/$id/assets?name=$file" -H 'Content-Type: application/octet-stream' --data-binary "@$scratch/out/$file")
  [ "$status" = 201 ] || { echo "release-darwin-cross: the forge answered $status uploading $file" >&2; exit 1; }
done
echo "release-darwin-cross: $name and $bundle are in the draft $tag"
