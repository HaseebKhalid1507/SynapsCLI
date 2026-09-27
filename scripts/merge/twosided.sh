#!/usr/bin/env bash
# twosided.sh <base> <branch> [files...] — for each file, show what HEAD carries from each side.
# Left diff (branch..HEAD) must contain only BASE-side changes; right diff (base..HEAD) only BRANCH-side.
# Anything else is a lost or invented hunk. Exit 1 if any file has conflict markers.
set -euo pipefail
base=$1; branch=$2; shift 2
files=("$@"); [ ${#files[@]} -eq 0 ] && mapfile -t files < <(git diff --name-only "$base"..HEAD)
rc=0
printf '%-60s %10s %10s\n' FILE "vs-branch" "vs-base"
for f in "${files[@]}"; do
  if git show HEAD:"$f" 2>/dev/null | grep -qE '^(<<<<<<<|=======|>>>>>>>)( |$)'; then echo "!! CONFLICT MARKERS: $f"; rc=1; fi
  l=$(git diff --numstat "$branch"..HEAD -- "$f" | awk '{print "+"$1"/-"$2}'); r=$(git diff --numstat "$base"..HEAD -- "$f" | awk '{print "+"$1"/-"$2}')
  printf '%-60s %10s %10s\n' "$f" "${l:-0}" "${r:-0}"
done
echo; echo "review: git diff $branch..HEAD -- <file>   (should read as OUR-side changes only)"
echo "        git diff $base..HEAD -- <file>     (should read as THEIR-side changes only)"
exit $rc
