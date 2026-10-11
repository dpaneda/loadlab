---
title: "Scenarios"
---

## POST /scenarios/load

Stores a named, versioned workload definition. It does **not** run it: loading a library of
scenarios must not start generating load as a side effect of defining one.

```bash
curl -sS -X POST http://127.0.0.1:8080/scenarios/load -d '{
  "name": "steady",
  "version": "1",
  "workload": {
    "controller": { "tick_ms": 250 },
    "anonymous": { "rss": "256MiB" },
    "cpu": { "utilization": "50" }
  }
}'
```

```json
{
  "name": "steady",
  "version": "1",
  "workload": { "controller": { "tick_ms": 250 }, "memory": { "anonymous_rss": "256MiB" }, "...": "..." }
}
```

The stored `workload` is normalized and validated **before** it is stored, and `running` is always
stored as `false`: a scenario describes *what* to generate, and whether it is generating is what
`POST /scenarios/run` decides. A scenario that validated on the way in therefore cannot fail on the
way out.

Rejected with `400`:

- a workload that does not deserialize, or fails validation;
- a `name` or `version` that is empty, over 128 bytes, or contains a control character;
- a **different** workload re-loaded under a name and version that already exist. A version has to
  identify a document, so changing a scenario means bumping its version. Re-loading identical
  content is idempotent, and loading content that differs only in which spelling of the composite
  aliases it uses is the same scenario, not a conflict.

## POST /scenarios/run

Applies a stored scenario and starts it.

```bash
curl -sS -X POST http://127.0.0.1:8080/scenarios/run -d '{"name":"steady"}'
```

Returns the applied desired state with `running` set to `true`, exactly as `PUT /workload` echoes
back what it installed. A `name` that was never loaded is a `400`.

## POST /scenarios/stop

Stops the workload and clears the active scenario marker. Equivalent to `POST /workload/stop` plus
that marker, which is the point: a scenario drives the same single desired state as any other
client write, so "stop the scenario" and "stop the workload" cannot mean different things. The
desired state itself survives, so restarting does not need the scenario again.

## GET /scenarios

Lists the stored scenarios and names the active one.

```json
{
  "active": "steady",
  "scenarios": [
    { "name": "ramp", "version": "2" },
    { "name": "steady", "version": "1" }
  ]
}
```

`scenarios` is sorted by name. `active` is `null` whenever the desired state is not a scenario's:
after any direct `PUT /workload`, `PATCH /workload`, `POST /workload/stop` or
`POST /scenarios/stop`. A scenario that stayed flagged as active while the workload had been changed
underneath it would be claiming a workload is being driven by something it is not.

