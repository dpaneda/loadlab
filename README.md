<h1 align="center">loadlab</h1>
<h3 align="center">A programmable Linux workload laboratory</h3>

<p align="center">
<img alt="License" src="https://img.shields.io/badge/license-MIT-blue.svg">
<img alt="Language" src="https://img.shields.io/badge/language-Rust-orange.svg">
<img alt="Platform" src="https://img.shields.io/badge/Linux-FCC624?logo=linux&logoColor=black">
<img alt="MSRV" src="https://img.shields.io/badge/MSRV-1.85-blue.svg">
</p>

**Documentation: [https://dpaneda.github.io/loadlab/](https://dpaneda.github.io/loadlab/)** — the
full book: concepts, every controller, the expression language, the HTTP API and how to deploy it.
This README is the short version.

## What it is

loadlab expresses a desired resource workload as **state**. You point a target at a resource
dimension over HTTP, keep changing your mind while the process runs, and read back what the
kernel actually measured — never just what was requested:

> Express a desired resource workload as state, change that state over HTTP while the process runs,
> and converge toward it continuously while reporting what the kernel actually measured.

- **Independent dimensions.** Anonymous, file-backed and shared memory, page access, CPU, disk
  I/O and raw TCP/UDP network — each with its own target and its own `/proc` observation.
- **Requested versus actual is first-class.** The two are separate fields everywhere, and an
  unconverged controller says so rather than rounding up to its target.
- **Reproducible.** A per-controller `random_seed` replays the same page and block walks.
- **A control plane you can `curl`.** No config file, and no restart to change the workload.

It is a single-host research tool: the observable is the Linux kernel under an intentional
workload, not an application's throughput. For the latter, use `wrk`, `sysbench`, `fio` or
`stress-ng`.

## Quick start

```bash
cargo run --release                                   # API on http://127.0.0.1:8080
curl -s -X PUT localhost:8080/workload \
  -H 'content-type: application/json' \
  -d '{"memory":{"anonymous_rss":"512MiB"}}'          # ask for 512 MiB anonymous RSS
curl -s -X POST localhost:8080/workload/start         # and run it
curl -s localhost:8080/metrics | jq '.controller | {last_target_arena_bytes, arena_resident_bytes, convergence_error_bytes, converged}'
```

Every magnitude also accepts an expression in `t` (seconds since the workload started), so a
workload can be a curve rather than a step:

```json
{ "memory": { "anonymous_rss": "min(2GiB, t * 64MiB)" } }
```

## Development

```bash
make check     # cargo check
make test      # 206 unit + 79 integration tests
make fmt       # cargo fmt
make smoke     # scripts/smoke.sh against a running instance
make docs      # build the Starlight site into starlight/dist
make run       # cargo run --release
```

`make lint` runs clippy with `-D warnings` and does not currently pass; run
`cargo clippy --all-targets` to see what remains.

`examples/basic.sh` is a short end-to-end tour of the API. `scripts/smoke.sh` is what CI-style
checks run.

The documentation lives in [`starlight/`](starlight/) and is deployed to GitHub Pages by
`.github/workflows/docs.yml`.

## License

MIT.