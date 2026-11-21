import asyncio, json, subprocess, time, urllib.request, urllib.error, os, sys
import nats, pika

def post(path, body):
    req = urllib.request.Request("http://127.0.0.1:3000"+path, data=json.dumps(body).encode(), method="POST",
                                 headers={"Content-Type":"application/json"})
    try:
        with urllib.request.urlopen(req, timeout=20) as r: return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e: return e.code, json.loads(e.read())

results = []
def check(name, cond, extra=""):
    results.append(cond); print(("PASS " if cond else "FAIL ") + name + (f"  [{extra}]" if extra and not cond else ""))

async def main():
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
        s, r = post("/webhook/fan", {"action": "opened", "user": "alice"})
        check("broadcast: 200 success, 5/5", s == 200 and r["status"] == "success" and r["delivered"] == 5, r)
        check("broadcast: each result ok", all(x["status"] == "ok" for x in r["results"]), r)
        await asyncio.sleep(0.5)
        check("nats core received rendered payload", len(got["fan"]) == 1 and json.loads(got["fan"][0][0]) == {"event":"opened","who":"alice"}, got["fan"])
        check("nats header delivered", got["fan"] and got["fan"][0][1].get("X-Source") == "hermes", got["fan"])

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
