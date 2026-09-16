#!/usr/bin/env bash
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
floss="$root/third_party/floss"
state="${DORSCHE_FLOSS_STATE:-$root/.cache/floss}"
jobs="${DORSCHE_BUILD_JOBS:-$(nproc)}"
command="${1:-build}"

# The pinned Floss revision predates Clang 18's dedicated diagnostic for C++
# variable-length arrays. Keep upstream's global -Werror policy, but do not let
# that newly split host-compiler diagnostic break otherwise accepted code.
export PATH="$root/scripts/floss/host-tools:$PATH"

# bindgen (used by uhidrs-sys) needs the shared libclang directory, which is
# versioned and is not on Ubuntu's default loader search path.
if [[ -z "${LIBCLANG_PATH:-}" ]]; then
  for llvm_config in llvm-config-18 llvm-config; do
    if command -v "$llvm_config" >/dev/null 2>&1; then
      export LIBCLANG_PATH="$("$llvm_config" --libdir)"
      break
    fi
  done
fi

usage() {
  cat <<'EOF'
Usage: scripts/floss/build.sh <bootstrap|build|test|clean|env>

Environment:
  DORSCHE_FLOSS_STATE  Out-of-tree staging/output directory
  DORSCHE_BUILD_JOBS   Parallel jobs
  LIBCLANG_PATH        libclang directory for Rust bindgen (auto-detected)
  DORSCHE_HOST_CXX     Real C++ compiler behind the Clang compatibility shim

The script invokes AOSP's build.py without modifying the pristine import.
Host-only Cargo compatibility changes are applied to the staging cache.
Bootstrap only reports missing apt/cargo packages; it never invokes sudo.
EOF
}

ensure_host_overlay() {
  local staged="$state/staging/bt"
  local overlay="$state/staging/bt.host-overlay"
  local marker="$staged/.dorsche-host-overlay"
  local revision
  local overlay_id
  revision="$(sed -n 's/^revision=//p' "$root/vendor/floss.lock")"
  overlay_id="$revision:$(sha256sum "$root"/scripts/floss/patches/*.patch | cut -d' ' -f1 | sha256sum | cut -d' ' -f1)"

  if [[ -f "$marker" ]] && [[ "$(<"$marker")" == "$overlay_id" ]]; then
    return
  fi

  # build.py normally points staging/bt directly at the pristine import. Make
  # a cheap reflink-capable cache copy so host-only Cargo feature fixes never
  # mutate third_party/floss. The protocol-stack source remains byte-identical.
  rm -rf "$overlay"
  mkdir -p "$overlay"
  cp -a --reflink=auto "$floss/." "$overlay/"
  rm -f "$overlay/Cargo.lock"
  patch --directory="$overlay" --strip=1 \
    < "$root/scripts/floss/patches/0001-syn-extra-traits.patch"
  patch --directory="$overlay" --strip=1 \
    < "$root/scripts/floss/patches/0002-linux-hci-driver-altsetting.patch"
  printf '%s\n' "$overlay_id" > "$overlay/.dorsche-host-overlay"
  rm -rf "$staged"
  mv "$overlay" "$staged"
}

ensure_pristine_staging_link() {
  mkdir -p "$state/staging"
  rm -rf "$state/staging/bt"
  ln -s "$floss" "$state/staging/bt"
}

case "$command" in
  bootstrap)
    ensure_pristine_staging_link
    python3 "$floss/build.py" \
      --bootstrap-dir "$state" \
      --run-bootstrap \
      --partial-staging
    ;;
  build)
    [[ -e "$state/.setup-complete" ]] || {
      printf 'Floss is not staged. Run: %s bootstrap\n' "$0" >&2
      exit 2
    }
    ensure_host_overlay
    python3 "$state/staging/bt/build.py" \
      --bootstrap-dir "$state" \
      --no-vendored-rust \
      --notest \
      --jobs "$jobs" \
      --target all
    ;;
  test)
    [[ -e "$state/.setup-complete" ]] || {
      printf 'Floss is not staged. Run: %s bootstrap\n' "$0" >&2
      exit 2
    }
    ensure_host_overlay
    python3 "$state/staging/bt/build.py" \
      --bootstrap-dir "$state" \
      --no-vendored-rust \
      --jobs "$jobs" \
      --target test
    ;;
  clean)
    python3 "$state/staging/bt/build.py" --bootstrap-dir "$state" --target clean
    ;;
  env)
    python3 "$state/staging/bt/build.py" --bootstrap-dir "$state" --print-env
    ;;
  -h|--help|help)
    usage
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
