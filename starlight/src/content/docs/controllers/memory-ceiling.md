---
title: "Memory ceiling"
---

`controller.max_memory_bytes` in desired state is a *request*, not a limit the operator can be
overruled on. The process is started with a hard ceiling:

```bash
cargo run --release -- --max-memory-bytes 536870912   # or LOADLAB_MAX_MEMORY_BYTES
```

On every `PUT` and `PATCH` the requested value is clamped to that process ceiling **before**
validation, so:

- a client may lower the ceiling at runtime;
- a client can never raise it above the process ceiling, including by omitting `controller`
  entirely (a missing field would otherwise fall back to the built-in default);
- a target above the effective ceiling is rejected with `400` rather than silently truncated.

`GET /state` always reports the effective, already-clamped value.

