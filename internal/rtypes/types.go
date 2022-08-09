package rtypes

import (
	"rdb/internal/store"
	"rdb/internal/utils"

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

type MigrateTask struct {
	StartNode  string
	TargetNode string
	EndNode    string
	Enable     bool
	StartSlot  int64
	CurSlot    int64
	EndSlot    int64
}

type Sentinel struct {
	RTime int64
}

type RaftLogEntryData struct {
	Key   string
	Value string
}

type Helper struct {
	CLock utils.CLock
	DB    *store.Pebble
}
