package command

import (
	types "rdb/internal/rtypes"
)

var CommandHander = map[string]func(types.CommandContext){
	"ping":    pingHandler,
	"quit":    quitHandler,
	"get":     getHandler,
	"set":     setHandler,
	"del":     delHandler,
	"mget":    mgetHandler,
	"mset":    msetHandler,
	"config":  configHandler,
	"cluster": clusterHandler,
	"raft":    raftHandler,
	"migrate": migrateHandler,
}

var Whitelist = map[string]bool{
	"ping":    true,
	"quit":    true,
	"config":  true,
	"cluster": true,
	"raft":    true,
	"migrate": true,
}
