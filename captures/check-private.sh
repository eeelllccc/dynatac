#!/usr/bin/env bash
# Pre-commit check: refuse a commit whose staged files contain the real
# phone number from captures/.private-number (gitignored), in plain or SMS
# PDU (swapped-nibble BCD) form, national or international.
#
# Install once per clone:  ln -s ../../captures/check-private.sh .git/hooks/pre-commit
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
[[ -f captures/.private-number ]] || exit 0
REAL=$(tr -d '[:space:]' < captures/.private-number)
[[ -n "$REAL" ]] || exit 0

bcd() {
  local d=$1 out=""
  (( ${#d} % 2 )) && d="${d}F"
  for ((i = 0; i < ${#d}; i += 2)); do out+="${d:i+1:1}${d:i:1}"; done
  echo "$out"
}
INTL="44${REAL:1}"
PATTERN="${REAL}|${INTL}|$(bcd "$REAL")|$(bcd "$INTL")"

# Staged content of every added / changed file (not the working tree).
FILES=()
while IFS= read -r -d '' f; do FILES+=("$f"); done < <(git diff --cached --name-only --diff-filter=ACMR -z)
(( ${#FILES[@]} )) || exit 0
if HITS=$(git grep --cached -l -i -E "$PATTERN" -- "${FILES[@]}"); then
  echo "pre-commit: these staged files contain the private phone number:" >&2
  echo "$HITS" | sed 's/^/  /' >&2
  echo "Scrub them (captures/make-fixtures.sh) or unstage them." >&2
  exit 1
fi
