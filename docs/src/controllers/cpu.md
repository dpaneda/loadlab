# CPU utilization

`cpu.utilization` is a number or an expression in **percent of a single logical CPU**, so `100` is one
saturated core, `200` is two, and `400` is four. It is not a percentage of the host: on a
sixteen-core machine `800` saturates the whole machine, and there is no separate "of the host"
form. The reported `actual_utilization_pct` uses the same unit, so a target and its measurement
can be compared directly.

The expression is dimensionless — byte suffixes such as `1MiB` are rejected rather than
silently reinterpreted — and `t` is the experiment clock, as in `memory.anonymous_rss`.
It is evaluated every tick, so `150 + 50*sin(t/30)` tracks a moving setpoint rather than
settling on its value at `t=0`. A negative result is clamped to zero at runtime; a negative
value at `t=0` is a validation error.

`t` is seconds since the workload started, and it restarts with it. `experiment.start_time`
moves that origin to a fixed instant instead, which is what lets two hosts running one scenario
share a timeline; see [The experiment](../experiment.md#the-experiment). It is *not* process uptime:
`loadlab_process_uptime_seconds` is a separate series, and a workload restarted ten minutes into
a process reports a `t` near zero while uptime keeps climbing.

`workers` threads split the target evenly, so two workers at `150` are each asked for `75`, and
the pool's capacity is `workers * 100`. A request above capacity is **clamped to capacity and
reported as saturated**, not silently truncated and not treated as success:

```json
{"cpu":{"utilization":"900","workers":2}}   // delivers 200, reports saturated
```

Oversubscription is allowed: more workers than selected CPUs simply share cores, and it does
not raise capacity. `workers` is capped at 1024, and a CPU ID that does not exist on the host is
rejected. `affinity` accepts only `"auto"` or `"pin"`; under `"pin"` worker `i` is bound to
`cpus[i % cpus.len()]`.

### No threads until there is something to do

A target of zero does not create a pool. A running workload with `cpu.utilization` of `0`
holds no worker threads at all, and `controller.cpu.worker_count` is `0`. This matters more
than it sounds: `workers: 0` means *one per logical CPU*, so spawning on a default configuration
would start a thread per core to sit at duty zero — and the default is what any `PATCH` that
never mentions `cpu` restores. A stopped workload also reports `requested_utilization_pct` as
`0` regardless of what its expression would evaluate to, so a dashboard cannot mistake a
paused experiment for a live one.

### What the controller reports

`GET /metrics` exposes the CPU controller under `controller.cpu`:

| field | meaning |
| --- | --- |
| `worker_count` | worker threads currently running |
| `capacity_pct` | `workers * 100` |
| `requested_utilization_pct` | what the expression asked for, before clamping |
| `target_utilization_pct` | what the pool was told to deliver, after clamping |
| `actual_utilization_pct` | smoothed measured CPU, same unit as the target |
| `duty_cycle` | the fraction of a core each worker is burning, `0.0..=1.0` |
| `converged` | within tolerance of the target, and not saturated |
| `saturated` | the request exceeded capacity |
| `scheduling_available` | whether the kernel exposes per-thread scheduler counters |
| `worker_runqueue_wait_nanoseconds` | nanoseconds the pool spent runnable but not running |
| `worker_worst_runqueue_wait_nanoseconds` | the same figure for the worst single worker |

The last three come from `/proc/self/task/<tid>/schedstat` and need `CONFIG_SCHEDSTATS`. Where
the kernel does not provide it, `scheduling_available` is `false` and both counts are `0` —
which means *not measured*, not *nothing waited*. Gate any panel or alert on the flag; see
[Run-queue wait](#run-queue-wait).

### Run-queue wait

`actual_utilization_pct` measures CPU time consumed. It cannot distinguish a worker that
decided to sleep from one that was ready to run and had no CPU to run on, and the two look
identical from outside the process. The run-queue counters are the missing half, read per
thread rather than from the process total, because the process total would fold in the HTTP
server and the controller and tell you nothing about the pool.

Read them together:

- **utilization at target, wait flat** — the pool is delivering what was asked;
- **utilization below target, wait climbing** — the scheduler, not the duty cycle, is the
  limit; the controller is asking for duty the machine cannot grant;
- **utilization above target with wait flat** — the duty cycle is too high.

`worker_worst_runqueue_wait_nanoseconds` answers a question the total cannot: whether the
wait was spread evenly or one worker was starved while its siblings ran. It is the difference
between "the box is busy" and "this one thread is not getting a CPU".

Both are cumulative since each thread started, and **both restart with the pool**: changing
`cpu.workers` or `cpu.cpus` tears the threads down and builds new ones whose counters begin at
zero. They are therefore exported as a gauge, not a counter. That is the honest shape for a
quantity that genuinely falls to zero and climbs again — accumulated into a Prometheus counter,
whose readings are differenced and never allowed to decrease, this would freeze at its previous
peak for as long as the new pool took to exceed it, and a metric that silently stops reporting is
worse than one that visibly resets. Use `deriv()` rather than `rate()` for nanoseconds of wait per
second, and read a drop to zero as "the pool was rebuilt", not as "nobody waited".

A worker that exits before its `schedstat` is read leaves no trace in the pool total. The
figures are therefore a slight undercount across worker churn, and an exact one for the steady
state the controller spends nearly all its time in.

`actual_utilization_pct` is *smoothed*, not the raw per-tick sample. `/proc` reports CPU time
in whole clock ticks, so a single 100ms window can only be counted in steps of about ten points
of a core, and a perfectly steady load reads as 40/50/60/50 if published raw. The controller
has always steered on the smoothed value; publishing the same number keeps `converged` from
appearing to flap when it is not.

### The measurement floor

A reading over a window of `tick_ms` can only move in whole clock ticks, so the finest
difference it can express is `1000 / tick_ms` points of a core — ten points at the default
100ms tick. A target below that **cannot be verified**, and the controller treats it
differently on purpose:

- **at or above the floor**: the duty is feedforward plus a small integral correction, so a
  steady bias between the requested fraction of a core and the fraction a busy machine actually
  delivers is cancelled out;
- **below the floor**: only the feedforward is applied, and the integral stands still.

That is not a workaround. Below the floor the reading is rounding error, and integrating it
turns a load generator into a noise amplifier: a `0.5` target on a 100ms tick reads as `0`, then
`10`, then `0`, and an uncorrected controller swings its duty between nothing and 15% of a core
chasing a signal two hundred times smaller than the quantisation. With the integral held, a
`0.5` target delivers a steady duty of `0.005`.

`converged` still reports true in that regime, and it should be read carefully there: it means
"as close as the measurement can see", not "verified". Lowering `controller.tick_ms` narrows
the window and buys finer resolution, at the cost of a busier control loop. The floor is
exported as `loadlab_cpu_measurement_resolution_percent` so the two can be compared directly.

