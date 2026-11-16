#!/usr/bin/env bash
# Build, ad-hoc sign and package the macOS alpha assets into DIST_DIR:
#   install-macos.sh, ninox-<v>-aarch64-apple-darwin.tar.gz, Ninox.app.zip,
#   alpha-metadata.json
# Run on Apple silicon from a checkout of the commit to publish. The version
# is stamped into the working tree (see stamp-alpha-version.sh) and NOT
# reverted — use a throwaway checkout or `git checkout .` afterwards.
#
# Usage: scripts/package-macos-alpha.sh VERSION DIST_DIR
set -euo pipefail

version="${1:?usage: $0 X.Y.Z-alpha.N DIST_DIR}"
dist="${2:?usage: $0 X.Y.Z-alpha.N DIST_DIR}"
mkdir -p "$dist"
dist="$(cd "$dist" && pwd)"
cd "$(dirname "$0")/.."

[ "$(uname -s)-$(uname -m)" = Darwin-arm64 ] || { echo "needs Apple silicon macOS" >&2; exit 1; }

scripts/stamp-alpha-version.sh "$version"
cargo build --release -p ninox-gui -p ninox-tui
cargo bundle --release -p ninox-gui --format osx
app=target/release/bundle/osx/Ninox.app
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"

stem="ninox-${version}-aarch64-apple-darwin"
stage="$(mktemp -d)/$stem"
mkdir -p "$stage"
for bin in ninox-gui ninox-tui; do
  [ "$(lipo -archs "target/release/$bin")" = arm64 ]
  cp "target/release/$bin" "$stage/$bin"
  codesign --force --sign - "$stage/$bin"
  codesign --verify --strict "$stage/$bin"
  [ "$("$stage/$bin" --version | tail -n1)" = "ninox $version" ]
done
cp LICENSE "$stage/LICENSE"
printf 'ninox %s (aarch64-apple-darwin), alpha channel\n\nContains ninox-gui and ninox-tui. Install with scripts/install-macos.sh --channel alpha.\n' \
  "$version" > "$stage/README.txt"
COPYFILE_DISABLE=1 tar -C "$(dirname "$stage")" -czf "$dist/${stem}.tar.gz" "$stem"
cp scripts/install-macos.sh "$dist/install-macos.sh"
(cd target/release/bundle/osx && ditto -c -k --sequesterRsrc --keepParent Ninox.app "$dist/Ninox.app.zip")
printf '{"version":"%s","commit":"%s","ref":"%s","built_by":"%s"}\n' \
  "$version" "$(git rev-parse HEAD)" "$(git rev-parse --abbrev-ref HEAD)" "${GITHUB_RUN_ID:-local}" > "$dist/alpha-metadata.json"
ls -l "$dist"
