#pip install redis-py-cluster
# The password is the node's raft_token -- pass your own via env, never
# inline a real one (the placeholder below is FAKE).
import os
from rediscluster import RedisCluster
startup_nodes = [{"host": "127.0.0.1", "port": os.environ.get("RDB_PORT", "32681")}]
rc = RedisCluster(startup_nodes=startup_nodes, decode_responses=True,
                  password=os.environ.get("RDB_TOKEN", "replace-with-your-raft-token"))
rc.set("hello", "world")
print(rc.get("hello"))
