#!/usr/bin/env bash
# Turn raw capture logs into golden fixtures for core/tests/golden_traces.rs.
#
#   ./captures/make-fixtures.sh
#
# Keeps only TRACE records, drops the capture tool's own diagnostics
# (heartbeats, slow-loop and key notes), and replaces personal data:
# the dialled number becomes 07700900123 (Ofcom drama range) and the IMEI
# becomes zeros. The modem's bytes are otherwise untouched.
set -euo pipefail

cd "$(dirname "$0")/.."
OUT=core/tests/fixtures/modem
mkdir -p "$OUT"

# capture log → fixture name
FIXTURES=(
  "01-boot:01-boot"
  "02-out-answered-remote-hangup:02-out-answered-remote-hangup"
  "03-out-answered-local-hangup-3:03-out-answered-voicemail-hangup"
  "03-out-answered-local-hangup-4:03-out-answered-local-hangup"
  "04-out-rejected:04-out-rejected"
  "05-out-unanswered:05-out-unanswered"
  "06-out-busy-or-unreachable:06-out-busy-or-unreachable"
  "07-in-answered:07-in-answered"
  "08-in-missed:08-in-missed"
  "09-sms-during-call:09-sms-during-call"
  "10-soft-power-cycle:10-soft-power-cycle"
  "11-hard-power-cut:11-hard-power-cut"
  "12-command-during-urc:12-command-during-urc"
)

for pair in "${FIXTURES[@]}"; do
  src="captures/${pair%%:*}.log"
  dst="$OUT/${pair##*:}.trace"
  grep -E '^TRACE ' "$src" \
    | grep -vE '^TRACE [0-9]+ # (rx alive|alive |slow loop|key |kb error)' \
    | sed -E 's/07700900123/07700900123/g; s/IMEI: [0-9]{15}/IMEI: 000000000000000/' \
    > "$dst"
  echo "$dst: $(wc -l < "$dst") records"
done
