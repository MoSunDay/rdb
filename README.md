### 支持 Redis Cluster 协议的可持久化存储

#### TodoList
1. - [x] 单节点支持 `pebble`
2. - [x] `set/mset/get/mget` 常用命令支持
3. - [x] 支持 `redis cluster` 通过 `redis-benchmark` && `redis-py-cluster` 的测试
4. - [] 引入配置管理
6. - [] `cluster` 数据交给 `Raft` 管理
7. - [] 支持数据迁移
8. - [] 进一步的性能优化

#### Benchmark
`3` 个 `rdb` 实例组成的集群以及压力发生器「redis-benchmark」在同一台服务器上运行，机器为 `DELL XPS`
```
top - 23:56:37 up 2 days, 11:31,  5 users,  load average: 11.65, 5.75, 2.32
任务: 266 total,   3 running, 263 sleeping,   0 stopped,   0 zombie
%Cpu(s):  6.6 us, 25.6 sy, 47.6 ni,  7.0 id,  5.9 wa,  0.0 hi,  7.2 si,  0.0 st
MiB Mem :  15889.2 total,   7279.0 free,   1416.1 used,   7194.2 buff/cache
MiB Swap:      0.0 total,      0.0 free,      0.0 used.  14030.1 avail Mem

 进程号 USER      PR  NI    VIRT    RES    SHR    %CPU  %MEM     TIME+ COMMAND
3702307 m         25   5  713068  49940   7568 R 174.8   0.3   3:31.12 rdb
3711763 m         20   0  240224   6892   3340 S 170.2   0.0   0:28.99 redis-benchmark
3702334 m         25   5  712812  48468   7504 R 168.9   0.3   3:34.17 rdb
3702279 m         25   5  712748  51192   7504 S 167.9   0.3   3:29.55 rdb
```
系统环境
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
```

[友情链接 sdb 基于 rpc 的 KV 持久化存储](https://github.com/yemingfeng/sdb)
