# Contributed tooling for loadlab

Grafana dashboard, Prometheus rules and a throwaway docker-compose stack for looking
at a loadlab instance locally. Nothing here is required by the binary — loadlab
exports its own metrics either way.

## Quick start

```bash
ssh -N -L 8080:127.0.0.1:8080 loadlab-host
docker compose -f contrib/docker-compose.yml up
```

Then open http://localhost:3000 (admin/admin). The dashboard loads automatically from
`contrib/grafana/loadlab.json`, and Prometheus serves the host through the tunnel at
`host.docker.internal:8080` (see the comment in `contrib/docker-compose.yml` for why the
target is that name rather than 127.0.0.1).

If you already have a Grafana and Prometheus on the host, point them at
`contrib/prometheus/prometheus.yml` and `contrib/prometheus/loadlab.rules.yml`. Validate
with:

```bash
promtool check config    contrib/prometheus/prometheus.yml
promtool check rules     contrib/prometheus/loadlab.rules.yml
```

## The dashboard

`contrib/grafana/loadlab.json` has 50 panels across ten rows: state, the control loop,
CPU, memory detail, file-backed memory, shared memory, disk I/O, transparent huge pages,
controller health, and the CPU/page-fault cost of running the workload. Every panel queries the
metrics that loadlab exposes, and each expression was verified against a live Prometheus
scraping a running instance, so no panel is "No data".

The only template variable is a `datasource` one (so the dashboard works against an
existing Prometheus) plus an `instance` one that matches `loadlab_build_info`.

### What to look at first

1. **Peak RSS vs ceiling** and **Headroom to ceiling**. Peak RSS is the high-water
   mark since startup and is the only value that cannot miss a peak between scrapes.
   Headroom is the distance to the OOM killer on a host without swap.
2. **Tick staleness vs budget** — the liveness signal. The controller runs on its own
   thread, so a wedged controller keeps the HTTP server answering perfectly. The
   staleness threshold is five times `loadlab_configured_tick_ms`, which is exported
   specifically so the rule does not need a hardcoded constant.
3. **Target vs resident** — the control loop doing its job: the arena tracking its
   setpoint.

## Alert rules

`contrib/prometheus/loadlab.rules.yml` ships with twenty-two rules: eight recording rules
(the staleness budget, headroom in bytes and as a fraction, CPU shortfall, the file-backed
and shared ceiling ratios, the huge-page fraction, and the I/O throughput shortfall) and
fourteen alerts, of which the memory-headroom and liveness pair are the most important:

- `LoadlabControllerStale` — the most important one. A scrape-based liveness alert would
  stay green through it.
- `LoadlabControllerReconcileErrors` — catches the intermittent failures that
  `loadlab_controller_error` clears on the next good tick.
- `LoadlabNearMemoryCeiling` / `LoadlabNearMemoryCeilingCritical` — the headroom
  threshold.

The rest cover the CPU, file-backed, shared-memory, disk I/O and transparent huge page
controllers; `LoadlabThpRequestIgnored` is deliberately written against the measured
huge-page fraction rather than the requested policy, so it catches a request the kernel is
not honouring, and `LoadlabIoThroughputFarBelowTarget` reads the measured throughput
shortfall rather than trusting the request.

All four were observed `inactive` against a live instance, as expected for a healthy system.

## Why `loadlab_configured_tick_ms` exists

The docs recommend alerting on
`loadlab_controller_last_tick_age_seconds > 5 * tick_ms / 1000`. That rule cannot be
written in PromQL without exposing `tick_ms`, so the exporter carries it. It also means
the liveness threshold follows `tick_ms` when it is changed at runtime, which a
hardcoded number would not.

## A curve can never go below zero

The expression is `mean + amplitude*sin(t/N)`. If the mean is below the amplitude the
curve asks for a negative target part of every cycle. A negative target is rejected at
`loadlab` startup by `validate` **only when the mean itself is negative**, because `validate` evaluates the expression at `t=0`, which for a plain sinusoid is the mean. A curve like `100MiB + 200MiB*sin(t/N)` passes validation and then spends part of
every cycle with a negative target, which the controller clamps to zero. The result: the
arena sits empty, `loadlab_controller_target_arena_bytes` reads zero and
`converged` stays true. A silent flat spot at zero, not a failure.

So keep the mean at or above the amplitude. See `starlight/src/content/docs/deploy.md` for the full
table of working examples.

## A note on `/metrics` vs the Prometheus scrape

`GET /metrics` returns a JSON object and omits a few labels that the Prometheus exposition
does carry (e.g. `loadlab_memory_baseline_rss_anon_bytes`, whose name is only meaningful
next to `loadlab_memory_rss_anon_bytes`). The dashboard and the rules use the Prometheus
form, which is why the baseline metric is not exposed on `/metrics`.
