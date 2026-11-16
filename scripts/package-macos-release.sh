#!/usr/bin/env bash
# Build, ad-hoc sign and package the stable macOS release tarball that
# `cargo binstall ninox-gui ninox-tui` downloads (see package-release.sh), for
# attaching by hand to the GitHub release until CI has a macOS runner. Run on
# Apple silicon from a checkout of the release tag. Builds and packs only —
# it prints the upload command and never runs it.
#
# Usage: scripts/package-macos-release.sh DIST_DIR
set -euo pipefail

dist="${1:?usage: $0 DIST_DIR}"
cd "$(dirname "$0")/.."

[ "$(uname -s)-$(uname -m)" = Darwin-arm64 ] || { echo "needs Apple silicon macOS" >&2; exit 1; }

version=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name=="ninox-tui") | .version')
tag="v${version}"
[ "$(git describe --tags --exact-match 2>/dev/null || true)" = "$tag" ] \
  || { echo "HEAD is not tagged $tag; check out the release tag first" >&2; exit 1; }

cargo build --release --locked -p ninox-gui -p ninox-tui
target=aarch64-apple-darwin
for bin in ninox-gui ninox-tui; do
  [ "$(lipo -archs "target/release/$bin")" = arm64 ] || { echo "$bin is not arm64" >&2; exit 1; }
  codesign --force --sign - "target/release/$bin"
  codesign --verify --strict "target/release/$bin"
  [ "$("target/release/$bin" --version | tail -n1)" = "ninox $version" ] \
    || { echo "$bin does not report ninox $version" >&2; exit 1; }
done

tarball=$(scripts/package-release.sh "$version" "$target" target/release "$dist" ninox-gui ninox-tui)
echo "packaged $tarball"
echo "to attach it (not run here): gh release upload $tag \"$tarball\" --repo <owner>/<repo>"
