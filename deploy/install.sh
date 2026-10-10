#!/usr/bin/env bash
# Build loadlab and install it as a systemd service on a remote host over SSH.
#
#   ./deploy/install.sh user@host
#
# By default the binary is built natively. Set LOADLAB_TARGET to cross-compile instead (the target
# must be installed, and `cargo zigbuild` + `zig` supply the linker for targets without one); set
# LOADLAB_BINARY to install an already-built binary and skip the build entirely.
#
# Remote layout (system-wide):
#   /usr/local/bin/loadlab
#   /usr/local/share/doc/loadlab/deploy.md
#   /var/lib/loadlab                         writable dir for the file-backed controller
#   /etc/systemd/system/loadlab.service      (+ loadlab-sink.service)
#
# Environment overrides:
#   LOADLAB_TARGET   cross-compile target (e.g. aarch64-unknown-linux-gnu); unset builds natively
#   LOADLAB_BINARY   path to a prebuilt binary (skips the build)
#   LOADLAB_BIND     API bind address written into the unit   (default 127.0.0.1:8080)
#   LOADLAB_SINK     sink address written into the unit       (default 127.0.0.1:9000)

set -euo pipefail

HOST="${1:-}"
if [ -z "$HOST" ]; then
    echo "usage: $0 user@host" >&2
    exit 2
fi

TARGET="${LOADLAB_TARGET:-}"
BIND="${LOADLAB_BIND:-127.0.0.1:8080}"
SINK="${LOADLAB_SINK:-127.0.0.1:9000}"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(dirname "$here")"

if [ -n "${LOADLAB_BINARY:-}" ]; then
    bin="$LOADLAB_BINARY"
elif [ -n "$TARGET" ]; then
    command -v cargo-zigbuild >/dev/null || {
        echo "cargo-zigbuild not found: cargo install cargo-zigbuild" >&2
        exit 1
    }
    echo "==> building $TARGET (release)"
    cargo zigbuild --release --target "$TARGET" --manifest-path "$repo/Cargo.toml"
    bin="$repo/target/$TARGET/release/loadlab"
    # A glibc newer than the target's would refuse to start. Check the highest symbol version.
    required="$(objdump -T "$bin" | grep -oE 'GLIBC_[0-9.]+' | sed 's/^GLIBC_//' | sort -uV | tail -1)"
    remote_glibc="$(ssh "$HOST" "ldd --version | head -1 | grep -oE '[0-9]+\.[0-9]+$'")"
    echo "==> binary needs glibc $required, target has glibc $remote_glibc"
    older="$(printf '%s\n%s\n' "$required" "$remote_glibc" | sort -V | head -1)"
    if [ "$older" != "$required" ]; then
        echo "glibc mismatch: binary needs $required but the target only has $remote_glibc" >&2
        exit 1
    fi
else
    echo "==> building (release, native)"
    cargo build --release --manifest-path "$repo/Cargo.toml"
    bin="$repo/target/release/loadlab"
fi

[ -f "$bin" ] || { echo "binary not found: $bin" >&2; exit 1; }

echo "==> installing to $HOST"
# A running executable cannot be overwritten (ETXTBSY), so stop the services before replacing it.
ssh "$HOST" "sudo systemctl stop loadlab.service loadlab-sink.service >/dev/null 2>&1 || true
            pkill -x loadlab >/dev/null 2>&1 || true
            sleep 1
            sudo install -d -m 0755 /usr/local/bin /usr/local/share/doc/loadlab /var/lib/loadlab /etc/systemd/system"

scp -q "$bin" "$HOST:/tmp/loadlab.bin"
scp -q "$repo/docs/src/deploy.md" "$HOST:/tmp/loadlab-deploy.md"
scp -q "$here/loadlab.service" "$HOST:/tmp/loadlab.service"
scp -q "$here/loadlab-sink.service" "$HOST:/tmp/loadlab-sink.service"

# Install the binary and the guide, then the units with the configured bind/sink addresses. `tee`
# runs as root because the shell redirect into /etc/systemd/system would not.
ssh "$HOST" "sudo install -m 0755 /tmp/loadlab.bin /usr/local/bin/loadlab
            sudo install -m 0644 /tmp/loadlab-deploy.md /usr/local/share/doc/loadlab/deploy.md
            sed -e 's#127.0.0.1:8080#${BIND}#g' /tmp/loadlab.service |
                sudo tee /etc/systemd/system/loadlab.service > /dev/null
            sed -e 's#127.0.0.1:9000#${SINK}#g' /tmp/loadlab-sink.service |
                sudo tee /etc/systemd/system/loadlab-sink.service > /dev/null
            sudo chmod 0644 /etc/systemd/system/loadlab.service /etc/systemd/system/loadlab-sink.service
            rm -f /tmp/loadlab.bin /tmp/loadlab-deploy.md /tmp/loadlab.service /tmp/loadlab-sink.service"

echo "==> starting services"
ssh "$HOST" "sudo systemctl daemon-reload &&
            sudo systemctl enable --now loadlab.service loadlab-sink.service &&
            sleep 2 &&
            systemctl --no-pager --full status loadlab.service loadlab-sink.service | head -24"

port="${BIND##*:}"
echo
echo "==> smoke test over the loopback API"
ssh "$HOST" "curl -fsS http://127.0.0.1:${port}/health && echo &&
            curl -fsS http://127.0.0.1:${port}/state && echo &&
            curl -fsS http://127.0.0.1:${port}/prometheus |
              grep -c '^loadlab_' |
              xargs -I{} test {} -gt 20 && echo 'prometheus exposition ok'"

cat <<EOF

Done. loadlab is running on ${HOST} on ${BIND}.
      A network sink is running on ${SINK}.

  tunnel:   ssh -L ${port}:127.0.0.1:${port} ${HOST}
  then:     curl -sS http://127.0.0.1:${port}/health
  scrape:   curl -sS http://127.0.0.1:${port}/prometheus
  sinusoid: ${SINK} is ready as the network target; see examples/network_sinusoid.sh
  logs:     ssh ${HOST} 'journalctl -u loadlab -f'
  stop:     ssh ${HOST} 'sudo systemctl stop loadlab loadlab-sink'
EOF
