import asyncio, nats
async def main():
    nc = await nats.connect("nats://127.0.0.1:4222")
    js = nc.jetstream()
    try:
        await js.add_stream(name="HOOKS", subjects=["js.>"])
    except Exception as e:
        print("stream:", e)
    print("stream ready")
    await nc.close()
asyncio.run(main())
