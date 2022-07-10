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
	"keys":    keysHandler,
	"cluster": clusterHandler,
}

var Whitelist = map[string]bool{
	"ping":    true,
	"quit":    true,
	"keys":    true,
	"cluster": true,
}
