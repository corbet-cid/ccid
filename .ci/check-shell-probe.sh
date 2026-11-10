#!/usr/bin/env bash
# The rendered check-shell's runtime contract: what it links runs on its own.
#
# Inside the shell (src/render/check-shell.nix, entered with nix-shell exactly as
# CI enters a rendered .ci/check-shell.nix) this links three probes against
# libc++:
#   - a C++ program through the shell's C++ driver;
#   - a Rust binary with a C++ dependency through the link the environment's
#     cargo configuration selects;
#   - the same binary through rustc's own default link (`cc` as the driver; on
#     x86_64 Linux rustc >= 1.90 that is its bundled rust-lld).
# Each must have a RUNPATH whose every entry exists, none of them inside the
# shell's own $out (a shell has no output), and must start and print its line
# under `env -i`: no LD_LIBRARY_PATH, nothing from the shell's environment.
#
# A persistent target directory too: the Rust binary is first linked in a
# PREVIOUS shell (this shell file with one more runtime library directory,
# which never exists), then built again in this shell into the same target
# directory. Unchanged sources must not keep the previous link: the shell's
# link environment has to reach Cargo's freshness check, or every existing
# target keeps binaries linked against a shell that is gone.
#
# The Rust probes run without a compiler wrapper (RUSTC_WRAPPER unset): the
# check proves what the linker writes, not what a cache holds.
#
# PROBE_CHECK_SHELL names another shell file (default: the template ccid renders).
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
probe="$root/.ci/check-shell-probe"
shell=${PROBE_CHECK_SHELL:-$root/src/render/check-shell.nix}

if [ "${PROBE_IN_SHELL:-}" != 1 ]; then
  command -v nix-shell >/dev/null || { echo "check-shell-probe: nix-shell is not on PATH" >&2; exit 2; }
  work=$(mktemp -d "${TMPDIR:-/tmp}/check-shell-probe.XXXXXXXX")
  trap 'rm -rf -- "$work"' EXIT
  cp -R "$probe/rust" "$work/rust"
  previous="$work/previous-shell.nix"
  sed "s|^  rpathDirs = lib.unique (|  rpathDirs = [ \"$work/previous-shell/lib\" ] ++ lib.unique (|" "$shell" >"$previous"
  if cmp -s "$shell" "$previous"; then
    echo "check-shell-probe: cannot derive a previous shell from $shell (no rpathDirs line)" >&2
    exit 2
  fi
  PROBE_IN_SHELL=1 PROBE_WORK=$work PROBE_PREVIOUS=1 nix-shell "$previous" --run "$(printf 'bash %q' "${BASH_SOURCE[0]}")"
  trap - EXIT
  PROBE_IN_SHELL=1 PROBE_WORK=$work exec nix-shell "$shell" --run "$(printf 'bash %q' "${BASH_SOURCE[0]}")"
fi

work=${PROBE_WORK:?check-shell-probe: PROBE_WORK is not set}
unset RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER

# libc++'s headers: the shell puts <libcxx-dev>/include on the include path; the C++ headers live in c++/v1.
libcxx_include=""
for word in ${NIX_CFLAGS_COMPILE:-}; do
  case "$word" in
    */*-libcxx-*-dev/include) [ -d "$word/c++/v1" ] && libcxx_include="$word/c++/v1" ;;
  esac
done
[ -n "$libcxx_include" ] || { echo "check-shell-probe: no libc++ headers in NIX_CFLAGS_COMPILE" >&2; exit 2; }
export PROBE_LIBCXX_INCLUDE="$libcxx_include"

if [ "${PROBE_PREVIOUS:-}" = 1 ]; then
  # The previous shell: link into the persistent target and stop.
  cd "$work/rust"
  cargo build --locked --quiet --target-dir "$work/target-persistent"
  readelf -d "$work/target-persistent/debug/check-shell-probe" | grep -q -F "$work/previous-shell/lib" || {
    echo "check-shell-probe: the previous shell did not reach the link (no $work/previous-shell/lib in the RUNPATH)" >&2
    exit 2
  }
  exit 0
fi
trap 'rm -rf -- "$work"' EXIT
failures=0
fail() { echo "check-shell-probe: FAIL $*" >&2; failures=$((failures + 1)); }

check() { # label binary expected-output
  local label=$1 binary=$2 expected=$3 runpath entry output status
  if [ ! -x "$binary" ]; then fail "$label: $binary was not built"; return; fi
  runpath=$(readelf -d "$binary" | sed -n 's/.*(R\(UN\)\{0,1\}PATH).*\[\(.*\)\]/\2/p')
  echo "check-shell-probe: $label RUNPATH ${runpath:-<none>}"
  local IFS=:
  for entry in $runpath; do
    [ -d "$entry" ] || fail "$label: RUNPATH entry $entry does not exist"
    case "$entry" in
      "${out:-/nonexistent-shell-out}"|"${out:-/nonexistent-shell-out}"/*) fail "$label: RUNPATH entry $entry is inside the shell's \$out" ;;
    esac
  done
  unset IFS
  status=0
  output=$(env -i "$binary" 2>&1) || status=$?
  if [ "$status" != 0 ] || [ "$output" != "$expected" ]; then
    fail "$label: env -i $binary exited $status: $output"
  else
    echo "check-shell-probe: $label runs under env -i: $output"
  fi
}

"${CXX:-c++}" -nostdinc++ -isystem "$libcxx_include" "$probe/main.cpp" -nostdlib++ -lc++ -lc++abi -o "$work/cxx-probe"
check "c++ ($CXX)" "$work/cxx-probe" "check-shell probe: c++ ok"

cd "$work/rust"
cargo build --locked --quiet --target-dir "$work/target-configured"
check "rust (configured link)" "$work/target-configured/debug/check-shell-probe" "check-shell probe: rust ok (6)"
host=$(rustc -vV | sed -n 's/^host: //p')
cargo build --locked --quiet --config "target.$host.linker = \"cc\"" --target-dir "$work/target-default"
check "rust (rustc's default link)" "$work/target-default/debug/check-shell-probe" "check-shell probe: rust ok (6)"
cargo build --locked --quiet --target-dir "$work/target-persistent"
check "rust (persistent target, linked first in a previous shell)" "$work/target-persistent/debug/check-shell-probe" "check-shell probe: rust ok (6)"

if [ "$failures" != 0 ]; then
  echo "check-shell-probe: $failures failure(s): a binary linked in this shell does not run on its own" >&2
  exit 1
fi
echo "check-shell-probe: ok"
