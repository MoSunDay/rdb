package server

import (
	"bytes"
	"fmt"
	"path/filepath"
	"strings"

	"rdb/internal/command"
	"rdb/internal/conf"
	types "rdb/internal/rtypes"
	"rdb/internal/store"
	"rdb/internal/utils"

	"github.com/MoSunDay/redcon"
)

var confLogger = utils.GetLogger("server")

func NewServer() *redcon.Server {
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
