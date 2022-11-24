#pip install redis-py-cluster
from rediscluster import RedisCluster
startup_nodes = [{"host": "127.0.0.1", "port": "32681"}]
rc = RedisCluster(startup_nodes=startup_nodes, decode_responses=True, password="6Mwqjg7TF9jcnF8PRxrq7jK3pG3DZ28fy5guk3Hm264smyftmuhUGtz5mssHfG7Ztg23j4FMb5qmpxdhBusfpevkveyR93eNfx6uv5YEZYyczfdYn4rpKxcRxiXhirik")
rc.set("hello", "world")
print(rc.get("hello"))