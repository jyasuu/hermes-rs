import asyncio, json, subprocess, time, urllib.request, urllib.error, os, sys, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import nats, pika

def post(path, body, headers=None):
    req = urllib.request.Request("http://127.0.0.1:3000"+path, data=json.dumps(body).encode(), method="POST",
                                 headers={"Content-Type":"application/json", **(headers or {})})
    try:
        with urllib.request.urlopen(req, timeout=20) as r: return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e: return e.code, json.loads(e.read())

# ---- controllable HTTP target ------------------------------------------------
SEEN = []          # (path, idempotency-key) for every request received
COUNTS = {}        # path -> number of requests so far
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        n = COUNTS[self.path] = COUNTS.get(self.path, 0) + 1
        SEEN.append((self.path, self.headers.get("Idempotency-Key")))
        code, extra = 200, {}
        if self.path == "/flaky" and n <= 2: code = 500
        elif self.path == "/perm": code = 400
        elif self.path == "/rl" and n == 1: code, extra = 429, {"Retry-After": "1"}
        elif self.path == "/always500": code = 500
        body = json.dumps({"seen": n}).encode()
        self.send_response(code)
        for k, v in extra.items(): self.send_header(k, v)
        self.send_header("Content-Type", "application/json"); self.send_header("Content-Length", str(len(body)))
        self.end_headers(); self.wfile.write(body)
