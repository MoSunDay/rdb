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
