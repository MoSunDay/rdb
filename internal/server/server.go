package server

import (
	"bytes"
	"fmt"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"

	"rdb/internal/command"
	"rdb/internal/conf"
	"rdb/internal/monitor"
	"rdb/internal/rcache"
	types "rdb/internal/rtypes"
	"rdb/internal/store"
	"rdb/internal/utils"

	"github.com/MoSunDay/redcon"
)

var confLogger = utils.GetLogger("server")

type RDB struct {
	KVServer *redcon.Server
	RCache   *rcache.Cached
}

func newServer() *redcon.Server {
	confLogger.Println("start pebble")
	host := conf.Content.Bind
	db, err := store.OpenPebble(filepath.Join(conf.Content.StorePath, host))

	if err != nil {
		confLogger.Println("start leveldb failed", err)
	}

	confLogger.Println("start server bind:", host)

	Server := redcon.NewServer(
		host,
		func(conn redcon.Conn, cmd redcon.Command) {
			defer (func() {
				if err := recover(); err != nil {
					conn.WriteError(fmt.Sprintf("fatal error: %s", (err.(error)).Error()))
				}
			})()
			startTime := conf.Content.Sentinel.RTime
			firstCmd := strings.ToLower(string(cmd.Args[0]))
			var prefixKey []byte

			if fn, ok := command.CommandHander[firstCmd]; ok {
				if _, ok := command.Whitelist[firstCmd]; !ok {
					var start, end, slotNumber int

					key := cmd.Args[1]
					start = bytes.Index(key, []byte("{"))
					if start != -1 {
						start += 1
						end = bytes.Index(key[start:], []byte("}"))
					}

					if start != -1 && end != -1 {
						slotNumber, prefixKey = utils.GetSlotNumberWithPrefixKey(key[start : start+end])
					} else {
						slotNumber, prefixKey = utils.GetSlotNumberWithPrefixKey(cmd.Args[1])
					}

					addrs := conf.Content.StableAddrs
					perNodeslots := conf.Content.PerNodeslots
					for index, addr := range addrs {
						if slotNumber <= (index+1)*perNodeslots {
							if addr == host {
								break
							} else {
								conn.WriteError(fmt.Sprintf("MOVED %d %s", slotNumber, addr))
								return
							}
						}
					}
				}
				cmdArgsList := cmd.Args[1:]
				fn(types.CommandContext{
					Conn:      conn,
					DB:        db,
					PrefixKey: prefixKey,
					Args:      cmdArgsList,
				})
			} else {
				conn.WriteError("ERR unknown command '" + string(cmd.Args[0]) + "'")
			}
			endTime := conf.Content.Sentinel.RTime
			monitor.Collector.Latency.WithLabelValues(firstCmd).Observe(float64(endTime - startTime))
		},
		func(conn redcon.Conn) bool {
			// Use this function to accept or deny the connection.
			// log.Printf("accept: %s", conn.RemoteAddr())
			return true
		},
		func(conn redcon.Conn, err error) {
			// This is called when the connection has been closed
			// log.Printf("closed: %s, err: %v", conn.RemoteAddr(), err)
		},
	)
	return Server
}

func newRcache() *rcache.Cached {
	opts := rcache.NewOptions()
	opts.DataDir = conf.Content.StorePath + "/" + conf.Content.Bind + "/raft"
	opts.HttpAddress = conf.Content.HttpAddress

	if !utils.Exists(opts.DataDir) {
		opts.JoinAddress = os.Getenv("RAFT_JOIN_ADDR")
	} else {
		opts.JoinAddress = ""
	}

	opts.RaftTCPAddress = conf.Content.RaftTCPAddress
	opts.RaftToken = conf.Content.RaftToken
	SlotCache := &rcache.Cached{
		Opts: opts,
		Log:  confLogger,
		CM:   rcache.NewCacheManager(),
	}
	ctx := &rcache.CachedContext{SlotCache}

	raft, err := rcache.NewRaftNode(SlotCache.Opts, ctx)
	if err != nil {
		SlotCache.Log.Fatal(fmt.Sprintf("new raft node failed:%v", err))
	}
	SlotCache.Raft = raft

	if SlotCache.Opts.JoinAddress != "" {
		err = rcache.JoinRaftCluster(SlotCache.Opts)
		if err != nil {
			SlotCache.Log.Fatal(fmt.Sprintf("join raft cluster failed:%v", err))
		}
	}

	httpServer := rcache.NewHttpServer(ctx, confLogger)
	SlotCache.HttpServer = httpServer

	go func() {
		for {
			select {
			case leader := <-SlotCache.Raft.LeaderNotifyCh:
				if leader {
					SlotCache.Log.Println("become leader, enable write api")
					SlotCache.HttpServer.SetWriteFlag(true)
				} else {
					SlotCache.Log.Println("become follower, close write api")
					SlotCache.HttpServer.SetWriteFlag(false)
				}
			}
		}
	}()

	go func() {
		var l net.Listener
		var err error
		l, err = net.Listen("tcp", SlotCache.Opts.HttpAddress)
		if err != nil {
			confLogger.Fatal(fmt.Sprintf("listen %s failed: %s", SlotCache.Opts.HttpAddress, err))
		}
		confLogger.Printf("http server listen:%s", l.Addr())
		http.Serve(l, httpServer.Mux)
	}()

	return SlotCache
}

func NewRDB() *RDB {
	RCache := newRcache()
	instances := RCache.CM.Get("cluster_slots_stable_instances")
	if instances != "" || len(instances)%2 != 0 {
		conf.Content.ClusterReady = true
	} else {
		conf.Content.ClusterReady = false
	}
	conf.Content.CRaft = RCache
	KVServer := newServer()

	go func() {
		ticker := time.NewTicker(5 * time.Millisecond)
		defer ticker.Stop()
		for range ticker.C {
			conf.Content.Sentinel.RTime += 5
		}
	}()

	go func() {
		ticker := time.NewTicker(3 * time.Second)
		defer ticker.Stop()

		for range ticker.C {
			instances := RCache.CM.Get("cluster_slots_stable_instances")
			addrs := strings.Split(instances, ",")
			if instances != "" || (len(addrs)%2 != 0 && addrs[0] != "") {
				conf.Content.ClusterReady = true
				conf.Content.StableAddrs = addrs
				conf.Content.PerNodeslots = 16384 / len(addrs)
			} else {
				conf.Content.ClusterReady = false
			}
		}
	}()

	go func() {
		ticker := time.NewTicker(3 * time.Second)
		defer ticker.Stop()

		for range ticker.C {
			instances := RCache.CM.Get("cluster_slots_backup_instances")
			addrs := strings.Split(instances, ",")
			if instances != "" {
				conf.Content.ClusterReady = true
				conf.Content.BackupAddrs = addrs
			}
		}
	}()

	return &RDB{RCache: RCache, KVServer: KVServer}
}
