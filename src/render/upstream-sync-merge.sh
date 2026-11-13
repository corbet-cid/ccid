set -eu
# Upstream sync, step 1: merge one upstream commit into the downstream's default branch, locally. Nothing is pushed here.
# Inputs (workflow environment): SYNC_SERVER, SYNC_REPOSITORY, SYNC_TOKEN, SYNC_UPSTREAM_COMMIT, SYNC_UPSTREAM_URL,
# SYNC_UPSTREAM_BRANCH, SYNC_BRANCH. Optional runner settings: CCID_SOURCE_ROOT (stable work directory), CCID_LFS_STORAGE.
# Result, in the file named by GITHUB_ENV: SYNC_REPO (the work tree, which holds the history), SYNC_WORK (a shallow export of the merge, the
# checks run on it), SYNC_MERGE (the commit to publish), SYNC_CHECK=1 when the merge changed the branch and must be checked, SYNC_PUSH=1
# when it must be pushed.
env_out="${GITHUB_ENV:-/dev/null}"
say() { printf 'upstream-sync: %s\n' "$*" >&2; }
fail() { say "$*"; exit 1; }
case "$SYNC_UPSTREAM_COMMIT" in
  *[!0-9a-f]*|"") fail "upstream_commit is not a commit id" ;;
esac
[ "${#SYNC_UPSTREAM_COMMIT}" -eq 40 ] || fail "upstream_commit is not a full commit id"
commit=$SYNC_UPSTREAM_COMMIT
case "$SYNC_UPSTREAM_URL" in https://*/*/*.git) ;; *) fail "the declared upstream url is not an https clone url" ;; esac
# The declared url identifies the upstream; it is read from the forge that runs the job (jobs have no route to a public host).
up_slug=${SYNC_UPSTREAM_URL#https://*/}
up_slug=${up_slug%.git}
served="${SYNC_SERVER%/}/$up_slug.git"
api() { # url: the body of a successful answer, from the repository this job runs for (the job token reaches this repository only)
  curl --fail --silent --show-error --location --max-time 60 --retry 3 -H "Authorization: token $SYNC_TOKEN" "$1"
}
up_api() { # url: the same for the upstream, which is another repository: anonymously first (a public upstream), then with the job token
  curl --fail --silent --location --max-time 60 --retry 3 "$1" 2>/dev/null \
    || curl --fail --silent --show-error --location --max-time 60 --retry 3 -H "Authorization: token $SYNC_TOKEN" "$1"
}

# 1. Only a commit whose own CI run is green may travel. The question goes to the forge that hosts the upstream.
runs=$(up_api "${SYNC_SERVER%/}/api/v1/repos/$up_slug/actions/runs?head_sha=$commit&workflow_id=ci.yml&status=success&limit=1") \
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
git remote add upstream "$served"
git config advice.detachedHead false
git config gc.auto 0
gitauth() { git -c "http.extraHeader=Authorization: token $SYNC_TOKEN" "$@"; }

# The runner may route a fetch to a local copy of the repository (its own git configuration). That copy can lag a push by seconds, or not
# have a new repository yet: every fetch goes through that route once, and what it did not bring is asked of the forge itself.
tip=$(api "${SYNC_SERVER%/}/api/v1/repos/$SYNC_REPOSITORY/branches/$SYNC_BRANCH" | jq -r '.commit.id') \
  || fail "cannot read the branch $SYNC_BRANCH"
case "$tip" in *[!0-9a-f]*|"") fail "the forge did not name the tip of $SYNC_BRANCH" ;; esac
fetch_origin() { gitauth fetch --quiet --force --no-tags origin "+refs/heads/$SYNC_BRANCH:refs/remotes/origin/$SYNC_BRANCH"; }
have_tip() { git cat-file -e "$tip^{commit}" 2>/dev/null; }
direct() { GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_COUNT=0 "$@"; } # without the runner's route to a local copy
if ! { fetch_origin 2>/dev/null && have_tip; }; then
  say "the local copy lacks $tip: asking the forge itself"
  if ! { direct fetch_origin && have_tip; }; then fail "cannot fetch $tip"; fi
fi
git update-ref "refs/remotes/origin/$SYNC_BRANCH" "$tip"

