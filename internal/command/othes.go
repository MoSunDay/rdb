package command

import types "rdb/internal/rtypes"

func pingHandler(c types.CommandContext) {
	conn := c.Conn
	conn.WriteString("PONG")
}

func quitHandler(c types.CommandContext) {
	conn := c.Conn
	conn.WriteString("PONG")
	conn.WriteString("OK")
	conn.Close()
}

func gcHandler(c types.CommandContext) {
	conn, db := c.Conn, c.DB
	db.GC()
	conn.WriteString("OK")
}

func configHandler(c types.CommandContext) {
	conn := c.Conn
	conn.WriteArray(2)
	conn.WriteBulkString("cluster-require-full-coverage")
	conn.WriteBulkString("no")
}
