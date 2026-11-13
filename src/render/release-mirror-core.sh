set +x
set -eu
# Release mirror (pull model): on a pushed release tag, read the Forgejo release of the same tag,
# check every asset against the release's checksum file and hand the verified files to the
# provider-specific step below. Not a build: no Forgejo release for the tag ends this step at once.
# Public repositories are read anonymously; a private one uses FORGE_RELEASE_TOKEN (read-only,
# limited to this repository), when the provider holds it.
MIRROR_SOURCE='CCID_MIRROR_SOURCE'
mirror_log() { printf 'release-mirror: %s\n' "$*" >&2; }
mirror_curl() {
  if [ -n "${FORGE_RELEASE_TOKEN:-}" ]; then
    curl --silent --show-error --location --max-redirs 5 --retry 3 --retry-connrefused --max-time 900 \
      -H "Authorization: token ${FORGE_RELEASE_TOKEN}" -H 'User-Agent: ccid-release-mirror' "$@"
  else
    curl --silent --show-error --location --max-redirs 5 --retry 3 --retry-connrefused --max-time 900 \
      -H 'User-Agent: ccid-release-mirror' "$@"
  fi
}
# The pointer URL baked into this file names the repository; the forge it redirects to serves the API.
# One hop only, and only when it keeps the repository path (a login redirect must not become the origin).
mirror_resolve() {
  MIRROR_PATH="${MIRROR_SOURCE#https://*/}"
  MIRROR_ORIGIN="${MIRROR_SOURCE%/"$MIRROR_PATH"}"
  hop=$(curl --silent --show-error --max-time 30 --retry 3 -o /dev/null -w '%{redirect_url}' \
    -H 'User-Agent: ccid-release-mirror' "$MIRROR_SOURCE" || true)
  case "$hop" in
    https://*/"$MIRROR_PATH" | https://*/"$MIRROR_PATH"/ | https://*/"$MIRROR_PATH".git)
      MIRROR_ORIGIN="${hop%/"$MIRROR_PATH"*}" ;;
  esac
  MIRROR_API="$MIRROR_ORIGIN/api/v1/repos/$MIRROR_PATH"
}
# mirror_fetch TAG DIR: DIR/release.json, DIR/notes.md, DIR/files/<asset>, DIR/assets.txt (verified names).
# Returns 3 when the Forgejo side has no published release for the tag.
mirror_fetch() {
  tag="$1"; dir="$2"
  mkdir -p "$dir/files"
  mirror_resolve
  code=$(mirror_curl -o "$dir/release.json" -w '%{http_code}' "$MIRROR_API/releases/tags/$tag" || true)
  case "$code" in
    200) ;;
    404 | 401 | 403)
      mirror_log "no Forgejo release for $tag is visible (HTTP $code); if this repository is private, its FORGE_RELEASE_TOKEN is missing or not allowed to read it"
      return 3 ;;
    *) mirror_log "the Forgejo release lookup for $tag answered HTTP ${code:-none}"; return 1 ;;
  esac
  if [ "$(jq -r '.tag_name // ""' "$dir/release.json")" != "$tag" ] || [ "$(jq -r '.draft // false' "$dir/release.json")" != false ]; then
    mirror_log "the Forgejo release of $tag is a draft or another tag: nothing to mirror"
    return 3
  fi
  MIRROR_NAME=$(jq -r '.name // ""' "$dir/release.json")
  [ -n "$MIRROR_NAME" ] || MIRROR_NAME="$tag"
  MIRROR_PRERELEASE=$(jq -r '(.prerelease // false) | tostring' "$dir/release.json")
  jq -j '.body // ""' "$dir/release.json" > "$dir/notes.md"
  : > "$dir/assets.txt"; : > "$dir/expected.txt"
  jq -r '.assets[]? | [.name, (.size | tostring), .browser_download_url] | @tsv' "$dir/release.json" > "$dir/list.tsv"
  # The forge may be reached under another name than the one it declares for itself (split-horizon DNS):
  # its own root URL, as the release states it, is as good as the origin asked.
  declared=$(jq -r '.html_url // ""' "$dir/release.json" | sed -n 's|^\(https://[^/]*\)/.*|\1|p')
  while IFS="$(printf '\t')" read -r name size url; do
    case "$name" in
      '' | .* | -* | */* | *[!A-Za-z0-9._+-]*) mirror_log "refusing the asset name '$name'"; return 1 ;;
    esac
    case "$url" in
      "$MIRROR_ORIGIN"/*) ;;
      *) if [ -n "$declared" ] && [ "${url#"$declared"/}" != "$url" ]; then :; else mirror_log "the download URL of $name is not on the forge ($MIRROR_ORIGIN)"; return 1; fi ;;
    esac
    mirror_curl --fail -o "$dir/files/$name" "$url"
    got=$(wc -c < "$dir/files/$name" | tr -d ' ')
    [ "$got" = "$size" ] || { mirror_log "$name: $got bytes downloaded, the release lists $size"; return 1; }
    printf '%s\n' "$name" >> "$dir/assets.txt"
  done < "$dir/list.tsv"
  # Checksums: SHA256SUMS-style files (coreutils format) and <asset>.sha256 files. Every other asset needs an entry.
  sumfiles=$(grep -E '^(SHA256SUMS|SHA256SUMS\.txt|sha256sums\.txt|checksums\.txt|checksums\.sha256|CHECKSUMS)$|\.sha256(sum)?$' "$dir/assets.txt" || true)
  printf '%s\n' "$sumfiles" > "$dir/sumfiles.txt"
  if [ -s "$dir/assets.txt" ] && [ -z "$sumfiles" ]; then
    mirror_log "the release of $tag has assets but no checksum file (SHA256SUMS or <asset>.sha256)"
    return 1
  fi
  for sumfile in $sumfiles; do
    case "$sumfile" in
      *.sha256 | *.sha256sum)
        base="${sumfile%.sha256sum}"; base="${base%.sha256}"
        awk -v base="$base" 'NF >= 1 && $1 ~ /^[0-9a-fA-F]{64}$/ { n = (NF >= 2) ? $2 : base; sub(/^\*/, "", n); print tolower($1), n }' "$dir/files/$sumfile" >> "$dir/expected.txt" ;;
      *)
        awk 'NF >= 2 && $1 ~ /^[0-9a-fA-F]{64}$/ { n = $2; sub(/^\*/, "", n); print tolower($1), n }' "$dir/files/$sumfile" >> "$dir/expected.txt" ;;
    esac
  done
  while IFS= read -r name; do
    if grep -qxF -- "$name" "$dir/sumfiles.txt"; then continue; fi
    want=$(awk -v n="$name" '$2 == n { print $1 }' "$dir/expected.txt" | sort -u)
    case "$want" in
      '') mirror_log "$name has no entry in the release's checksum files"; return 1 ;;
      *[[:space:]]*) mirror_log "$name has conflicting checksum entries"; return 1 ;;
    esac
    have=$(sha256sum "$dir/files/$name" | awk '{ print $1 }')
    [ "$have" = "$want" ] || { mirror_log "$name: sha256 $have, the checksum file says $want"; return 1; }
  done < "$dir/assets.txt"
  mirror_log "$tag: $(wc -l < "$dir/assets.txt" | tr -d ' ') asset(s) downloaded and verified from $MIRROR_PATH"
}
