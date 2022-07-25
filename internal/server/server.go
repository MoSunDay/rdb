package server

import (
	"bytes"
	"fmt"
	"net"
	"net/http"
	"path/filepath"
	"strings"

	"rdb/internal/command"
	"rdb/internal/conf"
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

func newServer(RCache *rcache.Cached) *redcon.Server {
	confLogger.Println("start pebble")
	host, addrs := conf.Content.Bind, conf.Content.Instances
	db, err := store.OpenPebble(filepath.Join(conf.Content.StorePath, host))

	if err != nil {
		confLogger.Println("start leveldb failed", err)
	}

	confLogger.Println("start server bind:", host)
	perNodeslots := 16384 / len(addrs)

	Server := redcon.NewServer(
		host,
		func(conn redcon.Conn, cmd redcon.Command) {
			defer (func() {
				if err := recover(); err != nil {
					conn.WriteError(fmt.Sprintf("fatal error: %s", (err.(error)).Error()))
				}
			})()

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
	opts.DataDir = conf.Content.StorePath + "/raft"
	opts.HttpAddress = conf.Content.HttpAddress
	opts.Bootstrap = conf.Content.Bootstrap
	opts.RaftTCPAddress = conf.Content.RaftTCPAddress
	opts.JoinAddress = conf.Content.JoinAddress

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
	KVServer := newServer(RCache)
	return &RDB{RCache: RCache, KVServer: KVServer}
}
