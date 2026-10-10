#!/usr/bin/env bash
# Drive a network workload that breathes: `messages_per_second` follows a sinusoid.
#
# The period is `2*pi*N` seconds, so a 30-minute cycle is `N = 1800 / (2*pi) = 286.5`.
# The rate is `mean + amplitude*sin(t/N)`, evaluated by the controller once per tick.
#
#   ./examples/network_sinusoid.sh                         # 500 +/- 250 msg/s, 30-min period
#   LOADLAB_NETWORK_PERIOD_S=600 ./examples/network_sinusoid.sh   # 10-minute period
#   LOADLAB_BASE_URL=http://127.0.0.1:8080 ./examples/network_sinusoid.sh   # a remote host over a tunnel
#
# It needs a destination: the default is the discarding sink installed by deploy/install.sh
# (`loadlab --sink 127.0.0.1:9000`). On a dev machine, `cargo run --release -- --sink 127.0.0.1:9000`.
set -euo pipefail

BASE="${LOADLAB_BASE_URL:-http://127.0.0.1:8080}"
TARGET="${LOADLAB_NETWORK_TARGET:-127.0.0.1:9000}"
PERIOD_S="${LOADLAB_NETWORK_PERIOD_S:-1800}"
MEAN="${LOADLAB_NETWORK_MEAN:-500}"
AMPLITUDE="${LOADLAB_NETWORK_AMPLITUDE:-250}"
SIZE="${LOADLAB_NETWORK_MESSAGE_SIZE:-1024}"

N="$(awk -v p="$PERIOD_S" 'BEGIN { printf "%.4f", p / (2 * 3.141592653589793) }')"
EXPR="${MEAN} + ${AMPLITUDE}*sin(t/${N})"

echo "==> target=${TARGET}  period=${PERIOD_S}s  expression=${EXPR}"

curl -fsS -X PUT "$BASE/workload" \
  -H 'content-type: application/json' \
  -d "$(printf '{"running":true,"network":{"transport":"tcp","target":"%s","concurrency":4,"messages_per_second":"%s","message_size":%s}}' \
        "$TARGET" "$EXPR" "$SIZE")"
echo

sleep 2
echo "==> one reading"
curl -fsS "$BASE/metrics" | jq -r '"requested \(.controller.network.messages_per_second|floor) msg/s  actual \(.controller.network.messages_per_second_actual|floor) msg/s  converged \(.controller.network.converged)  errors \(.controller.network.errors_total)"'

cat <<EOF

The rate now tracks mean + ${AMPLITUDE}*sin(t/${N}); it peaks ${AMPLITUDE} msg/s above ${MEAN} a
quarter period in. Watch it with:

  watch -n5 'curl -sS ${BASE}/metrics |
    jq -r "\\(.controller.network.messages_per_second|floor) requested / \\(.controller.network.messages_per_second_actual|floor) actual msg/s"'

Stop it with:  curl -sS -X POST ${BASE}/workload/stop
EOF
