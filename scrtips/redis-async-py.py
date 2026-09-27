#pip install aredis[hiredis]
# The password is the node's raft_token -- pass your own via env, never
# inline a real one (the placeholder below is FAKE).
import asyncio
import os
from aredis import StrictRedisCluster

async def example():
    client = StrictRedisCluster(host='127.0.0.1',
                                port=int(os.environ.get("RDB_PORT", "32681")),
                                password=os.environ.get("RDB_TOKEN", "replace-with-your-raft-token"))
    print(await client.cluster_slots())
    await client.set('hello', 'world')
    print(await client.get('hello'))

loop = asyncio.get_event_loop()
loop.run_until_complete(example())
