#!/usr/bin/env bash
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
floss="$root/third_party/floss"
state="${DORSCHE_FLOSS_STATE:-$root/.cache/floss}"
jobs="${DORSCHE_BUILD_JOBS:-$(nproc)}"
command="${1:-build}"

usage() {
  cat <<'EOF'
Usage: scripts/floss/build.sh <bootstrap|build|test|clean|env>

Environment:
  DORSCHE_FLOSS_STATE  Out-of-tree staging/output directory
  DORSCHE_BUILD_JOBS   Parallel jobs

The script deliberately invokes AOSP's build.py without patching Floss.
Bootstrap only reports missing apt/cargo packages; it never invokes sudo.
EOF
}

case "$command" in
  bootstrap)
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
    python3 "$floss/build.py" \
      --bootstrap-dir "$state" \
      --notest \
      --jobs "$jobs" \
      --target all
    ;;
  test)
    python3 "$floss/build.py" \
      --bootstrap-dir "$state" \
      --jobs "$jobs" \
      --target test
    ;;
  clean)
    python3 "$floss/build.py" --bootstrap-dir "$state" --target clean
    ;;
  env)
    python3 "$floss/build.py" --bootstrap-dir "$state" --print-env
    ;;
  -h|--help|help)
    usage
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
