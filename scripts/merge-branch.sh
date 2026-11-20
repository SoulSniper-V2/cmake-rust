#!/bin/bash
# Merge a worker branch into master; union-resolve registry files.
set -u
b=$1
git merge --no-edit -q "t3/$b" 2>&1 | grep -E "CONFLICT" || true
c=$(git diff --name-only --diff-filter=U)
if [ -n "$c" ]; then
  echo "conflicts in $b: $c"
  exit 1
fi
echo "merged $b"
