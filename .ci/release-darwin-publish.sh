#!/usr/bin/env bash
# Publish the macOS binary that .ci/release-darwin.sh left in the draft `darwin-<commit>` to the registry the tool
# channel reads (the generic package registry, next to the Linux binary of the same commit), then delete the draft.
# Run it where the package-write credential lives, never in a job.
#
#   CFRG_RELEASE_TOKEN            the registry token (not needed with --dry-run)
#   CCID_FORGE_READ_TOKEN_FILE    a file with a token that can read the assets of a draft
#   GITHUB_SERVER_URL, GITHUB_REPOSITORY   the forge and this repository (owner/name)
#
#   .ci/release-darwin-publish.sh COMMIT [--dry-run]
#
# It prints what `cfrg release publish` prints (url, sha256, size, commit): the sha256 is the value of
# `classes.macos` in channel.json, with the commit as its revision.
set -euo pipefail
sha=${1:?usage: release-darwin-publish.sh COMMIT [--dry-run]}
dry=${2:-}
[[ $sha =~ ^[0-9a-f]{40}$ ]] || { echo 'release-darwin-publish: COMMIT is a full lowercase commit id' >&2; exit 2; }
: "${GITHUB_SERVER_URL:?}" "${GITHUB_REPOSITORY:?}" "${CCID_FORGE_READ_TOKEN_FILE:?}"
[ "$dry" = --dry-run ] || : "${CFRG_RELEASE_TOKEN:?CFRG_RELEASE_TOKEN must hold the registry token (or pass --dry-run)}"
triple=x86_64-apple-darwin
name=ccid-$triple
scratch=$(mktemp -d "${TMPDIR:-/tmp}/ccid-darwin-publish.XXXXXX")
trap 'find "$scratch" -delete' EXIT
printf 'header = "Authorization: token %s"\n' "$(tr -d '\n' < "$CCID_FORGE_READ_TOKEN_FILE")" > "$scratch/auth"
chmod 0600 "$scratch/auth"
api=${GITHUB_API_URL:-${GITHUB_SERVER_URL%/}/api/v1}/repos/$GITHUB_REPOSITORY
tag=darwin-$sha
curl -q --silent --show-error --fail --max-time 120 -K "$scratch/auth" -o "$scratch/release.json" "$api/releases/tags/$tag"
[ "$(jq -r .draft "$scratch/release.json")" = true ] || { echo "release-darwin-publish: $tag is not a draft" >&2; exit 1; }
for file in "$name" "$name.sha256"; do
  url=$(jq -er --arg n "$file" '.assets[] | select(.name == $n) | .browser_download_url' "$scratch/release.json")
  curl -q --silent --show-error --fail --max-time 600 -K "$scratch/auth" -o "$scratch/$file" "$url"
done
(cd "$scratch" && sha256sum --check --strict "$name.sha256")
args=(release --config .ci/release.json publish --file "$scratch/$name" --commit "$sha" --name "$name")
[ "$dry" = --dry-run ] && args+=(--dry-run)
cfrg "${args[@]}"
if [ "$dry" != --dry-run ]; then
  curl -q --silent --show-error --fail --max-time 120 -K "$scratch/auth" -X DELETE "$api/releases/$(jq -r .id "$scratch/release.json")"
fi