# The upstream is public or readable by the job; its branch and its release tags travel, nothing else.
# The optional arguments are extra git options, passed through direct().
# shellcheck disable=SC2119,SC2120
fetch_upstream() { git "$@" fetch --quiet --force --no-tags upstream "+refs/heads/$SYNC_UPSTREAM_BRANCH:refs/remotes/upstream/$SYNC_UPSTREAM_BRANCH" "+refs/tags/v*:refs/upstream-tags/v*"; }
have_commit() { git cat-file -e "$commit^{commit}" 2>/dev/null; }
if ! { fetch_upstream 2>/dev/null && have_commit; }; then
  say "the local copy lacks the upstream: asking the forge itself"
  if ! direct fetch_upstream 2>/dev/null && ! direct fetch_upstream -c "http.extraHeader=Authorization: token $SYNC_TOKEN"; then fail "cannot fetch the upstream"; fi
fi
git cat-file -e "$commit^{commit}" 2>/dev/null || fail "the upstream does not have $commit"
git merge-base --is-ancestor "$commit" "refs/remotes/upstream/$SYNC_UPSTREAM_BRANCH" \
  || fail "$commit is not on the history of upstream/$SYNC_UPSTREAM_BRANCH"

# 3. Merge. A superseded or repeated event finds the commit already contained and changes nothing.
git checkout --quiet --force --detach "$tip"
git clean --quiet -ffdx
{
  echo "SYNC_REPO=$work"
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
# --no-renames: with rename detection git lists a moved file by its NEW path only, so an upstream file deleted from outside the
# downstream's paths and re-added byte for byte inside them would show only the owned side. Both sides must be listed.
policy=$(git show "$tip:ccvl-downstream.json" 2>/dev/null) || fail "ccvl-downstream.json is missing on $SYNC_BRANCH: nothing is merged"
stray=$(git -c core.quotepath=false diff --no-renames --name-only "$commit" "$merge" | jq -Rrs --argjson policy "$policy" '
  ($policy.allowed_paths // []) as $allowed
  | split("\n") | map(select(length > 0))
  | map(select(. as $path | ($allowed | any(. as $entry | if ($entry | endswith("/")) then ($path | startswith($entry)) else ($path == $entry) end)) | not))
  | join(" ")') || fail "cannot read the ownership policy of $SYNC_BRANCH"
if [ -n "$stray" ]; then
  git reset --quiet --hard "$tip"
  fail "paths outside the downstream's own, nothing is pushed: $stray"
fi

# 4. What the checks see is the layout of every other job: a shallow checkout of the one commit, detached. Real files, not Git LFS pointers:
# the objects come from the shared store, the upstream's, then this repository's.
export_dir="$work-check"
if [ -d "$export_dir" ]; then find "$export_dir" -delete; fi
mkdir -p "$export_dir"
if git show "$merge:.gitattributes" 2>/dev/null | grep -q 'filter=lfs'; then
  lfs_store="${CCID_LFS_STORAGE:-$work/.git/lfs}"
  mkdir -p "$lfs_store"
  git lfs install --local --skip-smudge >/dev/null
  git config lfs.storage "$lfs_store"
  ( [ -z "${CCID_LFS_STORAGE:-}" ] || umask 002; git lfs fetch "$served" "$commit" >/dev/null 2>&1 || true )
fi
# The merge exists only here, so the export's history is completed from this work tree (a branch names the merge for that): the
# checks that complete a shallow checkout from `origin` (the private check does) then find both parents.
git branch -f ccid-sync "$merge"
git -C "$export_dir" init --quiet .
git -C "$export_dir" remote add origin "${SYNC_SERVER%/}/$SYNC_REPOSITORY.git"
git -C "$export_dir" fetch --quiet --depth 1 "file://$work" "$merge"
GIT_LFS_SKIP_SMUDGE=1 git -C "$export_dir" checkout --quiet --force --detach FETCH_HEAD
if git -C "$export_dir" show "FETCH_HEAD:.gitattributes" 2>/dev/null | grep -q 'filter=lfs'; then
  git -C "$export_dir" lfs install --local --skip-smudge >/dev/null
  git -C "$export_dir" config lfs.storage "$lfs_store"
  # The helper runs in git's shell: $CCID_LFS_TOKEN must stay unexpanded here.
  # shellcheck disable=SC2016
  helper='!f() { echo username=x-access-token; echo "password=$CCID_LFS_TOKEN"; }; f'
  ( [ -z "${CCID_LFS_STORAGE:-}" ] || umask 002
    CCID_LFS_TOKEN=$SYNC_TOKEN git -C "$export_dir" -c "credential.helper=$helper" lfs pull origin ) \
    || fail "cannot materialise the Git LFS files of $merge"
fi
git -C "$export_dir" remote set-url origin "file://$work"
{ echo "SYNC_WORK=$export_dir"; echo "SYNC_MERGE=$merge"; echo "SYNC_CHECK=1"; echo "SYNC_PUSH=1"; } >> "$env_out"
