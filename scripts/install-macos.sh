#!/usr/bin/env bash
# Install prebuilt ninox (Apple silicon macOS) from Synthesia's CodeArtifact.
#
# Downloads the `ninox-macos` generic package that publish-codeartifact.yml
# publishes on every release, verifies each asset against the SHA-256
# CodeArtifact recorded at publish time, installs `ninox` + an `nx` symlink,
# and optionally Ninox.app. Coordinates and asset names mirror
# crates/ninox-core/src/lifecycle/binary_update.rs — change both together.
#
# Usage: scripts/install-macos.sh [--version X.Y.Z] [--app | --no-app] [--user-apps]
#
# Environment:
#   NINOX_INSTALL_DIR               binary install dir (default ~/.local/bin)
#   NINOX_CODEARTIFACT_DOMAIN       default synthesia-build
#   NINOX_CODEARTIFACT_DOMAIN_OWNER default: the account of your AWS session
#   NINOX_CODEARTIFACT_REPOSITORY   default synthesia-cargo
#   NINOX_CODEARTIFACT_REGION       default eu-west-1
#   AWS_PROFILE                     the SSO profile to use, as for any aws call
set -euo pipefail

DOMAIN="${NINOX_CODEARTIFACT_DOMAIN:-synthesia-build}"
DOMAIN_OWNER="${NINOX_CODEARTIFACT_DOMAIN_OWNER:-}"
REPOSITORY="${NINOX_CODEARTIFACT_REPOSITORY:-synthesia-cargo}"
REGION="${NINOX_CODEARTIFACT_REGION:-eu-west-1}"
INSTALL_DIR="${NINOX_INSTALL_DIR:-$HOME/.local/bin}"
NAMESPACE=ninox
PACKAGE=ninox-macos
TRIPLE=aarch64-apple-darwin

version=""
want_app=""
apps_dir=/Applications

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
warn() { printf 'warning: %s\n' "$*" >&2; }
info() { printf '%s\n' "$*"; }

usage() {
  sed -n '2,/^set -euo/p' "$0" | sed -e '/^set -euo/d' -e 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) [ $# -ge 2 ] || die "--version needs a value"; version="${2#v}"; shift 2 ;;
    --version=*) version="${1#--version=}"; version="${version#v}"; shift ;;
    --app) want_app=yes; shift ;;
    --no-app) want_app=no; shift ;;
    --user-apps) apps_dir="$HOME/Applications"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
done

# Rosetta shells report x86_64 from uname -m, so also ask the hardware.
if [ "$(uname -s)" != Darwin ] || { [ "$(uname -m)" != arm64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" != 1 ]; }; then
  die "prebuilt ninox binaries are only published for Apple silicon macOS ($TRIPLE).
Build from source instead: cargo install ninox   (or: cargo install --registry synthesia-cargo ninox)"
fi

command -v aws >/dev/null 2>&1 || die "the aws CLI is required (brew install awscli)"

if ! account=$(aws sts get-caller-identity --query Account --output text 2>/dev/null); then
  die "no valid AWS session. Run: aws sso login${AWS_PROFILE:+ --profile $AWS_PROFILE}
(use the profile for the account that owns the $DOMAIN CodeArtifact domain, e.g. AWS_PROFILE=<profile> $0)"
fi
DOMAIN_OWNER="${DOMAIN_OWNER:-$account}"

ca() {
  local sub="$1"; shift
  AWS_PAGER="" aws codeartifact "$sub" \
    --domain "$DOMAIN" --domain-owner "$DOMAIN_OWNER" --repository "$REPOSITORY" \
    --region "$REGION" --format generic --namespace "$NAMESPACE" --package "$PACKAGE" "$@"
}

owner_hint="looked in $DOMAIN/$REPOSITORY ($REGION) owned by account $DOMAIN_OWNER; if your SSO profile is a different AWS account than the CodeArtifact domain owner, set NINOX_CODEARTIFACT_DOMAIN_OWNER=<owner account id> or switch AWS_PROFILE"

if [ -z "$version" ]; then
  # The API only sorts by publish time; pick the highest version instead.
  # Prereleases (X.Y.Z-pre) install only via --version, as with `ninox update`.
  versions=$(ca list-package-versions --status Published --query 'versions[].version' --output text) \
    || die "could not list $PACKAGE versions.
$owner_hint"
  version=$(printf '%s\n' "$versions" | tr '\t' '\n' | sed '/^$/d' | grep -v -- - | sort -V | tail -n 1 || true)
  [ -n "$version" ] && [ "$version" != None ] || die "no published $PACKAGE versions found.
$owner_hint"
fi
info "Installing ninox $version ($TRIPLE)"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/ninox-install.XXXXXX")
staging=""
new_bin="$INSTALL_DIR/.ninox.install-$$"
cleanup() {
  rm -rf "$tmp"
  rm -f "$new_bin"
  if [ -n "$staging" ]; then rm -rf "$staging"; fi
}
trap cleanup EXIT

fetch() {
  local asset="$1" expected actual
  expected=$(ca list-package-version-assets --package-version "$version" \
    --query "assets[?name=='$asset'].hashes.\"SHA-256\" | [0]" --output text) \
    || die "could not look up $asset for $PACKAGE $version.
$owner_hint"
  [ -n "$expected" ] && [ "$expected" != None ] || die "$PACKAGE $version has no $asset asset"
  info "Downloading $asset"
  ca get-package-version-asset --package-version "$version" --asset "$asset" "$tmp/$asset" >/dev/null
  actual=$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)
  expected=$(printf '%s' "$expected" | tr '[:upper:]' '[:lower:]')
  [ "$actual" = "$expected" ] || die "checksum mismatch for $asset: expected $expected, got $actual"
}

stem="ninox-${version}-${TRIPLE}"
fetch "$stem.tar.gz"
tar -xzf "$tmp/$stem.tar.gz" -C "$tmp" "$stem/ninox"
{ [ -f "$tmp/$stem/ninox" ] && [ ! -L "$tmp/$stem/ninox" ] && [ -x "$tmp/$stem/ninox" ]; } \
  || die "$stem.tar.gz has no executable regular-file ninox"

mkdir -p "$INSTALL_DIR"
cp "$tmp/$stem/ninox" "$new_bin"
chmod 755 "$new_bin"
xattr -d com.apple.quarantine "$new_bin" 2>/dev/null || true
mv -f "$new_bin" "$INSTALL_DIR/ninox"
info "Installed $INSTALL_DIR/ninox"

nx="$INSTALL_DIR/nx"
if [ -L "$nx" ] && [ "$(readlink "$nx")" = ninox ]; then
  :
elif [ -e "$nx" ] || [ -L "$nx" ]; then
  warn "$nx exists and is not ninox's alias; leaving it alone (bare \`nx\` won't open the ninox TUI)"
else
  ln -s ninox "$nx"
  info "Linked $nx -> ninox"
fi

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) warn "$INSTALL_DIR is not on your PATH. Add it, e.g.:
  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.zshrc" ;;
