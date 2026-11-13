set -eu
# Upstream sync, step 3: runs only after the merge was clean and every check was green. Pushes the merge to the branch, gives the
# downstream the upstream's release tags on the first of its commits that contains them, and records each upstream release as a
# release of the downstream that carries no build of its own.
# Inputs: SYNC_SERVER, SYNC_REPOSITORY, SYNC_TOKEN, SYNC_UPSTREAM_URL, SYNC_BRANCH, and from step 1: SYNC_REPO, SYNC_MERGE, SYNC_PUSH.
say() { printf 'upstream-sync: %s\n' "$*" >&2; }
fail() { say "$*"; exit 1; }
cd "$SYNC_REPO"
gitauth() { git -c "http.extraHeader=Authorization: token $SYNC_TOKEN" "$@"; }
server=${SYNC_SERVER%/}
up_slug=${SYNC_UPSTREAM_URL#https://*/}
up_slug=${up_slug%.git}
down_api="$server/api/v1/repos/$SYNC_REPOSITORY"
mine() { curl --silent --show-error --location --max-time 60 --retry 3 -H "Authorization: token $SYNC_TOKEN" "$@"; }
theirs() { # url: the upstream is another repository: anonymously first (a public upstream), then with the job token
  curl --fail --silent --location --max-time 60 --retry 3 "$1" 2>/dev/null \
    || curl --fail --silent --show-error --location --max-time 60 --retry 3 -H "Authorization: token $SYNC_TOKEN" "$1"
}

# 1. The merge. A plain push: if the branch moved meanwhile the forge refuses and the run is red; the next sync merges again.
if [ "${SYNC_PUSH:-0}" = 1 ]; then
  gitauth push --quiet origin "$SYNC_MERGE:refs/heads/$SYNC_BRANCH" || fail "the push of $SYNC_MERGE to $SYNC_BRANCH was refused (the branch moved or the forge denies the job token)"
  say "pushed $SYNC_MERGE to $SYNC_BRANCH"
fi

# 2. Release tags. The tag of the upstream commit X goes on the first first-parent commit of the downstream branch that contains X.
chain=$(mktemp)
trap 'find "$chain" -delete' EXIT
git rev-list --first-parent --reverse "$SYNC_MERGE" > "$chain"
total=$(wc -l < "$chain")
first_containing() { # X: the oldest commit of the chain that contains X (binary search: containment never turns off along the chain)
  low=1; high=$total
  while [ "$low" -lt "$high" ]; do
    mid=$(( (low + high) / 2 ))
    candidate=$(sed -n "${mid}p" "$chain")
    if git merge-base --is-ancestor "$1" "$candidate"; then high=$mid; else low=$((mid + 1)); fi
  done
  sed -n "${low}p" "$chain"
}
git for-each-ref --format='%(refname:strip=2) %(objectname)' 'refs/upstream-tags/v*' | while read -r tag object; do
  printf '%s\n' "$tag" | grep -Eq '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' || { say "$tag is not a release tag: skipped"; continue; }
  target=$(git rev-parse --verify --quiet "$object^{commit}") || { say "$tag has no commit: skipped"; continue; }
  if ! git merge-base --is-ancestor "$target" "$SYNC_MERGE"; then say "$tag is not part of the downstream yet: the sync that merges it will carry it"; continue; fi
  existing=$(gitauth ls-remote --tags origin "refs/tags/$tag" | cut -f1)
  if [ -n "$existing" ]; then say "$tag exists in the downstream ($existing): never replaced"
  else
    place=$(first_containing "$target")
    git tag "$tag" "$place" 2>/dev/null || true
    gitauth push --quiet origin "refs/tags/$tag:refs/tags/$tag" || fail "the push of the tag $tag was refused"
    say "tag $tag -> $place"
  fi
  # 3. The release of that tag: pointing at the upstream's assets, no build and no copy of its own.
  if [ "$(mine -o /dev/null -w '%{http_code}' "$down_api/releases/tags/$tag")" = 200 ]; then say "release $tag exists in the downstream"; continue; fi
  release=$(theirs "$server/api/v1/repos/$up_slug/releases/tags/$tag" 2>/dev/null) || { say "the upstream has no release for $tag yet: a later sync carries it"; continue; }
  [ "$(printf '%s' "$release" | jq -r '.draft')" = false ] || { say "the upstream release $tag is still a draft"; continue; }
  version=${tag#v}
  body=$(printf '%s' "$release" | jq -r --arg tag "$tag" '
    "This release has no build of its own: the program is the upstream release " + $tag + ", published at " + .html_url + ".\n\nAssets of the upstream release:\n" +
    ((.assets // []) | map("- [" + .name + "](" + .browser_download_url + ")") | join("\n"))')
  payload=$(printf '%s' "$release" | jq -n --arg tag "$tag" --arg name "$version" --arg body "$body" --argjson pre "$(printf '%s' "$release" | jq '.prerelease')" \
    '{tag_name: $tag, name: $name, body: $body, draft: false, prerelease: $pre}')
  code=$(printf '%s' "$payload" | mine -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' --data-binary @- "$down_api/releases")
  [ "$code" = 201 ] || fail "creating the release $tag answered HTTP $code"
  say "release $tag created (notes point to the upstream assets)"
done
