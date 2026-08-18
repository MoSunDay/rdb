package command

import (
	types "rdb/internal/rtypes"
	"strconv"
)

func keysHandler(c types.CommandContext) {
	conn, db := c.Conn, c.DB
	conn.WriteString(strconv.Itoa(int(db.Size())))
}
