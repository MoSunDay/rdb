package command

import (
	"fmt"
	types "rdb/internal/rtypes"
)

func getHandler(c types.CommandContext) {
	conn, db, args := c.Conn, c.DB, c.Args
	if len(args) != 1 {
		conn.WriteError("ERR wrong number of arguments for get command")
		return
	}
	val, err := db.Get(args[0])
	if err != nil {
		conn.WriteNull()
		return
	}
	conn.WriteBulk(val)
}

func setHandler(c types.CommandContext) {
	conn, db, args := c.Conn, c.DB, c.Args
	if len(args) != 2 {
		conn.WriteError("ERR wrong number of arguments for set command")
		return
	}
	err := db.Set(args[0], args[1])
	if err != nil {
		conn.WriteError("ERR: set key failed")
		return
	}
	conn.WriteString("OK")
}

func mgetHandler(c types.CommandContext) {
	conn, db, args := c.Conn, c.DB, c.Args
	if len(args) < 1 {
		conn.WriteError("MGET command must have at least 1 argument: MGET <key1> [<key2> ...]")
		return
	}

	data := db.MGet(args)
	conn.WriteArray(len(data))
	for _, v := range data {
		if len(v) == 0 {
			conn.WriteNull()
			continue
		}
		conn.WriteBulk(v)
	}
}

func msetHandler(c types.CommandContext) {
	conn, db, args := c.Conn, c.DB, c.Args
	argsLen := len(args)
	if argsLen%2 != 0 {
		conn.WriteError("ERR wrong number of arguments: " + fmt.Sprint(argsLen))
	}
	err := db.MSet(args)
	if err != nil {
		conn.WriteError("ERR: set key failed")
		return
	}
	conn.WriteString("OK")
}

func delHandler(c types.CommandContext) {
	conn, db, args := c.Conn, c.DB, c.Args
	if len(args) != 1 {
		conn.WriteError("ERR wrong number of arguments for del command")
		return
	}
	err := db.Del(args[0])
	if err != nil {
		conn.WriteInt(0)
	} else {
		conn.WriteInt(1)
	}
}
