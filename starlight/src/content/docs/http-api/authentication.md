---
title: "Authentication and security"
---

By default, loadlab starts with no token configured and binds to loopback (`127.0.0.1:8080`). In this
mode the API is open to any local process.

When `--api-token <token>` or `LOADLAB_API_TOKEN` is set:

- All routes except `GET /health` require authentication.
- Standard client authorization header:
  ```http
  Authorization: Bearer <token>
  ```
- Scheme name matching is case-insensitive (`Bearer` or `bearer`). Tokens must not contain whitespace.
- Scrapes from loopback may pass `?token=<token>` in the query string (e.g. for Prometheus scrapes where
  header injection is awkward). Non-loopback requests ignore this parameter and require the header.
- Unauthorized requests return `401 Unauthorized` with `WWW-Authenticate: Bearer` and an error body.

### Request limits

- **Payload size:** Limited by `--max-body-bytes` / `LOADLAB_MAX_BODY_BYTES` (default: 64 KiB).
  Requests exceeding this return `413 Payload Too Large`.
- **Mutation rate:** Mutations (`PUT`, `PATCH`, `POST`, `DELETE`) are governed by a token bucket
  configured via `--mutation-rate-per-second` (default: 20) and `--mutation-burst` (default: 40).
  Requests exceeding the bucket return `429 Too Many Requests` with a `Retry-After: <seconds>` header.
- **Reads (`GET`):** Unthrottled so scrapes and monitoring never interfere with or get blocked by
  workload management.

