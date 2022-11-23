package main

import (
	"flag"
	"rdb/internal/conf"
	"rdb/internal/monitor"
	"rdb/internal/server"
	"rdb/internal/utils"
	// _ "github.com/MoSunDay/redix/hash"
)

var confLogger = utils.GetLogger("main")

func main() {
	flag.Parse()
	confLogger.Println("Start..")
	confLogger.Println("Bind:", conf.Content.Bind)
	confLogger.Println("Path:", conf.Content.StorePath)
	conf.Content.Monitor = monitor.NewCustomCollector(conf.Content.MonitorAddr)
	rdb := server.NewRDB()

	if conf.Content.BackupBind != "" {
		go func() {
			err := rdb.BackupServer.KV.ListenAndServe()
			if err != nil {
				confLogger.Fatal(err)
			}
		}()
	}

	err := rdb.Server.KV.ListenAndServe()
	if err != nil {
		confLogger.Fatal(err)
	}
}
