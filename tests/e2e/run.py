import asyncio, hashlib, hmac as _hmac, base64, json, shutil, signal, subprocess, time, urllib.request, urllib.error, os, sys, threading
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
HEALTHY = {"dlq": False}
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def handle(self):
        try: super().handle()
        except (BrokenPipeError, ConnectionResetError): pass   # Hermes was killed on purpose mid-request
    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        n = COUNTS[self.path] = COUNTS.get(self.path, 0) + 1
        SEEN.append((self.path, self.headers.get("Idempotency-Key")))
        code, extra = 200, {}
        if self.path == "/flaky" and n <= 2: code = 500
        elif self.path == "/perm": code = 400
        elif self.path == "/rl" and n == 1: code, extra = 429, {"Retry-After": "1"}
        elif self.path == "/always500": code = 500
        elif self.path == "/aflaky" and n <= 2: code = 500
        elif self.path == "/dlq" and not HEALTHY["dlq"]: code = 500
        elif self.path == "/slow": time.sleep(1.5)
        elif self.path == "/slow2": time.sleep(1.5)
        body = json.dumps({"seen": n}).encode()
        self.send_response(code)
        for k, v in extra.items(): self.send_header(k, v)
        self.send_header("Content-Type", "application/json"); self.send_header("Content-Length", str(len(body)))
        self.end_headers(); self.wfile.write(body)
def serve(port):
    srv = ThreadingHTTPServer(("127.0.0.1", port), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv

ROOT = os.path.dirname(os.path.abspath(__file__))
SRC_CFG = os.path.join(ROOT, "config.yml")
WORK = "/tmp/hermes-e2e-config"          # Hermes runs from a COPY so the test can edit it
CFG = os.path.join(WORK, "config.yml")
ROT_FILE = os.path.join(WORK, "rot_secret")
OUTBOX = "/tmp/hermes-e2e-outbox/outbox.db"
HERMES = {"p": None}
def hermes_env(**extra):
    env = dict(os.environ, HERMES_CONFIG_PATH=CFG, HERMES_LOG_FORMAT="json", HERMES_LOG_LEVEL="info",
               HERMES_OUTBOX_PATH=OUTBOX, E2E_METRICS_TOKEN="metrics-tok", E2E_SHOPIFY_SECRET="shop-secret", E2E_ROT_SECRET_FILE=ROT_FILE)
    env.update(extra)
    return env

def http_req(path, data=None, headers=None, method="POST", port=3000):
    r = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=data, method=method, headers=headers or {})
    try:
        with urllib.request.urlopen(r, timeout=20) as resp: return resp.status, {k.lower(): v for k, v in resp.headers.items()}, resp.read()
    except urllib.error.HTTPError as e: return e.code, {k.lower(): v for k, v in e.headers.items()}, e.read()

def hex_sig(secret, body): return "sha256=" + _hmac.new(secret.encode(), body, hashlib.sha256).hexdigest()
def b64_sig(secret, body): return base64.b64encode(_hmac.new(secret.encode(), body, hashlib.sha256).digest()).decode()

def stripe_header(secret, t, body):
    sig = _hmac.new(secret.encode(), f"{t}.".encode() + body, hashlib.sha256).hexdigest()
    return f"t={t},v1={sig}"
def slack_headers(secret, ts, body):
    sig = _hmac.new(secret.encode(), f"v0:{ts}:".encode() + body, hashlib.sha256).hexdigest()
    return {"X-Slack-Signature": "v0=" + sig, "X-Slack-Request-Timestamp": str(ts)}

def cfg_text(): return open(CFG).read()
def write_cfg(t): open(CFG, "w").write(t)
def reload_counts():
    s, text = scrape()
    return (metric(text, 'hermes_config_reloads_total{result="success"}') or 0, metric(text, 'hermes_config_reloads_total{result="failure"}') or 0)
