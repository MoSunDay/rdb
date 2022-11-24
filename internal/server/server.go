package server

import (
	"bytes"
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"time"

	"rdb/internal/command"
	"rdb/internal/conf"
	"rdb/internal/rcache"
	"rdb/internal/rtypes"
	types "rdb/internal/rtypes"
	"rdb/internal/store"
	"rdb/internal/utils"

	"github.com/MoSunDay/redcon"
	"github.com/hashicorp/raft"
)

var confLogger = utils.GetLogger("server")

type DB struct {
	KV *redcon.Server
}

type RDB struct {
	Server       *DB
	BackupServer *DB
	RCache       *rcache.Cached
}

func newDB(bind, storePath, mode string) *DB {
	confLogger.Println("start pebble")
	host := bind
	db, err := store.OpenPebble(filepath.Join(storePath, host))

	if err != nil {
		confLogger.Println("start leveldb failed", err)
	}

	confLogger.Println("start server bind:", host)
	KV := redcon.NewServer(
		host,
		conf.Content.RaftToken,
		func(conn redcon.Conn, cmd redcon.Command) {
			defer (func() {
				if err := recover(); err != nil {
					conn.WriteError(fmt.Sprintf("fatal error: %s", (err.(error)).Error()))
				}
			})()
			startTime := conf.Content.Sentinel.RTime
			isMoved := "false"
			firstCmd := strings.ToLower(utils.BytesToString(cmd.Args[0]))
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
								isMoved = "true"
								conn.WriteError(fmt.Sprintf("MOVED %d %s", slotNumber, addr))
								endTime := conf.Content.Sentinel.RTime
								conf.Content.Monitor.Latency.WithLabelValues(mode, firstCmd, isMoved).Observe(float64(endTime - startTime))
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

				endTime := conf.Content.Sentinel.RTime
				conf.Content.Monitor.Latency.WithLabelValues(mode, firstCmd, isMoved).Observe(float64(endTime - startTime))
			} else {
				conn.WriteError("ERR unknown command '" + string(cmd.Args[0]) + "'")
			}
		},
		func(conn redcon.Conn) bool {
			return true
		},
		func(conn redcon.Conn, err error) {
		},
	)
	return &DB{KV: KV}
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
	raftInstance, err := rcache.NewRaftNode(SlotCache.Opts, ctx)
	if err != nil {
		SlotCache.Log.Fatal(fmt.Sprintf("new raft node failed:%v", err))
	}
	SlotCache.Raft = raftInstance

	if SlotCache.Opts.JoinAddress != "" {
		err = rcache.JoinRaftCluster(SlotCache.Opts)
		if err != nil {
			SlotCache.Log.Fatal(fmt.Sprintf("join raft cluster failed:%v", err))
		}
	}

	httpServer := rcache.NewHttpServer(ctx, confLogger)
	SlotCache.HttpServer = httpServer

	go func() {
		getServerID := func(unknown interface{}) (retType, ServerID string) {
			confLogger.Println("######################", reflect.TypeOf(unknown))
			switch unknown := unknown.(type) {
			case raft.FailedHeartbeatObservation:
				return "FailedHeartbeatObservation", string(unknown.PeerID)
			case raft.ResumedHeartbeatObservation:
				return "ResumedHeartbeatObservation", string(unknown.PeerID)
			case raft.PeerObservation:
				return "PeerObservation", string(unknown.Peer.ID)
			default:
				return "unknown", ""
			}
		}

		handlerObserver := func(retType, serverID string) {
			if retType != "ResumedHeartbeatObservation" && retType != "FailedHeartbeatObservation" && retType != "PeerObservation" {
				return
			}
			key := "cluster_slots_stable_instances"
			val := SlotCache.CM.Get("backup_target_map_" + serverID)
			failedNodeBackupMap := strings.Split(val, ",")
			if len(failedNodeBackupMap) != 2 {
				confLogger.Println("failedNodeBackupMap error:", failedNodeBackupMap)
				return
			}
			val = SlotCache.CM.Get(key)
			stableInstances := strings.Split(val, ",")

			if retType == "FailedHeartbeatObservation" {
				stableInstances = utils.StringSliceReplaceItem(stableInstances, failedNodeBackupMap[0], failedNodeBackupMap[1])
			} else {
				stableInstances = utils.StringSliceReplaceItem(stableInstances, failedNodeBackupMap[1], failedNodeBackupMap[0])
			}

			clusterInstaces := strings.Join(stableInstances, ",")
			if val == clusterInstaces {
				confLogger.Printf("%s %s don't need update", retType, serverID)
				return
			}
			event := rtypes.RaftLogEntryData{Key: key, Value: clusterInstaces}
			eventBytes, err := json.Marshal(event)
			if err != nil {
				confLogger.Printf("json.Marshal failed, err:%v", err)
				return
			}
			applyFuture := SlotCache.Raft.Raft.Apply(eventBytes, 5*time.Second)
			if err := applyFuture.Error(); err != nil {
				confLogger.Printf("raft.Apply failed:%v", err)
			}
			confLogger.Printf("%s %s done", retType, serverID)
		}

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
			case observer := <-SlotCache.Raft.ObserverChan:
				retType, serverID := getServerID(observer.Data)
				handlerObserver(retType, serverID)
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

	Server := newDB(conf.Content.Bind, conf.Content.StorePath, "normal")
	var BackupServer *DB
	if conf.Content.BackupBind != "" {
		BackupServer = newDB(conf.Content.BackupBind, conf.Content.BackupStorePath, "backup")
	}

	go func() {
		raftApply := func(key, value string) {
			event := rtypes.RaftLogEntryData{Key: key, Value: value}
			eventBytes, err := json.Marshal(event)
			if err != nil {
				confLogger.Fatalln("raft Write backup_target_map failed")
			}
			applyFuture := RCache.Raft.Raft.Apply(eventBytes, 5*time.Second)
			if err := applyFuture.Error(); err != nil {
				confLogger.Fatalf("raft.Apply backup_target_map failed:%v\n", err)
			}
		}
		for {
			backupMap := RCache.CM.Get("backup_target_map_init")
			allowIPs := RCache.CM.Get("allow_ip_list_init")
			if conf.Content.BackupTargetMap != nil && RCache.Raft.Raft.Leader() == raft.ServerAddress(conf.Content.RaftTCPAddress) {
				if backupMap == "" {
					for k, vMap := range conf.Content.BackupTargetMap {
						raftApply("backup_target_map_"+k, vMap["src"]+","+vMap["target"])
					}
					raftApply("backup_target_map_init", "done")
				}
				if allowIPs == "" {
					for _, item := range conf.Content.IPList {
						raftApply("allow_ip_"+item, "on")
					}
					raftApply("allow_ip_list_init", "done")
				}
			}
			if backupMap != "" && allowIPs != "" {
				confLogger.Println("init backup_target_map && allowIPs done.")
				return
			}
			time.Sleep(1 * time.Second)
		}
	}()

	go func() {
		ticker := time.NewTicker(3 * time.Second)
		defer ticker.Stop()

		stableIntances := ""
		for range ticker.C {
			instances = RCache.CM.Get("cluster_slots_stable_instances")
			if stableIntances != instances {
				addrs := strings.Split(instances, ",")
				stableIntances = instances
				if instances != "" || (len(addrs)%2 != 0 && addrs[0] != "") {
					conf.Content.ClusterReady = true
					conf.Content.StableAddrs = addrs
					conf.Content.PerNodeslots = 16384 / len(addrs)
				} else {
					conf.Content.ClusterReady = false
				}
			}
		}
	}()

	return &RDB{RCache: RCache, Server: Server, BackupServer: BackupServer}
}
