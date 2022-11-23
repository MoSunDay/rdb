package command

import (
	"encoding/json"
	"rdb/internal/conf"
	"rdb/internal/rtypes"
	types "rdb/internal/rtypes"
	"rdb/internal/utils"
	"time"
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
		"set":    raftSet,
		"SET":    raftSet,
		"get":    raftGet,
		"GET":    raftGet,
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
	stats := conf.Content.CRaft.Raft.Raft.Stats()
	conn.WriteString(text + ", nodes: " + stats["latest_configuration"])
}

func raftSet(c types.CommandContext) {
	conn, args := c.Conn, c.Args
	if len(args) != 3 {
		conn.WriteError("ERR wrong number of arguments for set command")
		return
	}

	event := rtypes.RaftLogEntryData{Key: utils.BytesToString(args[1]), Value: utils.BytesToString(args[2])}
	eventBytes, err := json.Marshal(event)
	if err != nil {
		conn.WriteError("internal error err: " + err.Error())
		return
	}

	applyFuture := conf.Content.CRaft.Raft.Raft.Apply(eventBytes, 5*time.Second)
	if err := applyFuture.Error(); err != nil {
		conn.WriteError("internal error err: " + err.Error())
		return
	}

	conn.WriteString("OK")
}

func raftGet(c types.CommandContext) {
	conn, args := c.Conn, c.Args
	if len(args) != 2 {
		conn.WriteError("ERR wrong number of arguments for get command")
		return
	}
	key := utils.BytesToString(args[1])
	val := conf.Content.CRaft.CM.Get(key)

	if val == "" {
		conn.WriteNull()
		return
	}
	conn.WriteString(val)
}