async def do_reload():
    """SIGHUP, wait until Hermes has finished handling it. True = applied, False = rejected."""
    before = reload_counts()
    os.kill(HERMES["p"].pid, signal.SIGHUP)
    done = await wait_for(lambda: reload_counts() != before, timeout=6)
    after = reload_counts()
    return bool(done and after[0] > before[0])

def metric(text, series):
    """Value of an exact series line, e.g. 'hermes_x_total{a="b"}', or None."""
    for line in text.splitlines():
        if line.startswith(series + " "): return float(line.rsplit(" ", 1)[1])
    return None
def scrape(token="metrics-tok"):
    s, h, b = http_req("/metrics", method="GET", headers={"Authorization": f"Bearer {token}"} if token else {})
    return s, b.decode()
def start_hermes():
    HERMES["p"] = subprocess.Popen([os.environ.get("HERMES_BIN", "target/debug/hermes-rs")], env=hermes_env(),
                                   stdout=open(os.environ.get("HERMES_E2E_LOG", "/tmp/hermes-e2e.log"), "a"),
                                   stderr=subprocess.STDOUT)
    for _ in range(100):
        try: urllib.request.urlopen("http://127.0.0.1:3000/health", timeout=1); return
        except Exception: time.sleep(0.1)
    raise RuntimeError("hermes did not start")
def kill_hermes():          # SIGKILL: no graceful shutdown, like a crash / OOM kill
    HERMES["p"].kill(); HERMES["p"].wait(timeout=10)
def admin(*args):
    r = subprocess.run([os.environ.get("HERMES_ADMIN_BIN", "target/debug/hermes-admin"), "outbox", "-c", CFG, *args],
                       env=hermes_env(), capture_output=True, text=True)
    return r.returncode, r.stdout + r.stderr
async def wait_for(pred, timeout=10, step=0.1):
    t0 = time.time()
    while time.time() - t0 < timeout:
        if pred(): return True
        await asyncio.sleep(step)
    return pred()

results = []
def check(name, cond, extra=""):
    results.append(cond); print(("PASS " if cond else "FAIL ") + name + (f"  [{extra}]" if extra and not cond else ""))

