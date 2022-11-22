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
	monitor.Setup()
	err := server.NewRDB().KVServer.ListenAndServe()
	if err != nil {
		confLogger.Println(err)
	}
}