esac

# First `ninox` on PATH, without relying on the shell's command hash.
first_on_path() {
  local dir IFS=:
  for dir in $PATH; do
    if [ -x "$dir/$1" ] && [ ! -d "$dir/$1" ]; then printf '%s\n' "$dir/$1"; return 0; fi
  done
  return 1
}
winner=$(first_on_path ninox || true)
if [ -n "$winner" ] && [ "$winner" != "$INSTALL_DIR/ninox" ]; then
  warn "\`ninox\` on your PATH resolves to $winner, not $INSTALL_DIR/ninox.
Remove it (cargo uninstall ninox, for a ~/.cargo/bin copy) or put $INSTALL_DIR earlier on PATH."
elif [ -x "$HOME/.cargo/bin/ninox" ] && [ "$INSTALL_DIR" != "$HOME/.cargo/bin" ]; then
  info "Note: a cargo-installed ~/.cargo/bin/ninox also exists; $INSTALL_DIR/ninox wins on PATH. \`cargo uninstall ninox\` removes the other."
fi
nx_winner=$(first_on_path nx || true)
if [ -n "$nx_winner" ] && [ "$nx_winner" != "$nx" ]; then
  warn "\`nx\` on your PATH resolves to $nx_winner (another program), not ninox."
fi

if [ -z "$want_app" ]; then
  if [ -t 0 ] && [ -t 1 ]; then
    printf 'Also install Ninox.app into %s? [y/N] ' "$apps_dir"
    read -r reply || reply=""
    case "$reply" in [yY]*) want_app=yes ;; *) want_app=no ;; esac
  else
    want_app=no
  fi
fi

if [ "$want_app" = yes ]; then
  mkdir -p "$apps_dir"
  [ -w "$apps_dir" ] || die "$apps_dir is not writable; re-run with --user-apps to install into ~/Applications"
  fetch Ninox.app.zip
  # Staged beside the destination so the final swap is a same-volume rename.
  staging=$(mktemp -d "$apps_dir/.ninox-install.XXXXXX")
  ditto -x -k "$tmp/Ninox.app.zip" "$staging"
  [ -d "$staging/Ninox.app" ] || die "Ninox.app.zip has no Ninox.app"
  xattr -dr com.apple.quarantine "$staging/Ninox.app" 2>/dev/null || true
  dest="$apps_dir/Ninox.app"
  if [ -e "$dest" ]; then
    # Outside $staging so the EXIT trap can never delete the only copy.
    old="$apps_dir/.Ninox.app.old-$$"
    mv "$dest" "$old"
    if ! mv "$staging/Ninox.app" "$dest"; then
      mv "$old" "$dest" || die "could not install $dest, and restoring the previous copy failed; it is at $old — move it back by hand"
      die "could not install $dest (previous copy restored)"
    fi
    rm -rf "$old" || warn "installed $dest but could not remove the previous copy at $old; delete it by hand"
  else
    mv "$staging/Ninox.app" "$dest"
  fi
  info "Installed $dest"
fi

info "Done: ninox $version. Update later with \`ninox update\` (add --app to refresh Ninox.app)."
