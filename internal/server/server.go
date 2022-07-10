package server

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"

	"rdb/internal/command"
	types "rdb/internal/rtypes"
	"rdb/internal/store"
	"rdb/internal/utils"

	"github.com/MoSunDay/redcon"
)

var confLogger = utils.GetLogger("server")

func NewServer() *redcon.Server {
	confLogger.Println("start leveldb")
	addr := utils.GetEnvDefault("RDB_ADDR", ":32680")
	db, err := store.OpenLevelDB(filepath.Join("/tmp", addr, "leveldb"))

	go func() {
		t := time.NewTicker(20 * time.Millisecond)
		for {
			select {
			case <-t.C:
				db.GC()
			}
		}
	}()

	if err != nil {
		confLogger.Println("start leveldb failed", err)
	}
	confLogger.Println("start server bind:", addr)
	host, addrs := os.Args[1], strings.Split(os.Args[2], ",")
	perNodeslots := 16384 / len(addrs)

	Server := redcon.NewServer(
		addr,
		func(conn redcon.Conn, cmd redcon.Command) {
			defer (func() {
				if err := recover(); err != nil {
					conn.WriteError(fmt.Sprintf("fatal error: %s", (err.(error)).Error()))
				}
			})()
			firstCmd := strings.ToLower(string(cmd.Args[0]))

			if _, ok := command.Whitelist[firstCmd]; !ok {
				slotNumber := int(utils.GetSlotNumber(cmd.Args[1]))
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

			if fn, ok := command.CommandHander[firstCmd]; ok {
				cmdArgsList := cmd.Args[1:]
				fn(types.CommandContext{
					Conn: conn,
					DB:   db,
					Args: cmdArgsList,
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
