# Broadcast & message-broker targets

A webhook register can deliver to **several destinations at once** and to
**RabbitMQ** and **NATS** in addition to plain HTTP.

```yaml
registers:
  - endpoint: /webhook/github
    method: POST
    delivery: all            # all (default) | any | best_effort
    template: '{"event": "{{ action }}"}'
    targets:
      - { name: audit, url: "http://audit/ingest" }                       # http (default type)
      - { name: bus,   type: rabbitmq, url: "amqp://u:p@mq:5672/%2f", exchange: events, exchange_type: topic, routing_key: github }
      - { name: fan,   type: nats, url: "nats://nats:4222", subject: hooks.github }
```

See `config.broadcast.example.yml` for every option.

## Compatibility

The original `target:` (single, no `type:`) still works unchanged. `target`
and `targets` can be combined; `target` is delivered first.

## Behaviour

* Targets are delivered **concurrently**; a slow/failing target never delays
  or cancels the others.
* `delivery` decides the HTTP status: `all` → every target must succeed,
  `any` → at least one, `best_effort` → always 200. Unmet policy → **502**.
* Response body: `status` (`success` | `partial` | `failed`), `delivered`,
  `total`, and a per-target `results[]` (`ok`/`error`, response or error text,
  `elapsed_ms`). Single-target registers also keep the legacy `target_response`.
* A target may override the register template with its own `template:`.
* Every target is bounded by `HERMES_REQUEST_TIMEOUT` (HTTP targets may
  override with `timeout_seconds`).
* **HTTP targets now count a non-2xx reply as a failed delivery** (previously
  any reply was reported as success).
* Every response carries a `message_id`, and every result an `attempts` count.

## Target types

| type | required | notes |
|------|----------|-------|
| `http` (default) | `url` | `method` (default POST), `headers`, `timeout_seconds` |
| `rabbitmq` | `url` (`amqp://`/`amqps://`) and `exchange` **or** `routing_key`/`queue` | Publisher confirms always on. `exchange_type` declares a durable exchange once; `queue` declares + binds a durable queue; `persistent` (default true); `mandatory` (default true → unroutable = failure); `headers` |
| `nats` | `url`, `subject` | `jetstream: true` waits for the stream ack (stream must already exist); `headers` (server ≥ 2.2) |

Connections are created lazily, cached per URL, and re-established
automatically (RabbitMQ: on the next delivery after a failure; NATS: by the
client). Credentials embedded in URLs are **redacted** from logs and from
`hermes-admin list-endpoints`.

## Reliability: retries, idempotency, circuit breaker

Delivery is **at-least-once**. Retries can produce duplicates (e.g. a target
processed the request but its reply timed out), so every delivery carries the
same **message id**, which consumers can use to discard repeats.

### Retries
Per target; precedence: `target.retry` > register `retry_config` > `settings`.
Waits grow exponentially (`delay_ms` × `backoff_multiplier`ⁿ, capped by
`max_delay_ms`) with jitter. `max_elapsed_ms` (default 15 s) bounds how long the
webhook caller can be kept waiting. **Defaults: 3 attempts, 1 s first wait.**
Set `settings.retry_attempts: 1` to disable.

| retried (transient) | not retried (final) |
|---|---|
| network error, reset, timeout | HTTP 4xx (except 408, 425, 429), 501 |
| HTTP 5xx, 408, 425, 429 (`Retry-After` seconds honoured) | RabbitMQ unroutable message |
| RabbitMQ connect/publish error or nack | RabbitMQ exchange/queue declare error |
| NATS / JetStream connect, publish or ack error | invalid header / method / template output |

### Idempotency key
One id per inbound request, shared by all targets and all retries. Taken from
the first valid header of `Idempotency-Key`, `X-Idempotency-Key`,
`X-GitHub-Delivery`, `X-Request-Id` (1–128 visible ASCII chars); otherwise a
random UUID is generated. Sent as:

| target | where |
|---|---|
| http | `Idempotency-Key` header (a header set on the target wins) |
| rabbitmq | AMQP `message_id` property |
| nats | `Nats-Msg-Id` header — **JetStream drops a repeat inside the stream's duplicate window** (default 2 min). Needs NATS ≥ 2.2 |

