#!/usr/bin/env bash
set -euo pipefail

status=0
shopt -s nullglob

while IFS= read -r workflow; do
  while IFS= read -r line; do
    if [[ ! $line =~ uses:[[:space:]]+([^@[:space:]]+)@([^[:space:]]+) ]]; then
      continue
    fi

    action="${BASH_REMATCH[1]}"
    ref="${BASH_REMATCH[2]}"

    if ! [[ "$ref" =~ ^[0-9a-f]{40}$ ]]; then
      echo "Workflow '${workflow}' uses non-pinned action reference: ${action}@${ref}"
      status=1
    fi
  done < <(rg -N '^\\s*uses:\\s+[^#]+' "$workflow")
done < <(find .github/workflows -name '*.yml' -print)

if [[ $status -ne 0 ]]; then
  echo "Fix the workflow action references above to use full 40-char commit SHAs."
  exit 1
fi

echo "All workflow action references are pinned to full commit SHAs."
