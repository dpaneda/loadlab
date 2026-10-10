# Deploying

loadlab runs anywhere Linux does. This chapter covers a systemd deployment on a remote host, which
is how you run it on a small always-on box and still drive it from your workstation.

The `deploy/` directory holds everything the installer needs:

- `deploy/install.sh` — build (natively or cross-compiled) and install over SSH.
- `deploy/loadlab.service` — the API and controller.
- `deploy/loadlab-sink.service` — the same binary in **sink mode**, a discarding TCP/UDP blackhole.

## TL;DR

```bash
./deploy/install.sh user@host
```

Builds the binary, installs it to `/usr/local/bin/loadlab`, registers and starts the two systemd
services on `127.0.0.1:8080`, and smoke-tests them. Re-run the same command after any code change.

The API binds to loopback only, so reach it through an SSH tunnel:

```bash
ssh -L 8080:127.0.0.1:8080 user@host     # in one terminal
curl -sS http://127.0.0.1:8080/health
```

## Requirements on the build machine

- A Rust toolchain to build natively. To cross-compile, `cargo zigbuild` (`cargo install
  cargo-zigbuild`) and `zig` supply the linker for targets that have none.
- SSH access to the target host with passwordless `sudo`, for the systemd part.

Nothing is installed on the target beyond the binary, one document and two unit files.

## What gets installed

| Path | Contents |
|---|---|
| `/usr/local/bin/loadlab` | the binary |
| `/usr/local/share/doc/loadlab/deploy.md` | this chapter, so the unit's `Documentation=` link resolves on the host |
| `/var/lib/loadlab` | writable directory for the file-backed controller |
| `/etc/systemd/system/loadlab.service` | API + controller |
| `/etc/systemd/system/loadlab-sink.service` | discarding network sink |

Override the installed addresses with `LOADLAB_BIND` and `LOADLAB_SINK`, and pick a prebuilt binary
with `LOADLAB_BINARY`.

## Cross-compiling

If the target host cannot build Rust (a small single-board computer, say), cross-compile and let the
installer copy the result over. Set the target triple and use `cargo zigbuild`, which bundles a
linker so no target C toolchain is needed:

```bash
rustup target add aarch64-unknown-linux-gnu
LOADLAB_TARGET=aarch64-unknown-linux-gnu ./deploy/install.sh user@host
```

The installer reads the highest `GLIBC_*` symbol version the binary needs and refuses to deploy a
binary that requires a newer glibc than the target has, so a mismatch fails at install time rather
than at first run. Common 64-bit targets are `aarch64-unknown-linux-gnu` and
`x86_64-unknown-linux-gnu`; for a 32-bit ARM userland it is `arm-unknown-linux-gnueabihf`, and
`uname -m` alone is not enough to tell — check `getconf LONG_BIT`.

To build and install a 32-bit binary, the 32-bit `usize` has one consequence worth knowing: the
default `--max-memory-bytes` of 8 GiB does not fit, so `main` clamps the effective ceiling to
`usize::MAX` and logs a warning. Check `/state` for the effective value.

## The systemd unit

`deploy/loadlab.service`, installed to `/etc/systemd/system/loadlab.service`. The relevant choices:

- **Loopback bind.** The API can be told to reserve large amounts of memory and has no
  authentication, so it is not reachable from the LAN. Reach it through an SSH tunnel, or set
  `LOADLAB_API_TOKEN` and put a TLS reverse proxy in front.
- **A process ceiling via `--max-memory-bytes`.** The process ceiling defaults to 8 GiB. A request
  may lower this but never raise it, so an API client cannot talk the host into allocating more than
  the operator allowed. Lower it to the host's budget.
- **`MemorySwapMax=0`.** loadlab shrinks memory with `MADV_DONTNEED`, not by swapping. Allowing swap
  would make `/proc` readings diverge from the model the tool exists to demonstrate. The unit ships
  `MemoryHigh`/`MemoryMax` commented out as independent cgroup backstops: size them to the machine.
  `MemoryHigh` makes the kernel reclaim and throttle the cgroup *before* anything is killed, which is
  far gentler than an OOM kill when a workload climbs near the ceiling; `MemoryMax` is the last
  resort.
- **`ProtectSystem=strict`, `ProtectHome=true`, `NoNewPrivileges`.** The filesystem is read-only
  apart from the one directory named in `ReadWritePaths`, and the service needs no access to home.
