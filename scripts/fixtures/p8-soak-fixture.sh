#!/usr/bin/env bash
set -euo pipefail
duration="${1:?duration required}"; deltas=0; tools=0
for ((i=0;i<10000;i++));do deltas=$((deltas+1));done
for ((i=0;i<1000;i++));do tools=$((tools+1));done
sleep "$duration"
printf '{"deltas":%s,"tool_events":%s,"queue_watermark":1,"child_watermark":0}\n' "$deltas" "$tools"