async def main():
    os.makedirs(WORK, exist_ok=True)
    shutil.copy(SRC_CFG, CFG)
    open(ROT_FILE, "w").write("rot-1\n")
    for suffix in ("", "-wal", "-shm"):
        try: os.remove(OUTBOX + suffix)
        except FileNotFoundError: pass
    os.makedirs(os.path.dirname(OUTBOX), exist_ok=True)
    open(os.environ.get("HERMES_E2E_LOG", "/tmp/hermes-e2e.log"), "w").close()
    serve(3999)
    start_hermes()
    try:
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


        # ======================= async mode: durable outbox =======================
        _c = pika.BlockingConnection(pika.ConnectionParameters("127.0.0.1")); _ch = _c.channel()
        try: _ch.queue_delete("hermes.async.q")
        except Exception: pass
        _c.close()
        got["async"] = []
        await mk("hooks.async", "async"); await nc.flush()

        t0 = time.time()
        s, r = post("/webhook/async-fan", {"action": "opened"}, {"Idempotency-Key": "async-1"})
        el = time.time() - t0
        check("async: 202 accepted, 3 jobs queued", s == 202 and r["status"] == "accepted" and r["queued"] == 3 and r["duplicate"] is False, (s, r))
        check("async: replies fast, before any delivery (<0.4s)", el < 0.4, el)
        check("async: response echoes message_id", r.get("message_id") == "async-1", r)

        ok = await wait_for(lambda: len(got["async"]) >= 1 and COUNTS.get("/aflaky", 0) >= 3, timeout=10)
        check("async: nats delivered", len(got["async"]) == 1 and json.loads(got["async"][0][0]) == {"event": "opened"}, got["async"])
        check("async: http retried 500,500,200 in background (3 hits)", COUNTS.get("/aflaky") == 3, COUNTS.get("/aflaky"))
        keys = [k for p, k in SEEN if p == "/aflaky"]
        check("async: SAME Idempotency-Key across background retries", keys == ["async-1"] * 3, keys)
        check("async: Nats-Msg-Id == key", got["async"] and got["async"][0][1].get("Nats-Msg-Id") == "async-1", got["async"])
        _c = pika.BlockingConnection(pika.ConnectionParameters("127.0.0.1")); _ch = _c.channel()
        meth, props, body = _ch.basic_get("hermes.async.q", auto_ack=True); _c.close()
        check("async: rabbitmq delivered with message_id", meth is not None and props.message_id == "async-1", props)

        await wait_for(lambda: "pending    0" in admin("stats")[1] and "in_flight  0" in admin("stats")[1], timeout=5)
        rc, out = admin("stats")
        check("admin outbox stats: 3 delivered, 0 pending, 0 dead", rc == 0 and "delivered  3" in out and "pending    0" in out and "dead       0" in out, out)

        # sender redelivery with the same key is recognised, nothing delivered twice
        s, r = post("/webhook/async-fan", {"action": "opened"}, {"Idempotency-Key": "async-1"})
        check("async: repeated sender key => 202 duplicate, 0 queued", s == 202 and r["duplicate"] is True and r["queued"] == 0, (s, r))
        await asyncio.sleep(1.0)
        check("async: duplicate was NOT delivered again", COUNTS.get("/aflaky") == 3 and len(got["async"]) == 1, (COUNTS.get("/aflaky"), len(got["async"])))

        # a template that renders invalid JSON is refused up front
        s, r = post("/webhook/async-badtpl", {"x": 1})
        check("async: bad template => 500 and nothing queued", s == 500 and "not valid JSON" in r["error"] and "delivered  3" in admin("stats")[1] and "pending    0" in admin("stats")[1], (s, r))

        # ---- dead letter + replay ----
        s, r = post("/webhook/async-dlq", {}, {"Idempotency-Key": "dlq-1"})
        check("dlq: accepted", s == 202, (s, r))
        ok = await wait_for(lambda: "dead       1" in admin("stats")[1], timeout=8)
        rc, out = admin("list", "--state", "dead")
        check("dlq: retries exhausted => dead letter with reason", ok and "dlq" in out and "HTTP 500" in out and " 2 " in out, out)
        check("dlq: target got exactly 2 attempts", COUNTS.get("/dlq") == 2, COUNTS.get("/dlq"))
        rc, out = admin("replay")
        check("admin replay: refuses without --id/--endpoint/--all", rc != 0, out)
        HEALTHY["dlq"] = True                                  # the outage is over…
        rc, out = admin("replay", "--endpoint", "/webhook/async-dlq")
        check("admin replay --endpoint re-queues 1 job", rc == 0 and "Re-queued 1" in out, out)
        ok = await wait_for(lambda: "dead       0" in admin("stats")[1] and COUNTS.get("/dlq") == 3, timeout=8)
        keys = [k for p, k in SEEN if p == "/dlq"]
        check("dlq: replayed job delivered, same idempotency key", ok and keys == ["dlq-1"] * 3, (COUNTS.get("/dlq"), keys))

        # ---- crash safety 1: queued jobs survive kill -9 ----
        for i in (1, 2, 3):
            s, r = post("/webhook/async-crash", {}, {"Idempotency-Key": f"crash-{i}"})
            assert s == 202, (s, r)
        kill_hermes()
        check("crash: jobs are on disk after kill -9 (outbox file present)", os.path.getsize(OUTBOX) > 0)
        crash_srv = serve(3997)                                 # destination becomes reachable
        start_hermes()                                          # restart: workers resume from disk
        ok = await wait_for(lambda: {k for p, k in SEEN if p == "/crash"} == {"crash-1", "crash-2", "crash-3"}, timeout=15)
        check("crash: all 3 accepted webhooks delivered after restart (none lost)", ok, [k for p, k in SEEN if p == "/crash"])
        await wait_for(lambda: "pending    0" in admin("stats")[1], timeout=5)
        check("crash: queue drained", "pending    0" in admin("stats")[1] and "in_flight  0" in admin("stats")[1], admin("stats")[1])

        # ---- crash safety 2: kill while a delivery is IN FLIGHT ----
        s, r = post("/webhook/async-slow", {}, {"Idempotency-Key": "slow-1"})
        check("slow: 202 immediately although target takes 1.5s", s == 202, (s, r))
        ok = await wait_for(lambda: COUNTS.get("/slow", 0) >= 1, timeout=5)   # request reached the target…
        kill_hermes()                                                          # …and Hermes dies mid-request
        in_flight = admin("stats")[1]
        check("slow: job is stuck in_flight at crash time", ok and "in_flight  1" in in_flight, in_flight)
        start_hermes()
        ok = await wait_for(lambda: COUNTS.get("/slow", 0) >= 2 and "delivered" in admin("stats")[1] and "in_flight  0" in admin("stats")[1], timeout=12)
        keys = [k for p, k in SEEN if p == "/slow"]
        check("slow: orphaned in-flight job redelivered after restart (at-least-once)", ok and len(keys) >= 2, (COUNTS.get("/slow"), keys))
        check("slow: redelivery carries the SAME key so the consumer can dedupe", set(keys) == {"slow-1"}, keys)
        crash_srv.shutdown()


        # ============================ inbound authentication ============================
        N0 = COUNTS.get("/auth-ok", 0)
        body = b'{"action": "opened"}'
        s, h, b = http_req("/webhook/auth-github", body, {"X-Hub-Signature-256": hex_sig("gh-secret", body)})
        check("auth hmac: valid signature => 200 and delivered", s == 200 and COUNTS.get("/auth-ok") == N0 + 1, (s, b))

        s, h, b = http_req("/webhook/auth-github", body, {"X-Hub-Signature-256": hex_sig("wrong", body)})
        check("auth hmac: wrong secret => 401, generic body", s == 401 and json.loads(b) == {"error": "Unauthorized"}, (s, b))
        s, h, b = http_req("/webhook/auth-github", body)
        check("auth hmac: missing header => 401 (same body, nothing to probe)", s == 401 and json.loads(b) == {"error": "Unauthorized"}, (s, b))
        s, h, b = http_req("/webhook/auth-github", b'{"action": "CLOSED"}', {"X-Hub-Signature-256": hex_sig("gh-secret", body)})
        check("auth hmac: tampered body => 401", s == 401, (s, b))
        s, h, b = http_req("/webhook/auth-github", body, {"X-Hub-Signature-256": "sha256=nothex"})
        check("auth hmac: malformed signature => 401, no crash", s == 401, (s, b))
        check("auth: rejected requests never reached the target", COUNTS.get("/auth-ok") == N0 + 1, COUNTS.get("/auth-ok"))

        s, h, b = http_req("/webhook/auth-github", b"this is not json")
        check("auth runs BEFORE parsing: bad JSON + no signature => 401, not 400", s == 401, (s, b))
        junk = b"this is not json"
        s, h, b = http_req("/webhook/auth-github", junk, {"X-Hub-Signature-256": hex_sig("gh-secret", junk)})
        check("auth: bad JSON WITH a valid signature => 400", s == 400, (s, b))

        s, h, b = http_req("/webhook/auth-shopify", body, {"X-Shopify-Hmac-Sha256": b64_sig("shop-secret", body)})
        check("auth hmac: base64 + secret read from ${ENV}", s == 200, (s, b))
        s, h, b = http_req("/webhook/auth-shopify", body, {"X-Shopify-Hmac-Sha256": b64_sig("${E2E_SHOPIFY_SECRET}", body)})
        check("auth: the literal '${…}' text is not accepted as the secret", s == 401, (s, b))

        s, h, b = http_req("/webhook/auth-bearer", body, {"Authorization": "Bearer tok-123"})
        check("auth bearer: valid token => 200", s == 200, (s, b))
        s, h, b = http_req("/webhook/auth-bearer", body, {"Authorization": "Bearer tok-124"})
        check("auth bearer: wrong token => 401", s == 401, (s, b))
        s, h, b = http_req("/webhook/auth-bearer", body, {"Authorization": "tok-123"})
        check("auth bearer: missing scheme => 401", s == 401, (s, b))
        s, h, b = http_req("/webhook/auth-gitlab", body, {"X-Gitlab-Token": "glpat-xyz"})
        check("auth bearer: custom header (GitLab) => 200", s == 200, (s, b))
        s, h, b = http_req("/webhook/auth-gitlab", body, {"Authorization": "Bearer glpat-xyz"})
        check("auth bearer: custom-header endpoint ignores Authorization => 401", s == 401, (s, b))

        # auth protects the outbox too
        before = admin("stats")[1]
        s, h, b = http_req("/webhook/auth-async", body, {"X-Hub-Signature-256": hex_sig("nope", body)})
        check("auth + async: bad signature => 401 and NOTHING queued", s == 401 and admin("stats")[1] == before, (s, b))
        s, h, b = http_req("/webhook/auth-async", body, {"X-Hub-Signature-256": hex_sig("gh-secret", body), "Idempotency-Key": "auth-async-1"})
        check("auth + async: valid signature => 202", s == 202 and json.loads(b)["queued"] == 1, (s, b))

        # unknown endpoint is still a plain 404 (no auth to evaluate)
        s, h, b = http_req("/webhook/does-not-exist", body)
        check("unknown endpoint => 404", s == 404, s)

        # ---- the declared method is enforced ----
        s, h, b = http_req("/webhook/legacy", method="GET")
        check("method enforced: GET on a POST endpoint => 405 + Allow: POST", s == 405 and h.get("allow") == "POST", (s, h))

        # ---- body size limit ----
        big = json.dumps({"message": "x" * 3_000_000}).encode()
        try: s, h, b = http_req("/webhook/legacy", big)
        except Exception as e: s = f"conn-error {type(e).__name__}"
        check("body limit: 3 MB payload => 413", s == 413, s)

        # ================================== metrics ==================================
        s, text = scrape(token=None)
        check("metrics: no token => 401", s == 401, s)
        s, text = scrape(token="wrong")
        check("metrics: wrong token => 401", s == 401, s)
        s, text = scrape()
        check("metrics: valid token => 200 Prometheus text", s == 200 and "# TYPE hermes_webhook_requests_total counter" in text, s)

        R = lambda ep, out: metric(text, f'hermes_webhook_requests_total{{endpoint="{ep}",outcome="{out}"}}')
        check("metrics: request outcomes counted exactly (hmac: 1 success, 5 unauthorized, 1 bad-json-after-auth)",
              R("/webhook/auth-github", "success") == 1 and R("/webhook/auth-github", "unauthorized") == 5 and R("/webhook/auth-github", "bad_request") == 1,
              [l for l in text.splitlines() if "auth-github" in l and "requests_total" in l])
        check("metrics: async accepted counted", R("/webhook/auth-async", "accepted") == 1 and R("/webhook/auth-async", "unauthorized") == 1, text[:0])
        check("metrics: 405 counted", R("/webhook/legacy", "method_not_allowed") == 1)
        check("metrics: delivery attempts labelled by endpoint/target/kind/result",
              metric(text, 'hermes_delivery_attempts_total{endpoint="/webhook/auth-github",kind="http",result="ok",target="http:0"}') == 1)
        check("metrics: attempt-duration histogram present", (metric(text, 'hermes_delivery_attempt_duration_seconds_count{kind="http"}') or 0) >= 1)
        check("metrics: build info present", 'hermes_build_info{version="' in text)

        # cardinality: random unknown URLs must NOT create series
        for i in range(25): http_req(f"/random/scan/{i}", b"{}")
        s, text = scrape()
        series = [l for l in text.splitlines() if l.startswith("hermes_webhook_requests_total") and "not_found" in l]
        check("metrics: 25 random URLs => ONE 'unknown' series, no per-URL labels",
              len(series) == 1 and 'endpoint="unknown"' in series[0] and series[0].endswith(" 26"), series)

        # dead letters become visible
        HEALTHY["dlq"] = False
        s, r = post("/webhook/async-dlq", {}, {"Idempotency-Key": "dlq-2"})
        await wait_for(lambda: "dead       1" in admin("stats")[1], timeout=8)
        s, text = scrape()
        check("metrics: dead-letter counter incremented", metric(text, 'hermes_outbox_dead_lettered_total{endpoint="/webhook/async-dlq",target="dlq"}') == 1, text[:0])
        check("metrics: outbox gauge shows 1 dead job", metric(text, 'hermes_outbox_jobs{state="dead"}') == 1)
        HEALTHY["dlq"] = True
        admin("replay", "--all")
        DELIVERED = 'hermes_outbox_jobs_delivered_total{endpoint="/webhook/async-dlq",target="dlq"}'
        await wait_for(lambda: metric(scrape()[1], DELIVERED) == 1, timeout=8)
        s, text = scrape()
        check("metrics: after replay the dead gauge returns to 0 and delivery is counted",
              metric(text, 'hermes_outbox_jobs{state="dead"}') == 0 and metric(text, DELIVERED) == 1,
              (metric(text, 'hermes_outbox_jobs{state="dead"}'), metric(text, DELIVERED)))
        check("metrics: oldest-waiting-job gauge exported", metric(text, "hermes_outbox_oldest_waiting_job_age_seconds") is not None)

        # ======================= concurrency limit (second instance) =======================
        second = subprocess.Popen([os.environ.get("HERMES_BIN", "target/debug/hermes-rs")],
                                  env=hermes_env(HERMES_PORT="3001", HERMES_MAX_CONCURRENT_REQUESTS="1",
                                                 HERMES_OUTBOX_PATH="/tmp/hermes-e2e-outbox/second.db"),
                                  stdout=open("/tmp/hermes-e2e-2.log", "w"), stderr=subprocess.STDOUT)
        try:
            for _ in range(100):
                try: urllib.request.urlopen("http://127.0.0.1:3001/health", timeout=1); break
                except Exception: time.sleep(0.1)
            out = []
            def slow_call(): out.append(http_req("/webhook/slow-sync", b"{}", port=3001))
            t1 = threading.Thread(target=slow_call); t1.start(); time.sleep(0.4)
            s2, h2, b2 = http_req("/webhook/slow-sync", b"{}", port=3001)       # arrives while the first is in flight
            t1.join()
            check("limit=1: second concurrent request => 503 + Retry-After", s2 == 503 and h2.get("retry-after") == "1", (s2, h2))
            check("limit=1: the first request still completed normally", out and out[0][0] == 200, out)
            s3, h3, b3 = http_req("/webhook/slow-sync", b"{}", port=3001)
            check("limit=1: capacity is released afterwards", s3 == 200, s3)
        finally:
            second.terminate(); second.wait(timeout=15)


        # ================= timestamped signatures: replay protection =================
        N1 = COUNTS.get("/auth-ok", 0)
        sbody = b'{"id": "evt_1", "type": "invoice.paid"}'
        now = int(time.time())
        s, h, b = http_req("/webhook/stripe-in", sbody, {"Stripe-Signature": stripe_header("whsec_e2e", now, sbody)})
        check("stripe: fresh valid signature => 200", s == 200, (s, b))
        s, h, b = http_req("/webhook/stripe-in", sbody, {"Stripe-Signature": stripe_header("whsec_e2e", now - 1000, sbody)})
        check("stripe: genuine signature but 1000s old (replay) => 401", s == 401, (s, b))
        s, h, b = http_req("/webhook/stripe-in", sbody, {"Stripe-Signature": stripe_header("whsec_e2e", now + 1000, sbody)})
        check("stripe: timestamp from the future => 401", s == 401, (s, b))
        s, h, b = http_req("/webhook/stripe-in", sbody, {"Stripe-Signature": stripe_header("not-the-secret", now, sbody)})
        check("stripe: wrong secret => 401", s == 401, (s, b))
        old = stripe_header("whsec_e2e", now - 1000, sbody).split("v1=")[1]
        s, h, b = http_req("/webhook/stripe-in", sbody, {"Stripe-Signature": f"t={now},v1={old}"})
        check("stripe: replay with a rewritten timestamp => 401", s == 401, (s, b))
        good = stripe_header("whsec_e2e", now, sbody).split("v1=")[1]
        s, h, b = http_req("/webhook/stripe-in", sbody, {"Stripe-Signature": f"t={now},v1={'0'*64},v1={good}"})
        check("stripe: several v1 (secret rotation), one valid => 200", s == 200, (s, b))
        s, h, b = http_req("/webhook/stripe-in", sbody, {"Stripe-Signature": "garbage"})
        check("stripe: malformed header => 401", s == 401, (s, b))

        slbody = b'{"type": "event_callback", "team_id": "T1", "event": {"type": "message", "text": "hello"}}'
        s, h, b = http_req("/webhook/slack-in", slbody, slack_headers("slack_e2e", now, slbody))
        check("slack: fresh valid signature => 200", s == 200, (s, b))
        s, h, b = http_req("/webhook/slack-in", slbody, slack_headers("slack_e2e", now - 1000, slbody))
        check("slack: genuine signature but stale timestamp => 401", s == 401, (s, b))
        s, h, b = http_req("/webhook/slack-in", b'{"type": "event_callback", "team_id": "T1", "event": {"type": "message", "text": "HELLO"}}', slack_headers("slack_e2e", now, slbody))
        check("slack: tampered body => 401", s == 401, (s, b))
        check("timestamped auth: only the 3 valid requests reached the target", COUNTS.get("/auth-ok") == N1 + 3, (COUNTS.get("/auth-ok"), N1))

        # ============================== hot config reload ==============================
        statuses = []; stop = threading.Event()
        def load():
            payload = json.dumps({"message": "steady traffic"}).encode()
            while not stop.is_set():
                try: statuses.append(http_req("/webhook/legacy", payload)[0])
                except Exception as e: statuses.append(f"error:{type(e).__name__}")
                time.sleep(0.002)
        loaders = [threading.Thread(target=load, daemon=True) for _ in range(4)]
        for t in loaders: t.start()

        # -- add an endpoint without restarting --
        s, h, b = http_req("/webhook/reload-new", b'{"x":1}')
        check("reload: endpoint does not exist yet => 404", s == 404, s)
        NEW_REG = """
  - endpoint: /webhook/reload-new
    method: POST
    template: '{"v": 1}'
    target: { url: "http://127.0.0.1:3999/auth-ok" }
"""
        write_cfg(cfg_text() + NEW_REG)
        check("reload: SIGHUP applies a valid edit", await do_reload())
        s, h, b = http_req("/webhook/reload-new", b'{"x":1}')
        check("reload: the new endpoint is live immediately (200)", s == 200, (s, b))

        # -- rotate a secret that lives in the file --
        body = b'{"action": "rotate"}'
        s, h, b = http_req("/webhook/auth-github", body, {"X-Hub-Signature-256": hex_sig("gh-secret", body)})
        check("reload: old secret works before rotation", s == 200, s)
        write_cfg(cfg_text().replace('"gh-secret"', '"gh-secret-2"'))
        check("reload: secret rotation applied", await do_reload())
        s, h, b = http_req("/webhook/auth-github", body, {"X-Hub-Signature-256": hex_sig("gh-secret", body)})
        check("reload: OLD secret is rejected after rotation (401)", s == 401, s)
        s, h, b = http_req("/webhook/auth-github", body, {"X-Hub-Signature-256": hex_sig("gh-secret-2", body)})
        check("reload: NEW secret accepted (200)", s == 200, s)

        # -- rotate a secret supplied through ${VAR}_FILE (Docker/K8s secrets style) --
        s, h, b = http_req("/webhook/auth-rot", body, {"X-Hub-Signature-256": hex_sig("rot-1", body)})
        check("reload: secret from E2E_ROT_SECRET_FILE works at startup", s == 200, s)
        open(ROT_FILE, "w").write("rot-2\n")
        check("reload: file-based secret rotation applied (config text unchanged)", await do_reload())
        s1, _, _ = http_req("/webhook/auth-rot", body, {"X-Hub-Signature-256": hex_sig("rot-1", body)})
        s2, _, _ = http_req("/webhook/auth-rot", body, {"X-Hub-Signature-256": hex_sig("rot-2", body)})
        check("reload: rotated file secret takes effect (old 401, new 200)", (s1, s2) == (401, 200), (s1, s2))

        # -- a broken edit must change NOTHING --
        good_cfg = cfg_text()
        write_cfg(good_cfg + """
  - endpoint: /webhook/broken
    method: POST
    template: '{{#if}}'
    target: { url: "http://127.0.0.1:3999/auth-ok" }
""")
        ok = await do_reload()
        check("reload: invalid config is REJECTED", ok is False)
        s, h, b = http_req("/webhook/reload-new", b'{"x":1}')
        check("reload: previous config keeps serving after a rejected reload", s == 200, s)
        s, h, b = http_req("/webhook/broken", b'{"x":1}')
        check("reload: nothing from the rejected file leaked in (404)", s == 404, s)
        sc, text = scrape()
        check("reload: failure counted in metrics", metric(text, 'hermes_config_reloads_total{result="failure"}') == 1, "")
        write_cfg("registers: [")
        check("reload: unparsable YAML is rejected too", (await do_reload()) is False)
        write_cfg(good_cfg)
        check("reload: fixing the file and reloading recovers", await do_reload())

        # -- restart-only setting: applied elsewhere, flagged here --
        write_cfg(cfg_text().replace("workers: 2", "workers: 3", 1))
        check("reload: a restart-only change still reloads the rest", await do_reload())
        log = open("/tmp/hermes-e2e.log").read()
        check("reload: restart-required settings are called out in the log", "need a restart" in log and "outbox.workers" in log)
        write_cfg(good_cfg)
        await do_reload()

        # -- queued async jobs whose target disappears are detected --
        s, h, b = http_req("/webhook/async-orphan", b'{}', {"Idempotency-Key": "orphan-1"})
        check("reload: async job queued towards a down target", s == 202, (s, b))
        write_cfg(cfg_text().replace("name: ghost,", "name: ghost2,"))
        check("reload: renaming that target reloads", await do_reload())
        log = open("/tmp/hermes-e2e.log").read()
        check("reload: orphaned queued jobs are warned about BEFORE they die", "Queued async jobs refer to targets that no longer exist" in log and "ghost" in log)
        ok = await wait_for(lambda: "dead       1" in admin("stats")[1], timeout=8)
        rc, out = admin("list", "--state", "dead")
        check("reload: orphaned job is dead-lettered (not sent anywhere wrong) and replayable", ok and "no longer in the" in out and "ghost" in out, out)
        admin("purge", "--all")
        write_cfg(good_cfg)
        await do_reload()

        # -- remove an endpoint --
        write_cfg(good_cfg.replace(NEW_REG, ""))
        check("reload: endpoint removal applied", await do_reload())
        s, h, b = http_req("/webhook/reload-new", b'{"x":1}')
        check("reload: removed endpoint is gone (404)", s == 404, s)

        stop.set()
        for t in loaders: t.join(timeout=5)
        bad = [x for x in statuses if x != 200]
        check(f"reload: ZERO failed requests across {len(statuses)} requests during the reloads", len(statuses) > 200 and not bad, (len(statuses), bad[:5]))
        sc, text = scrape()
        check("reload: success counter and timestamp exported", (metric(text, 'hermes_config_reloads_total{result="success"}') or 0) >= 8 and (metric(text, "hermes_config_last_success_timestamp_seconds") or 0) > 1.7e9)

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
        HERMES['p'].terminate(); HERMES['p'].wait(timeout=15)
    print(f"\n{sum(results)}/{len(results)} checks passed")
    sys.exit(0 if all(results) else 1)
asyncio.run(main())
