package rtypes

import (
	"rdb/internal/store"

	"github.com/MoSunDay/redcon"
)

type RDBServer struct {
	DB     *store.LevelDB
	Server *redcon.Server
}

type CommandContext struct {
	Conn redcon.Conn
	DB   *store.LevelDB
	Args [][]byte
}