def serve(port):
    srv = ThreadingHTTPServer(("127.0.0.1", port), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv

results = []
def check(name, cond, extra=""):
    results.append(cond); print(("PASS " if cond else "FAIL ") + name + (f"  [{extra}]" if extra and not cond else ""))

async def main():
    serve(3999)
    env = dict(os.environ, HERMES_CONFIG_PATH=os.path.join(os.path.dirname(os.path.abspath(__file__)), "config.yml"), HERMES_LOG_FORMAT="json", HERMES_LOG_LEVEL="info")
    srv = subprocess.Popen([os.environ.get("HERMES_BIN", "target/debug/hermes-rs")], env=env,
                           stdout=open(os.environ.get("HERMES_E2E_LOG", "/tmp/hermes-e2e.log"),"w"), stderr=subprocess.STDOUT)
    try:
        for _ in range(50):
            try: urllib.request.urlopen("http://127.0.0.1:3000/health", timeout=1); break
            except Exception: time.sleep(0.2)

        nc = await nats.connect("nats://127.0.0.1:4222")
        # Start from a clean slate so the suite is re-runnable.
        _js = nc.jetstream()
        try: await _js.delete_stream("HOOKS")
        except Exception: pass
        await _js.add_stream(name="HOOKS", subjects=["js.>"])
        _c = pika.BlockingConnection(pika.ConnectionParameters("127.0.0.1")); _ch = _c.channel()
        for q in ("hermes.events.q", "hermes.direct.q"): _ch.queue_delete(q)
        _c.close()
        got = {"fan": [], "any": [], "all": []}
        async def mk(subj, key):
            async def cb(m): got[key].append((m.data.decode(), dict(m.headers or {})))
            await nc.subscribe(subj, cb=cb)
        await mk("hooks.fan","fan"); await mk("hooks.any","any"); await mk("hooks.all","all")
        await nc.flush()

        # 1. legacy single target, original response shape
        s, r = post("/webhook/legacy", {"message": "line1\nline2"})
        check("legacy: 200 + status success", s == 200 and r["status"] == "success", r)
        check("legacy: target_response preserved", r.get("target_response", {}).get("message") == "Payload logged", r)

        # 2. full broadcast
        s, r = post("/webhook/fan", {"action": "opened", "user": "alice"}, {"Idempotency-Key": "e2e-key-1"})
        check("broadcast: 200 success, 5/5", s == 200 and r["status"] == "success" and r["delivered"] == 5, r)
        check("broadcast: each result ok", all(x["status"] == "ok" for x in r["results"]), r)
        await asyncio.sleep(0.5)
        check("nats core received rendered payload", len(got["fan"]) == 1 and json.loads(got["fan"][0][0]) == {"event":"opened","who":"alice"}, got["fan"])
        check("nats header delivered", got["fan"] and got["fan"][0][1].get("X-Source") == "hermes", got["fan"])
        check("idempotency: response echoes sender's key", r["message_id"] == "e2e-key-1", r.get("message_id"))
        check("idempotency: Nats-Msg-Id == key", got["fan"][0][1].get("Nats-Msg-Id") == "e2e-key-1", got["fan"])

        js = nc.jetstream()
        info = await js.stream_info("HOOKS")
        check("jetstream stored 1 msg", info.state.messages == 1, info.state)
        m = await js.get_msg("HOOKS", seq=1)
        check("jetstream used per-target template", json.loads(m.data) == {"js": True, "event": "opened"}, m.data)
        check("jetstream ack in response", any(x["target"]=="nats-js" and x["response"].get("sequence")==1 for x in r["results"]), r)

        conn = pika.BlockingConnection(pika.ConnectionParameters("127.0.0.1"))
        ch = conn.channel()
        meth, props, body = ch.basic_get("hermes.events.q", auto_ack=True)
        check("rabbitmq fanout->queue got message", meth is not None and json.loads(body) == {"event":"opened","who":"alice"}, body)
        check("rabbitmq persistent + header + content-type", meth is not None and props.delivery_mode == 2 and props.headers.get("X-Source") == "hermes" and props.content_type == "application/json", props)
        check("idempotency: AMQP message_id == key", props.message_id == "e2e-key-1", props.message_id)
        meth, props, body = ch.basic_get("hermes.direct.q", auto_ack=True)
        check("rabbitmq default-exchange queue got message (non-persistent)", meth is not None and props.delivery_mode == 1, props)

        # 3. repeat to prove connection reuse (no reconnect storm) & ordering
        for i in range(20): post("/webhook/fan", {"action": f"n{i}", "user": "bob"})
        await asyncio.sleep(0.5)
        q = ch.queue_declare("hermes.events.q", passive=True).method.message_count
        check("20 more publishes -> 20 in rabbit queue", q == 20, q)
        conns = len(json.loads(subprocess.check_output(["rabbitmqctl","list_connections","--formatter","json","name"])))
        check("rabbitmq: only ONE hermes connection reused (+1 pika)", conns <= 2, conns)

        # 4. delivery policies
        t0 = time.time(); s, r = post("/webhook/any", {})
        check("policy any: dead+alive => 200 partial", s == 200 and r["status"] == "partial" and r["delivered"] == 1, (s, r))
        check("policy any: runs concurrently, honors timeout_seconds=2", time.time()-t0 < 4, time.time()-t0)
        s, r = post("/webhook/all", {})
        check("policy all: dead+alive => 502 failed", s == 502 and r["status"] == "failed" and "dead:" in r["error"], (s, r))
        await asyncio.sleep(0.3)
        check("policy all: alive target STILL delivered (failure isolation)", len(got["all"]) == 1 and len(got["any"]) == 1, got)

        # 5. unroutable mandatory publish
        s, r = post("/webhook/unroutable", {})
        check("rabbit mandatory unroutable => 502 w/ reason", s == 502 and "unroutable" in r["error"], (s, r))

        # 6. error paths unchanged
        s, r = post("/webhook/nope", {}); check("unknown endpoint 404", s == 404, (s, r))
        req = urllib.request.Request("http://127.0.0.1:3000/webhook/fan", data=b"not json", method="POST")
        try: urllib.request.urlopen(req); s = 200
        except urllib.error.HTTPError as e: s = e.code
        check("invalid JSON 400", s == 400, s)


        # ================= reliability: idempotency / retries / breaker =================
        s, r = post("/webhook/legacy", {"message": "x"})
        check("idempotency: id generated when sender gives none (uuid)", len(r["message_id"]) == 36, r.get("message_id"))
        s, r = post("/webhook/legacy", {"message": "x"}, {"X-GitHub-Delivery": "gh-guid-9"})
        check("idempotency: X-GitHub-Delivery honoured", r["message_id"] == "gh-guid-9", r.get("message_id"))

        # JetStream dedupe: same key again must NOT store a second copy
        before = (await nc.jetstream().stream_info("HOOKS")).state.messages
        s, r = post("/webhook/fan", {"action": "opened", "user": "alice"}, {"Idempotency-Key": "e2e-key-1"})
        after = (await nc.jetstream().stream_info("HOOKS")).state.messages
        js = [x for x in r["results"] if x["target"] == "nats-js"][0]
        check("jetstream dedupes a replay of the same key", after == before and js["response"]["duplicate"] is True, (before, after, js))

        # retries: 500, 500, 200
        SEEN.clear(); t0 = time.time()
        s, r = post("/webhook/flaky", {}, {"Idempotency-Key": "retry-key"})
        res = r["results"][0]
        check("retry: 500,500,200 => success after 3 attempts", s == 200 and res["attempts"] == 3, (s, res))
        keys = [k for p, k in SEEN if p == "/flaky"]
        check("retry: SAME Idempotency-Key on every attempt", keys == ["retry-key"] * 3, keys)
        check("retry: backoff waited (100ms + 200ms)", time.time() - t0 >= 0.29, time.time() - t0)

        # permanent 4xx is not retried
        s, r = post("/webhook/permanent", {})
        check("no retry on 400 (1 attempt, 502)", s == 502 and r["results"][0]["attempts"] == 1 and COUNTS["/perm"] == 1, (s, r, COUNTS.get("/perm")))

        # Retry-After honoured
        t0 = time.time(); s, r = post("/webhook/ratelimited", {}); el = time.time() - t0
        check("429 Retry-After:1 honoured (>=1s, 2 attempts, ok)", s == 200 and r["results"][0]["attempts"] == 2 and el >= 0.95, (s, el, r))

        # retry time budget
        t0 = time.time(); s, r = post("/webhook/budget", {}); el = time.time() - t0
        a = r["results"][0]["attempts"]
        check("retry budget stops retries early (<10 attempts, <3s)", s == 502 and 2 <= a < 10 and el < 3 and "budget" in r["error"], (s, a, el, r.get("error")))

        # per-target retry overrides register retry_config (2 attempts, not 5)
        s, r = post("/webhook/override", {})
        check("per-target retry overrides register (2 attempts, not 5)", r["results"][0]["attempts"] == 2 and COUNTS["/always500"] == 2, (r["results"][0], COUNTS.get("/always500")))

        # circuit breaker: threshold 4, cooldown 3s
        att = []
        for _ in range(4):
            s, r = post("/webhook/breaker", {}); att.append(r["results"][0]["attempts"])
        check("breaker: 4 real failures first", att == [1, 1, 1, 1], att)
        t0 = time.time(); s, r = post("/webhook/breaker", {}); el = time.time() - t0
        res = r["results"][0]
        check("breaker: 5th call rejected instantly, 0 attempts, 'circuit open'", s == 502 and res["attempts"] == 0 and "circuit open" in res["error"] and el < 0.5, (s, res, el))
        bsrv = serve(3998)                       # destination comes back…
        s, r = post("/webhook/breaker", {})
        check("breaker: still open during cooldown even though target is back", "circuit open" in r["results"][0]["error"], r)
        time.sleep(3.2)                         # …cooldown elapses
        s, r = post("/webhook/breaker", {})
        check("breaker: half-open probe succeeds -> closed", s == 200 and r["results"][0]["attempts"] == 1, (s, r))
        s, r = post("/webhook/breaker", {})
        check("breaker: stays closed afterwards", s == 200, (s, r))
        bsrv.shutdown()

        # 7. resilience: kill RabbitMQ connection from broker side, next publish must reconnect
        for c in json.loads(subprocess.check_output(["rabbitmqctl","list_connections","--formatter","json","name","user"])):
            if "pika" not in c["name"]: pass
        subprocess.run(["rabbitmqctl","close_all_connections","e2e-test"], capture_output=True)
        await asyncio.sleep(1)
        conn = pika.BlockingConnection(pika.ConnectionParameters("127.0.0.1")); ch = conn.channel()
        ok = False
        for _ in range(3):  # allow at most one failed attempt while hermes notices the dead channel
            s, r = post("/webhook/fan", {"action": "after-kill", "user": "carol"})
            if s == 200: ok = True; break
        check("rabbit: recovers after broker closes connections", ok, (s, r))
        await nc.close()
    finally:
        srv.terminate(); srv.wait(timeout=10)
    print(f"\n{sum(results)}/{len(results)} checks passed")
    sys.exit(0 if all(results) else 1)
asyncio.run(main())
