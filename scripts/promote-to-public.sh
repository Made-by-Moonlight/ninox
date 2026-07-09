#!/usr/bin/env bash
set -euo pipefail

# Cherry-picks one or more commits from this (internal) checkout onto a
# branch based on the public mirror's main, then pushes it and opens a PR
# against the public repo — for promoting generic commits that landed
# directly on internal/main back out to Made-by-Moonlight/ninox.
#
# Usage:
#   scripts/promote-to-public.sh [--force] <commit>...
#   scripts/promote-to-public.sh [--force] <commit1>..<commit2>
#
# A <a>..<b> argument is expanded to the commits in between (oldest
# first, same order git log would list them); otherwise each commit is
# cherry-picked in the order given.
#
# Before pushing, this greps the diff being promoted for a short list of
# internal-only strings (the private remote's host alias, "synthesia",
# "codeartifact") and refuses to push if any show up — pass --force once
# you've confirmed a match is a false positive.
#
# Assumes remotes named "public" (Made-by-Moonlight/ninox) and "origin"
# (the internal mirror this script runs from) — override with the
# PUBLIC_REMOTE / INTERNAL_REMOTE env vars if your checkout names them
# differently.

PUBLIC_REMOTE="${PUBLIC_REMOTE:-public}"
INTERNAL_REMOTE="${INTERNAL_REMOTE:-origin}"

force=false
args=()
for arg in "$@"; do
  case "$arg" in
    --force) force=true ;;
    *) args+=("$arg") ;;
  esac
done

if [ "${#args[@]}" -eq 0 ]; then
  echo "usage: $0 [--force] <commit>... | <commit1>..<commit2>" >&2
  exit 1
fi

git fetch "$PUBLIC_REMOTE" main

commits=()
if [ "${#args[@]}" -eq 1 ] && [[ "${args[0]}" == *..* ]]; then
  while IFS= read -r sha; do
    commits+=("$sha")
  done < <(git rev-list --reverse "${args[0]}")
else
  commits=("${args[@]}")
fi

if [ "${#commits[@]}" -eq 0 ]; then
  echo "no commits to promote" >&2
  exit 1
fi

last_commit="${commits[$((${#commits[@]}-1))]}"
branch="promote/$(date +%Y%m%d)-$(git rev-parse --short "$last_commit")"

git switch -c "$branch" "$PUBLIC_REMOTE/main"

if ! git cherry-pick "${commits[@]}"; then
  echo "cherry-pick stopped with a conflict on branch '$branch'." >&2
  echo "Resolve it, then: git cherry-pick --continue" >&2
  echo "Then push and open the PR yourself:" >&2
  echo "  git push $PUBLIC_REMOTE $branch" >&2
  echo "  gh pr create --repo Made-by-Moonlight/ninox --base main --head $branch" >&2
  exit 1
fi

sensitive_pattern='synthesia|codeartifact|github\.com-synthesia'
if diff_hits=$(git diff "$PUBLIC_REMOTE/main..HEAD" | grep -Eio "$sensitive_pattern" | sort -u) && [ -n "$diff_hits" ]; then
  echo "Found strings that look internal-only in the promoted diff:" >&2
  echo "$diff_hits" | sed 's/^/  - /' >&2
  if [ "$force" != true ]; then
    echo "Refusing to push. Re-run with --force once you've confirmed this is a false positive." >&2
    exit 1
  fi
  echo "Continuing anyway (--force)." >&2
fi

git push "$PUBLIC_REMOTE" "$branch"

body=$(
  printf 'Promoted from the internal mirror (%s). Commits:\n\n' "$INTERNAL_REMOTE"
  for c in "${commits[@]}"; do
    printf -- '- %s\n' "$(git log -1 --format='%h %s' "$c")"
  done
)

gh pr create --repo Made-by-Moonlight/ninox --base main --head "$branch" \
  --title "chore: promote ${#commits[@]} commit(s) from internal" \
  --body "$body"
