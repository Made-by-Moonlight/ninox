#!/usr/bin/env bash
# Install prebuilt ninox (Apple silicon macOS) from Synthesia's CodeArtifact.
#
# Downloads the `ninox-macos` generic package that publish-codeartifact.yml
# publishes on every release (or, with --channel alpha, the `ninox-macos-alpha`
# package that publish-alpha.yml publishes from the v1 branch), verifies each asset against the SHA-256
# CodeArtifact recorded at publish time, and installs both `ninox-gui` (the
# desktop app) and `ninox-tui` (the terminal app), plus a `ninox` alias
# (symlinked to ninox-gui — bare `ninox` has always opened the desktop app)
# and an `nx` alias (symlinked to ninox-tui — bare `nx` has always opened
# the terminal UI), and optionally Ninox.app. Coordinates and asset names
# mirror crates/ninox-core/src/lifecycle/binary_update.rs — change both
# together.
#
# Usage: scripts/install-macos.sh [--channel stable|alpha] [--version X.Y.Z] [--app | --no-app] [--user-apps]
#
#   --channel   stable (default) installs the newest release; alpha installs the
#               newest alpha. Also persist the choice for `ninox update` with
#               `ninox update --channel alpha`.
#
# Environment:
#   NINOX_INSTALL_DIR               binary install dir (default ~/.local/bin)
#   NINOX_CODEARTIFACT_DOMAIN       default synthesia-build
#   NINOX_CODEARTIFACT_DOMAIN_OWNER default: from the CodeArtifact registry URL in
#                                   ~/.cargo/config.toml, else your AWS session's account
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

channel=stable
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
    --channel) [ $# -ge 2 ] || die "--channel needs a value"; channel="$2"; shift 2 ;;
    --channel=*) channel="${1#--channel=}"; shift ;;
    --version) [ $# -ge 2 ] || die "--version needs a value"; version="${2#v}"; shift 2 ;;
    --version=*) version="${1#--version=}"; version="${version#v}"; shift ;;
    --app) want_app=yes; shift ;;
    --no-app) want_app=no; shift ;;
    --user-apps) apps_dir="$HOME/Applications"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
done

case "$channel" in
  stable) ;;
  alpha) PACKAGE=ninox-macos-alpha ;;
  *) die "unknown channel: $channel (expected stable or alpha)" ;;
esac

# Test hook: report the parsed arguments without touching AWS or the machine.
if [ -n "${NINOX_INSTALL_PARSE_ONLY:-}" ]; then
  printf 'channel=%s package=%s version=%s app=%s apps_dir=%s\n' "$channel" "$PACKAGE" "$version" "$want_app" "$apps_dir"
  exit 0
fi

