package command

import (
	"encoding/json"
	"rdb/internal/conf"
	"rdb/internal/rtypes"
	types "rdb/internal/rtypes"
	"strings"
	"time"
)

func migrateHandler(c types.CommandContext) {
	args := c.Args
	if len(args) < 1 {
		migrateHelper(c)
		return
	}
	subCommand := map[string]func(c types.CommandContext){
		"help": migrateHelper,
		"task": migrateTaskHandler,
		"list": migrateListHandler,
	}
	if fn, ok := subCommand[string(args[0])]; ok {
		fn(c)
	} else {
		migrateHelper(c)
	}
}

func migrateHelper(c types.CommandContext) {
	conn := c.Conn
	conn.WriteError("migrate [ list | task ]")
}

func migrateListHandler(c types.CommandContext) {
	conn := c.Conn
	taskKey := "migrate_task"
	tasks := strings.Split(conf.Content.CRaft.CM.Get(taskKey), ",")
	conn.WriteArray(len(tasks))
	for _, item := range tasks {
		conn.WriteBulkString(strings.ReplaceAll(item, "_", " "))
	}
}

func migrateTaskHandler(c types.CommandContext) {
	conn, args := c.Conn, c.Args
	if len(args) != 4 {
		migrateHelper(c)
		return
	}
	taskKey := "migrate_task"
	tasks := conf.Content.CRaft.CM.Get(taskKey)
	val := string(args[1]) + "_" + string(args[2]) + "_" + string(args[3])
	if tasks != "" {
		tasks += "," + val
	}

	event := rtypes.RaftLogEntryData{Key: taskKey, Value: val}
	eventBytes, err := json.Marshal(event)
	if err != nil {
		conn.WriteError("Raft internal error")
		return
	}

	applyFuture := conf.Content.CRaft.Raft.Raft.Apply(eventBytes, 5*time.Second)
	if err := applyFuture.Error(); err != nil {
		conn.WriteError("Raft Apply failed")
		return
	}

	conn.WriteString("OK")
}
