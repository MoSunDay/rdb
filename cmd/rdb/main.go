package main

import (
	"flag"
	"rdb/internal/conf"
	"rdb/internal/server"
	"rdb/internal/utils"
	// _ "github.com/MoSunDay/redix/hash"
)

var confLogger = utils.GetLogger("main")

func main() {
	flag.Parse()
	confLogger.Println("Start..")
	confLogger.Println("Bind:", conf.Content.Bind)
	confLogger.Println("Instances:", conf.Content.Instances)
	confLogger.Println("Path:", conf.Content.StorePath)
	err := server.NewServer().ListenAndServe()
	if err != nil {
		confLogger.Println(err)
	}
}