# Rosetta shells report x86_64 from uname -m, so also ask the hardware.
if [ "$(uname -s)" != Darwin ] || { [ "$(uname -m)" != arm64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" != 1 ]; }; then
  die "prebuilt ninox binaries are only published for Apple silicon macOS ($TRIPLE).
On other platforms use cargo binstall ninox-tui (Linux x86_64/aarch64 prebuilts), or build from source with cargo install."
fi

command -v aws >/dev/null 2>&1 || die "the aws CLI is required (brew install awscli)"

if ! account=$(aws sts get-caller-identity --query Account --output text 2>/dev/null); then
  die "no valid AWS session. Run: aws sso login${AWS_PROFILE:+ --profile $AWS_PROFILE}
(use the profile for the account that owns the $DOMAIN CodeArtifact domain, e.g. AWS_PROFILE=<profile> $0)"
fi
if [ -z "$DOMAIN_OWNER" ]; then
  cargo_config="${CARGO_HOME:-$HOME/.cargo}/config.toml"
  [ -f "$cargo_config" ] || cargo_config="${CARGO_HOME:-$HOME/.cargo}/config"
  if [ -f "$cargo_config" ]; then
    DOMAIN_OWNER=$(grep -oE "${DOMAIN}-[0-9]{12}\.d\.codeartifact\." "$cargo_config" | head -n1 | sed -E "s/^${DOMAIN}-([0-9]{12}).*/\1/" || true)
  fi
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
  # Stable skips prereleases (X.Y.Z-pre), as with `ninox update`; the alpha
  # package holds only prereleases, ordered by sort -V (numeric runs compare
  # numerically, so alpha.10 > alpha.9).
  versions=$(ca list-package-versions --status Published --query 'versions[].version' --output text) \
    || die "could not list $PACKAGE versions.
$owner_hint"
  versions=$(printf '%s\n' "$versions" | tr '\t' '\n' | sed '/^$/d')
  [ "$channel" = stable ] && versions=$(printf '%s\n' "$versions" | grep -v -- - || true)
  version=$(printf '%s\n' "$versions" | sort -V | tail -n 1 || true)
  [ -n "$version" ] && [ "$version" != None ] || die "no published $PACKAGE versions found.
$owner_hint"
fi
info "Installing ninox $version ($TRIPLE, $channel channel)"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/ninox-install.XXXXXX")
staging=""
new_bins=()
cleanup() {
  rm -rf "$tmp"
  for b in "${new_bins[@]:-}"; do rm -f "$b"; done
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
mkdir -p "$INSTALL_DIR"

tar -xzf "$tmp/$stem.tar.gz" -C "$tmp" "$stem/ninox-gui" "$stem/ninox-tui"

install_bin() {
  local name="$1"
  { [ -f "$tmp/$stem/$name" ] && [ ! -L "$tmp/$stem/$name" ] && [ -x "$tmp/$stem/$name" ]; } \
    || die "$stem.tar.gz has no executable regular-file $name"
  local new_bin="$INSTALL_DIR/.$name.install-$$"
  new_bins+=("$new_bin")
  cp "$tmp/$stem/$name" "$new_bin"
  chmod 755 "$new_bin"
  xattr -d com.apple.quarantine "$new_bin" 2>/dev/null || true
  mv -f "$new_bin" "$INSTALL_DIR/$name"
  info "Installed $INSTALL_DIR/$name"
}
install_bin ninox-gui
install_bin ninox-tui

link_alias() {
  local alias_name="$1" target="$2" hint="$3"
  local link="$INSTALL_DIR/$alias_name"
  if [ -L "$link" ] && [ "$(readlink "$link")" = "$target" ]; then
    :
  elif [ -e "$link" ] || [ -L "$link" ]; then
    warn "$link exists and is not ninox's alias; leaving it alone ($hint)"
  else
    ln -s "$target" "$link"
    info "Linked $link -> $target"
  fi
}
# `ninox` has always opened the desktop app and `nx` the terminal UI —
# preserved here as fixed aliases to the matching binary, independent of
# install order.
link_alias ninox ninox-gui "bare \`ninox\` won't open the desktop app"
link_alias nx ninox-tui "bare \`nx\` won't open the ninox TUI"

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
Remove it (cargo uninstall ninox-gui, for a ~/.cargo/bin copy) or put $INSTALL_DIR earlier on PATH."
elif [ -x "$HOME/.cargo/bin/ninox-gui" ] && [ "$INSTALL_DIR" != "$HOME/.cargo/bin" ]; then
  info "Note: a cargo-installed ~/.cargo/bin/ninox-gui also exists; $INSTALL_DIR/ninox wins on PATH. \`cargo uninstall ninox-gui\` removes the other."
fi
nx_winner=$(first_on_path nx || true)
if [ -n "$nx_winner" ] && [ "$nx_winner" != "$INSTALL_DIR/nx" ]; then
  warn "\`nx\` on your PATH resolves to $nx_winner (another program), not ninox."
elif [ -x "$HOME/.cargo/bin/ninox-tui" ] && [ "$INSTALL_DIR" != "$HOME/.cargo/bin" ]; then
  info "Note: a cargo-installed ~/.cargo/bin/ninox-tui also exists. \`cargo uninstall ninox-tui\` removes it."
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

info "Done: ninox-gui + ninox-tui $version. Update later with \`ninox update\` (add --app to refresh Ninox.app)."
