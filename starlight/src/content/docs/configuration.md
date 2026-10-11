---
title: "Configuration"
---

Every flag has an environment variable of the same name in `LOADLAB_` upper case.

| Flag | Default | Meaning |
|---|---|---|
| `--bind` | `127.0.0.1:8080` | HTTP bind address; keep it on loopback |
| `--max-memory-bytes` | 8 GiB | ceiling on anonymous memory; a request can lower it, never raise it |
| `--max-file-backed-bytes` | 1 GiB | ceiling on the file the file-backed controller creates |
| `--file-backed-dir` | cwd | where those files are created; must be a real filesystem |
| `--max-shared-bytes` | 2 GiB | ceiling on shared **plus** copy-on-write, combined |
| `--max-io-bytes` | 1 GiB | ceiling on the I/O region |
| `--max-network-ops-per-second` | 100000 | ceiling on messages plus fresh connections |
| `--api-token` | unset | Bearer token required on every endpoint but `/health` |
| `--max-body-bytes` | 65536 | largest accepted request body |
| `--mutation-rate-per-second` / `--mutation-burst` | 20 / 40 | mutations accepted before `429` |
| `--sink` | unset | run as a discarding TCP/UDP endpoint instead |

These are the *process* limits. The per-workload desired-state fields that a client writes over
HTTP — `controller.tick_ms`, the memory, CPU, I/O and network knobs — are catalogued in
[GET /state](../http-api/state/).

## Safety

The ceilings are the point of the tool. Memory and disk are clamped and reported as `saturated`
rather than refused, so a run degrades visibly instead of failing mysteriously. The network
ceiling is separate for a different reason: it is the one limit that protects systems other than
the one running loadlab.

The file-backed controller **refuses to start on tmpfs**. Those pages would be accounted as shared
memory rather than file memory, and the workload would silently not be doing what you asked — the
one failure mode worse than an error.

## Exposing it beyond loopback

Setting `--bind` to a routable address hands anyone who can reach the port the ability to allocate
memory and generate traffic. If you do it, set a token:

```bash
LOADLAB_API_TOKEN=s3cret cargo run --release -- --bind 0.0.0.0:8080
```

loadlab does not terminate TLS; put it behind a reverse proxy if the port leaves the host.