- **`--file-backed-dir /var/lib/loadlab`, with `ReadWritePaths` for that directory.** The
  file-backed controller is the only part of loadlab that writes to disk: it creates a temporary
  file to page memory in from. The directory is created by the installer, since a read-only
  filesystem means systemd cannot. Raise `--max-file-backed-bytes` if you need a larger file, and
  watch free disk, not just memory: the file's high-water length is never truncated back down for
  the life of the process, so the peak is what you need room for.

### Why the file-backed directory is not /tmp

`/tmp` is tmpfs on most modern hosts. A file there is shared memory, not file memory: the kernel
accounts its pages under `RssShmem`, there is no page cache and no writeback, and no disk is
consumed. A file-backed workload pointed at tmpfs would report exactly the residency it was asked
for, charge nothing to disk, and reproduce none of the behaviour that makes file-backed memory worth
a separate controller — while looking, to every metric in the system, like the real thing.

loadlab therefore defaults to its working directory and **refuses to start** if that directory turns
out to be memory-backed, rather than silently generating the wrong phenomenon. The installer points
it at `/var/lib/loadlab` because `ProtectSystem=strict` leaves that read-only otherwise.

## Operating

```bash
ssh user@host 'systemctl status loadlab'
ssh user@host 'systemctl status loadlab-sink'     # the discarding sink on 127.0.0.1:9000
ssh user@host 'journalctl -u loadlab -f'          # logs
ssh user@host 'sudo systemctl restart loadlab'
ssh user@host 'sudo systemctl stop loadlab'       # stop (survives until next boot)
ssh user@host 'systemctl is-enabled loadlab'      # autostart on boot
```

Drive it through a tunnel:

```bash
ssh -N -L 8080:127.0.0.1:8080 user@host &
curl -sS -X PUT http://127.0.0.1:8080/workload \
  -H 'content-type: application/json' \
  -d '{"running":true,"memory":{"anonymous_rss":"96MiB + 48MiB*sin(t/10)"}}'
curl -sS http://127.0.0.1:8080/metrics | jq .controller
```

### Controlling the speed of a sinusoid

The divisor is the period control. `sin(t/N)` completes one full cycle every `2*pi*N` seconds, so
`N = 573` is one cycle per hour:

| Expression | Full cycle | Range |
|---|---|---|
| `350MiB + 250MiB*sin(t/1)` | 6.3 s | 100..600 MiB |
| `350MiB + 250MiB*sin(t/10)` | 63 s | 100..600 MiB |
| `350MiB + 250MiB*sin(t/191)` | 20 min | 100..600 MiB |
| `350MiB + 250MiB*sin(t/573)` | 60 min | 100..600 MiB |
| `350MiB + 250MiB*sin(t/1800)` | 3.1 h | 100..600 MiB |

Mean and amplitude are just `(max+min)/2` and `(max-min)/2`. A cycle starts at the mean and reaches
the maximum a quarter period in (5 min for the `t/191` curve, 15 min for the hourly one).

`validate` only evaluates the expression at `t=0`, which for a plain sinusoid is the mean. A curve
whose mean is positive but whose amplitude exceeds it -- `100MiB + 200MiB*sin(t/191)`, say -- is
therefore accepted, and then spends part of every cycle asking for a negative target. The controller
clamps that to zero, so the arena sits empty and `converged` stays true: a silent flat spot at zero,
not a failure. **Keep the mean at or above the amplitude.**

`tick_ms` does not change the curve, only how finely it is sampled: the controller gets
`period / (tick_ms/1000)` samples per cycle. The hourly curve at the default 100 ms is sampled
36000 times per cycle, so it is smooth by a wide margin. A fast curve needs a smaller `tick_ms` to
avoid visible stepping -- `sin(t/1)` at `tick_ms=1000` is only 6 samples per cycle.

```bash
curl -sS -X PUT http://127.0.0.1:8080/workload \
  -H 'content-type: application/json' \
  -d '{"running":true,"memory":{"anonymous_rss":"350MiB + 250MiB*sin(t/191)"}}'
```

### Watching it

For a one-off look, `/metrics` has everything including the `last_error` string:

```bash
watch -n5 'curl -sS http://127.0.0.1:8080/metrics |
  jq -r "\"\(.process.memory.rss_anon_bytes/1048576|floor) MiB  target \(.controller.last_target_arena_bytes/1048576|floor) MiB  err \(.controller.last_error // \"-\")\""'
```