Note: a generated id is new for every inbound request, so it protects against
*Hermes's own* retries. To also collapse the *sender's* redeliveries, the
sender must supply a stable key (GitHub's `X-GitHub-Delivery` already is).

### Circuit breaker
Keyed by destination (HTTP URL, or broker URL — all targets on one broker share
it). After `failure_threshold` consecutive unhealthy attempts the destination is
skipped instantly (`attempts: 0`, error `circuit open …`) for `cooldown_secs`,
then a single probe decides whether to close it. Only *unhealthy* outcomes
count: a 4xx or an unroutable message proves the destination is alive.
State is per process and in memory.

## Async mode: durable outbox & dead letters

```yaml
registers:
  - endpoint: /webhook/stripe
    method: POST
    mode: async              # default: sync
    template: '{"type": "{{ type }}"}'
    targets: [ ... ]
```

In `sync` mode (default) the caller waits for delivery and gets the per-target
result. In `async` mode Hermes renders the payloads, **writes one job per target
to a SQLite outbox and fsyncs it, then answers `202`** —
`{"status":"accepted","message_id":…,"queued":N,"duplicate":false}`. Background
workers deliver, so a slow or dead destination never slows the sender down.

| situation | behaviour |
|---|---|
| Hermes crashes / is killed / restarts | queued jobs are on disk and resume; jobs that were mid-delivery are redelivered (same idempotency key) |
| destination down | retried with backoff (`outbox.retry`, default 8 attempts ≈ 4 min, 5 min cap); while its circuit breaker is open jobs wait **without using up attempts** |
| permanent error (4xx, unroutable) or attempts exhausted | job becomes a **dead letter** — kept (7 days by default) until replayed or purged |
| sender repeats a webhook with the same `Idempotency-Key` / `X-GitHub-Delivery` | answered `202` with `"duplicate": true`, **not queued again** (window = `delivered_retention_secs`, 24 h) |
| too many jobs waiting (`max_pending`) | `503` + `Retry-After: 5`, so an outage can't fill the disk |
| template renders invalid JSON | `500` up front, nothing queued |
| a target is removed/reordered in the config while jobs are queued | those jobs are dead-lettered with an explanatory error rather than sent to the wrong place |

Precedence for the retry policy of an async delivery: target `retry` > register
`retry_config` > `outbox.retry`. `delivery:` (all/any/best_effort) only applies
to `sync` registers; async targets are independent.

```bash
hermes-admin outbox stats                              # counts per state, age of oldest job
hermes-admin outbox list --state dead                  # what failed and why
hermes-admin outbox replay --endpoint /webhook/stripe  # re-queue after fixing the cause
hermes-admin outbox replay --id 42                     # …or one job (--all for everything)
hermes-admin outbox purge  --id 42                     # delete dead letters for good
```
`replay`/`purge` require `--id`, `--endpoint` or `--all` and only ever touch
dead letters. They are safe to run while the server is up. In Docker:
`docker exec <container> ./hermes-admin outbox stats`.

**Operating notes**
* Put `HERMES_OUTBOX_PATH` (default `/data/outbox.db` in the images) on a
  **persistent volume**; the bundled compose file does.
* **One Hermes instance per outbox file.** It is a local SQLite database, not a
  shared queue. On start, jobs left "in flight" by a previous process are
  redelivered immediately.
* **Delivery is at-least-once and unordered.** Parallel workers and retries mean
  event B can reach a target before event A. Consumers must tolerate repeats
  (use the idempotency key) and must not depend on order.
* The sender only learns "accepted", not the eventual outcome. **Watch for dead
  letters** (`outbox stats`, `dead N`) — nothing alerts you yet.
* The file holds rendered payloads **in clear text** (created `0600`); treat it
  like the webhooks themselves.
* `HERMES_REQUEST_TIMEOUT` must be below `outbox.lease_secs` (default 300);
  Hermes refuses to start otherwise.

## Admin CLI

`hermes-admin validate-config` now also checks target types, URL schemes,
NATS subjects, duplicate endpoints and every (per-target) template.
`test-template` renders the register template and each target override.

## Testing

```bash
cargo test                                    # unit tests
docker compose -f docker-compose.brokers.yml up -d
cargo build && python3 tests/e2e/run.py       # needs: pip install nats-py pika
```
The e2e suite also `kill -9`s Hermes mid-test to prove the outbox loses nothing,
and exercises dead letters and replay through the real `hermes-admin` binary.
It needs a local NATS (with JetStream) and RabbitMQ.

Requires Rust ≥ 1.89 (lapin 4 / async-nats 0.53).
