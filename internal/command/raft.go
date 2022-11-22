package command

import (
	"rdb/internal/conf"
	types "rdb/internal/rtypes"
)

func raftHandler(c types.CommandContext) {
	args := c.Args
	subCommand := map[string]func(c types.CommandContext){
		"help":   raftHelp,
		"HELP":   raftHelp,
		"stats":  raftStats,
		"STATS":  raftStats,
		"leader": raftLeader,
		"LEADER": raftLeader,
		"NODES":  raftNode,
		"nodes":  raftNode,
	}
	if len(args) == 0 {
		raftHelp(c)
		return
	}
	if fn, ok := subCommand[string(args[0])]; ok {
		fn(c)
	} else {
		raftHelp(c)
	}
}

func raftHelp(c types.CommandContext) {
	conn := c.Conn
	conn.WriteString("raft [ help | stats | nodes | leader ]")
}

func raftStats(c types.CommandContext) {
	conn := c.Conn
	stats := conf.Content.CRaft.Raft.Raft.Stats()
	conn.WriteArray(len(stats))
	for k, v := range stats {
		conn.WriteBulkString(k + ": " + v)
	}
}

func raftLeader(c types.CommandContext) {
	conn := c.Conn
	leader := conf.Content.CRaft.Raft.Raft.Leader()
	conn.WriteString("raft addr: " + string(leader))
}

func raftNode(c types.CommandContext) {
	conn := c.Conn
	text := conf.Content.CRaft.Raft.Raft.String()
	conn.WriteString(text)
}
