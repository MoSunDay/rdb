package main

import (
	"flag"
	"rdb/internal/conf"
	"rdb/internal/monitor"
	"rdb/internal/server"
	"rdb/internal/utils"
	"strings"
	"time"
	// _ "github.com/MoSunDay/redix/hash"
)

var confLogger = utils.GetLogger("main")

func main() {
	flag.Parse()
	confLogger.Println("Start..")
	confLogger.Println("Bind:", conf.Content.Bind)
	confLogger.Println("Path:", conf.Content.StorePath)
	monitor := monitor.NewCustomCollector(conf.Content.MonitorAddr)
	conf.Content.Monitor = monitor
	rdb := server.NewRDB()

	if conf.Content.BackupBind != "" {
		go func() {
			err := rdb.BackupServer.KV.ListenAndServe()
			if err != nil {
				confLogger.Fatal(err)
			}
		}()
	}

	go func() {
		ticker := time.NewTicker(5 * time.Second)
		defer ticker.Stop()
		for range ticker.C {
			allStatus := []string{"Shutdown", "Follower", "Leader", "Candidate", "Unknown"}
			for _, item := range allStatus {
				monitor.RaftStatus.WithLabelValues(item).Set(0)
			}
			state := conf.Content.CRaft.Raft.Raft.String()
			index := strings.LastIndex(state, " ")
			lenState := len(state)
			if (index == -1) || (index+2 > lenState-1) {
				monitor.RaftStatus.WithLabelValues("Unknown").Set(1)
			} else {
				monitor.RaftStatus.WithLabelValues(state[index+2 : lenState-1]).Set(1)
			}
		}
	}()

	err := rdb.Server.KV.ListenAndServe()
	if err != nil {
		confLogger.Fatal(err)
	}
}
