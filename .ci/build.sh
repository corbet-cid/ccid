#!/usr/bin/env bash
set -euo pipefail
ci_mode=${1:-${MODE:-build}}
export CCID_SOURCE_REVISION="${CI_COMMIT_SHA:-unversioned}"
ci_output_root=${CCID_OUTPUT_ROOT:-${CARGO_HOME:-$HOME/.cargo}/ccid-artifacts}
if [[ -z ${CARGO_TARGET_DIR:-${CARGO_BUILD_TARGET_DIR:-}} ]]; then
  ci_namespace=$(python3 - <<'PY'
import hashlib,os,urllib.parse
url=os.environ.get('CI_REPOSITORY_URL') or 'https://github.com/corbet-labs/ccid'
parsed=urllib.parse.urlsplit(url)
if parsed.scheme != 'https' or not parsed.hostname or parsed.username or parsed.password:
    raise SystemExit('Bootstrap cache identity requires canonical HTTPS CI_REPOSITORY_URL')
identity=parsed.netloc.lower()+'/'+parsed.path.strip('/').removesuffix('.git')
print('ccid-'+hashlib.sha256(identity.encode()).hexdigest())
PY
)
  export CARGO_TARGET_DIR="${CI_CACHE_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/targets/$ci_namespace"
else
  export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$CARGO_BUILD_TARGET_DIR}"
fi
mkdir -p "$CARGO_TARGET_DIR/.ccid"
if [[ ${CCID_TARGET_LOCK_HELD:-} != "$(realpath "$CARGO_TARGET_DIR")" ]]; then
  exec 9>"$CARGO_TARGET_DIR/.ccid/lock"
  flock -w 60 9
fi
# This bootstrap can compile changed tool source into the selected warm target.
# Retire only this command library's own freshness metadata, never artifacts.
rm -f -- "$CARGO_TARGET_DIR/.ccid/source-state.json"
ci_revision_root="$ci_output_root/$CCID_SOURCE_REVISION"
if [[ $ci_mode != test ]]; then
  [[ $CCID_SOURCE_REVISION =~ ^[a-f0-9]{40}$ ]] || { echo 'Artifact publication requires an exact CI_COMMIT_SHA' >&2; exit 2; }
  mkdir -p "$ci_revision_root"
fi
if [[ $ci_mode == lock ]]; then
  cargo generate-lockfile
  ci_lock_file=$(mktemp "$ci_revision_root/.Cargo.lock.XXXXXX")
  trap 'rm -f -- "$ci_lock_file"' EXIT
  cp Cargo.lock "$ci_lock_file"
  if [[ -e $ci_revision_root/Cargo.lock ]]; then
    cmp "$ci_lock_file" "$ci_revision_root/Cargo.lock"
  else
    mv -n "$ci_lock_file" "$ci_revision_root/Cargo.lock"
  fi
  sha256sum "$ci_revision_root/Cargo.lock"
  exit 0
fi
[[ $ci_mode == build || $ci_mode == test ]] || { echo 'MODE must be lock, test or build' >&2; exit 2; }
ci_target=$(rustc -vV | sed -n 's/^host: //p')
[[ -n $ci_target ]] || { echo 'Missing native Rust target' >&2; exit 2; }
rustc --version
python3 .ci/registry_publish_test.py
python3 .ci/test_hosted_budget.py
if [[ $ci_mode == test ]]; then
  PYTHONPATH=. python3 .ci/release_publish_test.py
fi
cargo fmt --all -- --check
cargo test --release --locked --target "$ci_target"
[[ $ci_mode != test ]] || exit 0
cargo build --release --locked --target "$ci_target"
ci_binary="$CARGO_TARGET_DIR/$ci_target/release/ccid"
test "$("$ci_binary" source-revision)" = "$CI_COMMIT_SHA"
"$ci_binary" check --repo "$PWD" --check test --plan
ci_stage=$(mktemp -d "$ci_revision_root/.$ci_target.XXXXXX")
trap 'rm -rf -- "$ci_stage"' EXIT
install -m 0755 "$ci_binary" "$ci_stage/ccid"
python3 - "$CI_COMMIT_SHA" "$ci_target" "$ci_stage" <<'PY'
import hashlib,json,pathlib,sys
revision,target,directory=sys.argv[1:]
root=pathlib.Path(directory)
digest=hashlib.file_digest((root/'ccid').open('rb'),'sha256').hexdigest()
(root/'receipt.json').write_text(json.dumps({'source_revision':revision,'target':target,'binary_sha256':digest},sort_keys=True)+'\n')
PY
ci_destination="$ci_revision_root/$ci_target"
if [[ -e $ci_destination ]]; then
  cmp "$ci_stage/ccid" "$ci_destination/ccid"
  cmp "$ci_stage/receipt.json" "$ci_destination/receipt.json"
else
  mv -T -n "$ci_stage" "$ci_destination"
fi
cat "$ci_destination/receipt.json"
