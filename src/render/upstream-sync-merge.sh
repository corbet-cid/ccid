set -eu
# Upstream sync, step 1: merge one upstream commit into the downstream's default branch, locally. Nothing is pushed here.
# Inputs (workflow environment): SYNC_SERVER, SYNC_REPOSITORY, SYNC_TOKEN, SYNC_UPSTREAM_COMMIT, SYNC_UPSTREAM_URL,
# SYNC_UPSTREAM_BRANCH, SYNC_BRANCH. Optional runner settings: CCID_SOURCE_ROOT (stable work directory), CCID_LFS_STORAGE.
# Result, in the file named by GITHUB_ENV: SYNC_WORK (the work tree), SYNC_MERGE (the commit to publish), SYNC_CHECK=1 when the
# merge changed the branch and must be checked, SYNC_PUSH=1 when it must be pushed.
env_out="${GITHUB_ENV:-/dev/null}"
say() { printf 'upstream-sync: %s\n' "$*" >&2; }
fail() { say "$*"; exit 1; }
case "$SYNC_UPSTREAM_COMMIT" in
  *[!0-9a-f]*|"") fail "upstream_commit is not a commit id" ;;
esac
[ "${#SYNC_UPSTREAM_COMMIT}" -eq 40 ] || fail "upstream_commit is not a full commit id"
commit=$SYNC_UPSTREAM_COMMIT
case "$SYNC_UPSTREAM_URL" in https://*/*/*.git) ;; *) fail "the declared upstream url is not an https clone url" ;; esac
up_origin=${SYNC_UPSTREAM_URL%/*/*.git}
up_slug=${SYNC_UPSTREAM_URL#"$up_origin"/}
up_slug=${up_slug%.git}
auth_for() { # forge origin -> extra header arguments (the job token only ever goes to the forge that runs the job)
  case "$1" in "$SYNC_SERVER"|"${SYNC_SERVER%/}") printf 'Authorization: token %s' "$SYNC_TOKEN" ;; esac
}
api() { # url [curl arguments]: the body of a successful answer
  url=$1; shift
  header=$(auth_for "${url%%/api/*}")
  if [ -n "$header" ]; then curl --fail --silent --show-error --location --max-time 60 --retry 3 -H "$header" "$@" "$url"
  else curl --fail --silent --show-error --location --max-time 60 --retry 3 "$@" "$url"; fi
}

# 1. Only a commit whose own CI run is green may travel. The question goes to the forge that hosts the upstream.
runs=$(api "$up_origin/api/v1/repos/$up_slug/actions/runs?head_sha=$commit&workflow_id=ci.yml&status=success&limit=1") \
  || fail "cannot read the upstream CI runs of $commit"
[ "$(printf '%s' "$runs" | jq -r '(.total_count // (.workflow_runs | length) // 0)')" -ge 1 ] \
  || fail "the upstream commit $commit has no successful ci run: nothing is merged"

# 2. The work tree: stable per repository when the runner names a root (history and LFS objects stay), else disposable.
if [ -n "${CCID_SOURCE_ROOT:-}" ]; then
  work="$CCID_SOURCE_ROOT/$SYNC_REPOSITORY/upstream-sync"
else
  work="$(mktemp -d)/upstream-sync"
fi
mkdir -p "$work"
cd "$work"
[ -d .git ] || git init --quiet .
git remote remove origin 2>/dev/null || true
git remote remove upstream 2>/dev/null || true
git remote add origin "${SYNC_SERVER%/}/$SYNC_REPOSITORY.git"
git remote add upstream "$SYNC_UPSTREAM_URL"
git config advice.detachedHead false
git config gc.auto 0
gitauth() { git -c "http.extraHeader=Authorization: token $SYNC_TOKEN" "$@"; }

# The runner may route a fetch to a local copy that lags the push by seconds: compare with what the forge says the branch is.
tip=$(api "${SYNC_SERVER%/}/api/v1/repos/$SYNC_REPOSITORY/branches/$SYNC_BRANCH" | jq -r '.commit.id') \
  || fail "cannot read the branch $SYNC_BRANCH"
case "$tip" in *[!0-9a-f]*|"") fail "the forge did not name the tip of $SYNC_BRANCH" ;; esac
have=
for attempt in 1 2 3; do
  gitauth fetch --quiet --force --no-tags origin "+refs/heads/$SYNC_BRANCH:refs/remotes/origin/$SYNC_BRANCH" || true
  if git cat-file -e "$tip^{commit}" 2>/dev/null; then have=1; break; fi
  [ "$attempt" -lt 3 ] && sleep 5
done
if [ -z "$have" ]; then
  say "the local copy lacks $tip: asking the forge itself"
  if ! { GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_COUNT=0 gitauth fetch --quiet --force --no-tags origin "+refs/heads/$SYNC_BRANCH:refs/remotes/origin/$SYNC_BRANCH" \
      && git cat-file -e "$tip^{commit}" 2>/dev/null; }; then fail "cannot fetch $tip"; fi
fi
git update-ref "refs/remotes/origin/$SYNC_BRANCH" "$tip"

# The upstream is public or readable by the job; its branch and its release tags travel, nothing else.
if ! git fetch --quiet --force --no-tags upstream "+refs/heads/$SYNC_UPSTREAM_BRANCH:refs/remotes/upstream/$SYNC_UPSTREAM_BRANCH" "+refs/tags/v*:refs/upstream-tags/v*" 2>/dev/null; then
  gitauth fetch --quiet --force --no-tags upstream "+refs/heads/$SYNC_UPSTREAM_BRANCH:refs/remotes/upstream/$SYNC_UPSTREAM_BRANCH" "+refs/tags/v*:refs/upstream-tags/v*" \
    || fail "cannot fetch the upstream"
fi
git cat-file -e "$commit^{commit}" 2>/dev/null || fail "the upstream does not have $commit"
git merge-base --is-ancestor "$commit" "refs/remotes/upstream/$SYNC_UPSTREAM_BRANCH" \
  || fail "$commit is not on the history of upstream/$SYNC_UPSTREAM_BRANCH"

# 3. Merge. A superseded or repeated event finds the commit already contained and changes nothing.
git checkout --quiet --force --detach "$tip"
git clean --quiet -ffdx
{
  echo "SYNC_WORK=$work"
  echo "SYNC_TIP=$tip"
} >> "$env_out"
if git merge-base --is-ancestor "$commit" "$tip"; then
  say "$commit is already part of $SYNC_BRANCH: no merge"
  { echo "SYNC_MERGE=$tip"; echo "SYNC_CHECK=0"; echo "SYNC_PUSH=0"; } >> "$env_out"
  exit 0
fi
name=$(git log -1 --format=%an "$tip"); email=$(git log -1 --format=%ae "$tip")
if ! GIT_LFS_SKIP_SMUDGE=1 git -c "user.name=$name" -c "user.email=$email" -c commit.gpgsign=false merge --no-edit \
    -m "Merge upstream $SYNC_UPSTREAM_BRANCH at $(git rev-parse --short=12 "$commit")" "$commit" >/dev/null 2>&1; then
  conflicts=$(git diff --name-only --diff-filter=U | head -40 | tr '\n' ' ')
  git merge --abort 2>/dev/null || true
  fail "the merge conflicts (${conflicts:-no file list}): nothing is pushed, resolve it by hand"
fi
merge=$(git rev-parse HEAD)
say "merged $commit into $SYNC_BRANCH: $merge"

# 3b. Ownership, asked of git alone (not of any program in the merged tree, which could vouch for itself): outside the paths the
# downstream declares as its own, the merge must equal the upstream commit exactly. A path git has to quote is refused.
policy=$(git show "$tip:ccvl-downstream.json" 2>/dev/null) || fail "ccvl-downstream.json is missing on $SYNC_BRANCH: nothing is merged"
stray=$(git -c core.quotepath=false diff --name-only "$commit" "$merge" | jq -Rrs --argjson policy "$policy" '
  ($policy.allowed_paths // []) as $allowed
  | split("\n") | map(select(length > 0))
  | map(select(. as $path | ($allowed | any(. as $entry | if ($entry | endswith("/")) then ($path | startswith($entry)) else ($path == $entry) end)) | not))
  | join(" ")') || fail "cannot read the ownership policy of $SYNC_BRANCH"
if [ -n "$stray" ]; then
  git reset --quiet --hard "$tip"
  fail "paths outside the downstream's own, nothing is pushed: $stray"
fi

# 4. The checks need real files, not Git LFS pointers: objects come from the shared store, the upstream, then this repository.
if git show "$merge:.gitattributes" 2>/dev/null | grep -q 'filter=lfs'; then
  git lfs install --local --skip-smudge >/dev/null
  if [ -n "${CCID_LFS_STORAGE:-}" ]; then mkdir -p "$CCID_LFS_STORAGE"; git config lfs.storage "$CCID_LFS_STORAGE"; fi
  ( [ -z "${CCID_LFS_STORAGE:-}" ] || umask 002
    git lfs fetch "$SYNC_UPSTREAM_URL" "$commit" >/dev/null 2>&1 || true
    # The helper runs in git's shell: $CCID_LFS_TOKEN must stay unexpanded here.
    # shellcheck disable=SC2016
    helper='!f() { echo username=x-access-token; echo "password=$CCID_LFS_TOKEN"; }; f'
    CCID_LFS_TOKEN=$SYNC_TOKEN git -c "credential.helper=$helper" lfs pull origin ) \
    || fail "cannot materialise the Git LFS files of $merge"
fi
{ echo "SYNC_MERGE=$merge"; echo "SYNC_CHECK=1"; echo "SYNC_PUSH=1"; } >> "$env_out"
