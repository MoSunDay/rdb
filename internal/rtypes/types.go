package rtypes

import (
	"rdb/internal/store"

	"github.com/MoSunDay/redcon"
)

type RDBServer struct {
	DB     *store.Pebble
	Server *redcon.Server
}

type CommandContext struct {
	Conn      redcon.Conn
	DB        *store.Pebble
	PrefixKey []byte
	Args      [][]byte
}
