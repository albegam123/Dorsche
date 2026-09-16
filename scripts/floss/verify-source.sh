#!/usr/bin/env bash
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
source_dir="$root/third_party/floss"
lock_file="$root/vendor/floss.lock"

expected="$(sed -n 's/^tree_sha256=//p' "$lock_file")"
actual="$({
  find "$source_dir" -type f -print0 \
    | LC_ALL=C sort -z \
    | xargs -0 sha256sum
} | sed "s#${source_dir}/#third_party/floss/#" | sha256sum | cut -d' ' -f1)"

if [[ "$actual" != "$expected" ]]; then
  printf 'Floss source differs from the pinned pristine tree.\n' >&2
  printf 'expected: %s\nactual:   %s\n' "$expected" "$actual" >&2
  exit 1
fi

printf 'Floss source verified: %s\n' "$actual"
