### 支持 Redis Cluster 协议的可持久化存储

#### TodoList
1. - [x] 单节点支持 `LevelDB`
2. - [x] `set/mset/get/mget` 常用命令支持
3. - [x] 支持 `redis cluster` 通过 `redis-benchmark` && `redis-py-cluster` 的测试
4. - [] 加入配置管理
5. - [] 支持其他例如 `pebble`
6. - [] `cluster` 数据交给 `Raft` 管理
7. - [] 支持数据迁移
8. - [] 加入其他数据结构

#### benchmark
压力发生器与三节点同时部署在 `DELL XPS` 
```
$ redis-benchmark -h 192.168.2.122 -p 32680 -t set -n 100000000000000 -c 100 -q -r 100000000000000 -d 1000 --cluster  2>/dev/null
Cluster has 3 master nodes:

Master 0: 192a168a2a122a32680bbbbbbbbbbbbbbbbbbbbb 192.168.2.122:32680
Master 1: 192a168a2a122a32681bbbbbbbbbbbbbbbbbbbbb 192.168.2.122:32681
Master 2: 192a168a2a122a32682bbbbbbbbbbbbbbbbbbbbb 192.168.2.122:32682

SET: rps=72356.9 (overall: 69569.6) avg_msec=0.701 (overall: 0.778)
```
负载情况
```
进程号 USER      PR  NI    VIRT    RES    SHR    %CPU  %MEM     TIME+ COMMAND
2853006 m         20   0  251988   6524   3348 S 268.2   0.0  42:42.98 redis-benchmark
2835398 m         20   0 1048204  96520   2752 S 140.1   0.6  25:04.02 rdb
2832090 m         20   0 1047948  92420   2752 S 107.6   0.6  21:29.86 rdb
2837706 m         20   0 1048204  90992   2752 S 107.0   0.6  21:18.26 rdb
```
机器环境与配置
```
$ uname -a
Linux m 5.11.0-44-generic #48~20.04.2-Ubuntu SMP Tue Dec 14 15:36:44 UTC 2021 x86_64 x86_64 x86_64 GNU/Linux

$ lscpu
架构：                           x86_64
CPU 运行模式：                   32-bit, 64-bit
字节序：                         Little Endian
Address sizes:                   39 bits physical, 48 bits virtual
CPU:                             8
在线 CPU 列表：                  0-7
每个核的线程数：                 2
每个座的核数：                   4
座：                             1
NUMA 节点：                      1
厂商 ID：                        GenuineIntel
CPU 系列：                       6
型号：                           142
型号名称：                       Intel(R) Core(TM) i7-8550U CPU @ 1.80GHz
步进：                           10
CPU MHz：                        1800.000
CPU 最大 MHz：                   1800.0000
CPU 最小 MHz：                   400.0000
BogoMIPS：                       3999.93
虚拟化：                         VT-x
L1d 缓存：                       128 KiB
L1i 缓存：                       128 KiB
L2 缓存：                        1 MiB
L3 缓存：                        8 MiB

$ free -m
              总计         已用        空闲      共享    缓冲/缓存    可用
内存：       15889        1489        5928         109        8470       13956
交换：           0           0           0
```
