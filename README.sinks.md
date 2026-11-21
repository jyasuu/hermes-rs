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

Requires Rust ≥ 1.89 (lapin 4 / async-nats 0.53).
