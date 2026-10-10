#!/usr/bin/env bash
set -euo pipefail
#
# Combined sinusoidal CPU + memory workload demo.
#
# CPU:    150 + 50*sin(t/20)   (percent of one core, 2 workers)
# Memory: 250MB + 150MB*sin(t/30)
#
# Usage:
#   bash example_combined_sinusoid.sh [port]
#   If port is omitted, a random free port is chosen automatically.
#

PORT="${1:-$(python3 -c "import socket; s=socket.socket(); s.bind(('',0)); print(s.getsockname()[1]); s.close()")}"

start_server() {
    if curl -sf "127.0.0.1:$PORT/health" >/dev/null 2>&1; then
        echo "loadlab already listening on $PORT"
        return
    fi
    if [ ! -f target/release/loadlab ]; then
        cargo build --release
    fi
    ./target/release/loadlab --bind "127.0.0.1:$PORT" --max-memory-bytes 1073741824 &>/dev/null &
    local pid=$!
    for _ in $(seq 1 30); do
        if curl -sf "127.0.0.1:$PORT/health" >/dev/null 2>&1; then
            echo "loadlab ready on 127.0.0.1:$PORT"
            return
        fi
        sleep 0.5
    done
    kill "$pid" 2>/dev/null || true
    echo "loadlab failed to start" >&2
    exit 1
}

set_workload() {
    local cpu_expr="150 + 50*sin(t/20)"
    local mem_expr="250MB + 150MB*sin(t/30)"
    echo "Setting combined sinusoidal workload on port $PORT"
    echo "  CPU:    $cpu_expr   (workers=2)"
    echo "  Memory: $mem_expr"
    local code
    code=$(curl -s -o /dev/null -w "%{http_code}" -H "Content-Type: application/json" -XPUT         "127.0.0.1:$PORT/workload"         -d "{"running":true,"cpu":{"utilization":"$cpu_expr","workers":2},"memory":{"anonymous_rss":"$mem_expr"}}")
    echo "  PUT -> $code"
}

print_status() {
    echo "--- /state ---"
    curl -sf "127.0.0.1:$PORT/state" | python3 -c "import json,sys;print(json.dumps(json.load(sys.stdin),indent=2))"
    echo "--- /metrics snapshot ---"
    curl -sf "127.0.0.1:$PORT/metrics" | python3 <<'EOF'
import json,sys
d = json.load(sys.stdin)
c = d['controller']
m = d['process']['memory']
mib = 2**20
print(f"  memory: target={c['last_target_arena_bytes']/mib:.1f}MiB resident={c['arena_resident_bytes']/mib:.1f}MiB peak={m['peak_rss_bytes']/mib:.1f}MiB error={c['convergence_error_bytes']/mib:+.2f}MiB converged={c['converged']}")
cpu = c['cpu']
print(f"  cpu:    workers={cpu['worker_count']} capacity={cpu['capacity_pct']:.0f} target={cpu['target_utilization_pct']:.1f}% actual={cpu['actual_utilization_pct']:.1f}% duty={cpu['duty_cycle']:.4f} converged={cpu['converged']} saturated={cpu['saturated']}")
EOF
}

start_server
sleep 1
set_workload

echo "Monitoring convergence (30 samples x 4s = 120s) ..."
for i in $(seq 1 30); do
    curl -sf "127.0.0.1:$PORT/metrics" | python3 -c "import json,sys;d=json.load(sys.stdin);c=d['controller'];cpu=c['cpu'];print(f'  tick={c["tick_count"]:4d} mem_err={(c["convergence_error_bytes"])/2**20:+.2f}MiB(conv={c["converged"]}) cpu_err={cpu["target_utilization_pct"]-cpu["actual_utilization_pct"]:+.1f}%(conv={cpu["converged"]})')"
    sleep 4
done

print_status
