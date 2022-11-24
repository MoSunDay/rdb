#pip install aredis[hiredis]
import asyncio
from aredis import StrictRedisCluster

async def example():
    client = StrictRedisCluster(host='127.0.0.1', port=32681, password="6Mwqjg7TF9jcnF8PRxrq7jK3pG3DZ28fy5guk3Hm264smyftmuhUGtz5mssHfG7Ztg23j4FMb5qmpxdhBusfpevkveyR93eNfx6uv5YEZYyczfdYn4rpKxcRxiXhirik")
    print(await client.cluster_slots())
    await client.set('hello', 'world')
    print(await client.get('hello'))

loop = asyncio.get_event_loop()
loop.run_until_complete(example())