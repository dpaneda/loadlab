---
title: "PUT and PATCH /workload"
---

## PUT /workload

Replaces the complete desired state.

The body must deserialize to `WorkloadState` and pass validation.

## PATCH /workload

Applies JSON Merge Patch semantics to the current desired state.

Example:

```json
{
  "memory": {
    "anonymous_rss": "2GiB"
  }
}
```

Set a nested value to `null` to remove it:

```json
{
  "memory": {
    "anonymous_rss": null
  }
}
```

