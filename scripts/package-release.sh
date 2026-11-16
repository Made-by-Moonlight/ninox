#!/usr/bin/env bash
# Pack already-built release binaries into the tarball that both
# `cargo binstall` and `ninox update` download:
#   ninox-<version>-<target>.tar.gz  ->  ninox-<version>-<target>/
#     <each BIN>   (ninox-gui and/or ninox-tui)
#     nx           (copy of ninox-tui, when ninox-tui is packed)
#     LICENSE, README.txt
# The name and layout must match [package.metadata.binstall] in
# crates/ninox-{gui,tui}/Cargo.toml (see crates/ninox-ui-shared/tests/binstall_metadata.rs).
#
# Usage: scripts/package-release.sh VERSION TARGET BIN_DIR DIST_DIR BIN...
set -euo pipefail

[ $# -ge 5 ] || { sed -n '2,/^set -euo/p' "$0" | sed -e '/^set -euo/d' -e 's/^# \{0,1\}//' >&2; exit 2; }
version="$1" target="$2" bin_dir="$3" dist="$4"; shift 4
mkdir -p "$dist"
dist="$(cd "$dist" && pwd)"
root="$(cd "$(dirname "$0")/.." && pwd)"

stem="ninox-${version}-${target}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
stage="$work/$stem"
mkdir -p "$stage"
for bin in "$@"; do
  [ -f "$bin_dir/$bin" ] || { echo "missing binary: $bin_dir/$bin" >&2; exit 1; }
  cp "$bin_dir/$bin" "$stage/$bin"
done
# cargo installs `nx` as a second bin of ninox-tui, so binstall requires it in
# the archive. A regular-file copy: binstall does not see hard-link entries.
[ ! -f "$stage/ninox-tui" ] || cp "$stage/ninox-tui" "$stage/nx"
cp "$root/LICENSE" "$stage/LICENSE"
# shellcheck disable=SC2016
printf 'ninox %s (%s)\n\nContains: %s\nInstall with `cargo binstall`, scripts/install-macos.sh, or `ninox update`.\n' \
  "$version" "$target" "$*" > "$stage/README.txt"
COPYFILE_DISABLE=1 tar -C "$work" -czf "$dist/${stem}.tar.gz" "$stem"
echo "$dist/${stem}.tar.gz"
