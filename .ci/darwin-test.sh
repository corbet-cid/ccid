#!/usr/bin/env bash
# The test suite of ccid ON macOS (the label `macos`), without a release: the macOS leg of a check run on the
# checked-out commit. .ci/release-darwin.sh runs the same suite before it leaves a binary in a draft; the tests that
# do not pass on macOS, each with its reason, are listed in .ci/darwin-skipped-tests.txt (the one list both read).
#
# Bash 3.2 and the BSD userland are enough (the clone has no node, flock or timeout, and umask 077).
#   .ci/darwin-test.sh            the commit checked out here is the one tested
set -euo pipefail
export LC_ALL=C
: "${CI_CACHE_ROOT:?CI_CACHE_ROOT must name the persistent build directory of the clone}"
for tool in cargo rustc git; do
  command -v "$tool" > /dev/null || { echo "darwin-test: missing tool: $tool" >&2; exit 2; }
done
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
git diff --quiet --ignore-submodules HEAD -- || { echo 'darwin-test: the checkout has changes; the test must be of the exact commit' >&2; exit 1; }
case $(rustc -vV | sed -n 's/^host: //p') in
  *-apple-darwin) ;;
  *) echo "darwin-test: this is not macOS" >&2; exit 2 ;;
esac
scratch=$(mktemp -d "${TMPDIR:-/tmp}/ccid-darwin-test.XXXXXX")
trap 'find "$scratch" -delete' EXIT
export CARGO_TARGET_DIR=$CI_CACHE_ROOT/ccid-release-target
CCID_SOURCE_REVISION=$(git rev-parse HEAD)
export CCID_SOURCE_REVISION
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
# A red suite names its failing tests (the tail above only shows the last test target): the names go into
# .ci/darwin-skipped-tests.txt with a reason, or the test gets fixed.
if [ "$rc" != 0 ]; then
  grep -E '^test .+ \.\.\. FAILED$' "$scratch/test.log" | sed -e 's/^test //' -e 's/ \.\.\. FAILED$//' | sort -u | sed 's/^/darwin-test: FAILED /' || true
  echo "darwin-test: the test suite failed on macOS (exit $rc)" >&2
  exit 1
fi
