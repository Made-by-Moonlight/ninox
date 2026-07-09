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
# A <a>..<b> argument is expanded to the commits strictly after <a> up to
# and including <b> (standard git range semantics — <a> itself is
# excluded), oldest first; otherwise each commit is cherry-picked in the
# order given. Merge commits are always rejected (from either form) —
# `git cherry-pick` can't apply one without an explicit -m mainline
# choice, and letting the script's own conflict-recovery path swallow
# that failure previously (see git blame) skipped the safety checks
# below entirely, which is exactly how the internal-only CODEOWNERS file
# could have leaked. Cherry-pick a merge commit by hand with -m if you
# really need its content promoted.
#
# Before pushing, this refuses (unless --force) to promote any commit
# whose diff touches a known internal-only path (CODEOWNERS, this
# script, the sync-from-public workflow) or whose added lines match a
# short list of internal-only strings (the private remote's host alias,
# "synthesia", "codeartifact"). Both checks are best-effort, not
# exhaustive — review the diff yourself before using --force.
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

for c in "${commits[@]}"; do
  parent_field_count=$(git rev-list --parents -n1 "$c" | wc -w)
  if [ "$parent_field_count" -gt 2 ]; then
    echo "refusing to promote merge commit $c — cherry-pick it by hand with -m if you really need to" >&2
    exit 1
  fi
done

last_commit="${commits[$((${#commits[@]}-1))]}"
branch="promote/$(date +%Y%m%d)-$(git rev-parse --short "$last_commit")"

git switch -c "$branch" "$PUBLIC_REMOTE/main"

if ! git cherry-pick "${commits[@]}"; then
  echo "cherry-pick stopped with a conflict on branch '$branch'." >&2
  echo "Resolve it, then: git cherry-pick --continue" >&2
  echo "Then re-run the internal-only-content check yourself before pushing:" >&2
  echo "  git diff --name-only $PUBLIC_REMOTE/main..HEAD" >&2
  echo "  git diff $PUBLIC_REMOTE/main..HEAD | grep -Ei 'synthesia|codeartifact|github\\.com-synthesia'" >&2
  echo "Then push and open the PR:" >&2
  echo "  git push $PUBLIC_REMOTE $branch" >&2
  echo "  gh pr create --repo Made-by-Moonlight/ninox --base main --head $branch" >&2
  exit 1
fi

internal_only_paths='^(CODEOWNERS|\.github/workflows/sync-from-public\.yml|scripts/promote-to-public\.sh)$'
touched_internal=$(git diff --name-only "$PUBLIC_REMOTE/main..HEAD" | grep -E "$internal_only_paths" || true)
if [ -n "$touched_internal" ]; then
  echo "Promoted diff touches internal-only file(s):" >&2
  echo "$touched_internal" | sed 's/^/  - /' >&2
  if [ "$force" != true ]; then
    echo "Refusing to push. Re-run with --force once you've confirmed this is intentional." >&2
    exit 1
  fi
  echo "Continuing anyway (--force)." >&2
fi

sensitive_pattern='synthesia|codeartifact|github\.com-synthesia'
added_lines=$(git diff "$PUBLIC_REMOTE/main..HEAD" | grep -E '^\+[^+]')
if diff_hits=$(printf '%s' "$added_lines" | grep -Eio "$sensitive_pattern" | sort -u) && [ -n "$diff_hits" ]; then
  echo "Found strings that look internal-only in the promoted diff's added lines:" >&2
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
