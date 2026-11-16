#!/usr/bin/env bash
# Stamp an alpha version (X.Y.Z-alpha.N) into every workspace crate in the
# current checkout — the package versions and the inter-crate path pins — so a
# CI/local build reports it from `ninox --version`. The result is never
# committed. Fails unless `cargo metadata` then reports exactly that version
# for every workspace member.
#
# Usage: scripts/stamp-alpha-version.sh 1.0.0-alpha.7
set -euo pipefail

version="${1:-}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+-alpha\.[0-9]+$ ]] \
  || { echo "usage: $0 X.Y.Z-alpha.N (got '${version}')" >&2; exit 2; }

cd "$(dirname "$0")/.."

python3 - "$version" <<'PY'
import pathlib, re, sys

new = sys.argv[1]
manifests = sorted(pathlib.Path("crates").glob("*/Cargo.toml"))
core = next(m for m in manifests if m.parent.name == "ninox-core")
old = re.search(r'^version\s*=\s*"([^"]+)"', core.read_text(), re.M).group(1)

for m in manifests:
    out = []
    for line in m.read_text().splitlines(keepends=True):
        is_pkg_version = re.match(r'version\s*=\s*"' + re.escape(old) + '"', line)
        is_internal_pin = 'path = "../ninox-' in line and f'version = "{old}"' in line
        if is_pkg_version or is_internal_pin:
            line = line.replace(f'"{old}"', f'"{new}"')
        out.append(line)
    m.write_text("".join(out))
PY

# Cargo.lock's entries for the workspace members are rewritten by the build
# that follows (don't build with --locked); no dependency versions move.
bad=$(cargo metadata --no-deps --format-version 1 \
  | jq -r --arg v "$version" '.packages[] | select(.version != $v) | "\(.name) \(.version)"')
if [ -n "$bad" ]; then
  echo "stamping failed — these crates are not at $version:" >&2
  echo "$bad" >&2
  exit 1
fi
count=$(cargo metadata --no-deps --format-version 1 | jq '.packages | length')
echo "stamped $count workspace crates at $version"
