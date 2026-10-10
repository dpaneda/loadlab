# Errors

All error responses share the same JSON shape:

```json
{"error": "message"}
```

Possible HTTP error status codes:

- `400 Bad Request`: Returned when a payload fails JSON parsing, fails schema deserialization, or
  violates validation rules (such as tick intervals outside 1..10000 ms, or requesting allocations above
  process ceilings). Rejected writes never modify desired state.
- `401 Unauthorized`: Returned when an API token is configured and the request does not provide a valid
  Bearer token in the `Authorization` header (or via `?token=` from loopback). Includes header:
  `WWW-Authenticate: Bearer`.
- `413 Payload Too Large`: Returned when the request body exceeds `--max-body-bytes` (default 64 KiB).
- `429 Too Many Requests`: Returned when the mutation rate limit is exhausted. Includes header:
  `Retry-After: <seconds>`.