`last_error` is where a failed reconciliation shows up, but it is cleared again on the next good
tick. `controller.reconcile_errors_total` is the one to watch for intermittent failures.

A workload that drifts from its target is not necessarily an error: `anonymous_rss` is a control
objective, so expect the arena to sit within `tolerance_bytes` (2 MiB by default) of the target, and
kernel accounting noise on top of that. `controller.convergence_error_bytes` is the signed gap and
`controller.converged` is the pass/fail version of the same thing.

### Prometheus

`GET /prometheus` serves the same numbers in the text exposition format. It binds to loopback like
everything else, so a Prometheus server on another host needs a tunnel:

```yaml
scrape_configs:
  - job_name: loadlab
    scrape_interval: 15s
    metrics_path: /prometheus
    static_configs:
      - targets: ['127.0.0.1:8080']
        labels:
          host: loadlab
```

Open the tunnel before starting Prometheus, and keep it open:

```bash
ssh -N -L 8080:127.0.0.1:8080 user@host
```

For production use, the canonical configuration lives in `contrib/prometheus/`, which includes the
same scrape interval and `file_sd`-based target discovery (it works both on-host and in Docker),
pre-built alert rules for controller staleness, reconcile errors, and memory headroom (matching the
recommendations in [Metrics and Prometheus](http-api/metrics.md#alerting-notes)), and a Grafana
dashboard (`contrib/grafana/loadlab.json`) that visualises the control loop, CPU and memory costs,
and health signals.

Two rules specific to this tool:

- **Alert on `loadlab_controller_last_tick_age_seconds`, not on scrape success.** The controller runs
  on its own thread, so a dead or wedged controller does not stop the API from answering. The scrape
  keeps looking fine while the workload silently stops tracking.
- **Use `loadlab_memory_peak_rss_bytes` for the maximum.** `loadlab_memory_rss_bytes` is
  instantaneous: at a 15 s scrape interval against a 63 s cycle, it under-reports the peak.
  `loadlab_memory_peak_rss_bytes` is the high-water mark since the process started and cannot miss
  it.

Each scrape reads `/proc/self/smaps_rollup`, which walks the page tables and therefore costs more the
bigger the arena is. At 15 s that is noise; at 1 s against a 600 MiB arena it is real CPU. See
[Metrics and Prometheus](http-api/metrics.md) for the full metric list.

Stop the workload when done so the arena is released:

```bash
curl -sS -X POST http://127.0.0.1:8080/workload/stop
```

## Verifying on the target

If you cross-compiled, some behaviour only exists on the target architecture. Build the tests for it
and run them there:

```bash
LOADLAB_TARGET=aarch64-unknown-linux-gnu
cargo zigbuild --release --tests --target "$LOADLAB_TARGET"
scp target/$LOADLAB_TARGET/release/deps/loadlab-<hash> user@host:/var/lib/loadlab/unit-tests
ssh user@host '/var/lib/loadlab/unit-tests'
```

The integration tests in `tests/api.rs` are not run this way: they spawn the binary through
`env!("CARGO_BIN_EXE_loadlab")`, which is an absolute path from the build machine. Run them locally
with `cargo test`, and cover the target through the HTTP checks above.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Address already in use` at startup | Port taken | Pick a free port: `LOADLAB_BIND=127.0.0.1:8090 ./deploy/install.sh user@host` |
| `scp: dest open ... Failure` | The old binary is still running; Linux refuses to overwrite a running executable (`ETXTBSY`) | `ssh user@host 'pkill -x loadlab'`. The script does this automatically |
| `glibc mismatch` in the installer | Cross-built binary needs a newer glibc than the target has | Re-run `rustup update` / use an older toolchain, or build natively on the target |
| `clamped max-memory-bytes` in the log | Expected on a 32-bit target: the 8 GiB default exceeds `usize::MAX` | None. Check `/state` for the effective value |
| `Failed to connect` from another machine | Expected: the service binds loopback only | Use `ssh -L 8080:127.0.0.1:8080 user@host` |

## Uninstall

```bash
ssh user@host 'sudo systemctl disable --now loadlab.service loadlab-sink.service
               sudo rm -f /etc/systemd/system/loadlab.service /etc/systemd/system/loadlab-sink.service
               sudo systemctl daemon-reload
               sudo rm -rf /usr/local/bin/loadlab /usr/local/share/doc/loadlab /var/lib/loadlab'
```
