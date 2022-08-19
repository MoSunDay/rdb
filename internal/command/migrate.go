package command

import types "rdb/internal/rtypes"

func migrateHandler(c types.CommandContext) {
	conn := c.Conn
	conn.WriteString("PONG")
}
