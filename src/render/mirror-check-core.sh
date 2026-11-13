set +x
set -eu
# Mirror check: run this repository's verdict jobs on the compute of the platform that holds this copy.
# This is not the forge's CI and not a build of ours. The ccid release is read anonymously from the
# forge (its release channel names it, its digest is checked) and `ccid job-run --mirror` runs the
# declared checks of each job. No secret, result cache or network of ours is involved: a verdict job
# that needs one is not listed in CHECK_JOBS (`ccid render --check` says which and why).
CHECK_TOOL_SOURCE='CCID_TOOL_SOURCE'
CHECK_JOBS='CCID_CHECK_JOBS'
CHECK_CLASS=linux
check_log() { printf 'mirror-check: %s\n' "$*" >&2; }
check_curl() {
  curl --silent --show-error --fail --location --max-redirs 5 --retry 3 --retry-connrefused --max-time 900 \
    -H 'User-Agent: ccid-mirror-check' "$@"
}
# The pointer URL baked into this file names the tool's repository; the forge it redirects to serves it.
# One hop only, and only when it keeps the repository path.
check_path="${CHECK_TOOL_SOURCE#https://*/}"
check_origin="${CHECK_TOOL_SOURCE%/"$check_path"}"
hop=$(curl --silent --show-error --max-time 30 --retry 3 -o /dev/null -w '%{redirect_url}' \
  -H 'User-Agent: ccid-mirror-check' "$CHECK_TOOL_SOURCE" || true)
case "$hop" in
  https://*/"$check_path" | https://*/"$check_path"/ | https://*/"$check_path".git)
    check_origin="${hop%/"$check_path"*}" ;;
esac
check_owner="${check_path%%/*}"
check_name="${check_path#*/}"
work=$(mktemp -d)
check_curl -o "$work/channel.json" "$check_origin/$check_path/raw/branch/main/channel.json"
# The channel is JSON; this image has no jq, so the one object is cut out of the flattened text.
flat=$(tr -d ' \t\r\n' < "$work/channel.json")
check_release() {
  entry=$(printf '%s' "$flat" | grep -o "\"$1\":{[^}]*}" | head -n 1 || true)
  [ -n "$entry" ] || return 1
  revision=$(printf '%s' "$entry" | grep -o '"revision":"[0-9a-f]\{40\}"' | cut -d'"' -f4)
  sha256=$(printf '%s' "$entry" | grep -o '"sha256":"[0-9a-f]\{64\}"' | cut -d'"' -f4)
  [ -n "$revision" ] && [ -n "$sha256" ]
}
check_release "$CHECK_CLASS" || check_release default || { check_log "the channel names no ccid release"; exit 1; }
triple="$(uname -m)-unknown-linux-gnu"
check_curl -o "$work/ccid" "$check_origin/api/packages/$check_owner/generic/$check_name/$revision/ccid-$triple"
printf '%s  %s\n' "$sha256" "$work/ccid" | sha256sum --check --strict --status \
  || { check_log "ccid $revision is not the release the channel names (sha256 differs)"; exit 1; }
chmod 0555 "$work/ccid"
# The release is linked against a Nix glibc: fetch exactly that one from the public Nix cache when this
# image does not have it.
loader=$(grep -a -o -m 1 '/nix/store/[a-z0-9]\{32\}-glibc-[^/ ]*/lib/ld-linux[a-z0-9_.-]*\.so\.[0-9]' "$work/ccid" | head -n 1 || true)
if [ -n "$loader" ] && [ ! -x "$loader" ]; then
  nix-store --realise "${loader%/lib/*}" > /dev/null \
    || { check_log "cannot fetch the loader $loader from the Nix cache"; exit 1; }
fi
export NIX_CONFIG='experimental-features = nix-command flakes'
export CCID_CHANNEL_FILE="$work/channel.json"
export CI_CACHE_ROOT="${CI_CACHE_ROOT:-${TMPDIR:-/tmp}/ccid-mirror-state}"
mkdir -p "$CI_CACHE_ROOT"
git config --global --add safe.directory "$PWD" 2> /dev/null || true
rc=0
for job in $CHECK_JOBS; do
  check_log "ccid $revision runs the job $job"
  # No stdin: when this script is read from it (the GitHub step pipes it into the container), a check must not eat the rest.
  "$work/ccid" job-run --mirror --job "$job" < /dev/null || rc=$?
done
rm -r "$work"
exit "$rc"
