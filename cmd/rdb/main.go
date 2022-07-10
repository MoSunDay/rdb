package main

import (
	"flag"
	"os"
	"rdb/internal/server"
	"rdb/internal/utils"
	// _ "github.com/MoSunDay/redix/hash"
)

var confLogger = utils.GetLogger("main")

func main() {
	flag.Parse()

	confLogger.Println("start..")
	confLogger.Println("args:", os.Args[1:])
	err := server.NewServer().ListenAndServe()
	if err != nil {
		confLogger.Println(err)
	}
}
