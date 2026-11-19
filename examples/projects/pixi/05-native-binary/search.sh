#!/bin/sh
# The lines matching a pattern, counted per file under a directory, as JSON: rg finds them, jq shapes them.
# Usage: search.sh PATTERN [DIR]
set -eu
pattern="${1:?usage: search.sh PATTERN [DIR]}"
dir="${2:-.}"
rg --json -e "$pattern" "$dir" \
  | jq -s '[.[] | select(.type == "match") | .data.path.text] | group_by(.) | map({file: .[0], lines: length})'
